//! `#[derive(Op)]` — operation metadata from `#[op(...)]` annotations.
//!
//! On an operation **enum** (one variant per operation — e.g. an api's
//! generated request enum) or a single operation **struct**, generates an
//! `impl simply_history::OpMeta`:
//!
//! ```rust,ignore
//! #[derive(Op)]
//! enum Request {
//!     /// Reads never enter history or a journal.
//!     #[op(read)] // the default — may be omitted
//!     GetGraph {},
//!
//!     /// Mutations do; the label template interpolates the fields.
//!     #[op(mutates, label = "connect {from}->{to}")]
//!     Connect { from: String, to: String },
//! }
//! ```
//!
//! - `mutates` / `read` — the classification `OpMeta::mutates` reports;
//!   history/journal machinery brackets transactions only around mutations.
//! - `label = "..."` — an inline-capture `format!` template over the
//!   operation's own (named) fields; without one, the label is the
//!   operation's snake_case name.
//! - `#[op(crate = "path")]` on the container re-points the generated
//!   `simply_history` path (for re-exports), serde-style.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;
use proc_macro2::TokenStream as TokenStream2;
use quote::{format_ident, quote};
use syn::{Data, DeriveInput, Fields, LitStr};

#[proc_macro_derive(Op, attributes(op))]
pub fn derive_op(item: TokenStream) -> TokenStream {
    let input = syn::parse_macro_input!(item as DeriveInput);
    match expand(input) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

/// One operation's parsed `#[op(...)]` metadata.
struct OpAttr {
    mutates: bool,
    label: Option<LitStr>,
}

fn parse_op_attr(attrs: &[syn::Attribute]) -> syn::Result<OpAttr> {
    let mut mutates = false;
    let mut read = false;
    let mut label = None;
    for attr in attrs {
        if !attr.path().is_ident("op") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("mutates") {
                mutates = true;
                Ok(())
            } else if meta.path.is_ident("read") {
                read = true;
                Ok(())
            } else if meta.path.is_ident("label") {
                label = Some(meta.value()?.parse::<LitStr>()?);
                Ok(())
            } else if meta.path.is_ident("crate") {
                // Container-level; consumed by `crate_path` — accept here so
                // a shared parser doesn't reject it.
                let _: LitStr = meta.value()?.parse()?;
                Ok(())
            } else {
                Err(meta.error(
                    "unsupported #[op(...)] key; expected `mutates`, `read`, or `label = \"...\"`",
                ))
            }
        })?;
    }
    if mutates && read {
        return Err(syn::Error::new_spanned(
            attrs.iter().find(|a| a.path().is_ident("op")).unwrap(),
            "#[op(...)] cannot be both `mutates` and `read`",
        ));
    }
    Ok(OpAttr { mutates, label })
}

/// The `simply_history` path the generated impl targets:
/// `#[op(crate = "...")]` on the container, else `::simply_history`.
fn crate_path(attrs: &[syn::Attribute]) -> syn::Result<TokenStream2> {
    let mut path = quote!(::simply_history);
    for attr in attrs {
        if !attr.path().is_ident("op") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            if meta.path.is_ident("crate") {
                let lit: LitStr = meta.value()?.parse()?;
                let parsed: syn::Path = lit.parse()?;
                path = quote!(#parsed);
            } else {
                // Other keys are operation-level; tolerate them here.
                let _ = meta.value().map(|v| v.parse::<LitStr>());
            }
            Ok(())
        })?;
    }
    Ok(path)
}

/// Which `{name}` placeholders a label template interpolates (`{{` escapes;
/// format specs after `:` are the template author's business).
fn template_fields(template: &str) -> Vec<String> {
    let mut fields = Vec::new();
    let mut chars = template.chars().peekable();
    while let Some(c) = chars.next() {
        if c != '{' {
            continue;
        }
        if chars.peek() == Some(&'{') {
            chars.next(); // escaped `{{`
            continue;
        }
        let mut name = String::new();
        for c in chars.by_ref() {
            if c == '}' || c == ':' {
                break;
            }
            name.push(c);
        }
        if !name.is_empty() {
            fields.push(name);
        }
    }
    fields
}

