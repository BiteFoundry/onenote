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
async fn unmapped_search_errors_without_network() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["search", "Architecture", "--json"],
    )
    .await;
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout must be valid JSON error object ({e}), got: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "unknown_identity");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Unknown identity"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Unknown identity"), "stderr={stderr}");
    assert!(stderr.contains("onenote whoami"), "stderr={stderr}");
    assert_eq!(mock.total_count(), 0);
    assert!(!data_dir.join("default").exists());

    server.abort();
}

#[tokio::test]
async fn whoami_writes_binding_then_local_paths_work() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let who = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["whoami", "--json"],
    )
    .await;
    assert!(
        who.status.success(),
        "whoami stderr={}",
        String::from_utf8_lossy(&who.stderr)
    );
    assert_eq!(identity_map_json_count(&data_dir), 1);

    let read = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--json"],
    )
    .await;
    assert!(
        read.status.success(),
        "read stderr={}",
        String::from_utf8_lossy(&read.stderr)
    );

    let calls_before = mock.total_count();

    for _ in 0..2 {
        let srch = run_onenote(
            &base_url,
            &cfg_dir,
            &data_dir,
            Some("opaque-a"),
            &["search", "Architecture", "--json"],
        )
        .await;
        assert!(
            srch.status.success(),
            "search stderr={}",
            String::from_utf8_lossy(&srch.stderr)
        );
        let v: Value = serde_json::from_slice(&srch.stdout).unwrap();
        assert!(!v["hits"].as_array().unwrap().is_empty());
    }

    let off = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--offline", "--json"],
    )
    .await;
    assert!(
        off.status.success(),
        "offline read stderr={}",
        String::from_utf8_lossy(&off.stderr)
    );

    assert_eq!(mock.total_count(), calls_before);
    assert!(data_dir.join("user-a").join("cache.db").is_file());
    assert!(!data_dir.join("default").exists());

    server.abort();
}

#[tokio::test]
async fn mapped_search_survives_closed_graph() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    assert!(run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["whoami", "--json"]
    )
    .await
    .status
    .success());
    assert!(run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["pages", "Engineering/Specs", "--json"],
    )
    .await
    .status
    .success());

    let online_srch = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["search", "Architecture", "--json"],
    )
    .await;
    assert!(online_srch.status.success());
    let online_hits = serde_json::from_slice::<Value>(&online_srch.stdout).unwrap()["hits"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(online_hits, 3);

    server.abort();
    let closed = closed_graph_url();
    let closed_srch = run_onenote(
        &closed,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["search", "Architecture", "--json"],
    )
    .await;
    assert!(
        closed_srch.status.success(),
        "closed search stderr={}",
        String::from_utf8_lossy(&closed_srch.stderr)
    );
    let closed_hits = serde_json::from_slice::<Value>(&closed_srch.stdout).unwrap()["hits"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(closed_hits, online_hits);
}

#[tokio::test]
async fn whoami_with_closed_graph_writes_nothing() {
    let closed = closed_graph_url();
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let who = run_onenote(
        &closed,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["whoami", "--json"],
    )
    .await;
    assert_eq!(who.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&who.stderr).contains("GET /me failed"),
        "stderr={}",
        String::from_utf8_lossy(&who.stderr)
    );
    assert_eq!(identity_map_json_count(&data_dir), 0);

    let srch = run_onenote(
        &closed,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["search", "x", "--json"],
    )
    .await;
    assert_eq!(srch.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&srch.stderr).contains("Unknown identity"),
        "stderr={}",
        String::from_utf8_lossy(&srch.stderr)
    );
    assert_eq!(identity_map_json_count(&data_dir), 0);
}

#[tokio::test]
async fn search_refresh_bootstraps_identity() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let first = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["search", "Architecture", "--refresh", "--json"],
    )
    .await;
    assert!(
        first.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(mock.me_count(), 1);

    let total_after_first = mock.total_count();
    let second = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["search", "Architecture", "--json"],
    )
    .await;
    assert!(
        second.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(mock.total_count(), total_after_first);

    server.abort();
}

