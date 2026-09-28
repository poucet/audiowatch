//! One async trait → every surface derived.
//!
//! `#[api_service]` on an `#[async_trait]` trait derives, with zero marginal
//! work per capability:
//!
//! - an **MCP tool router** (rmcp 3.x): each method is a tool — name from
//!   the method (or `#[api(name = "...")]`), description from its doc
//!   comment, input schema from a generated per-method params struct
//!   (schemars; parameter doc comments become field descriptions), output
//!   schema from the return type; `#[api(no_tool)]` opts a method out;
//! - a **dispatcher** (`{Trait}Dispatcher`) and a **typed remote client**
//!   (`Remote{Trait}` over any [`Caller`]) — the seam a real remote
//!   transport plugs into; in-process callers just use `Arc<dyn Trait>`
//!   (`Dyn{Trait}`) directly;
//! - a **method registry** (`{TRAIT}_META`) for lint tests and doc
//!   generation.
//!
//! The pattern is ported from lumina's `simply-rpc` (`#[rpc_service]`) and
//! its daemon `#[skill_router]`; HTTP/REST route generation is deliberately
//! omitted for now —.

#![forbid(unsafe_code)]

pub mod mcp;
pub mod meta;

pub use simply_api_macros::api_service;

/// Re-exports the generated code paths through, so downstream crates need
/// no direct dependency on (or version agreement with) rmcp, schemars,
/// serde, serde_json, async-trait, or the op-metadata machinery
/// (simply-history's `OpMeta` + simply-op-macros' `#[derive(Op)]`).
pub mod export {
    pub use async_trait;
    pub use rmcp;
    pub use schemars;
    pub use serde;
    pub use serde_json;
    pub use simply_history;
    pub use simply_op_macros;
}

// ── the error envelope ──────────────────────────────────────────────────

/// What every api method returns.
pub type ApiResult<T> = Result<T, ApiError>;

/// Broad failure class — lets callers branch without parsing messages.
#[derive(Debug, Clone, Copy, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ApiErrorKind {
    /// The method exists but this service can't perform it (e.g. a view
    /// operation on a headless service).
    NotSupported,
    /// The arguments didn't deserialize or failed validation.
    InvalidParams,
    /// The operation ran and failed; the message says why, in terms the
    /// caller can act on.
    Failed,
}

/// A readable error envelope: `message` is the full human/agent-facing
/// text (it is what MCP tool errors show), `kind` the machine-facing class.
#[derive(Debug, Clone, PartialEq, serde::Serialize, serde::Deserialize)]
pub struct ApiError {
    pub kind: ApiErrorKind,
    pub message: String,
}

impl ApiError {
    pub fn failed(message: impl Into<String>) -> Self {
        Self { kind: ApiErrorKind::Failed, message: message.into() }
    }

    pub fn invalid_params(message: impl Into<String>) -> Self {
        Self { kind: ApiErrorKind::InvalidParams, message: message.into() }
    }

    /// For declared-but-not-yet-supported methods (e.g. view-surface
    /// methods on a headless service).
    pub fn not_supported(what: &str) -> Self {
        Self {
            kind: ApiErrorKind::NotSupported,
            message: format!("{what} is not supported by this service yet"),
        }
    }
}

impl std::fmt::Display for ApiError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for ApiError {}

// ── the transport seam ──────────────────────────────────────────────────

/// Name+JSON method invocation — the seam between a generated
/// `Remote{Trait}` client and whatever carries the call (the generated
/// `{Trait}Dispatcher` in-process, an HTTP transport later).
#[async_trait::async_trait]
pub trait Caller: Send + Sync {
    async fn call(
        &self,
        method: &str,
        params: serde_json::Value,
    ) -> Result<serde_json::Value, ApiError>;
}
