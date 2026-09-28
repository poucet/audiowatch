//! The rmcp 3.x side of a derived api: a generic MCP server over any
//! `Arc<A>` plus the helpers the generated tool router calls.
//!
//! `#[api_service]` generates `{trait}_tool_router::<A>()`; mounting it is
//! two lines:
//!
//! ```rust,ignore
//! let server = McpApiServer::new(api, flux_api_tool_router())
//!     .with_instructions("...");
//! // serve `server` over any rmcp transport (streamable HTTP, stdio, …)
//! ```

use std::borrow::Cow;
use std::collections::BTreeSet;
use std::sync::{Arc, LazyLock, Mutex, RwLock};

use rmcp::{
    handler::server::{
        router::tool::ToolRouter,
        tool::{schema_for_input, ToolCallContext},
    },
    model::{
        CallToolRequestParams, CallToolResponse, CallToolResult, ContentBlock, Implementation,
        JsonObject, ListToolsResult, PaginatedRequestParams, ProtocolVersion, ServerCapabilities,
        ServerInfo, Tool,
    },
    service::{NotificationContext, Peer, RequestContext, RoleServer},
    ErrorData, ServerHandler,
};

use crate::meta::ServiceMeta;
use crate::ApiError;

// ── tiers: which slice of the surface a session is shown ────────────────

/// Which tiers of a service's tools `tools/list` advertises — one setting
/// shared by every session of a server, changeable while they are
/// connected. The first tier in `order` is always on: it is the floor, and
/// a client that sees no tools has nothing to ask for more with.
///
/// Every tool is still **callable** whatever the setting: the tiers decide
/// what is advertised (what an LLM pays for in context every turn), never
/// what answers.
pub struct Tiers {
    meta: &'static ServiceMeta,
    /// Every tier there is, in the order `tools/list` groups them.
    order: &'static [&'static str],
    enabled: RwLock<BTreeSet<&'static str>>,
    /// Every session that has completed `initialize` — notified when the
    /// setting changes so a connected client re-lists. Dead peers are
    /// dropped when their notification fails.
    peers: Mutex<Vec<Peer<RoleServer>>>,
}

impl Tiers {
    /// A setting over `meta`'s tools with `enabled` on (the first of
    /// `order` is added whatever `enabled` says). Panics on a tool whose
    /// tier is not in `order`: that is a trait attribute typo, and every
    /// test that builds a server catches it.
    pub fn new(
        meta: &'static ServiceMeta,
        order: &'static [&'static str],
        enabled: impl IntoIterator<Item = impl AsRef<str>>,
    ) -> Arc<Self> {
        for method in meta.tools() {
            assert!(
                order.contains(&method.tier),
                "{}::{} is in tier {:?}, which is not one of {order:?}",
                meta.service,
                method.rust_name,
                method.tier
            );
        }
        let tiers = Self {
            meta,
            order,
            enabled: RwLock::new(BTreeSet::new()),
            peers: Mutex::new(Vec::new()),
        };
        tiers.set(enabled);
        Arc::new(tiers)
    }

    /// Every tier, in advertised order.
    pub fn order(&self) -> &'static [&'static str] {
        self.order
    }

    /// The tiers currently advertised, in `order`.
    pub fn enabled(&self) -> Vec<&'static str> {
        let enabled = self.enabled.read().expect("tiers lock");
        self.order.iter().copied().filter(|t| enabled.contains(t)).collect()
    }

    /// Replace the enabled set. Names not in `order` are ignored; the
    /// first tier is always kept. Returns what is now advertised. Tell the
    /// connected sessions with [`Tiers::notify`].
    pub fn set(&self, enabled: impl IntoIterator<Item = impl AsRef<str>>) -> Vec<&'static str> {
        let mut set: BTreeSet<&'static str> = self.order.iter().take(1).copied().collect();
        for name in enabled {
            if let Some(known) = self.order.iter().find(|t| **t == name.as_ref()) {
                set.insert(known);
            }
        }
        *self.enabled.write().expect("tiers lock") = set;
        self.enabled()
    }

    /// Whether `tool` is advertised right now.
    pub fn advertises(&self, tool: &str) -> bool {
        let enabled = self.enabled.read().expect("tiers lock");
        self.meta.method(tool).is_some_and(|m| enabled.contains(m.tier))
    }

    /// The advertised tools of `router`, grouped by tier in `order` and in
    /// the trait's declaration order within a tier — the first tier first,
    /// so what a client reads first is the floor.
    pub fn list(&self, router_tool: impl Fn(&str) -> Option<Tool>) -> Vec<Tool> {
        self.enabled()
            .into_iter()
            .flat_map(|tier| self.meta.tier(tier))
            .filter_map(|m| router_tool(m.name))
            .collect()
    }

    /// Send `notifications/tools/list_changed` to every connected session,
    /// each given [`NOTIFY_TIMEOUT`] — a client that opened no event stream
    /// has nowhere to receive it, and must not hold the setting up. Sessions
    /// whose transport has closed are forgotten.
    pub async fn notify(&self) {
        let peers: Vec<Peer<RoleServer>> = self.peers.lock().expect("peers lock").clone();
        for peer in peers {
            let _ = tokio::time::timeout(NOTIFY_TIMEOUT, peer.notify_tool_list_changed()).await;
        }
        self.peers.lock().expect("peers lock").retain(|p| !p.is_transport_closed());
    }

    /// How many sessions would be told of a change.
    pub fn connected(&self) -> usize {
        self.peers.lock().expect("peers lock").len()
    }

    fn register(&self, peer: Peer<RoleServer>) {
        let mut peers = self.peers.lock().expect("peers lock");
        peers.retain(|p| !p.is_transport_closed());
        peers.push(peer);
    }
}

