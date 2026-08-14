//! `#[derive(Durable)]` for the `durable` crate.
//!
//! Turns a struct whose fields are durable schema types into a navigable schema:
//!
//! - implements `durable::Schema` for the struct,
//! - generates a `{Name}Fields` extension trait (implemented for
//!   `durable::Path<Name>`) with one navigator method per field, and
//! - adds `Name::root()` / `Name::namespaced(name)` constructors.
//!
//! Each field is assigned a stable numeric id from its declaration order, which
//! is encoded into the on-disk key. Reordering fields changes the layout; add new
//! fields at the end.

use proc_macro::TokenStream;
use quote::quote;
use syn::{parse_macro_input, Data, DeriveInput, Fields, Ident};

#[proc_macro_derive(Durable)]
pub fn derive_durable(input: TokenStream) -> TokenStream {
    let input = parse_macro_input!(input as DeriveInput);
    let name = &input.ident;
    let vis = &input.vis;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let fields = match &input.data {
        Data::Struct(data) => match &data.fields {
            Fields::Named(named) => &named.named,
            _ => {
                return syn::Error::new_spanned(
                    name,
                    "#[derive(Durable)] requires a struct with named fields",
                )
                .to_compile_error()
                .into();
            }
        },
        _ => {
            return syn::Error::new_spanned(name, "#[derive(Durable)] is only supported on structs")
                .to_compile_error()
                .into();
        }
    };

    let mut trait_methods = Vec::new();
    let mut impl_methods = Vec::new();

    for (index, field) in fields.iter().enumerate() {
        let field_ident = field.ident.as_ref().expect("named field");
        let field_ty = &field.ty;
        let field_id = index as u32;
        trait_methods.push(quote! {
            fn #field_ident(&self) -> ::durable::Path<#field_ty>;
        });
        impl_methods.push(quote! {
            fn #field_ident(&self) -> ::durable::Path<#field_ty> {
                self.child_field(#field_id)
            }
        });
    }

    let trait_name = Ident::new(&format!("{name}Fields"), name.span());
    let trait_doc = format!("Field navigators for [`{name}`], implemented for `durable::Path<{name}>`.");

    let mut describe_fields = Vec::new();
    for field in fields.iter() {
        let field_ident = field.ident.as_ref().expect("named field");
        let field_ty = &field.ty;
        let field_name = field_ident.to_string();
        describe_fields.push(quote! {
            (#field_name.to_string(), <#field_ty as ::durable::Describe>::shape())
        });
    }

    let expanded = quote! {
        impl #impl_generics ::durable::Schema for #name #ty_generics #where_clause {}

        impl #impl_generics ::durable::Describe for #name #ty_generics #where_clause {
            fn shape() -> ::durable::Shape {
                ::durable::Shape::record(vec![
                    #(#describe_fields),*
                ])
            }
        }

        #[doc = #trait_doc]
        #vis trait #trait_name {
            #(#trait_methods)*
        }

        impl #impl_generics #trait_name for ::durable::Path<#name #ty_generics> #where_clause {
            #(#impl_methods)*
        }

        impl #impl_generics #name #ty_generics #where_clause {
            /// The root path of this schema (empty prefix; one root per database).
            #vis fn root() -> ::durable::Path<#name #ty_generics> {
                ::durable::Path::root()
            }

            /// A root path namespaced under `name`, to share a database between schemas.
            #vis fn namespaced(name: &str) -> ::durable::Path<#name #ty_generics> {
                ::durable::Path::namespaced(name)
            }
        }
    };

    expanded.into()
}
