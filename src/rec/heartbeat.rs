//! Progress notifications for calls that take a while.
//!
//! `record` blocks for its take and `await_recording` for whatever is left of
//! one, and an MCP client times out a quiet request. When the caller sent a
//! progress token, this wrapper sends a progress notification every few
//! seconds for as long as a tool call runs — which clients that honour
//! progress (the MCP TypeScript SDK's `resetTimeoutOnProgress`) use to keep
//! the request alive. It knows nothing about recording: it is wrapped around
//! the derived server, so every tool gets it and none has to ask.

use std::borrow::Cow;
use std::time::{Duration, Instant};

use rmcp::model::{
    CallToolRequestParams, CallToolResponse, ListToolsResult, PaginatedRequestParams,
    ProgressNotificationParam, ProtocolVersion, ServerInfo, Tool,
};
use rmcp::service::{NotificationContext, RequestContext, RoleServer};
use rmcp::{ErrorData, ServerHandler};

/// How often a running call reports progress.
pub const EVERY: Duration = Duration::from_secs(5);

/// A server handler that reports progress while its inner handler's tool
/// calls run.
#[derive(Clone)]
pub struct Heartbeat<S> {
    inner: S,
}

impl<S> Heartbeat<S> {
    pub fn new(inner: S) -> Self {
        Heartbeat { inner }
    }

    pub fn inner(&self) -> &S {
        &self.inner
    }
}

impl<S: ServerHandler + Sync> ServerHandler for Heartbeat<S> {
    fn get_info(&self) -> ServerInfo {
        self.inner.get_info()
    }

    fn supported_protocol_versions(&self) -> Cow<'static, [ProtocolVersion]> {
        self.inner.supported_protocol_versions()
    }

    async fn call_tool(
        &self,
        request: CallToolRequestParams,
        context: RequestContext<RoleServer>,
    ) -> Result<CallToolResponse, ErrorData> {
        let Some(token) = context.meta.get_progress_token() else {
            return self.inner.call_tool(request, context).await;
        };
        let peer = context.peer.clone();
        let call = self.inner.call_tool(request, context);
        tokio::pin!(call);
        let started = Instant::now();
        let mut ticks = tokio::time::interval_at(tokio::time::Instant::now() + EVERY, EVERY);
        loop {
            tokio::select! {
                result = &mut call => return result,
                _ = ticks.tick() => {
                    let elapsed = started.elapsed().as_secs_f64();
                    let mut note = ProgressNotificationParam::new(token.clone(), elapsed);
                    note.message = Some(format!("still recording ({elapsed:.0} s)"));
                    let _ = peer.notify_progress(note).await;
                }
            }
        }
    }

    async fn list_tools(
        &self,
        request: Option<PaginatedRequestParams>,
        context: RequestContext<RoleServer>,
    ) -> Result<ListToolsResult, ErrorData> {
        self.inner.list_tools(request, context).await
    }

    fn get_tool(&self, name: &str) -> Option<Tool> {
        self.inner.get_tool(name)
    }

    async fn on_initialized(&self, context: NotificationContext<RoleServer>) {
        self.inner.on_initialized(context).await
    }
}