/// How long one session gets to take a `list_changed` notification.
pub const NOTIFY_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(1);

/// What a `#[api(opaque)]` parameter of a non-`Vec` type advertises: a
/// bare object. The description beside it says where the shape is seen.
pub struct OpaqueObject;

/// What a `#[api(opaque)]` `Vec<T>` parameter advertises: an array of bare
/// objects.
pub struct OpaqueArray;

impl schemars::JsonSchema for OpaqueObject {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "OpaqueObject".into()
    }
    fn inline_schema() -> bool {
        true
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "object" })
    }
}

impl schemars::JsonSchema for OpaqueArray {
    fn schema_name() -> std::borrow::Cow<'static, str> {
        "OpaqueArray".into()
    }
    fn inline_schema() -> bool {
        true
    }
    fn json_schema(_: &mut schemars::SchemaGenerator) -> schemars::Schema {
        schemars::json_schema!({ "type": "array", "items": { "type": "object" } })
    }
}

// ── the protocol revisions the replies actually satisfy ─────────────────

/// The revisions rmcp knows, kept to the ones it *emits*: everything up to
/// and including [`ProtocolVersion::LATEST`].
///
/// `KNOWN_VERSIONS` reaches one revision further than `LATEST` — rmcp 3.1.4
/// can parse `2026-07-28` but does not yet produce it, and the default
/// [`ServerHandler::supported_protocol_versions`] returns the whole list, so
/// `initialize` echoes back whatever a client asks for. That is how a client
/// talks the server into a wire shape it does not write: `2026-07-28` makes
/// `ttlMs` and `cacheScope` **required** on every paginated result, and
/// rmcp's list constructors leave both `None` (so serde skips them) while
/// still emitting `resultType: "complete"`. A `tools/list` answered under
/// that revision is therefore two required fields short, and a client that
/// validates the reply against the revision's schema rejects the whole tool
/// list — measured 2026-09-26, where an agent client could not load any tool
/// from the in-app server, while the same server answered `2025-06-18` and
/// `2025-11-25` with a reply it accepted.
///
/// **The cap lifts itself.** It is expressed as `<= LATEST` rather than as a
/// list, and `LATEST` is what rmcp defaults its own `ServerInfo` to — it
/// moves when the SDK fills the revision's required fields in, not when it
/// merely learns the version string. Nothing here needs editing then.
static SUPPORTED_PROTOCOL_VERSIONS: LazyLock<Vec<ProtocolVersion>> = LazyLock::new(|| {
    ProtocolVersion::KNOWN_VERSIONS
        .iter()
        .filter(|version| **version <= ProtocolVersion::LATEST)
        .cloned()
        .collect()
});

/// Every protocol revision an [`McpApiServer`] will negotiate, oldest first
/// — see [`SUPPORTED_PROTOCOL_VERSIONS`]. A client asking for anything
/// outside this set is answered the newest one in it, never its own.
pub fn supported_protocol_versions() -> &'static [ProtocolVersion] {
    &SUPPORTED_PROTOCOL_VERSIONS
}

// ── the server ──────────────────────────────────────────────────────────

/// An MCP server over any api implementor: the derived [`ToolRouter`] does
/// the tools, this does the rmcp plumbing (`ServerHandler`, server info).
pub struct McpApiServer<A: ?Sized + Send + Sync + 'static> {
    api: Arc<A>,
    tool_router: ToolRouter<Self>,
    info: ServerInfo,
    /// `None` advertises every tool, alphabetically.
    tiers: Option<Arc<Tiers>>,
}

impl<A: ?Sized + Send + Sync + 'static> McpApiServer<A> {
    pub fn new(api: Arc<A>, tool_router: ToolRouter<Self>) -> Self {
        let info = ServerInfo::new(
            ServerCapabilities::builder().enable_tools().enable_tool_list_changed().build(),
        );
        Self { api, tool_router, info, tiers: None }
    }

    /// Advertise only the tiers `tiers` has on (see [`Tiers`]); every tool
    /// stays callable. The setting is shared, so one `Arc` serves every
    /// session of a server.
    pub fn with_tiers(mut self, tiers: Arc<Tiers>) -> Self {
        self.tiers = Some(tiers);
        self
    }

    /// The implementor the generated tool handlers call into.
    pub fn api(&self) -> &Arc<A> {
        &self.api
    }

    pub fn tool_router(&self) -> &ToolRouter<Self> {
        &self.tool_router
    }

    /// Server name/version/title shown to clients.
    pub fn with_implementation(mut self, implementation: Implementation) -> Self {
        self.info = self.info.with_server_info(implementation);
        self
    }

    /// The instructions blurb clients show the model.
    pub fn with_instructions(mut self, instructions: impl Into<String>) -> Self {
        self.info = self.info.with_instructions(instructions);
        self
    }
}

