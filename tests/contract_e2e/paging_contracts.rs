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
async fn pages_paging_boundaries() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let cases: &[(&[&str], u64, Value, Value)] = &[
        (
            &["pages", "Engineering/Specs", "--limit", "2", "--json"],
            2,
            json!(true),
            json!("2"),
        ),
        (
            &[
                "pages",
                "Engineering/Specs",
                "--limit",
                "2",
                "--cursor",
                "2",
                "--json",
            ],
            1,
            json!(false),
            Value::Null,
        ),
        (
            &["pages", "Engineering/Specs", "--limit", "3", "--json"],
            3,
            json!(false),
            Value::Null,
        ),
        (
            &["pages", "Engineering/Specs", "--limit", "50", "--json"],
            3,
            json!(false),
            Value::Null,
        ),
        (
            &["pages", "Engineering/Specs", "--limit", "0", "--json"],
            0,
            Value::Null,
            Value::Null,
        ),
    ];

    let mut first_two_keys = Vec::new();
    for (idx, (args, ret, has_more, next_cursor)) in cases.iter().enumerate() {
        let out = run_onenote(&base_url, &cfg_dir, &data_dir, Some("opaque-a"), args).await;
        assert!(
            out.status.success(),
            "case {idx} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["returned"], json!(*ret), "case {idx}");
        assert_eq!(v["has_more"], *has_more, "case {idx}");
        assert_eq!(v["next_cursor"], *next_cursor, "case {idx}");
        if idx < 2 {
            for p in v["pages"].as_array().unwrap() {
                first_two_keys.push(p["page_key"].as_str().unwrap().to_string());
            }
        }
    }
    let uniq: BTreeSet<String> = first_two_keys.iter().cloned().collect();
    assert_eq!(first_two_keys.len(), 3);
    assert_eq!(uniq.len(), 3);

    server.abort();
}

#[tokio::test]
async fn pages_paging_offline_matches() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    // Populate cache online
    let seed = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["pages", "Engineering/Specs", "--limit", "50", "--json"],
    )
    .await;
    assert!(seed.status.success());

    let cases: &[(&[&str], u64, Value, Value)] = &[
        (
            &[
                "pages",
                "Engineering/Specs",
                "--limit",
                "2",
                "--offline",
                "--json",
            ],
            2,
            json!(true),
            json!("2"),
        ),
        (
            &[
                "pages",
                "Engineering/Specs",
                "--limit",
                "2",
                "--cursor",
                "2",
                "--offline",
                "--json",
            ],
            1,
            json!(false),
            Value::Null,
        ),
        (
            &[
                "pages",
                "Engineering/Specs",
                "--limit",
                "3",
                "--offline",
                "--json",
            ],
            3,
            json!(false),
            Value::Null,
        ),
        (
            &[
                "pages",
                "Engineering/Specs",
                "--limit",
                "50",
                "--offline",
                "--json",
            ],
            3,
            json!(false),
            Value::Null,
        ),
        (
            &[
                "pages",
                "Engineering/Specs",
                "--limit",
                "0",
                "--offline",
                "--json",
            ],
            0,
            Value::Null,
            Value::Null,
        ),
    ];

    for (idx, (args, ret, has_more, next_cursor)) in cases.iter().enumerate() {
        let out = run_onenote(&base_url, &cfg_dir, &data_dir, Some("opaque-a"), args).await;
        assert!(
            out.status.success(),
            "case {idx} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["returned"], json!(*ret), "case {idx}");
        assert_eq!(v["has_more"], *has_more, "case {idx}");
        assert_eq!(v["next_cursor"], *next_cursor, "case {idx}");
        assert!(v["cache_synced_at"].is_string(), "case {idx}: {v}");
    }

    server.abort();
}

