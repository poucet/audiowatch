//! The HTTP wiring: the derived tool surface over rmcp's streamable-HTTP
//! transport at `/mcp`, on 127.0.0.1 only.

use std::net::SocketAddr;
use std::sync::Arc;

use rmcp::transport::streamable_http_server::{
    session::local::LocalSessionManager, StreamableHttpServerConfig, StreamableHttpService,
};

use super::service::{mcp_server_over, AudioRecService};

/// Default bind port.
pub const DEFAULT_PORT: u16 = 3929;
/// Path the streamable-HTTP MCP endpoint is served under.
pub const MCP_PATH: &str = "/mcp";

/// The axum app serving the MCP endpoint at [`MCP_PATH`].
pub fn app(service: AudioRecService) -> axum::Router {
    let service = Arc::new(service);
    let mcp = StreamableHttpService::new(
        move || Ok(mcp_server_over(service.clone())),
        Arc::new(LocalSessionManager::default()),
        StreamableHttpServerConfig::default(),
    );
    axum::Router::new().nest_service(MCP_PATH, mcp)
}

/// Bind `127.0.0.1:port` and serve until ctrl-c.
pub async fn serve(port: u16, service: AudioRecService) -> std::io::Result<()> {
    let addr = SocketAddr::from(([127, 0, 0, 1], port));
    let listener = tokio::net::TcpListener::bind(addr).await?;
    eprintln!(
        "audiowatch: serving MCP at http://{addr}{MCP_PATH}; files go in {}",
        service.dir().display()
    );
    axum::serve(listener, app(service))
        .with_graceful_shutdown(async {
            let _ = tokio::signal::ctrl_c().await;
        })
        .await
}

const USAGE: &str =
    "usage: audiowatch --mcp [--port N] [--dir DIR] (defaults 3929, ~/Music/audio-rec)";

fn parse(args: &[String]) -> Result<(u16, std::path::PathBuf), String> {
    let (mut port, mut dir) = (DEFAULT_PORT, audiowatch_record::default_dir());
    let mut it = args.iter();
    while let Some(arg) = it.next() {
        match arg.as_str() {
            "--mcp" => {}
            "--port" | "-p" => {
                let value = it.next().ok_or("--port needs a value")?;
                port = value
                    .parse()
                    .map_err(|_| format!("invalid port {value:?}"))?;
            }
            "--dir" | "-d" => dir = it.next().ok_or("--dir needs a value")?.into(),
            "--help" | "-h" => return Err(USAGE.to_string()),
            other => return Err(format!("unknown argument {other:?}; {USAGE}")),
        }
    }
    Ok((port, dir))
}

/// `audiowatch --mcp [--port N] [--dir DIR]`: serve until ctrl-c.
pub fn main(args: &[String]) -> std::process::ExitCode {
    use std::process::ExitCode;
    let (port, dir) = match parse(args) {
        Ok(v) => v,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(2);
        }
    };
    let runtime = match tokio::runtime::Builder::new_multi_thread()
        .enable_all()
        .build()
    {
        Ok(r) => r,
        Err(e) => {
            eprintln!("audiowatch: no async runtime: {e}");
            return ExitCode::FAILURE;
        }
    };
    match runtime.block_on(serve(port, AudioRecService::new(dir))) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("audiowatch: {e}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args(list: &[&str]) -> Vec<String> {
        list.iter().map(|s| s.to_string()).collect()
    }

    #[test]
    fn the_port_and_folder_have_defaults_and_can_be_named() {
        let (port, _) = parse(&args(&["--mcp"])).unwrap();
        assert_eq!(port, 3929);
        let (port, dir) = parse(&args(&["--mcp", "--port", "4000", "--dir", "/r"])).unwrap();
        assert_eq!((port, dir), (4000, std::path::PathBuf::from("/r")));
        assert!(parse(&args(&["--mcp", "--bogus"]))
            .unwrap_err()
            .contains("unknown argument"));
    }
}
