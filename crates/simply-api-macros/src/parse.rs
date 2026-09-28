//! Parse an `#[api_service]` trait into the shape codegen consumes. The
//! pattern (an async trait as the single source of truth for server
//! dispatch, typed clients, and LLM tools) is ported from lumina's
//! `simply-rpc` `#[rpc_service]` —.

use syn::{FnArg, Ident, ItemTrait, LitStr, Pat, ReturnType, TraitItem, Type};

/// How a method's `ApiResult<T>` inner type surfaces in a tool result.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum OutputKind {
    /// `ApiResult<()>` — plain "ok" text.
    Unit,
    /// `ApiResult<String>` — the string as text content (confirmation
    /// messages, mirroring the hand-written tool style).
    Text,
    /// Any other `ApiResult<T>` — structured JSON content plus an output
    /// schema derived from `T`.
    Json,
}

/// One method parameter (after `&self`): owned type + optional `///` doc.
pub struct ParsedParam {
    pub name: Ident,
    pub ty: Type,
    /// The doc comment's first paragraph — the schemars `description` of
    /// the generated params-struct field, which is what a tool schema
    /// advertises.
    pub doc: Option<String>,
    /// The whole doc comment, paragraphs joined by a blank line — the long
    /// form the registry keeps for `help`.
    pub help: String,
    /// Whether the type is `Option<...>` (optional on the wire).
    pub is_optional: bool,
    /// `#[api(opaque)]` — advertised as a bare `object` / `array of object`
    /// instead of inlining the type's schema. Serde still moves the real
    /// type; only the advertised shape changes.
    pub opaque: bool,
}

pub struct ParsedMethod {
    pub ident: Ident,
    /// Tool + dispatch name: `#[api(name = "...")]` override or the ident.
    pub wire_name: String,
    /// The request/response enum variant: PascalCase of the wire name.
    pub variant: Ident,
    /// The doc comment's first paragraph — the MCP tool description.
    pub doc: Option<String>,
    /// The whole doc comment, paragraphs joined by a blank line — the long
    /// form `help` returns and the guide prints.
    pub help: String,
    /// `#[api(tier = "...")]` — which tier of the tool surface the method is
    /// advertised in. Optional here; a server that advertises by tier
    /// (`simply_api::mcp::Tiers`) refuses a tool without one.
    pub tier: Option<String>,
    /// `#[api(no_tool)]` — dispatchable and callable, but not an MCP tool.
    pub no_tool: bool,
    /// `#[api(no_output_schema)]` — the tool advertises no `outputSchema`;
    /// structured content still comes back exactly as before.
    pub no_output_schema: bool,
    /// `#[api(media)]` — the MCP tool renders the return value as rich
    /// content blocks (its `simply_api::mcp::MediaContent` impl: text
    /// summary + image/audio) instead of structured JSON; every other
    /// derived surface still carries the full serializable value.
    pub media: bool,
    /// `#[op(...)]` attributes, forwarded verbatim onto the generated
    /// request-enum variant (consumed there by `simply-op-macros`).
    pub op_attrs: Vec<syn::Attribute>,
    pub params: Vec<ParsedParam>,
    /// `T` in `ApiResult<T>` / `Result<T, _>`.
    pub inner_ty: Type,
    pub output: OutputKind,
    /// The method signature with parameter attributes stripped — what the
    /// generated remote-client impl reproduces.
    pub sig: syn::Signature,
}

pub struct ParsedTrait {
    pub ident: Ident,
    pub methods: Vec<ParsedMethod>,
}

impl ParsedTrait {
    pub fn from_item_trait(item: &ItemTrait) -> syn::Result<Self> {
        if !item.generics.params.is_empty() {
            return Err(syn::Error::new_spanned(
                &item.generics,
                "#[api_service] traits cannot be generic",
            ));
        }
        let mut methods = Vec::new();
        for trait_item in &item.items {
            let TraitItem::Fn(method) = trait_item else {
                return Err(syn::Error::new_spanned(
                    trait_item,
                    "#[api_service] traits may only contain methods",
                ));
            };
            methods.push(parse_method(method)?);
        }
        Ok(ParsedTrait { ident: item.ident.clone(), methods })
    }
}