/// A first page read with `has_more = true` caches only part of the section.
/// It must not mark the cache complete: page 2 read with `--cached` (or a CLI
/// cache TTL) used to be cut short to the cached rows.
#[tokio::test]
async fn cached_page_cursor_after_partial_first_page() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 5).await;

    let page_keys = |v: &Value| -> Vec<String> {
        v["pages"]
            .as_array()
            .unwrap()
            .iter()
            .map(|p| p["page_key"].as_str().unwrap().to_string())
            .collect()
    };

    for (label, extra_args, ttl) in [
        ("--cached", &["--cached"][..], None),
        ("cli_cache_ttl_secs", &[][..], Some("3600")),
    ] {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));
        let run = |cursor: &'static str, cached_read: bool| {
            let mut cmd = Command::new(env!("CARGO_BIN_EXE_onenote"));
            cmd.args([
                "pages",
                "Engineering/Specs",
                "--limit",
                "2",
                "--cursor",
                cursor,
                "--json",
            ]);
            if cached_read {
                cmd.args(extra_args);
            }
            configure_cmd(&mut cmd, &base_url, &cfg_dir, &data_dir, Some("opaque-a"));
            if let Some(ttl) = ttl {
                cmd.env("ONENOTE_CLI_CACHE_SECS", ttl);
            }
            cmd.output()
        };
        let parse = |out: std::process::Output| -> Value {
            assert!(
                out.status.success(),
                "{label}: stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
            serde_json::from_slice(&out.stdout).unwrap()
        };

        let p1 = parse(run("0", false).await.unwrap());
        assert_eq!(p1["returned"], 2, "{label}");
        assert_eq!(p1["has_more"], true, "{label}");
        assert_eq!(p1["next_cursor"], "2", "{label}");

        let p2 = parse(run("2", true).await.unwrap());
        assert_eq!(
            p2["returned"], 2,
            "{label}: page 2 must not be truncated: {p2}"
        );
        assert_eq!(p2["has_more"], true, "{label}");
        assert_eq!(p2["next_cursor"], "4", "{label}");

        let p3 = parse(run("4", true).await.unwrap());
        assert_eq!(p3["returned"], 1, "{label}");
        assert_eq!(p3["has_more"], false, "{label}");
        assert_eq!(p3["next_cursor"], Value::Null, "{label}");

        let mut all = page_keys(&p1);
        all.extend(page_keys(&p2));
        all.extend(page_keys(&p3));
        let uniq: BTreeSet<String> = all.iter().cloned().collect();
        assert_eq!((all.len(), uniq.len()), (5, 5), "{label}: {all:?}");
    }

    server.abort();
}

#[tokio::test]
async fn sections_default_50_and_follow() {
    let (base_url, _mock, server) = spawn_contract_mock(120, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let cases: &[(&[&str], u64, bool, Value)] = &[
        (&["sections", "--json"], 50, true, json!("50")),
        (
            &["sections", "--json", "--cursor", "50"],
            50,
            true,
            json!("100"),
        ),
        (
            &["sections", "--json", "--cursor", "100"],
            20,
            false,
            Value::Null,
        ),
        (
            &["sections", "--json", "--cursor", "120"],
            0,
            false,
            Value::Null,
        ),
        (
            &["sections", "--json", "--limit", "1000"],
            120,
            false,
            Value::Null,
        ),
    ];

    let mut union = BTreeSet::new();
    for (idx, (args, ret, has_more, next_cursor)) in cases.iter().enumerate() {
        let out = run_onenote(&base_url, &cfg_dir, &data_dir, Some("opaque-a"), args).await;
        assert!(
            out.status.success(),
            "case {idx} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["returned"], json!(*ret), "case {idx}");
        assert_eq!(v["has_more"], json!(*has_more), "case {idx}");
        assert_eq!(v["next_cursor"], *next_cursor, "case {idx}");
        if idx < 3 {
            for s in v["sections"].as_array().unwrap() {
                union.insert(s["section_key"].as_str().unwrap().to_string());
            }
        }
    }
    assert_eq!(union.len(), 120);

    server.abort();
}

#[tokio::test]
async fn sections_text_mode_unbounded() {
    let (base_url, _mock, server) = spawn_contract_mock(120, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["sections"],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let stdout = String::from_utf8_lossy(&out.stdout);
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert_eq!(stdout.lines().count(), 120);
    assert!(!stderr.contains("More results"), "stderr={stderr}");

    server.abort();
}

#[tokio::test]
async fn fresh_offline_lists_report_unsynced() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));
    let jwt = make_unsigned_jwt("user-a");

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some(&jwt),
        &["notebooks", "--offline", "--json"],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["notebooks"].as_array().unwrap().len(), 0);
    assert!(v.get("cache_synced_at").is_some());
    assert!(v["cache_synced_at"].is_null());
    assert_eq!(v["has_more"], false);

    server.abort();
}

#[tokio::test]
async fn search_paging_and_cache_counts() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 40).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    assert!(run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["pages", "Engineering/Specs", "--limit", "50", "--json"],
    )
    .await
    .status
    .success());
    assert!(run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--json"],
    )
    .await
    .status
    .success());

    let cases: &[(&[&str], u64, bool, Value)] = &[
        (&["search", "Architecture", "--json"], 30, true, json!("30")),
        (
            &["search", "Architecture", "--cursor", "30", "--json"],
            10,
            false,
            Value::Null,
        ),
        (
            &["search", "Architecture", "--limit", "1", "--json"],
            1,
            true,
            json!("1"),
        ),
    ];

    for (idx, (args, ret, has_more, next_cursor)) in cases.iter().enumerate() {
        let out = run_onenote(&base_url, &cfg_dir, &data_dir, Some("opaque-a"), args).await;
        assert!(
            out.status.success(),
            "case {idx} stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(v["returned"], json!(*ret), "case {idx}");
        assert_eq!(v["has_more"], json!(*has_more), "case {idx}");
        assert_eq!(v["next_cursor"], *next_cursor, "case {idx}");
        assert_eq!(v["cache"]["pages"], 40, "case {idx}");
        assert_eq!(v["cache"]["pages_with_body"], 1, "case {idx}");
    }

    server.abort();
}

