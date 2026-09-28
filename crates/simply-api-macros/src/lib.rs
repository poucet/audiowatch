//! `#[api_service]` — one async trait, every surface derived.
//!
//! Ported from lumina's `simply-rpc` `#[rpc_service]` (server dispatch +
//! typed client from one trait) and its daemon `#[skill_router]` (rmcp tool
//! glue), re-targeted at rmcp 3.x: the MCP tool router is generated directly
//! from the trait, so a new capability is one trait method — never a
//! hand-written tool wrapper. See.
//!
//! ```rust,ignore
//! #[simply_api::api_service]
//! #[async_trait]
//! pub trait FluxApi: Send + Sync {
//!     /// Tool description comes from the doc comment.
//!     async fn add_node(
//!         &self,
//!         /// Field descriptions come from parameter doc comments.
//!         kind: String,
//!         x: Option<f32>,
//!     ) -> ApiResult<NodeAdded>;
//!
//!     #[api(no_tool)]
//!     async fn internal_only(&self) -> ApiResult<()>;
//!
//!     #[api(name = "render_wav")]
//!     async fn render(&self, seconds: f32) -> ApiResult<RenderResult>;
//! }
//! ```
//!
//! Generated alongside the (unchanged) trait: per-method params structs,
//! a `ServiceMeta` registry, an rmcp `ToolRouter` factory, a `Caller`
//! dispatcher, a `Remote{Trait}` client, and a `Dyn{Trait}` alias — see
//! [`codegen`] for the full list. Place `#[api_service]` **above**
//! `#[async_trait]`.

#![forbid(unsafe_code)]

use proc_macro::TokenStream;

mod codegen;
mod parse;

#[proc_macro_attribute]
pub fn api_service(attr: TokenStream, item: TokenStream) -> TokenStream {
    if !attr.is_empty() {
        return syn::Error::new(
            proc_macro2::Span::call_site(),
            "#[api_service] takes no arguments",
        )
        .to_compile_error()
        .into();
    }
    let input = syn::parse_macro_input!(item as syn::ItemTrait);
    match expand(input) {
        Ok(tokens) => tokens.into(),
        Err(e) => e.to_compile_error().into(),
    }
}

fn expand(mut input: syn::ItemTrait) -> syn::Result<proc_macro2::TokenStream> {
    let parsed = parse::ParsedTrait::from_item_trait(&input)?;
    let generated = codegen::generate(&parsed, &input.vis);

    // Strip our attributes before re-emitting the trait: `#[api(...)]` and
    // `#[op(...)]` on methods (the latter forwarded onto the generated
    // request-enum variant), and doc attributes on parameters (rustc rejects
    // docs there; they only feed the generated params-struct fields).
    for item in &mut input.items {
        if let syn::TraitItem::Fn(method) = item {
            method.attrs.retain(|attr| !attr.path().is_ident("api") && !attr.path().is_ident("op"));
            for arg in &mut method.sig.inputs {
                if let syn::FnArg::Typed(pat_type) = arg {
                    pat_type.attrs.clear();
                }
            }
        }
    }

    Ok(quote::quote! {
        #input
        #generated
    })
}