fn parse_method(method: &syn::TraitItemFn) -> syn::Result<ParsedMethod> {
    let sig = &method.sig;
    if sig.asyncness.is_none() {
        return Err(syn::Error::new_spanned(sig, "#[api_service] methods must be `async fn`"));
    }
    if !sig.generics.params.is_empty() {
        return Err(syn::Error::new_spanned(
            &sig.generics,
            "#[api_service] methods cannot be generic",
        ));
    }
    match sig.inputs.first() {
        Some(FnArg::Receiver(recv)) if recv.reference.is_some() && recv.mutability.is_none() => {}
        _ => {
            return Err(syn::Error::new_spanned(
                &sig.inputs,
                "#[api_service] methods must take `&self`",
            ));
        }
    }

    let ApiAttrs { no_tool, media, no_output_schema, tier, name: name_override, opaque } =
        parse_api_attrs(&method.attrs)?;
    if opaque {
        return Err(syn::Error::new_spanned(
            &method.sig.ident,
            "#[api(opaque)] goes on a parameter, not a method",
        ));
    }
    let (doc, help) = extract_doc(&method.attrs);
    let params = parse_params(sig)?;
    let inner_ty = result_inner(&sig.output)?;
    let output = classify_output(&inner_ty);

    // The signature the client impl reproduces: parameter attributes (docs)
    // are ours, not rustc's — strip them.
    let mut sig = sig.clone();
    for input in &mut sig.inputs {
        if let FnArg::Typed(pat_type) = input {
            pat_type.attrs.clear();
        }
    }

    let wire_name = name_override.unwrap_or_else(|| method.sig.ident.to_string());
    let variant = Ident::new(&pascal_case(&wire_name), method.sig.ident.span());
    let op_attrs = method.attrs.iter().filter(|attr| attr.path().is_ident("op")).cloned().collect();

    Ok(ParsedMethod {
        ident: method.sig.ident.clone(),
        wire_name,
        variant,
        doc,
        help,
        tier,
        no_tool,
        no_output_schema,
        media,
        op_attrs,
        params,
        inner_ty,
        output,
        sig,
    })
}

/// The parsed `#[api(...)]` attributes — one grammar for methods and
/// parameters; [`parse_method`] and [`parse_params`] each refuse the keys
/// that make no sense where they stand.
#[derive(Default)]
struct ApiAttrs {
    no_tool: bool,
    media: bool,
    no_output_schema: bool,
    opaque: bool,
    tier: Option<String>,
    name: Option<String>,
}

/// `#[api(no_tool)]`, `#[api(media)]`, `#[api(no_output_schema)]`,
/// `#[api(tier = "...")]`, `#[api(name = "...")]` on a method and
/// `#[api(opaque)]` on a parameter — combinable in one list.
fn parse_api_attrs(attrs: &[syn::Attribute]) -> syn::Result<ApiAttrs> {
    let mut parsed = ApiAttrs::default();
    for attr in attrs {
        if !attr.path().is_ident("api") {
            continue;
        }
        attr.parse_nested_meta(|meta| {
            let string = |meta: &syn::meta::ParseNestedMeta| -> syn::Result<String> {
                Ok(meta.value()?.parse::<LitStr>()?.value())
            };
            if meta.path.is_ident("no_tool") {
                parsed.no_tool = true;
            } else if meta.path.is_ident("media") {
                parsed.media = true;
            } else if meta.path.is_ident("no_output_schema") {
                parsed.no_output_schema = true;
            } else if meta.path.is_ident("opaque") {
                parsed.opaque = true;
            } else if meta.path.is_ident("tier") {
                parsed.tier = Some(string(&meta)?);
            } else if meta.path.is_ident("name") {
                parsed.name = Some(string(&meta)?);
            } else {
                return Err(meta.error(
                    "unsupported #[api(...)] key; expected `no_tool`, `media`, \
                     `no_output_schema`, `opaque`, `tier = \"...\"` or `name = \"...\"`",
                ));
            }
            Ok(())
        })?;
    }
    Ok(parsed)
}

