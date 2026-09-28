//! Emit the derived surface for an `#[api_service]` trait:
//!
//! 1. one params struct per method (serde + schemars derives, field docs
//!    from the trait's parameter docs) — the MCP input schema and the wire
//!    shape of the dispatcher/remote client;
//! 2. a `ServiceMeta` static (`{TRAIT}_META`) — names, descriptions, and
//!    schema fns for registry lints and doc generation;
//! 3. `{trait}_tool_router()` — an rmcp 3.x `ToolRouter` over
//!    `simply_api::mcp::McpApiServer<A>`, one tool per non-`no_tool` method;
//! 4. `{Trait}Dispatcher` — name+JSON dispatch onto any implementor (the
//!    server half of a remote transport, and the loopback for tests);
//! 5. `Remote{Trait}` — implements the trait over any `simply_api::Caller`
//!    (the seam a real remote transport plugs into later).

use proc_macro2::TokenStream;
use quote::{format_ident, quote};

use crate::parse::{pascal_case, snake_case, OutputKind, ParsedMethod, ParsedTrait};

pub fn generate(parsed: &ParsedTrait, vis: &syn::Visibility) -> TokenStream {
    let trait_ident = &parsed.ident;
    let trait_snake = snake_case(&trait_ident.to_string());

    let params_structs = parsed.methods.iter().map(|m| params_struct(parsed, m, vis));
    let request = request_enum(parsed, vis);
    let response = response_enum(parsed, vis);
    let dispatch = dispatch_impls(parsed, vis);
    let meta = meta_static(parsed, vis, &trait_snake);
    let router = router_fn(parsed, vis, &trait_snake);
    let dispatcher = dispatcher(parsed, vis);
    let remote = remote_client(parsed, vis);

    let dyn_alias = format_ident!("Dyn{}", trait_ident);
    let dyn_doc = format!("A shared, dynamically-typed `{trait_ident}` — the in-process client.");

    quote! {
        #(#params_structs)*
        #request
        #response
        #dispatch
        #meta
        #router
        #dispatcher
        #remote

        #[doc = #dyn_doc]
        #vis type #dyn_alias = ::std::sync::Arc<dyn #trait_ident>;
    }
}

/// The request/response enums drop a trailing `Api` from the trait name:
/// `FluxApi` → `FluxRequest`/`FluxResponse`.
fn base_name(parsed: &ParsedTrait) -> String {
    let name = parsed.ident.to_string();
    name.strip_suffix("Api").filter(|s| !s.is_empty()).unwrap_or(&name).to_string()
}

fn request_ident(parsed: &ParsedTrait) -> syn::Ident {
    format_ident!("{}Request", base_name(parsed))
}

fn response_ident(parsed: &ParsedTrait) -> syn::Ident {
    format_ident!("{}Response", base_name(parsed))
}

/// The typed request enum: one variant per method, named fields = the real
/// parameter types (serde-tagged by wire name, schemars-documented from the
/// trait docs), plus the forwarded `#[op(...)]` metadata.
fn request_enum(parsed: &ParsedTrait, vis: &syn::Visibility) -> TokenStream {
    let trait_ident = &parsed.ident;
    let ident = request_ident(parsed);
    let doc = format!(
        "Every [`{trait_ident}`] request as one **typed value** — a variant per method \
         carrying the real parameter types. This is what enters machinery (dispatch, \
         history provenance, journals, replay); JSON exists only at the transport edge. \
         Wire shape: externally tagged by the method's wire name."
    );
    let variants = parsed.methods.iter().map(|m| {
        let variant = &m.variant;
        let wire_name = &m.wire_name;
        let doc = m.doc.iter().map(|d| quote!(#[doc = #d]));
        let op_attrs = &m.op_attrs;
        let fields = m.params.iter().map(|p| {
            let name = &p.name;
            let ty = &p.ty;
            let field_doc = p.doc.iter().map(|d| quote!(#[doc = #d]));
            quote! {
                #(#field_doc)*
                #name: #ty,
            }
        });
        quote! {
            #(#doc)*
            #(#op_attrs)*
            #[serde(rename = #wire_name)]
            #variant {
                #(#fields)*
            },
        }
    });
    quote! {
        #[doc = #doc]
        #[derive(
            ::std::fmt::Debug,
            ::std::clone::Clone,
            ::std::cmp::PartialEq,
            ::simply_api::export::serde::Serialize,
            ::simply_api::export::serde::Deserialize,
            ::simply_api::export::schemars::JsonSchema,
            ::simply_api::export::simply_op_macros::Op,
        )]
        #[serde(crate = "::simply_api::export::serde")]
        #[schemars(crate = "::simply_api::export::schemars")]
        #[op(crate = "::simply_api::export::simply_history")]
        #vis enum #ident {
            #(#variants)*
        }
    }
}