#[tokio::test]
async fn unmapped_offline_errors() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    for args in [
        &["notebooks", "--offline", "--json"][..],
        &["read", "me|page-1", "--offline", "--json"][..],
    ] {
        let out = run_onenote(&base_url, &cfg_dir, &data_dir, Some("opaque-a"), args).await;
        assert_eq!(out.status.code(), Some(1), "args={args:?}");
        let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "args={args:?} stdout must be JSON error ({e}), got: {}",
                String::from_utf8_lossy(&out.stdout)
            )
        });
        assert_eq!(v["ok"], false, "args={args:?}");
        assert_eq!(v["error"]["code"], "unknown_identity", "args={args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Unknown identity"),
            "args={args:?}, stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(mock.total_count(), 0);
    assert!(!data_dir.join("default").exists());

    server.abort();
}

#[tokio::test]
async fn two_accounts_do_not_share_cache() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    for tok in ["opaque-a", "opaque-b"] {
        assert!(run_onenote(
            &base_url,
            &cfg_dir,
            &data_dir,
            Some(tok),
            &["whoami", "--json"]
        )
        .await
        .status
        .success());
        assert!(run_onenote(
            &base_url,
            &cfg_dir,
            &data_dir,
            Some(tok),
            &["pages", "Engineering/Specs", "--json"],
        )
        .await
        .status
        .success());
    }

    assert!(data_dir.join("user-a").is_dir());
    assert!(data_dir.join("user-b").is_dir());
    assert_eq!(identity_map_json_count(&data_dir), 2);

    for (tok, own, other) in [
        ("opaque-a", "user-a", "user-b"),
        ("opaque-b", "user-b", "user-a"),
    ] {
        let pages_out = run_onenote(
            &base_url,
            &cfg_dir,
            &data_dir,
            Some(tok),
            &["pages", "Engineering/Specs", "--offline", "--json"],
        )
        .await;
        assert!(pages_out.status.success());
        let pv: Value = serde_json::from_slice(&pages_out.stdout).unwrap();
        let pages = pv["pages"].as_array().unwrap();
        assert_eq!(pages.len(), 3);
        for p in pages {
            let t = p["title"].as_str().unwrap();
            assert!(t.contains(own), "expected {own} in {t}");
            assert!(!t.contains(other), "unexpected {other} in {t}");
        }

        let srch_out = run_onenote(
            &base_url,
            &cfg_dir,
            &data_dir,
            Some(tok),
            &["search", "Architecture", "--json"],
        )
        .await;
        assert!(srch_out.status.success());
        let sv: Value = serde_json::from_slice(&srch_out.stdout).unwrap();
        let hits = sv["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 3);
        for h in hits {
            let t = h["title"].as_str().unwrap();
            assert!(t.contains(own), "expected {own} in {t}");
            assert!(!t.contains(other), "unexpected {other} in {t}");
        }
    }

    server.abort();
}

#[tokio::test]
async fn second_account_without_binding_is_refused() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    assert!(run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["whoami", "--json"]
    )
    .await
    .status
    .success());
    assert!(run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["pages", "Engineering/Specs", "--json"],
    )
    .await
    .status
    .success());

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-b"),
        &["search", "Architecture", "--json"],
    )
    .await;
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "unknown_identity");
    assert!(String::from_utf8_lossy(&out.stderr).contains("Unknown identity"));

    server.abort();
}

#[tokio::test]
async fn concurrent_first_use_two_accounts() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;

    for iter in 0..5 {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));
        let (out_a, out_b) = tokio::join!(
            run_onenote(
                &base_url,
                &cfg_dir,
                &data_dir,
                Some("opaque-a"),
                &["whoami", "--json"]
            ),
            run_onenote(
                &base_url,
                &cfg_dir,
                &data_dir,
                Some("opaque-b"),
                &["whoami", "--json"]
            ),
        );
        assert!(
            out_a.status.success(),
            "iter {iter} A failed: {}",
            String::from_utf8_lossy(&out_a.stderr)
        );
        assert!(
            out_b.status.success(),
            "iter {iter} B failed: {}",
            String::from_utf8_lossy(&out_b.stderr)
        );
        assert_eq!(identity_map_json_count(&data_dir), 2, "iter {iter}");

        for (tok, expected_dir) in [("opaque-a", "user-a"), ("opaque-b", "user-b")] {
            let doc = run_onenote(
                &base_url,
                &cfg_dir,
                &data_dir,
                Some(tok),
                &["doctor", "--json"],
            )
            .await;
            assert!(doc.status.success());
            let dv: Value = serde_json::from_slice(&doc.stdout).unwrap();
            assert_eq!(dv["cache_identity"], "identity_map");
            let db_file = dv["db_file"].as_str().unwrap();
            assert!(
                db_file.contains(expected_dir),
                "iter {iter}: expected {expected_dir} in {db_file}"
            );
        }
    }

    server.abort();
}