/// A doc comment as (first paragraph, whole text): `///` lines within a
/// paragraph are joined with a space, paragraphs (a blank `///` line
/// between them) with a blank line. The first paragraph is what a tool
/// advertises; the whole text is what `help` returns.
fn extract_doc(attrs: &[syn::Attribute]) -> (Option<String>, String) {
    let mut paragraphs: Vec<Vec<String>> = vec![Vec::new()];
    for attr in attrs.iter().filter(|attr| attr.path().is_ident("doc")) {
        let syn::Meta::NameValue(nv) = &attr.meta else { continue };
        let syn::Expr::Lit(lit) = &nv.value else { continue };
        let syn::Lit::Str(s) = &lit.lit else { continue };
        let line = s.value().trim().to_string();
        if line.is_empty() {
            if !paragraphs.last().is_some_and(Vec::is_empty) {
                paragraphs.push(Vec::new());
            }
        } else {
            paragraphs.last_mut().expect("never empty").push(line);
        }
    }
    let paragraphs: Vec<String> =
        paragraphs.iter().filter(|p| !p.is_empty()).map(|p| p.join(" ")).collect();
    (paragraphs.first().cloned(), paragraphs.join("\n\n"))
}

fn parse_params(sig: &syn::Signature) -> syn::Result<Vec<ParsedParam>> {
    let mut params = Vec::new();
    for arg in &sig.inputs {
        let FnArg::Typed(pat_type) = arg else { continue };
        let Pat::Ident(pat_ident) = pat_type.pat.as_ref() else {
            return Err(syn::Error::new_spanned(
                &pat_type.pat,
                "#[api_service]: only plain parameter names are supported",
            ));
        };
        if matches!(pat_type.ty.as_ref(), Type::Reference(_)) {
            return Err(syn::Error::new_spanned(
                &pat_type.ty,
                "#[api_service]: parameters must be owned types (they cross a wire)",
            ));
        }
        let is_optional = matches!(
            pat_type.ty.as_ref(),
            Type::Path(p) if p.path.segments.last().map(|s| s.ident == "Option").unwrap_or(false)
        );
        let attrs = parse_api_attrs(&pat_type.attrs)?;
        if attrs.no_tool
            || attrs.media
            || attrs.no_output_schema
            || attrs.tier.is_some()
            || attrs.name.is_some()
        {
            return Err(syn::Error::new_spanned(
                &pat_type.pat,
                "#[api(...)] on a parameter takes only `opaque`",
            ));
        }
        let (doc, help) = extract_doc(&pat_type.attrs);
        params.push(ParsedParam {
            name: pat_ident.ident.clone(),
            ty: (*pat_type.ty).clone(),
            doc,
            help,
            is_optional,
            opaque: attrs.opaque,
        });
    }
    Ok(params)
}

/// Extract `T` from `-> ApiResult<T>` (or `Result<T, ApiError>`).
fn result_inner(output: &ReturnType) -> syn::Result<Type> {
    let err = || {
        syn::Error::new_spanned(
            output,
            "#[api_service] methods must return `simply_api::ApiResult<T>`",
        )
    };
    let ReturnType::Type(_, ty) = output else { return Err(err()) };
    let Type::Path(path) = ty.as_ref() else { return Err(err()) };
    let segment = path.path.segments.last().ok_or_else(err)?;
    if segment.ident != "ApiResult" && segment.ident != "Result" {
        return Err(err());
    }
    let syn::PathArguments::AngleBracketed(args) = &segment.arguments else {
        return Err(err());
    };
    match args.args.first() {
        Some(syn::GenericArgument::Type(inner)) => Ok(inner.clone()),
        _ => Err(err()),
    }
}

fn classify_output(inner: &Type) -> OutputKind {
    match inner {
        Type::Tuple(t) if t.elems.is_empty() => OutputKind::Unit,
        Type::Path(p) if p.path.segments.last().map(|s| s.ident == "String").unwrap_or(false) => {
            OutputKind::Text
        }
        _ => OutputKind::Json,
    }
}

/// snake_case → PascalCase (for params-struct names).
pub fn pascal_case(s: &str) -> String {
    s.split('_')
        .map(|word| {
            let mut chars = word.chars();
            match chars.next() {
                None => String::new(),
                Some(c) => c.to_uppercase().chain(chars).collect(),
            }
        })
        .collect()
}

/// PascalCase → snake_case (for the generated router/meta idents).
pub fn snake_case(s: &str) -> String {
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
