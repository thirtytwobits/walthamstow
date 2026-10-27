//! Copyright (c) Scott A Dixon
//!
//! A CLI and its daemon: the CLI finds the daemon through the endpoint record the daemon
//! publishes, and a CLI running in-process is refused while a daemon holds the workspace.

use clapham::{Bounds, Cancellation, RecordSink};
use clapham_cli::{Cli, CliError, McpBackend};
use clapham_mcp_server::{Credential, HttpServer};
use ops_example::operator;
use serde_json::json;
use walthamstow_kernel::local_store::MemoryLocalStore;
use walthamstow_ops::{codes, EndpointRecord, Lease, MemoryLock};

const TOKEN: &str = "Bearer local-credential";

fn status_args() -> [&'static str; 4] {
    ["walthamstow", "walthamstow", "workspace", "status"]
}

#[tokio::test]
async fn a_cli_naming_no_endpoint_finds_the_daemon_through_its_record() {
    let store = MemoryLocalStore::new();
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let published = EndpointRecord {
        endpoint: format!("http://{}/mcp", listener.local_addr().unwrap()),
        token: TOKEN.trim_start_matches("Bearer ").into(),
    }
    .publish(&store)
    .unwrap();
    let server = HttpServer::new(
        ops_example::composed().unwrap(),
        |credential: &Credential| (credential.authorization()? == TOKEN).then(operator),
    )
    .unwrap();
    let stop = Cancellation::new();
    let stopped = stop.clone();
    let daemon = tokio::spawn(async move {
        axum::serve(listener, server.into_router())
            .with_graceful_shutdown(async move { stopped.cancelled().await })
            .await
    });

    let found = EndpointRecord::resolve(None, &store).unwrap().unwrap();
    let backend = McpBackend::http(found.endpoint, found.token).await.unwrap();
    let cli = Cli::connect("walthamstow", backend, Bounds::default())
        .await
        .unwrap();
    let outcome = cli
        .run_from(
            status_args(),
            std::io::empty(),
            RecordSink::default(),
            Cancellation::new(),
        )
        .await
        .unwrap();
    let in_process = ops_example::composed().unwrap();
    let expected = in_process
        .call(
            &in_process.all(),
            operator(),
            "walthamstow.workspace.status",
            json!({}),
            RecordSink::default(),
            Cancellation::new(),
            false,
        )
        .await
        .unwrap();
    assert_eq!(outcome.result, expected.result);

    drop(cli);
    stop.cancel();
    daemon.await.unwrap().unwrap();
    assert!(published.retire(&store).unwrap());
    assert_eq!(EndpointRecord::resolve(None, &store).unwrap(), None);
}

#[tokio::test]
async fn a_cli_running_in_process_is_refused_while_a_daemon_holds_the_workspace() {
    let lock = MemoryLock::new();
    let set = clapham::compose(
        [ops_example::operations(Lease::in_process_memory(lock.clone())).contribution()],
        Bounds::default(),
    )
    .unwrap();
    let cli = Cli::new("walthamstow", set, operator()).unwrap();
    let run = || {
        cli.run_from(
            status_args(),
            std::io::empty(),
            RecordSink::default(),
            Cancellation::new(),
        )
    };

    let daemon = lock.acquire().unwrap();
    let refused = run().await.unwrap_err();
    assert_eq!(refused.exit_code(), 69);
    let CliError::Refusal(refused) = refused else {
        panic!("a refusal, not help");
    };
    assert_eq!(refused.code, codes::WORKSPACE_HELD);

    drop(daemon);
    let outcome = run().await.unwrap();
    assert_eq!(outcome.result.unwrap()["holder"], "process");
}
