//! What an `McpApiServer` negotiates, and what the reply then looks like **on
//! the wire**.
//!
//! Every assertion here is against `serde_json::Value`, not against the Rust
//! types: the defect these tests pin was invisible at the type level, because
//! the fields a `2026-07-28` `tools/list` result requires exist on rmcp's
//! `ListToolsResult` as `Option`s that serde skips when they are `None`. The
//! Rust value was well typed and the JSON was two required fields short.
//!
//! The server is driven over an in-memory sink/stream transport with
//! hand-written JSON-RPC frames — the same bytes a client POSTs — so no HTTP
//! stack is needed to see the shape.

use std::sync::Arc;

use futures::{channel::mpsc, SinkExt, StreamExt};
use rmcp::{
    handler::server::router::tool::ToolRouter,
    model::{ClientJsonRpcMessage, ProtocolVersion, ServerJsonRpcMessage},
    serve_server,
};
use serde_json::{json, Value};
use simply_api::mcp::{supported_protocol_versions, McpApiServer};

/// An `initialize` at `asked`, then a `tools/list`, against a bare server:
/// the two replies as JSON. No tools are mounted — what is under test is the
/// envelope the version negotiation picks, not any tool's schema.
async fn initialize_then_list(asked: &str) -> (Value, Value) {
    let server: McpApiServer<()> = McpApiServer::new(Arc::new(()), ToolRouter::new());

    // The server's side of the pipe: it sends server messages, receives
    // client ones.
    let (to_client, mut from_server) = mpsc::unbounded::<ServerJsonRpcMessage>();
    let (mut to_server, from_client) = mpsc::unbounded::<ClientJsonRpcMessage>();

    let asked = asked.to_owned();
    let client = tokio::spawn(async move {
        let send = |message: Value| {
            serde_json::from_value::<ClientJsonRpcMessage>(message).expect("a client frame")
        };
        to_server
            .send(send(json!({
                "jsonrpc": "2.0",
                "id": 1,
                "method": "initialize",
                "params": {
                    "protocolVersion": asked,
                    "capabilities": {},
                    "clientInfo": {"name": "shape-test", "version": "0"},
                },
            })))
            .await
            .expect("initialize is sent");
        let initialized = from_server.next().await.expect("initialize is answered");
        to_server
            .send(send(json!({"jsonrpc": "2.0", "method": "notifications/initialized"})))
            .await
            .expect("the notification is sent");
        to_server
            .send(send(json!({"jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {}})))
            .await
            .expect("tools/list is sent");
        let listed = from_server.next().await.expect("tools/list is answered");
        (
            serde_json::to_value(initialized).expect("json"),
            serde_json::to_value(listed).expect("json"),
        )
    });

    let running = serve_server(server, (to_client, from_client)).await.expect("the server serves");
    let replies = client.await.expect("the client finishes");
    running.cancel().await.expect("the server stops");
    replies
}

fn result(reply: &Value) -> &Value {
    assert!(reply.get("error").is_none(), "the reply is an error: {reply}");
    reply.get("result").unwrap_or_else(|| panic!("a reply carries a result: {reply}"))
}

fn keys(value: &Value) -> Vec<&str> {
    value.as_object().expect("an object").keys().map(String::as_str).collect()
}

#[tokio::test]
async fn a_client_asking_for_a_revision_we_do_not_emit_is_answered_the_newest_one_we_do() {
    let (initialized, listed) = initialize_then_list("2026-07-28").await;

    assert_eq!(
        result(&initialized).get("protocolVersion").and_then(Value::as_str),
        Some("2025-11-25"),
        "the newest revision rmcp emits is what a 2026-07-28 client is answered, \
         not its own: {initialized}"
    );

    // The shape that made an agent client refuse the tool list: `resultType`
    // present (2026-07-28's discriminator) with neither of the cache fields
    // that revision also requires. Answered 2025-11-25, the result is the
    // legacy envelope and carries nothing but `tools`.
    assert_eq!(keys(result(&listed)), ["tools"], "the tools/list envelope: {listed}");
}

#[tokio::test]
async fn a_client_asking_for_a_revision_we_do_emit_is_answered_its_own() {
    for asked in ["2025-06-18", "2025-11-25", "2024-11-05"] {
        let (initialized, listed) = initialize_then_list(asked).await;
        assert_eq!(
            result(&initialized).get("protocolVersion").and_then(Value::as_str),
            Some(asked),
            "{asked} is echoed, not narrowed: {initialized}"
        );
        assert_eq!(
            keys(result(&listed)),
            ["tools"],
            "the tools/list envelope at {asked}: {listed}"
        );
    }
}

/// The reason for the cap, stated as an assertion: every revision the server
/// agrees to must be one whose *required* result fields rmcp fills in. The
/// paginated results carry `ttlMs`/`cacheScope` as `Option`s left unset, so
/// the honest boundary is rmcp's own `LATEST`.
#[test]
fn the_agreed_revisions_are_the_ones_rmcp_emits() {
    let supported = supported_protocol_versions();
    assert!(
        !supported.contains(&ProtocolVersion::V_2026_07_28),
        "2026-07-28 requires ttlMs and cacheScope on every list result, which rmcp \
         leaves unset: {supported:?}"
    );
    assert!(
        supported.contains(&ProtocolVersion::LATEST),
        "the newest revision rmcp emits is on the list: {supported:?}"
    );
    assert!(
        supported.iter().all(|version| *version <= ProtocolVersion::LATEST),
        "nothing past what rmcp emits is agreed to: {supported:?}"
    );
}
