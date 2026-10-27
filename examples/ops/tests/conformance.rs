//! Copyright (c) Scott A Dixon
//!
//! Walthamstow's operations through every Clapham front end: in-process, the CLI against its
//! daemon over Streamable HTTP, found through the daemon's endpoint record, and over stdio, raw
//! MCP over stdio and Streamable HTTP, and the MCP client bound to each. Every front end sees
//! the in-process result.

use clapham::{Cancellation, OAuthContext, RecordSink, ScopeGate};
use clapham_conformance::{
    run, Case, Fixture, FrontEnd, InProcess, Process, ProcessFrontEnd, ProcessKind,
};
use ops_example::{operator, AUDIENCE, ISSUER, UPGRADE};
use serde_json::{json, Value};
use std::sync::Arc;

fn case(operation: &str, input: Value) -> Case {
    let mut case = Case::new(operation, input);
    case.auth = operator();
    case
}

/// The revision a preview of the seeded upgrade reports.
async fn upgrade_revision() -> Value {
    let set = ops_example::composed().unwrap();
    let outcome = set
        .call(
            &set.all(),
            operator(),
            "walthamstow.workspace.upgrade",
            json!({ "upgrade": UPGRADE }),
            RecordSink::default(),
            Cancellation::new(),
            true,
        )
        .await
        .unwrap();
    outcome.result.unwrap()["revision"].clone()
}

async fn cases() -> Vec<Case> {
    vec![
        case("walthamstow.workspace.status", json!({})),
        case("walthamstow.workspace.documents", json!({})),
        case(
            "walthamstow.workspace.upgrade",
            json!({ "upgrade": UPGRADE, "expected_revision": upgrade_revision().await }),
        ),
        case(
            "walthamstow.workspace.upgrade",
            json!({ "upgrade": UPGRADE, "expected_revision": "stale" }),
        ),
        case(
            "walthamstow.workspace.upgrade",
            json!({ "upgrade": "format-9" }),
        ),
        case("walthamstow.workspace.recover", json!({})),
        case("walthamstow.journals.list", json!({})),
    ]
}

#[tokio::test]
async fn every_operation_has_one_result_through_every_front_end() {
    let mut fixture = Fixture::new(ops_example::contributions, cases().await);
    fixture.denied_auth =
        OAuthContext::from_verified(ISSUER, AUDIENCE, "visitor", Vec::<String>::new());
    let process = Process::new(env!("CARGO_BIN_EXE_ops-example"));
    let mut front_ends: Vec<Box<dyn FrontEnd>> = vec![Box::new(InProcess)];
    for kind in [
        ProcessKind::CommandLine,
        ProcessKind::CommandLineStdio,
        ProcessKind::McpStdio,
        ProcessKind::McpHttp,
        ProcessKind::BoundStdio,
        ProcessKind::BoundHttp,
    ] {
        front_ends.push(Box::new(ProcessFrontEnd::new(kind, process.clone())));
    }
    let report = run(&fixture, Arc::new(ScopeGate), front_ends)
        .await
        .unwrap();
    assert_eq!(report.front_ends, 7);
    assert_eq!(report.calls, fixture.cases.len() * 7);
}
