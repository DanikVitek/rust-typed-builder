use syn;
use proc_macro2::TokenStream;
use syn::spanned::Spanned;
use syn::parse::Error;
use quote::quote;

use util::{make_identifier, map_only_one, path_to_single_string};
use builder_attr::BuilderAttr;

pub struct FieldInfo<'a> {
    pub ordinal: usize,
    pub name: &'a syn::Ident,
    pub generic_ident: syn::Ident,
    pub ty: &'a syn::Type,
    // pub default: Option<TokenStream>,
}

impl<'a> FieldInfo<'a> {
    pub fn new(ordinal: usize, field: &syn::Field) -> Result<FieldInfo, Error> {
        if let Some(ref name) = field.ident {
            let builder_attr = Self::find_builder_attr(field)?;
            Ok(FieldInfo {
                ordinal: ordinal,
                name: &name,
                generic_ident: make_identifier("genericType", name),
                ty: &field.ty,
                // default: Self::find_builder_attr(field).unwrap_or_else(|f| panic!("Field {}: {}", name, f)),
            })
        } else {
            Err(Error::new(field.span(), "Nameless field in struct"))
        }
    }

    fn find_builder_attr(field: &syn::Field) -> Result<Option<BuilderAttr>, Error> {
        map_only_one(&field.attrs, |attr| {
            if path_to_single_string(&attr.path).as_ref().map(|s| &**s) == Some("builder") {
                Ok(Some(BuilderAttr::new(&attr.tts)?))
            } else {
                Ok(None)
            }
            // match attr.value {
                // syn::MetaItem::Word(ref name) if name == "default" => {
                    // Ok(Some(quote!(::std::default::Default::default())))
                // },
                // syn::MetaItem::List(ref name, _) if name == "default" => {
                    // Err("default can not be a list style attribute".into())
                // }
                // syn::MetaItem::NameValue(ref name, syn::Lit::Str(ref lit, _)) if name == "default" => {
                    // let field_value = syn::parse_token_trees(lit)?;
                    // Ok(Some(quote!(#( #field_value )*)))
                // },
                // _ => Ok(None)
            // }
        })
        // return Err(Error::new(field.span(), "bad"));
    }

    pub fn generic_ty_param(&self) -> syn::TypeParam {
        // syn::TypeParam::from(self.generic_ident.clone())
        panic!()
    }

    pub fn tuplized_type_ty_param(&self) -> syn :: TypeParam {
        let ref ty = self.ty;
        let quoted = quote!((#ty,));
        println!("hi");
        panic!();
        // syn::TypeParam::from(syn::Ident::new(quoted.into_string(), proc_macro2::Span::call_site()))
    }

    pub fn empty_ty_param() -> syn::TypeParam {
        syn::TypeParam::from(syn::Ident::new("()", proc_macro2::Span::call_site()))
    }
}