/// The typed response enum: one variant per method carrying the method's
/// return type (`ApiResult<()>` methods get a unit variant).
fn response_enum(parsed: &ParsedTrait, vis: &syn::Visibility) -> TokenStream {
    let trait_ident = &parsed.ident;
    let ident = response_ident(parsed);
    let request = request_ident(parsed);
    let doc = format!(
        "Every [`{trait_ident}`] response as one typed value — a variant per method \
         carrying the return type; the `Ok` half of [`{request}::dispatch`]."
    );
    let variants = parsed.methods.iter().map(|m| {
        let variant = &m.variant;
        let wire_name = &m.wire_name;
        let doc = format!("Response of `{wire_name}`.");
        let payload = match m.output {
            OutputKind::Unit => quote!(),
            _ => {
                let inner_ty = &m.inner_ty;
                quote!((#inner_ty))
            }
        };
        quote! {
            #[doc = #doc]
            #[serde(rename = #wire_name)]
            #variant #payload,
        }
    });
    quote! {
        #[doc = #doc]
        #[derive(
            ::std::fmt::Debug,
            ::std::clone::Clone,
            ::std::cmp::PartialEq,
            ::simply_api::export::serde::Serialize,
            ::simply_api::export::serde::Deserialize,
            ::simply_api::export::schemars::JsonSchema,
        )]
        #[serde(crate = "::simply_api::export::serde")]
        #[schemars(crate = "::simply_api::export::schemars")]
        #vis enum #ident {
            #(#variants)*
        }
    }
}

/// `Request::dispatch` (typed, no JSON), `Request::method`, and
/// `Response::into_json` (the transport edge).
fn dispatch_impls(parsed: &ParsedTrait, vis: &syn::Visibility) -> TokenStream {
    let trait_ident = &parsed.ident;
    let request = request_ident(parsed);
    let response = response_ident(parsed);

    let dispatch_arms = parsed.methods.iter().map(|m| {
        let variant = &m.variant;
        let method_ident = &m.ident;
        let fields: Vec<_> = m.params.iter().map(|p| &p.name).collect();
        let map = match m.output {
            OutputKind::Unit => quote!(.map(|()| #response::#variant)),
            _ => quote!(.map(#response::#variant)),
        };
        quote! {
            #request::#variant { #(#fields),* } => api.#method_ident(#(#fields),*).await #map,
        }
    });
    let method_arms = parsed.methods.iter().map(|m| {
        let variant = &m.variant;
        let wire_name = &m.wire_name;
        quote!(#request::#variant { .. } => #wire_name,)
    });
    let json_arms = parsed.methods.iter().map(|m| {
        let variant = &m.variant;
        let wire_name = &m.wire_name;
        match m.output {
            OutputKind::Unit => quote! {
                #response::#variant => ::std::result::Result::Ok(
                    ::simply_api::export::serde_json::Value::Null,
                ),
            },
            _ => quote! {
                #response::#variant(value) => {
                    ::simply_api::export::serde_json::to_value(value).map_err(|e| {
                        ::simply_api::ApiError::failed(::std::format!(
                            "{}: response failed to serialize: {e}", #wire_name,
                        ))
                    })
                }
            },
        }
    });

    let dispatch_doc = format!(
        "Execute this request against any [`{trait_ident}`] implementor — fully typed, \
         no JSON on the path. The name+JSON dispatcher (`{trait_ident}Dispatcher`) is a \
         thin serde shim over this."
    );
    let json_doc = "The method's payload as JSON — the transport edge (unit responses \
                    serialize as `null`).";
    quote! {
        impl #request {
            #[doc = #dispatch_doc]
            #vis async fn dispatch<A>(
                self,
                api: &A,
            ) -> ::simply_api::ApiResult<#response>
            where
                A: #trait_ident + ?Sized,
            {
                match self {
                    #(#dispatch_arms)*
                }
            }

            /// The method's wire name (the tool/dispatch name).
            #vis fn method(&self) -> &'static str {
                match self {
                    #(#method_arms)*
                }
            }
        }

        impl #response {
            #[doc = #json_doc]
            #vis fn into_json(
                self,
            ) -> ::std::result::Result<
                ::simply_api::export::serde_json::Value,
                ::simply_api::ApiError,
            > {
                match self {
                    #(#json_arms)*
                }
            }
        }
    }
}

