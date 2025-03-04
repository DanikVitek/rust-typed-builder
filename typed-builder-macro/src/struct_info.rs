use std::cell::OnceCell;
use std::rc::Rc;

use proc_macro2::{Ident, Span, TokenStream};
use quote::{format_ident, quote, quote_spanned, ToTokens};
use syn::punctuated::Punctuated;
use syn::{parse_quote, Error, GenericArgument, ItemFn, Token};

use crate::field_info::{FieldBuilderAttr, FieldInfo};
use crate::mutator::Mutator;
use crate::util::{
    empty_type, empty_type_tuple, first_visibility, modify_types_generics_hack, path_to_single_string, public_visibility,
    strip_raw_ident_prefix, type_tuple, ApplyMeta, AttrArg,
};

#[derive(Debug)]
pub struct StructInfo<'a> {
    pub vis: &'a syn::Visibility,
    pub name: &'a syn::Ident,
    pub generics: &'a syn::Generics,
    pub fields: Box<[FieldInfo<'a>]>,

    pub builder_attr: TypeBuilderAttr<'a>,
    pub builder_name: syn::Ident,
    pub core: syn::Ident,
}

impl<'a> StructInfo<'a> {
    pub fn included_fields(&self) -> impl Iterator<Item = &FieldInfo<'a>> {
        self.fields.iter().filter(|f| f.builder_attr.setter.skip.is_none())
    }
    pub fn setter_fields(&self) -> impl Iterator<Item = &FieldInfo<'a>> {
        self.included_fields().filter(|f| f.builder_attr.via_mutators.is_none())
    }

    pub fn generic_arguments(&self) -> Punctuated<GenericArgument, Token![,]> {
        self.generics
            .params
            .iter()
            .map(|generic_param| match generic_param {
                syn::GenericParam::Type(type_param) => {
                    let ident = type_param.ident.to_token_stream();
                    syn::parse2(ident).unwrap()
                }
                syn::GenericParam::Lifetime(lifetime_def) => syn::GenericArgument::Lifetime(lifetime_def.lifetime.clone()),
                syn::GenericParam::Const(const_param) => {
                    let ident = const_param.ident.to_token_stream();
                    syn::parse2(ident).unwrap()
                }
            })
            .collect()
    }

    pub fn new(ast: &'a syn::DeriveInput, fields: impl Iterator<Item = &'a syn::Field>) -> syn::Result<Self> {
        let builder_attr = TypeBuilderAttr::new(&ast.attrs)?;
        let builder_name = builder_attr
            .builder_type
            .get_name()
            .map(|name| strip_raw_ident_prefix(name.to_string()))
            .unwrap_or_else(|| strip_raw_ident_prefix(format!("{}Builder", ast.ident)));
        Ok(StructInfo {
            vis: &ast.vis,
            name: &ast.ident,
            generics: &ast.generics,
            fields: fields
                .enumerate()
                .map(|(i, f)| FieldInfo::new(i, f, builder_attr.field_defaults.clone()))
                .collect::<Result<_, _>>()?,
            builder_attr,
            builder_name: syn::Ident::new(&builder_name, proc_macro2::Span::call_site()),
            core: syn::Ident::new(&format!("{}_core", builder_name), proc_macro2::Span::call_site()),
        })
    }

    pub fn builder_creation_impl(&self) -> syn::Result<TokenStream> {
        let StructInfo {
            vis,
            ref name,
            ref builder_name,
            ..
        } = *self;
        let (impl_generics, ty_generics, where_clause) = self.generics.split_for_impl();
        let init_fields_type = type_tuple(self.included_fields().map(|f| {
            if f.builder_attr.via_mutators.is_some() {
                f.tuplized_type_ty_param()
            } else {
                empty_type()
            }
        }));
        let builder_method_const = Rc::new(OnceCell::new());
        let init_fields_expr = self
            .included_fields()
            .map({
                let builder_method_const = builder_method_const.clone();
                move |f| {
                    f.builder_attr.via_mutators.as_ref().map_or_else(
                        || quote!(()),
                        |via_mutators| {
                            let init = &via_mutators.init;
                            if !matches!(init, syn::Expr::Lit(_)) {
                                _ = builder_method_const.set(quote!());
                            }
                            quote!((#init,))
                        },
                    )
                }
            })
            .collect::<Box<[_]>>();
        let builder_method_const = Rc::into_inner(builder_method_const).unwrap();
        let builder_method_const = OnceCell::into_inner(builder_method_const).unwrap_or_else(|| quote!(const));
        let mut all_fields_param_type: syn::TypeParam =
            syn::Ident::new("TypedBuilderFields", proc_macro2::Span::call_site()).into();
        let all_fields_param = syn::GenericParam::Type(all_fields_param_type.clone());
        all_fields_param_type.default = Some(syn::Type::Tuple(init_fields_type.clone()));
        let b_generics = {
            let mut generics = self.generics.clone();
            generics.params.push(syn::GenericParam::Type(all_fields_param_type));
            generics
        };
        let generics_with_empty = modify_types_generics_hack(&ty_generics, |args| {
            args.push(syn::GenericArgument::Type(init_fields_type.clone().into()));
        });
        let phantom_generics = self.generics.params.iter().filter_map(|param| match param {
            syn::GenericParam::Lifetime(lifetime) => {
                let lifetime = &lifetime.lifetime;
                Some(quote!(&#lifetime ()))
            }
            syn::GenericParam::Type(ty) => {
                let ty = &ty.ident;
                Some(ty.to_token_stream())
            }
            syn::GenericParam::Const(_cnst) => None,
        });

        let builder_method_name = self.builder_attr.builder_method.get_name().unwrap_or_else(|| quote!(builder));
        let builder_method_visibility = first_visibility(&[
            self.builder_attr.builder_method.vis.as_ref(),
            self.builder_attr.builder_type.vis.as_ref(),
            Some(vis),
        ]);
        let builder_method_doc = self.builder_attr.builder_method.get_doc_or(|| {
            format!(
                "
                Create a builder for building `{name}`.
                On the builder, call {setters} to set the values of the fields.
                Finally, call `.build()` to create the instance of `{name}`.
                ",
                name = self.name,
                setters = {
                    let mut result = String::new();
                    let mut is_first = true;
                    for field in self.setter_fields() {
                        use std::fmt::Write;
                        if is_first {
                            is_first = false;
                        } else {
                            write!(&mut result, ", ").unwrap();
                        }
                        write!(&mut result, "`.{}(...)`", field.name).unwrap();
                        if field.builder_attr.default.is_some() {
                            write!(&mut result, "(optional)").unwrap();
                        }
                    }
                    result
                }
            )
        });

        let builder_type_visibility = first_visibility(&[self.builder_attr.builder_type.vis.as_ref(), Some(vis)]);
        let builder_type_doc = if self.builder_attr.doc {
            self.builder_attr.builder_type.get_doc_or(|| {
                format!(
                    "Builder for [`{name}`] instances.\n\nSee [`{name}::builder()`] for more info.",
                    name = name
                )
            })
        } else {
            quote!(#[doc(hidden)])
        };

        let (b_generics_impl, b_generics_ty, b_generics_where_extras_predicates) = b_generics.split_for_impl();
        let mut b_generics_where: syn::WhereClause = syn::parse2(quote! {
            where TypedBuilderFields: Clone
        })?;
        if let Some(predicates) = b_generics_where_extras_predicates {
            b_generics_where.predicates.extend(predicates.predicates.clone());
        }

        Ok(quote! {
            #[automatically_derived]
            impl #impl_generics #name #ty_generics #where_clause {
                #builder_method_doc
                #[allow(dead_code, clippy::default_trait_access)]
                #builder_method_visibility #builder_method_const fn #builder_method_name() -> #builder_name #generics_with_empty {
                    #builder_name {
                        fields: (#(#init_fields_expr,)*),
                        phantom: ::core::marker::PhantomData,
                    }
                }
            }

            #[must_use]
            #builder_type_doc
            #[allow(dead_code, non_camel_case_types, non_snake_case)]
            #builder_type_visibility struct #builder_name #b_generics {
                fields: #all_fields_param,
                phantom: ::core::marker::PhantomData<(#( #phantom_generics ),*)>,
            }

            #[automatically_derived]
            impl #b_generics_impl Clone for #builder_name #b_generics_ty #b_generics_where {
                #[allow(clippy::default_trait_access)]
                fn clone(&self) -> Self {
                    Self {
                        fields: self.fields.clone(),
                        phantom: ::core::marker::PhantomData,
                    }
                }
            }
        })
    }

    pub fn field_impl(&self, field: &FieldInfo<'_>) -> syn::Result<TokenStream> {
        let StructInfo { ref builder_name, .. } = *self;

        let descructuring = self.included_fields().map(|f| {
            if f.ordinal == field.ordinal {
                quote!(())
            } else {
                let name = f.name;
                name.to_token_stream()
            }
        });
        let reconstructing = self.included_fields().map(|f| f.name);

        let &FieldInfo {
            name: field_name,
            ty: field_type,
            ..
        } = field;
        let mut ty_generics = self.generic_arguments();
        let mut target_generics_tuple = empty_type_tuple();
        let mut ty_generics_tuple = empty_type_tuple();
        let generics = {
            let mut generics = self.generics.clone();
            for f in self.included_fields() {
                if f.ordinal == field.ordinal {
                    ty_generics_tuple.elems.push_value(empty_type());
                    target_generics_tuple.elems.push_value(f.tuplized_type_ty_param());
                } else {
                    generics.params.push(f.generic_ty_param());
                    let generic_argument: syn::Type = f.type_ident();
                    ty_generics_tuple.elems.push_value(generic_argument.clone());
                    target_generics_tuple.elems.push_value(generic_argument);
                }
                ty_generics_tuple.elems.push_punct(Default::default());
                target_generics_tuple.elems.push_punct(Default::default());
            }
            generics
        };
        let mut target_generics = ty_generics.clone();
        target_generics.push(syn::GenericArgument::Type(target_generics_tuple.into()));
        ty_generics.push(syn::GenericArgument::Type(ty_generics_tuple.into()));
        let (impl_generics, _, where_clause) = generics.split_for_impl();
        let doc = field.builder_attr.setter.doc.as_ref().map(|doc| quote!(#[doc = #doc]));
        let deprecated = &field.builder_attr.deprecated;

        // NOTE: both auto_into and strip_option affect `arg_type` and `arg_expr`, but the order of
        // nesting is different so we have to do this little dance.
        let arg_type = if field.builder_attr.setter.strip_option.is_some() && field.builder_attr.setter.transform.is_none() {
            field
                .type_from_inside_option()
                .ok_or_else(|| Error::new_spanned(field_type, "can't `strip_option` - field is not `Option<...>`"))?
        } else {
            field_type
        };
        let (arg_type, arg_expr) = if field.builder_attr.setter.auto_into.is_some() {
            (quote!(impl ::core::convert::Into<#arg_type>), quote!(#field_name.into()))
        } else {
            (arg_type.to_token_stream(), field_name.to_token_stream())
        };

        let (param_list, arg_expr) = if field.builder_attr.setter.strip_bool.is_some() {
            (quote!(), quote!(true))
        } else if let Some(transform) = &field.builder_attr.setter.transform {
            let params = transform.params.iter().map(|(pat, ty)| quote!(#pat: #ty));
            let body = &transform.body;
            (quote!(#(#params),*), quote!({ #body }))
        } else if field.builder_attr.setter.strip_option.is_some() {
            (quote!(#field_name: #arg_type), quote!(Some(#arg_expr)))
        } else {
            (quote!(#field_name: #arg_type), arg_expr)
        };

        let repeated_fields_error_type_name = syn::Ident::new(
            &format!(
                "{}_Error_Repeated_field_{}",
                builder_name,
                strip_raw_ident_prefix(field_name.to_string())
            ),
            proc_macro2::Span::call_site(),
        );
        let repeated_fields_error_message = format!("Repeated field {}", field_name);

        let method_name = field.setter_method_name();

        Ok(quote! {
            #[allow(dead_code, non_camel_case_types, missing_docs)]
            #[automatically_derived]
            impl #impl_generics #builder_name <#ty_generics> #where_clause {
                #deprecated
                #doc
                #[allow(clippy::used_underscore_binding)]
                pub fn #method_name (self, #param_list) -> #builder_name <#target_generics> {
                    let #field_name = (#arg_expr,);
                    let ( #(#descructuring,)* ) = self.fields;
                    #builder_name {
                        fields: ( #(#reconstructing,)* ),
                        phantom: self.phantom,
                    }
                }
            }
            #[doc(hidden)]
            #[allow(dead_code, non_camel_case_types, non_snake_case)]
            #[allow(clippy::exhaustive_enums)]
            pub enum #repeated_fields_error_type_name {}
            #[doc(hidden)]
            #[allow(dead_code, non_camel_case_types, missing_docs)]
            #[automatically_derived]
            impl #impl_generics #builder_name <#target_generics> #where_clause {
                #[deprecated(
                    note = #repeated_fields_error_message
                )]
                pub fn #method_name (self, _: #repeated_fields_error_type_name) -> #builder_name <#target_generics> {
                    self
                }
            }
        })
    }

    pub fn required_field_impl(&self, field: &FieldInfo<'_>) -> TokenStream {
        let StructInfo { builder_name, .. } = self;

        let FieldInfo { name: field_name, .. } = *field;
        let mut builder_generics: Vec<syn::GenericArgument> = self
            .generics
            .params
            .iter()
            .map(|generic_param| match generic_param {
                syn::GenericParam::Type(type_param) => {
                    let ident = type_param.ident.to_token_stream();
                    syn::parse2(ident).unwrap()
                }
                syn::GenericParam::Lifetime(lifetime_def) => syn::GenericArgument::Lifetime(lifetime_def.lifetime.clone()),
                syn::GenericParam::Const(const_param) => {
                    let ident = const_param.ident.to_token_stream();
                    syn::parse2(ident).unwrap()
                }
            })
            .collect();
        let mut builder_generics_tuple = empty_type_tuple();
        let generics = {
            let mut generics = self.generics.clone();
            for f in self.included_fields() {
                if f.builder_attr.default.is_some() || f.builder_attr.via_mutators.is_some() {
                    // `f` is not mandatory - it does not have it's own fake `build` method, so `field` will need
                    // to warn about missing `field` whether or not `f` is set.
                    assert!(
                        f.ordinal != field.ordinal,
                        "`required_field_impl` called for optional field {}",
                        field.name
                    );
                    generics.params.push(f.generic_ty_param());
                    builder_generics_tuple.elems.push_value(f.type_ident());
                } else if f.ordinal < field.ordinal {
                    // Only add a `build` method that warns about missing `field` if `f` is set. If `f` is not set,
                    // `f`'s `build` method will warn, since it appears earlier in the argument list.
                    builder_generics_tuple.elems.push_value(f.tuplized_type_ty_param());
                } else if f.ordinal == field.ordinal {
                    builder_generics_tuple.elems.push_value(empty_type());
                } else {
                    // `f` appears later in the argument list after `field`, so if they are both missing we will
                    // show a warning for `field` and not for `f` - which means this warning should appear whether
                    // or not `f` is set.
                    generics.params.push(f.generic_ty_param());
                    builder_generics_tuple.elems.push_value(f.type_ident());
                }

                builder_generics_tuple.elems.push_punct(Default::default());
            }
            generics
        };

        builder_generics.push(syn::GenericArgument::Type(builder_generics_tuple.into()));
        let (impl_generics, _, where_clause) = generics.split_for_impl();

        let early_build_error_type_name = syn::Ident::new(
            &format!(
                "{}_Error_Missing_required_field_{}",
                builder_name,
                strip_raw_ident_prefix(field_name.to_string())
            ),
            proc_macro2::Span::call_site(),
        );
        let early_build_error_message = format!("Missing required field {}", field_name);

        let build_method_name = self.build_method_name();
        let build_method_visibility = self.build_method_visibility();

        quote! {
            #[doc(hidden)]
            #[allow(dead_code, non_camel_case_types, non_snake_case)]
            #[allow(clippy::exhaustive_enums)]
            pub enum #early_build_error_type_name {}
            #[doc(hidden)]
            #[allow(dead_code, non_camel_case_types, missing_docs, clippy::panic)]
            #[automatically_derived]
            impl #impl_generics #builder_name < #( #builder_generics ),* > #where_clause {
                #[deprecated(
                    note = #early_build_error_message
                )]
                #build_method_visibility fn #build_method_name(self, _: #early_build_error_type_name) -> ! {
                    panic!()
                }
            }
        }
    }

    pub fn mutator_impl(
        &self,
        mutator @ Mutator {
            fun: mutator_fn,
            required_fields,
        }: &Mutator,
    ) -> syn::Result<TokenStream> {
        let StructInfo { ref builder_name, .. } = *self;

        let mut required_fields = required_fields.clone();

        let mut ty_generics = self.generic_arguments();
        let mut destructuring = TokenStream::new();
        let mut ty_generics_tuple = empty_type_tuple();
        let mut generics = self.generics.clone();
        let mut mutator_ty_fields = Punctuated::<_, Token![,]>::new();
        let mut mutator_destructure_fields = Punctuated::<_, Token![,]>::new();
        for f @ FieldInfo { name, ty, .. } in self.included_fields() {
            if f.builder_attr.via_mutators.is_some() || required_fields.remove(f.name) {
                ty_generics_tuple.elems.push(f.tuplized_type_ty_param());
                mutator_ty_fields.push(quote!(#name: #ty));
                mutator_destructure_fields.push(name);
                quote!((#name,),).to_tokens(&mut destructuring);
            } else {
                generics.params.push(f.generic_ty_param());
                let generic_argument: syn::Type = f.type_ident();
                ty_generics_tuple.elems.push(generic_argument.clone());
                quote!(#name,).to_tokens(&mut destructuring);
            }
        }
        ty_generics.push(syn::GenericArgument::Type(ty_generics_tuple.into()));
        let (impl_generics, _, where_clause) = generics.split_for_impl();

        let mutator_struct_name = format_ident!("TypedBuilderFieldMutator");

        let ItemFn { attrs, vis, .. } = mutator_fn;
        let sig = mutator.outer_sig(parse_quote!(#builder_name <#ty_generics>));
        let fn_name = &sig.ident;
        let mutator_args = mutator.arguments();

        Ok(quote! {
            #[allow(dead_code, non_camel_case_types, missing_docs)]
            #[automatically_derived]
            impl #impl_generics #builder_name <#ty_generics> #where_clause {
                #(#attrs)*
                #[allow(clippy::used_underscore_binding)]
                #vis #sig {
                    struct #mutator_struct_name {
                        #mutator_ty_fields
                    }
                    impl #mutator_struct_name {
                        #mutator_fn
                    }

                    let __args = (#mutator_args);

                    let ( #destructuring ) = self.fields;
                    let mut __mutator = #mutator_struct_name{ #mutator_destructure_fields };

                    // This dance is required to keep mutator args and destrucutre fields from interfering.
                    {
                        let (#mutator_args) = __args;
                        __mutator.#fn_name(#mutator_args);
                    }

                    let #mutator_struct_name {
                        #mutator_destructure_fields
                    } = __mutator;

                    #builder_name {
                        fields: ( #destructuring ),
                        phantom: self.phantom,
                    }
                }
            }
        })
    }

    fn build_method_name(&self) -> TokenStream {
        self.builder_attr.build_method.common.get_name().unwrap_or(quote!(build))
    }

    fn build_method_visibility(&self) -> TokenStream {
        first_visibility(&[self.builder_attr.build_method.common.vis.as_ref(), Some(&public_visibility())])
    }

    pub fn build_method_impl(&self) -> TokenStream {
        let StructInfo {
            ref name,
            ref builder_name,
            ..
        } = *self;

        let generics = {
            let mut generics = self.generics.clone();
            for field in self.included_fields() {
                if field.builder_attr.default.is_some() {
                    let trait_ref = syn::TraitBound {
                        paren_token: None,
                        lifetimes: None,
                        modifier: syn::TraitBoundModifier::None,
                        path: {
                            let mut path = self.builder_attr.crate_module_path.clone();
                            path.segments.push(syn::PathSegment {
                                ident: Ident::new("Optional", Span::call_site()),
                                arguments: syn::PathArguments::AngleBracketed(syn::AngleBracketedGenericArguments {
                                    colon2_token: None,
                                    lt_token: Default::default(),
                                    args: [syn::GenericArgument::Type(field.ty.clone())].into_iter().collect(),
                                    gt_token: Default::default(),
                                }),
                            });
                            path
                        },
                    };
                    let mut generic_param: syn::TypeParam = field.generic_ident.clone().into();
                    generic_param.bounds.push(trait_ref.into());
                    generics.params.push(generic_param.into());
                }
            }
            generics
        };
        let (impl_generics, _, _) = generics.split_for_impl();

        let (_, ty_generics, where_clause) = self.generics.split_for_impl();

        let modified_ty_generics = modify_types_generics_hack(&ty_generics, |args| {
            args.push(syn::GenericArgument::Type(
                type_tuple(self.included_fields().map(|field| {
                    if field.builder_attr.default.is_some() {
                        field.type_ident()
                    } else {
                        field.tuplized_type_ty_param()
                    }
                }))
                .into(),
            ));
        });

        let descructuring = self.included_fields().map(|f| f.name);

        // The default of a field can refer to earlier-defined fields, which we handle by
        // writing out a bunch of `let` statements first, which can each refer to earlier ones.
        // This means that field ordering may actually be significant, which isn't ideal. We could
        // relax that restriction by calculating a DAG of field default dependencies and
        // reordering based on that, but for now this much simpler thing is a reasonable approach.
        let assignments = self.fields.iter().map(|field| {
            let name = &field.name;

            let maybe_mut = if let Some(span) = field.builder_attr.mutable_during_default_resolution {
                quote_spanned!(span => mut)
            } else {
                quote!()
            };

            if let Some(ref default) = field.builder_attr.default {
                if field.builder_attr.setter.skip.is_some() {
                    quote!(let #maybe_mut #name = #default;)
                } else {
                    let crate_module_path = &self.builder_attr.crate_module_path;

                    quote!(let #maybe_mut #name = #crate_module_path::Optional::into_value(#name, || #default);)
                }
            } else {
                quote!(let #maybe_mut #name = #name.0;)
            }
        });
        let field_names = self.fields.iter().map(|field| field.name);

        let build_method_name = self.build_method_name();
        let build_method_visibility = self.build_method_visibility();
        let build_method_doc = if self.builder_attr.doc {
            self.builder_attr
                .build_method
                .common
                .get_doc_or(|| format!("Finalise the builder and create its [`{}`] instance", name))
        } else {
            quote!()
        };

        let type_constructor = {
            let ty_generics = ty_generics.as_turbofish();
            quote!(#name #ty_generics)
        };

        let (build_method_generic, output_type, build_method_where_clause) = match &self.builder_attr.build_method.into {
            IntoSetting::NoConversion => (None, quote!(#name #ty_generics), None),
            IntoSetting::GenericConversion => (
                Some(quote!(<__R>)),
                quote!(__R),
                Some(quote!(where #name #ty_generics: Into<__R>)),
            ),
            IntoSetting::TypeConversionToSpecificType(into) => (None, into.to_token_stream(), None),
        };

        quote!(
            #[allow(dead_code, non_camel_case_types, missing_docs)]
            #[automatically_derived]
            impl #impl_generics #builder_name #modified_ty_generics #where_clause {
                #build_method_doc
                #[allow(clippy::default_trait_access, clippy::used_underscore_binding)]
                #build_method_visibility fn #build_method_name #build_method_generic (self) -> #output_type #build_method_where_clause {
                    let ( #(#descructuring,)* ) = self.fields;
                    #( #assignments )*

                    #[allow(deprecated)]
                    #type_constructor {
                        #( #field_names ),*
                    }.into()
                }
            }
        )
    }
}

#[derive(Debug, Default, Clone)]
pub struct CommonDeclarationSettings {
    pub vis: Option<syn::Visibility>,
    pub name: Option<syn::Expr>,
    pub doc: Option<syn::Expr>,
}

impl ApplyMeta for CommonDeclarationSettings {
    fn apply_meta(&mut self, expr: AttrArg) -> syn::Result<()> {
        match expr.name().to_string().as_str() {
            "vis" => {
                let expr_str = expr.key_value()?.parse_value::<syn::LitStr>()?.value();
                self.vis = Some(syn::parse_str(&expr_str)?);
                Ok(())
            }
            "name" => {
                self.name = Some(expr.key_value()?.parse_value()?);
                Ok(())
            }
            "doc" => {
                self.doc = Some(expr.key_value()?.parse_value()?);
                Ok(())
            }
            _ => Err(Error::new_spanned(
                expr.name(),
                format!("Unknown parameter {:?}", expr.name().to_string()),
            )),
        }
    }
}

impl CommonDeclarationSettings {
    fn get_name(&self) -> Option<TokenStream> {
        self.name.as_ref().map(|name| name.to_token_stream())
    }

    fn get_doc_or(&self, gen_doc: impl FnOnce() -> String) -> TokenStream {
        if let Some(ref doc) = self.doc {
            quote!(#[doc = #doc])
        } else {
            let doc = gen_doc();
            quote!(#[doc = #doc])
        }
    }
}

/// Setting of the `into` argument.
#[derive(Debug, Clone)]
pub enum IntoSetting {
    /// Do not run any conversion on the built value.
    NoConversion,
    /// Convert the build value into the generic parameter passed to the `build` method.
    GenericConversion,
    /// Convert the build value into a specific type specified in the attribute.
    TypeConversionToSpecificType(syn::TypePath),
}

impl Default for IntoSetting {
    fn default() -> Self {
        Self::NoConversion
    }
}

#[derive(Debug, Default, Clone)]
pub struct BuildMethodSettings {
    pub common: CommonDeclarationSettings,

    /// Whether to convert the built type into another while finishing the build.
    pub into: IntoSetting,
}

impl ApplyMeta for BuildMethodSettings {
    fn apply_meta(&mut self, expr: AttrArg) -> syn::Result<()> {
        match expr.name().to_string().as_str() {
            "into" => match expr {
                AttrArg::Flag(_) => {
                    self.into = IntoSetting::GenericConversion;
                    Ok(())
                }
                AttrArg::KeyValue(key_value) => {
                    let type_path = key_value.parse_value::<syn::TypePath>()?;
                    self.into = IntoSetting::TypeConversionToSpecificType(type_path);
                    Ok(())
                }
                _ => Err(expr.incorrect_type()),
            },
            _ => self.common.apply_meta(expr),
        }
    }
}

#[derive(Debug)]
pub struct TypeBuilderAttr<'a> {
    /// Whether to show docs for the `TypeBuilder` type (rather than hiding them).
    pub doc: bool,

    /// Customize builder method, ex. visibility, name
    pub builder_method: CommonDeclarationSettings,

    /// Customize builder type, ex. visibility, name
    pub builder_type: CommonDeclarationSettings,

    /// Customize build method, ex. visibility, name
    pub build_method: BuildMethodSettings,

    pub field_defaults: FieldBuilderAttr<'a>,

    pub crate_module_path: syn::Path,

    /// Functions that are able to mutate fields in the builder that are already set
    pub mutators: Vec<Mutator>,
}

impl Default for TypeBuilderAttr<'_> {
    fn default() -> Self {
        Self {
            doc: Default::default(),
            builder_method: Default::default(),
            builder_type: Default::default(),
            build_method: Default::default(),
            field_defaults: Default::default(),
            crate_module_path: syn::parse_quote!(::typed_builder),
            mutators: Default::default(),
        }
    }
}

impl TypeBuilderAttr<'_> {
    pub fn new(attrs: &[syn::Attribute]) -> syn::Result<Self> {
        let mut result = Self::default();

        for attr in attrs {
            let list = match &attr.meta {
                syn::Meta::List(list) => {
                    if path_to_single_string(&list.path).as_deref() != Some("builder") {
                        continue;
                    }

                    list
                }
                _ => continue,
            };

            result.apply_subsections(list)?;
        }

        if result.builder_type.doc.is_some() || result.build_method.common.doc.is_some() {
            result.doc = true;
        }

        Ok(result)
    }
}

impl ApplyMeta for TypeBuilderAttr<'_> {
    fn apply_meta(&mut self, expr: AttrArg) -> syn::Result<()> {
        match expr.name().to_string().as_str() {
            "crate_module_path" => {
                let crate_module_path = expr.key_value()?.parse_value::<syn::ExprPath>()?;
                self.crate_module_path = crate_module_path.path;
                Ok(())
            }
            "builder_method_doc" => Err(Error::new_spanned(
                expr.name(),
                "`builder_method_doc` is deprecated - use `builder_method(doc = \"...\")`",
            )),
            "builder_type_doc" => Err(Error::new_spanned(
                expr.name(),
                "`builder_typemethod_doc` is deprecated - use `builder_type(doc = \"...\")`",
            )),
            "build_method_doc" => Err(Error::new_spanned(
                expr.name(),
                "`build_method_doc` is deprecated - use `build_method(doc = \"...\")`",
            )),
            "doc" => {
                expr.flag()?;
                self.doc = true;
                Ok(())
            }
            "mutators" => {
                self.mutators.extend(expr.sub_attr()?.undelimited()?);
                Ok(())
            }
            "field_defaults" => self.field_defaults.apply_sub_attr(expr.sub_attr()?),
            "builder_method" => self.builder_method.apply_sub_attr(expr.sub_attr()?),
            "builder_type" => self.builder_type.apply_sub_attr(expr.sub_attr()?),
            "build_method" => self.build_method.apply_sub_attr(expr.sub_attr()?),
            _ => Err(Error::new_spanned(
                expr.name(),
                format!("Unknown parameter {:?}", expr.name().to_string()),
            )),
        }
    }
}