#[tokio::test]
async fn jwt_token_never_calls_me() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));
    let jwt = make_unsigned_jwt("user-a");

    for args in [
        &["search", "x", "--json"][..],
        &["read", "me|page-1", "--json"][..],
        &["read", "me|page-1", "--offline", "--json"][..],
    ] {
        let out = run_onenote(&base_url, &cfg_dir, &data_dir, Some(&jwt), args).await;
        assert!(
            out.status.success(),
            "args={args:?}, stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(mock.me_count(), 0);
    assert!(!data_dir.join("identity-map").exists());

    server.abort();
}

#[tokio::test]
async fn no_token_uses_default_bucket() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        None,
        &["search", "x", "--json"],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["hits"].as_array().unwrap().len(), 0);
    assert_eq!(v["cache"]["pages"], 0);
    assert_eq!(mock.total_count(), 0);

    server.abort();
}

#[tokio::test]
async fn doctor_reports_cache_identity() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let before = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["doctor", "--json"],
    )
    .await;
    assert!(before.status.success());
    let bv: Value = serde_json::from_slice(&before.stdout).unwrap();
    assert_eq!(bv["cache_identity"], "unresolved");
    assert!(bv["db_file"].is_null());
    assert!(!data_dir.join("default").exists());

    assert!(run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["whoami", "--json"]
    )
    .await
    .status
    .success());

    let after = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["doctor", "--json"],
    )
    .await;
    assert!(after.status.success());
    let av: Value = serde_json::from_slice(&after.stdout).unwrap();
    assert_eq!(av["cache_identity"], "identity_map");
    assert!(av["db_file"].as_str().unwrap().contains("user-a"));

    server.abort();
}

#[tokio::test]
async fn mcp_catalogue_without_identity() {
    let closed = closed_graph_url();
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let mut proc = McpProc::spawn(&closed, &cfg_dir, &data_dir, Some("opaque-a"));

    let r1 = proc
        .call(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
        .await;
    assert!(r1["result"]["serverInfo"].is_object());
    assert!(proc.is_alive());

    let r2 = proc
        .call(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await;
    assert!(!r2["result"]["tools"].as_array().unwrap().is_empty());
    assert!(proc.is_alive());

    let r3 = proc
        .call(json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"search_pages","arguments":{"query":"x"}}
        }))
        .await;
    assert_eq!(r3["result"]["isError"], true);
    assert!(r3["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Unknown identity"));
    assert!(proc.is_alive());

    let r4 = proc
        .call(json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"list_notebooks","arguments":{}}
        }))
        .await;
    assert_eq!(r4["result"]["isError"], true);
    assert!(r4["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("GET /me failed"));
    assert!(proc.is_alive());

    proc.close().await;
}

#[tokio::test]
async fn mcp_first_online_call_binds_then_search_is_local() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let mut proc = McpProc::spawn(&base_url, &cfg_dir, &data_dir, Some("opaque-a"));

    let r1 = proc
        .call(json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"whoami","arguments":{}}
        }))
        .await;
    assert_eq!(r1["result"]["isError"], false);

    let r2 = proc
        .call(json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"get_page","arguments":{"page":"me|page-1"}}
        }))
        .await;
    assert_eq!(r2["result"]["isError"], false);
    assert_eq!(identity_map_json_count(&data_dir), 1);

    let calls_before = mock.total_count();
    let r3 = proc
        .call(json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"search_pages","arguments":{"query":"Architecture"}}
        }))
        .await;
    assert_eq!(r3["result"]["isError"], false);
    assert!(!r3["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(mock.total_count(), calls_before);

    proc.close().await;
    server.abort();
}

// ===========================================================================
// Read responses (U2)
// ===========================================================================