fn params_ident(parsed: &ParsedTrait, m: &ParsedMethod) -> syn::Ident {
    format_ident!("{}{}Params", parsed.ident, pascal_case(&m.ident.to_string()))
}

/// The schema a `#[api(opaque)]` parameter advertises in place of its type:
/// the same optionality, a bare `object` or `array of object` inside — the
/// wire still carries the real type, so nothing about a call changes.
fn opaque_schema_type(ty: &syn::Type) -> TokenStream {
    fn last_segment(ty: &syn::Type) -> Option<&syn::PathSegment> {
        match ty {
            syn::Type::Path(p) => p.path.segments.last(),
            _ => None,
        }
    }
    fn generic_arg(segment: &syn::PathSegment) -> Option<&syn::Type> {
        match &segment.arguments {
            syn::PathArguments::AngleBracketed(args) => args.args.iter().find_map(|a| match a {
                syn::GenericArgument::Type(t) => Some(t),
                _ => None,
            }),
            _ => None,
        }
    }
    let segment = last_segment(ty);
    if segment.is_some_and(|s| s.ident == "Option") {
        let inner = opaque_schema_type(generic_arg(segment.unwrap()).expect("Option<T>"));
        return quote!(::std::option::Option<#inner>);
    }
    if segment.is_some_and(|s| s.ident == "Vec") {
        return quote!(::simply_api::mcp::OpaqueArray);
    }
    quote!(::simply_api::mcp::OpaqueObject)
}

/// The per-method arguments struct: JSON-schema'd via schemars, (de)serialized
/// on both the MCP path and the dispatcher/remote path.
fn params_struct(parsed: &ParsedTrait, m: &ParsedMethod, vis: &syn::Visibility) -> TokenStream {
    let ident = params_ident(parsed, m);
    let doc = format!("Arguments of [`{}::{}`].", parsed.ident, m.ident);
    let fields = m.params.iter().map(|p| {
        let name = &p.name;
        let ty = &p.ty;
        let field_doc = p.doc.iter().map(|d| quote!(#[doc = #d]));
        let opaque = p.opaque.then(|| {
            let with = opaque_schema_type(ty).to_string();
            quote!(#[schemars(with = #with)])
        });
        quote! {
            #(#field_doc)*
            #opaque
            pub #name: #ty,
        }
    });
    quote! {
        #[doc = #doc]
        #[derive(
            ::simply_api::export::serde::Serialize,
            ::simply_api::export::serde::Deserialize,
            ::simply_api::export::schemars::JsonSchema,
        )]
        #[serde(crate = "::simply_api::export::serde")]
        #[schemars(crate = "::simply_api::export::schemars")]
        #vis struct #ident {
            #(#fields)*
        }
    }
}

fn meta_static(parsed: &ParsedTrait, vis: &syn::Visibility, trait_snake: &str) -> TokenStream {
    let trait_ident = &parsed.ident;
    let meta_ident = format_ident!("{}_META", trait_snake.to_uppercase());
    let doc = format!("Method registry of [`{trait_ident}`] — for lints and doc generation.");
    let service_name = trait_ident.to_string();
    let entries = parsed.methods.iter().map(|m| {
        let p_ident = params_ident(parsed, m);
        let name = &m.wire_name;
        let rust_name = m.ident.to_string();
        let description = m.doc.clone().unwrap_or_default();
        let help = &m.help;
        let tier = m.tier.clone().unwrap_or_default();
        let has_tool = !m.no_tool;
        let returns_media = m.media;
        let inner_ty = &m.inner_ty;
        let output_type = quote!(#inner_ty).to_string().replace(' ', "");
        let returns_text = m.output != OutputKind::Json;
        let params = m.params.iter().map(|p| {
            let p_name = p.name.to_string();
            let optional = p.is_optional;
            let doc = &p.help;
            quote! {
                ::simply_api::meta::ParamMeta { name: #p_name, optional: #optional, doc: #doc }
            }
        });
        quote! {
            ::simply_api::meta::MethodMeta {
                name: #name,
                rust_name: #rust_name,
                description: #description,
                help: #help,
                tier: #tier,
                has_tool: #has_tool,
                params: &[#(#params),*],
                params_schema: ::simply_api::meta::schema_of::<#p_ident>,
                output_schema: ::simply_api::meta::schema_of::<#inner_ty>,
                output_type: #output_type,
                returns_text: #returns_text,
                returns_media: #returns_media,
            }
        }
    });
    quote! {
        #[doc = #doc]
        #vis static #meta_ident: ::simply_api::meta::ServiceMeta = ::simply_api::meta::ServiceMeta {
            service: #service_name,
            methods: &[#(#entries),*],
        };
    }
}

fn router_fn(parsed: &ParsedTrait, vis: &syn::Visibility, trait_snake: &str) -> TokenStream {
    let trait_ident = &parsed.ident;
    let router_ident = format_ident!("{trait_snake}_tool_router");
    let doc = format!(
        "The derived MCP tool router of [`{trait_ident}`]: one tool per non-`no_tool` \
         method, mounted on a [`simply_api::mcp::McpApiServer`]."
    );

    let routes = parsed.methods.iter().filter(|m| !m.no_tool).map(|m| {
        let p_ident = params_ident(parsed, m);
        let handler = format_ident!("call_{}", m.ident);
        let method_ident = &m.ident;
        let wire_name = &m.wire_name;
        let description = m.doc.clone().unwrap_or_default();
        let inner_ty = &m.inner_ty;
        let fields: Vec<_> = m.params.iter().map(|p| &p.name).collect();
        let params_binding = if fields.is_empty() { quote!(_params) } else { quote!(params) };
        // `#[api(media)]`: content blocks instead of structured JSON, so no
        // output schema — the payload is the media itself.
        // `#[api(no_output_schema)]`: structured JSON still, just not
        // advertised — for a result whose schema would inline half the
        // document model.
        let output_schema = match m.output {
            _ if m.media || m.no_output_schema => quote!(::std::option::Option::None),
            OutputKind::Json => quote!(::simply_api::mcp::output_schema_for::<#inner_ty>()),
            OutputKind::Text | OutputKind::Unit => quote!(::std::option::Option::None),
        };
        let respond = match m.output {
            _ if m.media => quote!(::simply_api::mcp::media_response(result)),
            OutputKind::Json => quote!(::simply_api::mcp::json_response(result)),
            OutputKind::Text => quote!(::simply_api::mcp::text_response(result)),
            OutputKind::Unit => quote!(::simply_api::mcp::unit_response(result)),
        };
        quote! {
            {
                #[allow(clippy::type_complexity)]
                fn #handler<'a, A: #trait_ident + ?Sized + 'static>(
                    mut ctx: ::simply_api::export::rmcp::handler::server::tool::ToolCallContext<
                        'a,
                        ::simply_api::mcp::McpApiServer<A>,
                    >,
                ) -> ::std::pin::Pin<::std::boxed::Box<
                    dyn ::std::future::Future<
                            Output = ::std::result::Result<
                                ::simply_api::export::rmcp::model::CallToolResponse,
                                ::simply_api::export::rmcp::ErrorData,
                            >,
                        > + ::std::marker::Send
                        + 'a,
                >> {
                    ::std::boxed::Box::pin(async move {
                        let args = ctx.arguments.take().unwrap_or_default();
                        let #params_binding: #p_ident =
                            ::simply_api::export::rmcp::handler::server::tool::parse_json_object(args)?;
                        let api = ::std::sync::Arc::clone(ctx.service.api());
                        let result = api.#method_ident(#(#params_binding.#fields),*).await;
                        #respond
                    })
                }
                router.add_route(
                    ::simply_api::export::rmcp::handler::server::router::tool::ToolRoute::new_dyn(
                        ::simply_api::mcp::tool_spec::<#p_ident>(
                            #wire_name,
                            #description,
                            #output_schema,
                        ),
                        #handler::<A>,
                    ),
                );
            }
        }
    });

    quote! {
        #[doc = #doc]
        #vis fn #router_ident<A>() -> ::simply_api::export::rmcp::handler::server::router::tool::ToolRouter<
            ::simply_api::mcp::McpApiServer<A>,
        >
        where
            A: #trait_ident + ?Sized + 'static,
        {
            #[allow(unused_mut)]
            let mut router =
                ::simply_api::export::rmcp::handler::server::router::tool::ToolRouter::new();
            #(#routes)*
            router
        }
    }
}

fn dispatcher(parsed: &ParsedTrait, vis: &syn::Visibility) -> TokenStream {
    let trait_ident = &parsed.ident;
    let dispatcher_ident = format_ident!("{}Dispatcher", trait_ident);
    let request = request_ident(parsed);
    let doc = format!(
        "Name+JSON dispatch onto any [`{trait_ident}`] — the server half of a remote \
         transport (`simply_api::Caller` over an in-process implementor). A thin serde \
         shim over the typed path: params deserialize into [`{request}`], execute via \
         [`{request}::dispatch`], and the typed response serializes back at this edge."
    );
    let known: Vec<&str> = parsed.methods.iter().map(|m| m.wire_name.as_str()).collect();
    let known_list = known.join(", ");
    quote! {
        #[doc = #doc]
        #vis struct #dispatcher_ident<A: ?Sized>(pub ::std::sync::Arc<A>);

        #[::simply_api::export::async_trait::async_trait]
        impl<A: #trait_ident + ?Sized> ::simply_api::Caller for #dispatcher_ident<A> {
            async fn call(
                &self,
                method: &str,
                params: ::simply_api::export::serde_json::Value,
            ) -> ::std::result::Result<
                ::simply_api::export::serde_json::Value,
                ::simply_api::ApiError,
            > {
                if ![#(#known),*].contains(&method) {
                    return Err(::simply_api::ApiError::failed(format!(
                        "unknown method {method:?}; methods: {}",
                        #known_list,
                    )));
                }
                // `{method: params}` is exactly the request enum's external
                // tagging; null params mean "no arguments".
                let params = match params {
                    ::simply_api::export::serde_json::Value::Null => {
                        ::simply_api::export::serde_json::Value::Object(Default::default())
                    }
                    params => params,
                };
                let mut envelope = ::simply_api::export::serde_json::Map::new();
                envelope.insert(method.to_owned(), params);
                let request: #request = ::simply_api::export::serde_json::from_value(
                    ::simply_api::export::serde_json::Value::Object(envelope),
                )
                .map_err(|e| ::simply_api::ApiError::invalid_params(
                    format!("invalid parameters for {method}: {e}"),
                ))?;
                request.dispatch(&*self.0).await?.into_json()
            }
        }
    }
}

fn remote_client(parsed: &ParsedTrait, vis: &syn::Visibility) -> TokenStream {
    let trait_ident = &parsed.ident;
    let remote_ident = format_ident!("Remote{}", trait_ident);
    let doc = format!(
        "Implements [`{trait_ident}`] over any [`simply_api::Caller`]: the typed client. \
         Pair with `{trait_ident}Dispatcher` for an in-process loopback, or a remote \
         transport later."
    );
    let methods = parsed.methods.iter().map(|m| {
        let p_ident = params_ident(parsed, m);
        let sig = &m.sig;
        let wire_name = &m.wire_name;
        let fields: Vec<_> = m.params.iter().map(|p| &p.name).collect();
        quote! {
            #sig {
                let params = #p_ident { #(#fields: #fields),* };
                let params = ::simply_api::export::serde_json::to_value(&params).map_err(|e| {
                    ::simply_api::ApiError::failed(
                        format!("{}: parameters failed to serialize: {e}", #wire_name),
                    )
                })?;
                let value = ::simply_api::Caller::call(&self.0, #wire_name, params).await?;
                ::simply_api::export::serde_json::from_value(value).map_err(|e| {
                    ::simply_api::ApiError::failed(
                        format!("{}: invalid response: {e}", #wire_name),
                    )
                })
            }
        }
    });
    quote! {
        #[doc = #doc]
        #vis struct #remote_ident<C>(pub C);

        #[::simply_api::export::async_trait::async_trait]
        impl<C: ::simply_api::Caller> #trait_ident for #remote_ident<C> {
            #(#methods)*
        }
    }
}
