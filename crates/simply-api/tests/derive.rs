//! `#[api_service]` end-to-end over a demo trait: generated params structs,
//! typed request/response enums + typed dispatch, registry meta + lint, tool
//! router shape, dispatcher + remote-client loopback, `no_tool` / `name` /
//! `#[op]` attrs, and default (not-supported) methods.

use std::sync::Arc;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use simply_api::{api_service, ApiError, ApiErrorKind, ApiResult, Caller};
use tokio::sync::Mutex;

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize, JsonSchema)]
pub struct Item {
    /// The item's unique name.
    pub name: String,
    /// How many are stored.
    pub count: u32,
}

/// A tiny inventory service.
#[api_service]
#[async_trait]
pub trait InventoryApi: Send + Sync {
    /// List every item.
    async fn list(&self) -> ApiResult<Vec<Item>>;

    /// Add an item; returns the stored record.
    #[op(mutates, label = "add {count:?} {name}")]
    async fn add(
        &self,
        /// Item name.
        name: String,
        /// How many to add (default 1).
        count: Option<u32>,
    ) -> ApiResult<Item>;

    /// Remove an item by name; confirmation text.
    #[op(mutates)]
    async fn remove(
        &self,
        /// Item name.
        name: String,
    ) -> ApiResult<String>;

    /// Drop everything (not exposed as an MCP tool).
    #[api(no_tool)]
    #[op(mutates)]
    async fn wipe(&self) -> ApiResult<()>;

    /// How many distinct items exist.
    #[api(name = "count_items")]
    async fn count(&self) -> ApiResult<u32>;

    /// A capability this service doesn't have yet.
    async fn preview(&self) -> ApiResult<String> {
        Err(ApiError::not_supported("preview"))
    }
}

#[derive(Default)]
struct Inventory(Mutex<Vec<Item>>);

#[async_trait]
impl InventoryApi for Inventory {
    async fn list(&self) -> ApiResult<Vec<Item>> {
        Ok(self.0.lock().await.clone())
    }

    async fn add(&self, name: String, count: Option<u32>) -> ApiResult<Item> {
        let item = Item { name, count: count.unwrap_or(1) };
        self.0.lock().await.push(item.clone());
        Ok(item)
    }

    async fn remove(&self, name: String) -> ApiResult<String> {
        let mut items = self.0.lock().await;
        let before = items.len();
        items.retain(|i| i.name != name);
        if items.len() == before {
            return Err(ApiError::failed(format!("no item named {name:?}")));
        }
        Ok(format!("removed {name}"))
    }

    async fn wipe(&self) -> ApiResult<()> {
        self.0.lock().await.clear();
        Ok(())
    }

    async fn count(&self) -> ApiResult<u32> {
        Ok(self.0.lock().await.len() as u32)
    }
}

// ── registry meta ───────────────────────────────────────────────────────

#[test]
fn meta_records_every_method_and_lints_clean() {
    assert_eq!(INVENTORY_API_META.service, "InventoryApi");
    assert_eq!(INVENTORY_API_META.methods.len(), 6);
    assert_eq!(INVENTORY_API_META.tool_count(), 5, "wipe is no_tool");

    let issues = INVENTORY_API_META.lint();
    assert!(issues.is_empty(), "lint should be clean: {issues:?}");

    // name override lands in the wire name, not the rust name.
    let count = INVENTORY_API_META.method("count_items").unwrap();
    assert_eq!(count.rust_name, "count");
    assert!(INVENTORY_API_META.method("count").is_none());

    // parameter docs became schemars descriptions.
    let add = INVENTORY_API_META.method("add").unwrap();
    let schema = (add.params_schema)();
    assert_eq!(schema["properties"]["name"]["description"], "Item name.");
    assert_eq!(schema["properties"]["count"]["description"], "How many to add (default 1).");
    assert_eq!(schema["required"], serde_json::json!(["name"]));
    let params: Vec<(&str, bool)> = add.params.iter().map(|p| (p.name, p.optional)).collect();
    assert_eq!(params, [("name", false), ("count", true)], "declaration order + optionality");
    assert_eq!(add.output_type, "Item");
    assert!(!add.returns_text);
    assert!(INVENTORY_API_META.method("remove").unwrap().returns_text);
}

