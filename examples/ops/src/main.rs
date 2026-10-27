//! Copyright (c) Scott A Dixon
//!
//! The executable Clapham's conformance harness drives: a daemon serving Walthamstow's
//! operations over MCP stdio or Streamable HTTP, and the CLI assembled from them, which calls
//! the daemon over MCP. Over Streamable HTTP, the daemon publishes its endpoint record and the
//! CLI, naming no endpoint, finds it there. Authority is the harness's scripted grant.

use clapham::{Bounds, OAuthContext};
use clapham_cli::{Cli, McpBackend};
use clapham_conformance::scripted_access;
use clapham_mcp_server::{Credential, HttpServer, Server};
use serde_json::{json, Value};
use std::error::Error;
use std::io::Write;
use std::process::{ExitCode, Stdio};
use tokio::io::AsyncReadExt;
use walthamstow_kernel::local_store::MemoryLocalStore;
use walthamstow_ops::EndpointRecord;

type Result<T> = std::result::Result<T, Box<dyn Error + Send + Sync>>;

const NAME: &str = "walthamstow";

fn main() -> ExitCode {
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()
        .expect("a runtime");
    let outcome = runtime.block_on(run());
    runtime.shutdown_background();
    match outcome {
        Ok(code) => ExitCode::from(code),
        Err(error) => ExitCode::from(clapham_cli::write_refusal(
            &clapham::Refusal::not_dispatched(
                "application_configuration",
                clapham::Category::Configuration,
                error.to_string(),
            ),
            std::io::stderr(),
        )),
    }
}

fn fixture_mode() -> Result<()> {
    if std::env::var("CLAPHAM_FIXTURE_MODE").as_deref() != Ok("1") {
        return Err("ops-example runs only under the conformance harness".into());
    }
    Ok(())
}

/// The scripted grant the harness launched this process with.
fn fixture_authority() -> Result<OAuthContext> {
    fixture_mode()?;
    let claims: Value = serde_json::from_str(&std::env::var("CLAPHAM_FIXTURE_ACCESS")?)?;
    scripted_access(&claims).ok_or_else(|| "invalid scripted fixture authority".into())
}

/// The daemon's MCP server over Streamable HTTP. Each request's bearer token is its scripted
/// grant.
fn http_server() -> Result<HttpServer> {
    Ok(HttpServer::new(
        ops_example::composed()?,
        |credential: &Credential| {
            let claims = credential.authorization()?.strip_prefix("Bearer ")?;
            scripted_access(&serde_json::from_str::<Value>(claims).ok()?)
        },
    )?)
}

async fn run() -> Result<u8> {
    let args: Vec<_> = std::env::args().skip(1).collect();
    match args
        .iter()
        .map(String::as_str)
        .collect::<Vec<_>>()
        .as_slice()
    {
        ["serve", "--stdio"] => {
            Server::new(ops_example::composed()?, fixture_authority()?)?
                .serve_stdio()
                .await?
                .waiting()
                .await?;
            Ok(0)
        }
        ["serve", "--http", address] => {
            fixture_mode()?;
            let listener = tokio::net::TcpListener::bind(address).await?;
            if !listener.local_addr()?.ip().is_loopback() {
                return Err("scripted fixture authentication requires loopback".into());
            }
            let app = http_server()?.into_router();
            println!(
                "{}",
                json!({ "address": listener.local_addr()?.to_string() })
            );
            std::io::stdout().flush()?;
            axum::serve(listener, app)
                .with_graceful_shutdown(stdin_closed())
                .await?;
            Ok(0)
        }
        _ if std::env::var("CLAPHAM_TRANSPORT").as_deref() == Ok("stdio") => cli_over_stdio().await,
        _ => cli_over_http().await,
    }
}

/// Serves a daemon on an ephemeral loopback listener, which publishes its endpoint record, and
/// runs the CLI against the daemon the record names.
async fn cli_over_http() -> Result<u8> {
    let access = fixture_authority()?;
    // The daemon's data directory.
    let store = MemoryLocalStore::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let published = EndpointRecord {
        endpoint: format!("http://{}/mcp", listener.local_addr()?),
        token: serde_json::to_string(&access)?,
    }
    .publish(&store)?;
    let app = http_server()?.into_router();
    let stop = clapham::Cancellation::new();
    let stopped = stop.clone();
    let daemon = tokio::spawn(async move {
        axum::serve(listener, app)
            .with_graceful_shutdown(async move { stopped.cancelled().await })
            .await
    });
    let found = EndpointRecord::resolve(None, &store)?.ok_or("no daemon is running")?;
    let backend = McpBackend::http(found.endpoint, found.token).await?;
    let result = Cli::connect(NAME, backend, Bounds::default())
        .await?
        .run()
        .await;
    stop.cancel();
    daemon.await??;
    if !published.retire(&store)? {
        return Err("the daemon's endpoint record was replaced".into());
    }
    Ok(result)
}

/// Launches this executable's stdio daemon and runs the CLI over its standard streams.
async fn cli_over_stdio() -> Result<u8> {
    let mut daemon = tokio::process::Command::new(std::env::current_exe()?)
        .args(["serve", "--stdio"])
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::inherit())
        .kill_on_drop(true)
        .spawn()?;
    let input = daemon.stdin.take().ok_or("missing daemon stdin")?;
    let output = daemon.stdout.take().ok_or("missing daemon stdout")?;
    let backend = McpBackend::stdio(output, input).await?;
    // Dropping the CLI closes the connection and the daemon's stdin.
    let result = Cli::connect(NAME, backend, Bounds::default())
        .await?
        .run()
        .await;
    if !daemon.wait().await?.success() {
        return Err("the stdio daemon failed".into());
    }
    Ok(result)
}

/// Resolves when the harness closes this process's standard input.
async fn stdin_closed() {
    let mut stdin = tokio::io::stdin();
    let mut byte = [0];
    let _ = stdin.read(&mut byte).await;
}
