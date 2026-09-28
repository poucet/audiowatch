//! End to end over a real socket with a real rmcp client — the path
//! `claude mcp add --transport http` takes. No audio device is opened: every
//! call here is one the server answers or refuses before touching hardware,
//! so the test runs the same on a machine with no audio at all.

use audiowatch::rec::server::{app, MCP_PATH};
use audiowatch::rec::service::AudioRecService;
use audiowatch::rec::AUDIO_REC_API_META;
use rmcp::{
    model::{CallToolRequestParams, ClientInfo},
    transport::{
        streamable_http_client::StreamableHttpClientTransportConfig, StreamableHttpClientTransport,
    },
    ServiceExt,
};
use serde_json::{json, Value};
use tokio_util::sync::CancellationToken;

#[tokio::test]
async fn the_surface_is_three_tools_and_refusals_name_the_fix() {
    let ct = CancellationToken::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let dir = std::env::temp_dir().join(format!("audiowatch-rec-http-{}", std::process::id()));
    let server = tokio::spawn({
        let ct = ct.clone();
        async move {
            axum::serve(listener, app(AudioRecService::new(dir)))
                .with_graceful_shutdown(async move { ct.cancelled_owned().await })
                .await
                .unwrap();
        }
    });

    let transport = StreamableHttpClientTransport::from_config(
        StreamableHttpClientTransportConfig::with_uri(format!("http://{addr}{MCP_PATH}")),
    );
    let client = ClientInfo::default().serve(transport).await.unwrap();

    // Exactly the trait's tools — and no start/stop pair.
    let tools = client.list_all_tools().await.unwrap();
    let mut served: Vec<&str> = tools.iter().map(|t| t.name.as_ref()).collect();
    served.sort();
    assert_eq!(served, vec!["await_recording", "list_devices", "record"]);
    for tool in &tools {
        let meta = AUDIO_REC_API_META.method(&tool.name).unwrap();
        assert_eq!(
            tool.description.as_deref(),
            Some(meta.description),
            "{}",
            tool.name
        );
    }
    let instructions = client
        .peer_info()
        .and_then(|i| i.instructions.clone())
        .unwrap();
    assert!(
        instructions.contains("never") && instructions.contains("await_recording"),
        "{instructions}"
    );

    let refused = |name: &'static str, args: Value| {
        let client = &client;
        async move {
            let result = client
                .call_tool(
                    CallToolRequestParams::new(name)
                        .with_arguments(args.as_object().unwrap().clone()),
                )
                .await
                .unwrap();
            assert_eq!(result.is_error, Some(true), "{name} should refuse");
            serde_json::to_value(&result.content).unwrap()[0]["text"]
                .as_str()
                .unwrap()
                .to_string()
        }
    };

    let text = refused(
        "record",
        json!({"takes": ["BlackHole 16ch:in:1-2"], "seconds": 90}),
    )
    .await;
    assert!(
        text.contains("at most 50") && text.contains("wait: false"),
        "{text}"
    );
    let text = refused(
        "record",
        json!({"takes": ["BlackHole 16ch:in:1-3"], "seconds": 1}),
    )
    .await;
    assert!(text.contains("adjacent pair"), "{text}");
    let text = refused("await_recording", json!({"id": 42})).await;
    assert!(text.contains("no recording has that id"), "{text}");

    client.cancel().await.unwrap();
    ct.cancel();
    server.await.unwrap();
}

/// Not part of any gate: it opens a real device. Run by hand on a machine
/// with BlackHole 16ch to prove the `wait: false` → `await_recording` path:
/// `cargo test --test rec_http -- --ignored`.
#[tokio::test]
#[ignore = "opens a real audio device (BlackHole 16ch)"]
async fn a_background_take_on_blackhole_comes_back_finished() {
    use audiowatch::rec::AudioRecApi;
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("target/scratch/rec-mcp");
    let service = AudioRecService::new(dir);
    let started = service
        .record(
            vec!["BlackHole 16ch:in:1-2".into()],
            Some(1.0),
            None,
            None,
            None,
            None,
            Some(false),
        )
        .await
        .unwrap();
    assert!(!started.complete);
    let id = started.id.unwrap();
    let done = service.await_recording(id).await.unwrap();
    assert!(done.complete, "{done:?}");
    assert_eq!(done.takes[0].path, started.takes[0].path);
    assert_eq!(done.takes[0].duration_s, Some(1.0), "{done:?}");
    let reader = hound::WavReader::open(&done.takes[0].path).unwrap();
    assert_eq!(reader.spec().channels, 2);
    assert_eq!(reader.duration(), done.takes[0].sample_rate);
    let again = service.await_recording(id).await.unwrap_err();
    assert!(again.message.contains("already collected"));
    println!("{}", serde_json::to_string_pretty(&done).unwrap());
}