/// snake_case of a Pascal/camelCase name — the default label.
fn snake_case(s: &str) -> String {
    let mut out = String::new();
    for (i, c) in s.chars().enumerate() {
        if c.is_uppercase() {
            if i > 0 {
                out.push('_');
            }
            out.extend(c.to_lowercase());
        } else {
            out.push(c);
        }
    }
    out
}

/// The label expression for one operation, plus the destructuring pattern
/// binding exactly the fields the template references.
fn label_arm(
    fields: &Fields,
    attr: &OpAttr,
    name: &str,
    spanned: &dyn quote::ToTokens,
) -> syn::Result<(TokenStream2, TokenStream2)> {
    let Some(template) = &attr.label else {
        let text = snake_case(name);
        let pattern = match fields {
            Fields::Unit => quote!(),
            Fields::Named(_) => quote!({ .. }),
            Fields::Unnamed(_) => quote!((..)),
        };
        return Ok((pattern, quote!(::std::string::String::from(#text))));
    };
    let referenced = template_fields(&template.value());
    let named: Vec<String> = match fields {
        Fields::Named(named) => {
            named.named.iter().map(|f| f.ident.as_ref().unwrap().to_string()).collect()
        }
        _ if referenced.is_empty() => Vec::new(),
        _ => {
            return Err(syn::Error::new_spanned(
                spanned,
                "#[op(label = ...)] templates need named fields to interpolate",
            ))
        }
    };
    for field in &referenced {
        if !named.contains(field) {
            return Err(syn::Error::new_spanned(
                template,
                format!("label template references unknown field `{field}`"),
            ));
        }
    }
    let bind: Vec<syn::Ident> = referenced.iter().map(|f| format_ident!("{f}")).collect();
    let pattern = match fields {
        Fields::Unit => quote!(),
        Fields::Named(_) => quote!({ #(#bind,)* .. }),
        Fields::Unnamed(_) => quote!((..)),
    };
    Ok((pattern, quote!(::std::format!(#template))))
}

fn expand(input: DeriveInput) -> syn::Result<TokenStream2> {
    let ident = &input.ident;
    let history = crate_path(&input.attrs)?;
    let (impl_generics, ty_generics, where_clause) = input.generics.split_for_impl();

    let (mutates_body, label_body) = match &input.data {
        Data::Enum(data) => {
            let mut mutates_arms = Vec::new();
            let mut label_arms = Vec::new();
            for variant in &data.variants {
                let v_ident = &variant.ident;
                let attr = parse_op_attr(&variant.attrs)?;
                let wild = match &variant.fields {
                    Fields::Unit => quote!(),
                    Fields::Named(_) => quote!({ .. }),
                    Fields::Unnamed(_) => quote!((..)),
                };
                let mutates = attr.mutates;
                mutates_arms.push(quote!(Self::#v_ident #wild => #mutates,));
                let (pattern, expr) =
                    label_arm(&variant.fields, &attr, &v_ident.to_string(), variant)?;
                label_arms.push(quote!(Self::#v_ident #pattern => #expr,));
            }
            (quote!(match self { #(#mutates_arms)* }), quote!(match self { #(#label_arms)* }))
        }
        Data::Struct(data) => {
            let attr = parse_op_attr(&input.attrs)?;
            let mutates = attr.mutates;
            let (pattern, expr) = label_arm(&data.fields, &attr, &ident.to_string(), &input.ident)?;
            (quote!(#mutates), quote!(match self { Self #pattern => #expr }))
        }
        Data::Union(_) => {
            return Err(syn::Error::new_spanned(ident, "#[derive(Op)] supports enums and structs"))
        }
    };

    Ok(quote! {
        #[automatically_derived]
        impl #impl_generics #history::OpMeta for #ident #ty_generics #where_clause {
            fn mutates(&self) -> bool {
                #mutates_body
            }

            fn label(&self) -> ::std::string::String {
                #label_body
            }
        }
    })
}