impl<A: ?Sized + Send + Sync + 'static> Clone for McpApiServer<A> {
    fn clone(&self) -> Self {
        Self {
            api: self.api.clone(),
            tool_router: self.tool_router.clone(),
            info: self.info.clone(),
            tiers: self.tiers.clone(),
        }
    }
}

impl<A: ?Sized + Send + Sync + 'static> ServerHandler for McpApiServer<A> {
    fn get_info(&self) -> ServerInfo {
        self.info.clone()
    }

    /// Narrow rmcp's default (every version it can name) to the ones whose
    /// replies we actually write — see [`SUPPORTED_PROTOCOL_VERSIONS`]. This
    /// is what bounds `initialize` negotiation, on every transport: the
    /// streamable-HTTP tower negotiates through this list too.
    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        Cow::Borrowed(supported_protocol_versions())
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        self.tool_router.call(ToolCallContext::new(self, request, context)).await
    }

    async fn list_tools(
        &self,
        _request: Option<PaginatedRequestParams>,
        _context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        let tools = match &self.tiers {
            Some(tiers) => tiers.list(|name| self.tool_router.get(name).cloned()),
            None => self.tool_router.list_all(),
        };
        Ok(ListToolsResult::with_all_items(tools))
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.tool_router.get(name).cloned()
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        if let Some(tiers) = &self.tiers {
            tiers.register(context.peer);
        }
    }
}

// ── helpers the generated router calls ──────────────────────────────────

/// Build a [`Tool`] spec: input schema from the params struct, optional
/// output schema from the return type.
pub fn tool_spec<P: schemars::JsonSchema + 'static>(
    name: &'static str,
    description: &'static str,
    output_schema: Option<Arc<JsonObject>>,
) -> Tool {
    let input = schema_for_input::<P>()
        .unwrap_or_else(|e| panic!("invalid input schema for tool {name:?}: {e}"));
    let mut tool = Tool::new(name, description, input);
    tool.output_schema = output_schema;
    tool
}

/// Output schema for `T`, when representable (an MCP output schema must be
/// a JSON object — e.g. `serde_json::Value` yields `true`, so: `None`).
pub fn output_schema_for<T: schemars::JsonSchema>() -> Option<Arc<JsonObject>> {
    match crate::meta::schema_of::<T>() {
        serde_json::Value::Object(mut object) => {
            object.remove("title");
            object.remove("description");
            Some(Arc::new(object))
        }
        _ => None,
    }
}

fn error_response(e: ApiError) -> Result<CallToolResponse, ErrorData> {
    Ok(CallToolResult::error(vec![ContentBlock::text(e.message)]).into())
}

/// `ApiResult<T>` → structured JSON content (plus rmcp's text mirror);
/// errors become readable `is_error` results, never protocol errors.
pub fn json_response<T: serde::Serialize>(
    result: Result<T, ApiError>,
) -> Result<CallToolResponse, ErrorData> {
    match result {
        Ok(value) => {
            let value = serde_json::to_value(value).map_err(|e| {
                ErrorData::internal_error(format!("result failed to serialize: {e}"), None)
            })?;
            Ok(CallToolResult::structured(value).into())
        }
        Err(e) => error_response(e),
    }
}

/// Return types of `#[api(media)]` methods: how the value renders as MCP
/// content blocks (a text summary followed by image/audio blocks —
/// `ContentBlock::image`/`ContentBlock::audio` carry base64 data + a MIME
/// type). The typed dispatcher/client surfaces still move the full
/// serializable value; only the MCP tool result takes this shape.
pub trait MediaContent {
    fn into_content_blocks(self) -> Vec<ContentBlock>;
}

/// `ApiResult<T: MediaContent>` → the type's own content blocks (no
/// structured JSON — the payload IS the media).
pub fn media_response<T: MediaContent>(
    result: Result<T, ApiError>,
) -> Result<CallToolResponse, ErrorData> {
    match result {
        Ok(value) => Ok(CallToolResult::success(value.into_content_blocks()).into()),
        Err(e) => error_response(e),
    }
}

/// `ApiResult<String>` → plain text content (confirmation messages).
pub fn text_response(result: Result<String, ApiError>) -> Result<CallToolResponse, ErrorData> {
    match result {
        Ok(text) => Ok(CallToolResult::success(vec![ContentBlock::text(text)]).into()),
        Err(e) => error_response(e),
    }
}

/// `ApiResult<()>` → "ok".
pub fn unit_response(result: Result<(), ApiError>) -> Result<CallToolResponse, ErrorData> {
    text_response(result.map(|()| "ok".into()))
}