#[test]
fn lint_catches_missing_descriptions() {
    // A deliberately undocumented sibling trait: the lint must flag it.
    /// (method doc present)
    #[api_service]
    #[async_trait]
    #[allow(dead_code)]
    pub trait BareApi: Send + Sync {
        async fn undocumented(&self, value: f32) -> ApiResult<String>;
    }
    let issues = BARE_API_META.lint();
    assert_eq!(issues.len(), 2, "{issues:?}");
    assert!(issues[0].contains("undocumented") && issues[0].contains("no doc comment"));
    assert!(issues[1].contains("`value`") && issues[1].contains("no description"));
}

// ── tool router shape ───────────────────────────────────────────────────

#[test]
fn tool_router_mirrors_the_trait() {
    let router = inventory_api_tool_router::<Inventory>();
    let tools = router.list_all();
    let names: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    assert_eq!(names, ["add", "count_items", "list", "preview", "remove"], "sorted, no wipe");
    assert_eq!(tools.len(), INVENTORY_API_META.tool_count());

    let add = tools.iter().find(|t| t.name == "add").unwrap();
    assert_eq!(add.description.as_deref(), Some("Add an item; returns the stored record."));
    let input = serde_json::to_value(add.input_schema.as_ref()).unwrap();
    assert_eq!(input["type"], "object");
    assert_eq!(input["properties"]["name"]["description"], "Item name.");
    assert!(add.output_schema.is_some(), "structured returns carry an output schema");

    let remove = tools.iter().find(|t| t.name == "remove").unwrap();
    assert!(remove.output_schema.is_none(), "text returns don't");
}

// ── dispatcher + remote client loopback ─────────────────────────────────

fn remote() -> RemoteInventoryApi<InventoryApiDispatcher<Inventory>> {
    RemoteInventoryApi(InventoryApiDispatcher(Arc::new(Inventory::default())))
}

#[tokio::test]
async fn remote_client_round_trips_through_the_dispatcher() {
    let api = remote();
    let stored = api.add("plumbus".into(), Some(3)).await.unwrap();
    assert_eq!(stored, Item { name: "plumbus".into(), count: 3 });
    api.add("gizmo".into(), None).await.unwrap();

    assert_eq!(api.count().await.unwrap(), 2);
    assert_eq!(api.list().await.unwrap().len(), 2);
    assert_eq!(api.remove("gizmo".into()).await.unwrap(), "removed gizmo");

    // Errors cross the seam with kind and message intact.
    let e = api.remove("gizmo".into()).await.unwrap_err();
    assert_eq!(e.kind, ApiErrorKind::Failed);
    assert_eq!(e.message, "no item named \"gizmo\"");
    let e = api.preview().await.unwrap_err();
    assert_eq!(e.kind, ApiErrorKind::NotSupported);

    // no_tool methods still dispatch.
    api.wipe().await.unwrap();
    assert_eq!(api.count().await.unwrap(), 0);
}

#[tokio::test]
async fn dispatcher_rejects_unknown_methods_and_bad_params_readably() {
    let dispatcher = InventoryApiDispatcher(Arc::new(Inventory::default()));

    let e = dispatcher.call("frobnicate", serde_json::json!({})).await.unwrap_err();
    assert_eq!(e.kind, ApiErrorKind::Failed);
    assert!(e.message.contains("unknown method \"frobnicate\""), "{e}");
    assert!(e.message.contains("count_items"), "lists the known methods: {e}");

    let e = dispatcher.call("add", serde_json::json!({"count": 2})).await.unwrap_err();
    assert_eq!(e.kind, ApiErrorKind::InvalidParams);
    assert!(e.message.contains("missing field `name`"), "{e}");
}

#[tokio::test]
async fn dyn_alias_is_the_in_process_client() {
    let api: DynInventoryApi = Arc::new(Inventory::default());
    api.add("thing".into(), None).await.unwrap();
    assert_eq!(api.count().await.unwrap(), 1);
}

// ── typed request/response enums ────────────────────────────────────────

