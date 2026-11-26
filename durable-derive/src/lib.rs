//! `#[derive(Durable)]` for the `durable` crate.
//!
//! Turns a struct whose fields are durable schema types into a navigable schema:
//!
//! - implements `durable::Schema` (including structural fingerprinting),
//! - generates a `{Name}Fields` extension trait (implemented for
//!   `durable::Path<Name>`) with one navigator method per field, and
//! - adds `Name::root()` / `Name::namespaced(name)` constructors.
//!
//! At the database root (`Name::root()`), each non-scalar field maps to its own
//! dedicated `redb` table named after the field (giving O(1) `len()` and
//! `clear()` on top-level collections); top-level scalar fields (`Leaf`, `Sum`)
//! live in `"__scalars"`. When nested inside another collection or struct, fields
//! live inline under their parent's table prefix addressed by declaration-order
//! field id.

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
    let mut schema_steps = Vec::new();
    let field_count = fields.len() as u64;

    for (index, field) in fields.iter().enumerate() {
        let field_ident = field.ident.as_ref().expect("named field");
        let field_ty = &field.ty;
        let field_name = field_ident.to_string();
        let field_id = index as u32;
        trait_methods.push(quote! {
            fn #field_ident(&self) -> ::durable::Path<#field_ty>;
        });
        impl_methods.push(quote! {
            fn #field_ident(&self) -> ::durable::Path<#field_ty> {
                self.child_field(#field_name, #field_id)
            }
        });
        schema_steps.push(quote! {
            let __name: &str = #field_name;
            out.extend_from_slice(&(__name.len() as u64).to_be_bytes());
            out.extend_from_slice(__name.as_bytes());
            <#field_ty as ::durable::Schema>::encode_schema(out);
        });
    }

    let trait_name = Ident::new(&format!("{name}Fields"), name.span());
    let trait_doc = format!(
        "Field navigators for [`{name}`], implemented for `durable::Path<{name}>`."
    );

    let expanded = quote! {
        impl #impl_generics ::durable::Schema for #name #ty_generics #where_clause {
            fn encode_schema(out: &mut ::std::vec::Vec<u8>) {
                out.push(5);
                out.extend_from_slice(&(#field_count).to_be_bytes());
                #(#schema_steps)*
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
            /// The root path of this schema (one root per database).
            #vis fn root() -> ::durable::Path<#name #ty_generics> {
                ::durable::Path::root()
            }

            /// A root path namespaced under `name`, isolating tables as `"{name}/{field}"`.
            #vis fn namespaced(name: &str) -> ::durable::Path<#name #ty_generics> {
                ::durable::Path::namespaced(name)
            }
        }
    };

    expanded.into()
}
