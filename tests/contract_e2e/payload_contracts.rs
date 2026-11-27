#![allow(unused_imports)]

use std::{
    collections::{BTreeSet, HashMap},
    path::Path,
    process::Stdio,
    sync::{
        atomic::{AtomicUsize, Ordering},
        Arc,
    },
};

use axum::{
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, StatusCode},
    response::IntoResponse,
    routing::get,
    Json, Router,
};
use base64::Engine as _;
use onenote_cli::{
    auth::AuthManager, cli, config::AppConfig, graph::GraphClient, serve, storage::Storage,
};
use serde_json::{json, Value};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::TcpListener,
    process::Command,
};


use crate::harness::*;

#[tokio::test]
async fn read_json_is_compact_with_scope_key() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--json"],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let page = &v["page"];
    assert_eq!(page["page_key"], "me|page-1");
    assert_eq!(page["id"], "page-1");
    assert_eq!(page["scope"]["scope_type"], "me");
    assert!(page["title"]
        .as_str()
        .unwrap()
        .contains("Architecture Spec"));
    assert!(page["last_modified_at"].is_string());
    assert!(page["body_markdown"].is_string());
    assert_eq!(page["omitted"]["resources"], 2);
    assert_eq!(
        page["omitted"]["fields"],
        json!([
            "raw_xhtml",
            "body_plain",
            "primary_div_id",
            "summary.created_at",
            "summary.content_url",
            "summary.order",
            "summary.preview_text"
        ])
    );
    assert!(page.get("blocks").is_none());
    assert!(page.get("raw_xhtml").is_none());
    assert!(page.get("resources").is_none());
    assert!(page["hint"].as_str().unwrap().contains("--full"));

    server.abort();
}

#[tokio::test]
async fn read_json_full_matches_previous_shape() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--json", "--full"],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json_object_keys(&v["page"]), full_page_key_set());

    server.abort();
}

#[tokio::test]
async fn read_explicit_format_json_stays_full() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    for args in [
        &["read", "me|page-1", "--format", "xhtml", "--json"][..],
        &["read", "me|page-1", "--format", "md", "--json"][..],
        &[
            "read",
            "me|page-1",
            "--format",
            "blocks",
            "--json",
            "--full",
        ][..],
    ] {
        let out = run_onenote(&base_url, &cfg_dir, &data_dir, Some("opaque-a"), args).await;
        assert!(
            out.status.success(),
            "args={args:?}, stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            json_object_keys(&v["page"]),
            full_page_key_set(),
            "args={args:?}"
        );
        assert!(
            !v["page"]["raw_xhtml"].as_str().unwrap().is_empty(),
            "args={args:?}"
        );
    }

    server.abort();
}

#[tokio::test]
async fn read_explicit_format_json_compact() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &[
            "read",
            "me|page-1",
            "--format",
            "xhtml",
            "--json",
            "--compact",
        ],
    )
    .await;
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["page"].get("omitted").is_some());
    assert!(v["page"].get("raw_xhtml").is_none());

    server.abort();
}

#[tokio::test]
async fn read_compact_alias_equals_default() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out1 = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--json"],
    )
    .await;
    let out2 = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--json", "--compact"],
    )
    .await;
    assert!(out1.status.success() && out2.status.success());
    let mut v1: Value = serde_json::from_slice(&out1.stdout).unwrap();
    let mut v2: Value = serde_json::from_slice(&out2.stdout).unwrap();
    v1["page"].as_object_mut().unwrap().remove("synced_at");
    v2["page"].as_object_mut().unwrap().remove("synced_at");
    assert_eq!(v1, v2);

    server.abort();
}

#[tokio::test]
async fn read_selector_errors() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    for args in [
        &["read", "me|page-1", "--json", "--compact", "--full"][..],
        &["read", "me|page-1", "--full"][..],
    ] {
        let out = run_onenote(&base_url, &cfg_dir, &data_dir, Some("opaque-a"), args).await;
        assert_eq!(out.status.code(), Some(2), "args={args:?}");
        assert!(out.stdout.is_empty(), "args={args:?}");
    }

    // W6: cold-cache title returning 404 explains the cause and next actions (--section / pages).
    let cold = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "Cold Cache Spec"],
    )
    .await;
    assert_eq!(cold.status.code(), Some(1));
    assert!(cold.stdout.is_empty());
    let cold_err = String::from_utf8_lossy(&cold.stderr);
    assert!(
        cold_err.contains("OneNote page not found: 'Cold Cache Spec' is neither a cached title nor a page ID."),
        "stderr={cold_err}"
    );
    assert!(cold_err.contains("--section"), "stderr={cold_err}");

    server.abort();
}

#[tokio::test]
async fn runtime_json_errors_emit_single_stdout_object() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out_json = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|nonexistent-999", "--json"],
    )
    .await;
    assert_eq!(out_json.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out_json.stdout).unwrap_or_else(|e| {
        panic!(
            "expected single JSON error object ({e}), got: {}",
            String::from_utf8_lossy(&out_json.stdout)
        )
    });
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "not_found");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not found"));
    assert!(!out_json.stderr.is_empty());

    let out_text = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|nonexistent-999"],
    )
    .await;
    assert_eq!(out_text.status.code(), Some(1));
    assert!(out_text.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out_text.stderr).contains("OneNote page not found"));

    server.abort();
}