#[tokio::test]
async fn search_section_accepts_scoped_key_and_notebook_path() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    assert!(run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["pages", "Engineering/Specs", "--limit", "50", "--json"],
    )
    .await
    .status
    .success());

    let collect_keys = |out: &std::process::Output| -> BTreeSet<String> {
        assert!(
            out.status.success(),
            "stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        v["hits"]
            .as_array()
            .unwrap()
            .iter()
            .map(|h| h["page_key"].as_str().unwrap().to_string())
            .collect()
    };

    let by_raw_id = collect_keys(
        &run_onenote(
            &base_url,
            &cfg_dir,
            &data_dir,
            Some("opaque-a"),
            &["search", "Architecture", "--section", "sec-1", "--json"],
        )
        .await,
    );
    let by_scoped_key = collect_keys(
        &run_onenote(
            &base_url,
            &cfg_dir,
            &data_dir,
            Some("opaque-a"),
            &["search", "Architecture", "--section", "me|sec-1", "--json"],
        )
        .await,
    );
    let by_path = collect_keys(
        &run_onenote(
            &base_url,
            &cfg_dir,
            &data_dir,
            Some("opaque-a"),
            &[
                "search",
                "Architecture",
                "--section",
                "Engineering/Specs",
                "--json",
            ],
        )
        .await,
    );
    let by_name_sub = collect_keys(
        &run_onenote(
            &base_url,
            &cfg_dir,
            &data_dir,
            Some("opaque-a"),
            &["search", "Architecture", "--section", "Specs", "--json"],
        )
        .await,
    );

    assert_eq!(by_raw_id.len(), 3);
    assert_eq!(by_scoped_key, by_raw_id);
    assert_eq!(by_path, by_raw_id);
    assert_eq!(by_name_sub, by_raw_id);

    // Missing Notebook/Section path returns explicit not_found error instead of silent 0 hits.
    let missing = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &[
            "search",
            "Architecture",
            "--section",
            "Engineering/Nope",
            "--json",
        ],
    )
    .await;
    assert_eq!(missing.status.code(), Some(1));
    let mv: Value = serde_json::from_slice(&missing.stdout).unwrap();
    assert_eq!(mv["ok"], false);
    assert_eq!(mv["error"]["code"], "not_found");

    server.abort();
}

#[tokio::test]
async fn mcp_list_sections_limit_and_bad_cursor() {
    let (base_url, _mock, server) = spawn_contract_mock(120, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let mut proc = McpProc::spawn(&base_url, &cfg_dir, &data_dir, Some("opaque-a"));

    let r1 = proc
        .call(json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"list_sections","arguments":{"limit":5}}
        }))
        .await;
    assert_eq!(r1["result"]["isError"], false);
    let sc1 = &r1["result"]["structuredContent"];
    assert_eq!(sc1["sections"].as_array().unwrap().len(), 5);
    assert_eq!(sc1["returned"], 5);
    assert_eq!(sc1["has_more"], true);
    assert_eq!(sc1["next_cursor"], "5");

    let r2 = proc
        .call(json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"list_sections","arguments":{"cursor":"abc"}}
        }))
        .await;
    assert_eq!(r2["result"]["isError"], true);
    assert!(r2["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Invalid cursor"));

    let r3 = proc
        .call(json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"list_sections","arguments":{"limit":0}}
        }))
        .await;
    assert_eq!(r3["result"]["isError"], true);
    assert!(r3["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("limit must be between 1 and 1000"));

    // W7: list_pages and search_pages enforce the same 1..=1000 limit validation.
    let r4 = proc
        .call(json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"list_pages","arguments":{"section":"Engineering/Specs","limit":0}}
        }))
        .await;
    assert_eq!(r4["result"]["isError"], true);
    assert!(r4["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("limit must be between 1 and 1000"));

    let r5 = proc
        .call(json!({
            "jsonrpc":"2.0","id":5,"method":"tools/call",
            "params":{"name":"search_pages","arguments":{"query":"Architecture","limit":-5}}
        }))
        .await;
    assert_eq!(r5["result"]["isError"], true);
    assert!(r5["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("limit must be between 1 and 1000"));

    proc.close().await;
    server.abort();
}

#[tokio::test]
async fn mcp_whoami_has_auth_mode() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let mut proc = McpProc::spawn(&base_url, &cfg_dir, &data_dir, Some("opaque-a"));
    let r = proc
        .call(json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"whoami","arguments":{}}
        }))
        .await;
    assert_eq!(r["result"]["isError"], false);
    let sc = &r["result"]["structuredContent"];
    assert_eq!(sc["auth_mode"], "env_token");
    assert_eq!(sc["user"]["id"], "user-a");
    let serialized = serde_json::to_string(&r).unwrap();
    assert!(
        !serialized.contains("opaque-a"),
        "response leaked token: {serialized}"
    );

    proc.close().await;
    server.abort();
}