#[test]
fn request_enum_carries_real_types_and_the_wire_names() {
    // One variant per method, named fields = the real param types.
    let add = InventoryRequest::Add { name: "plumbus".into(), count: Some(2) };
    let json = serde_json::to_value(&add).unwrap();
    assert_eq!(json, serde_json::json!({"add": {"name": "plumbus", "count": 2}}));
    let back: InventoryRequest = serde_json::from_value(json).unwrap();
    assert_eq!(back, add);
    assert_eq!(add.method(), "add");

    // `#[api(name = ...)]` overrides land in the variant tag too.
    let count = InventoryRequest::CountItems {};
    assert_eq!(count.method(), "count_items");
    assert_eq!(serde_json::to_value(&count).unwrap(), serde_json::json!({"count_items": {}}));

    // Optional params may be omitted on the wire.
    let sparse: InventoryRequest =
        serde_json::from_value(serde_json::json!({"add": {"name": "gizmo"}})).unwrap();
    assert_eq!(sparse, InventoryRequest::Add { name: "gizmo".into(), count: None });
}

#[tokio::test]
async fn typed_dispatch_equals_the_direct_trait_call() {
    let api = Inventory::default();
    let response = InventoryRequest::Add { name: "plumbus".into(), count: Some(3) }
        .dispatch(&api)
        .await
        .unwrap();
    assert_eq!(
        response,
        InventoryResponse::Add(Item { name: "plumbus".into(), count: 3 }),
        "typed in, typed out — no JSON anywhere on this path"
    );

    let direct = api.list().await.unwrap();
    let dispatched = InventoryRequest::List {}.dispatch(&api).await.unwrap();
    assert_eq!(dispatched, InventoryResponse::List(direct));

    // Unit returns are unit variants; errors pass through typed.
    assert_eq!(InventoryRequest::Wipe {}.dispatch(&api).await.unwrap(), InventoryResponse::Wipe);
    let e = InventoryRequest::Remove { name: "gone".into() }.dispatch(&api).await.unwrap_err();
    assert_eq!(e.kind, ApiErrorKind::Failed);
}

#[tokio::test]
async fn json_dispatcher_is_a_shim_over_the_typed_path() {
    let api = Arc::new(Inventory::default());
    let dispatcher = InventoryApiDispatcher(Arc::clone(&api));

    // The name+JSON edge and the typed path produce identical payloads.
    let via_json =
        dispatcher.call("add", serde_json::json!({"name": "plumbus", "count": 3})).await.unwrap();
    let typed = InventoryRequest::Add { name: "plumbus".into(), count: Some(3) }
        .dispatch(&*api)
        .await
        .unwrap();
    assert_eq!(via_json, typed.into_json().unwrap());

    // Null params mean "no arguments" (transport tolerance).
    let via_json = dispatcher.call("count_items", serde_json::Value::Null).await.unwrap();
    assert_eq!(via_json, serde_json::json!(2));

    // Unit responses serialize as null at the edge.
    assert_eq!(
        dispatcher.call("wipe", serde_json::json!({})).await.unwrap(),
        serde_json::Value::Null
    );
}

#[test]
fn op_metadata_reaches_the_request_enum() {
    use simply_api::export::simply_history::OpMeta;

    assert!(InventoryRequest::Add { name: "x".into(), count: None }.mutates());
    assert!(InventoryRequest::Wipe {}.mutates());
    assert!(!InventoryRequest::List {}.mutates(), "unannotated methods default to read");
    assert!(!InventoryRequest::CountItems {}.mutates());

    // Label templates interpolate the request's own fields; the default
    // label is the wire name.
    assert_eq!(
        InventoryRequest::Add { name: "plumbus".into(), count: Some(2) }.label(),
        "add Some(2) plumbus"
    );
    assert_eq!(InventoryRequest::Remove { name: "x".into() }.label(), "remove");
    assert_eq!(InventoryRequest::List {}.label(), "list");
}

#[test]
fn request_schema_documents_every_variant() {
    let schema = simply_api::meta::schema_of::<InventoryRequest>();
    let one_of = schema["oneOf"].as_array().expect("externally tagged enum schema");
    assert_eq!(one_of.len(), INVENTORY_API_META.methods.len(), "a variant per method");
}