#[tokio::test]
async fn read_text_formats_unchanged() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let md = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1"],
    )
    .await;
    assert!(md.status.success());
    let md_text = String::from_utf8_lossy(&md.stdout);
    assert!(md_text.starts_with("# "), "md_text={md_text}");
    assert!(serde_json::from_str::<Value>(&md_text).is_err());

    let xhtml = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--format", "xhtml"],
    )
    .await;
    assert!(xhtml.status.success());
    let xhtml_text = String::from_utf8_lossy(&xhtml.stdout);
    assert!(xhtml_text.contains("<html"), "xhtml_text={xhtml_text}");
    assert!(serde_json::from_str::<Value>(&xhtml_text).is_err());

    server.abort();
}

#[tokio::test]
async fn write_json_is_compact_by_default_and_supports_full() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    // Default append --json is compact: no `page`, has `ok`, `page_key`, `title`, `last_modified_at`.
    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["append", "me|page-1", "hello", "--json"],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["page_key"], "me|page-1");
    assert!(v["title"].is_string());
    assert!(v.get("last_modified_at").is_some());
    assert!(v.get("page").is_none());

    // append --json --full includes the full page with blocks.
    let out_full = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["append", "me|page-1", "hello", "--json", "--full"],
    )
    .await;
    assert!(
        out_full.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out_full.stderr)
    );
    let vf: Value = serde_json::from_slice(&out_full.stdout).unwrap();
    assert_eq!(vf["ok"], true);
    assert_eq!(vf["page_key"], "me|page-1");
    assert!(vf["page"].get("summary").is_some());
    assert!(vf["page"].get("blocks").is_some());
    assert!(vf["page"].get("omitted").is_none());

    // update --block-id default compact includes block_id, omits page.
    let out_upd = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &[
            "update",
            "me|page-2",
            "--block-id",
            "p:body-2",
            "--body",
            "Updated body",
            "--json",
        ],
    )
    .await;
    assert!(
        out_upd.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out_upd.stderr)
    );
    let vu: Value = serde_json::from_slice(&out_upd.stdout).unwrap();
    assert_eq!(vu["ok"], true);
    assert_eq!(vu["page_key"], "me|page-2");
    assert_eq!(vu["block_id"], "p:body-2");
    assert!(vu.get("page").is_none());

    // check default compact includes block_id & completed, omits page.
    let out_chk = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["check", "me|page-1", "--block-id", "p:todo1", "--done", "--json"],
    )
    .await;
    assert!(
        out_chk.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out_chk.stderr)
    );
    let vc: Value = serde_json::from_slice(&out_chk.stdout).unwrap();
    assert_eq!(vc["ok"], true);
    assert_eq!(vc["page_key"], "me|page-1");
    assert_eq!(vc["block_id"], "p:todo1");
    assert_eq!(vc["completed"], true);
    assert!(vc.get("page").is_none());

    server.abort();
}

#[tokio::test]
async fn mcp_get_page_compact_default_and_full() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let mut proc = McpProc::spawn(&base_url, &cfg_dir, &data_dir, Some("opaque-a"));

    let r1 = proc
        .call(json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"get_page","arguments":{"page":"me|page-1"}}
        }))
        .await;
    assert_eq!(r1["result"]["isError"], false);
    let sc1 = &r1["result"]["structuredContent"];
    assert!(sc1["page"].get("omitted").is_some());
    assert!(sc1["page"].get("blocks").is_none());
    let parsed_text1: Value =
        serde_json::from_str(r1["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(parsed_text1, *sc1);

    let r2 = proc
        .call(json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"get_page","arguments":{"page":"me|page-1","compact":false}}
        }))
        .await;
    assert_eq!(r2["result"]["isError"], false);
    let sc2 = &r2["result"]["structuredContent"];
    assert!(sc2["page"].get("blocks").is_some());
    let parsed_text2: Value =
        serde_json::from_str(r2["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(parsed_text2, *sc2);

    proc.close().await;
    server.abort();
}

#[tokio::test]
async fn mcp_write_tool_is_compact_by_default_and_supports_full() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let mut proc = McpProc::spawn(&base_url, &cfg_dir, &data_dir, Some("opaque-a"));
    let r = proc
        .call(json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"append_page","arguments":{"page":"me|page-1","markdown":"hello"}}
        }))
        .await;
    assert_eq!(r["result"]["isError"], false, "{r}");
    let sc = &r["result"]["structuredContent"];
    assert_eq!(sc["ok"], true);
    assert_eq!(sc["page_key"], "me|page-1");
    assert!(sc["title"].is_string());
    assert!(sc.get("page").is_none());

    let r_full = proc
        .call(json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"append_page","arguments":{"page":"me|page-1","markdown":"hello","compact":false}}
        }))
        .await;
    assert_eq!(r_full["result"]["isError"], false, "{r_full}");
    let sc_full = &r_full["result"]["structuredContent"];
    assert_eq!(sc_full["ok"], true);
    assert_eq!(sc_full["page_key"], "me|page-1");
    assert!(sc_full["page"].get("summary").is_some());
    assert!(sc_full["page"].get("blocks").is_some());
    assert!(sc_full["page"].get("omitted").is_none());

    let r_todo = proc
        .call(json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"toggle_todo","arguments":{"page":"me|page-1","block_id":"p:todo1","completed":true}}
        }))
        .await;
    assert_eq!(r_todo["result"]["isError"], false, "{r_todo}");
    let sc_todo = &r_todo["result"]["structuredContent"];
    assert_eq!(sc_todo["ok"], true);
    assert_eq!(sc_todo["page_key"], "me|page-1");
    assert_eq!(sc_todo["block_id"], "p:todo1");
    assert_eq!(sc_todo["completed"], true);
    assert!(sc_todo.get("page").is_none());

    proc.close().await;
    server.abort();
}

// ===========================================================================
// Lists (U3), whoami (U4), HTTP (U5)
// ===========================================================================

