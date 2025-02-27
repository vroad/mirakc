use proc_macro2::Span;
use proc_macro2::TokenStream;
use quote::quote;
use quote::ToTokens;

#[proc_macro_derive(Message, attributes(reply))]
pub fn message_derive(input: proc_macro::TokenStream) -> proc_macro::TokenStream {
    let ast = syn::parse(input).unwrap();
    expand(&ast).into()
}

// Took from actix-derive/src/message.rs

const MESSAGE_ATTR: &str = "reply";

fn expand(ast: &syn::DeriveInput) -> TokenStream {
    let item_type = {
        match get_attribute_type_multiple(ast, MESSAGE_ATTR) {
            Ok(ty) => match ty.len() {
                1 => ty[0].clone(),
                _ => {
                    return syn::Error::new(
                        Span::call_site(),
                        format!(
                            "#[{}(type)] takes 1 parameters, given {}",
                            MESSAGE_ATTR,
                            ty.len()
                        ),
                    )
                    .to_compile_error()
                }
            },
            Err(_) => None,
        }
    };

    let name = &ast.ident;
    let (impl_generics, ty_generics, where_clause) = ast.generics.split_for_impl();

    let message_type_trait = if item_type.is_some() {
        quote! { actlet::Action }
    } else {
        quote! { actlet::Signal }
    };

    let item_type = item_type
        .map(ToTokens::into_token_stream)
        .unwrap_or_else(|| quote! { () });

    quote! {
        impl #impl_generics actlet::Message for #name #ty_generics #where_clause {
            type Reply = #item_type;
        }

        impl #impl_generics #message_type_trait for #name #ty_generics #where_clause {}
    }
}

fn get_attribute_type_multiple(
    ast: &syn::DeriveInput,
    name: &str,
) -> syn::Result<Vec<Option<syn::Type>>> {
    let attr = ast
        .attrs
        .iter()
        .find_map(|a| {
            let a = a.parse_meta();
            match a {
                Ok(meta) => {
                    if meta.path().is_ident(name) {
                        Some(meta)
                    } else {
                        None
                    }
                }
                _ => None,
            }
        })
        .ok_or_else(|| {
            syn::Error::new(Span::call_site(), format!("Expect an attribute `{}`", name))
        })?;

    if let syn::Meta::List(ref list) = attr {
        Ok(list
            .nested
            .iter()
            .map(|m| meta_item_to_ty(m).ok())
            .collect())
    } else {
        Err(syn::Error::new_spanned(
            attr,
            format!("The correct syntax is #[{}(type, type, ...)]", name),
        ))
    }
}

fn meta_item_to_ty(meta_item: &syn::NestedMeta) -> syn::Result<syn::Type> {
    match meta_item {
        syn::NestedMeta::Meta(syn::Meta::Path(ref path)) => match path.get_ident() {
            Some(ident) => syn::parse_str::<syn::Type>(&ident.to_string())
                .map_err(|_| syn::Error::new_spanned(ident, "Expect type")),
            None => Err(syn::Error::new_spanned(path, "Expect type")),
        },
        syn::NestedMeta::Meta(syn::Meta::NameValue(val)) => match val.path.get_ident() {
            Some(ident) if ident == "result" => {
                if let syn::Lit::Str(ref s) = val.lit {
                    if let Ok(ty) = syn::parse_str::<syn::Type>(&s.value()) {
                        return Ok(ty);
                    }
                }
                Err(syn::Error::new_spanned(&val.lit, "Expect type"))
            }
            _ => Err(syn::Error::new_spanned(
                &val.lit,
                r#"Expect `result = "TYPE"`"#,
            )),
        },
        syn::NestedMeta::Lit(syn::Lit::Str(ref s)) => syn::parse_str::<syn::Type>(&s.value())
            .map_err(|_| syn::Error::new_spanned(s, "Expect type")),

        meta => Err(syn::Error::new_spanned(meta, "Expect type")),
    }
}
