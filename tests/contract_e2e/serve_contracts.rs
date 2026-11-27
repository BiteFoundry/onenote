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

async fn make_inprocess_serve(
    base_url: String,
    dir: &Path,
) -> (
    std::net::SocketAddr,
    Storage,
    GraphClient,
    tokio::task::JoinHandle<()>,
) {
    let token_path = dir.join("tok.txt");
    std::fs::write(&token_path, "opaque-a").unwrap();
    let cfg = AppConfig {
        graph_base_url: base_url,
        token_file: token_path.to_string_lossy().into_owned(),
        share_m365_auth: false,
        ..AppConfig::default()
    };
    let auth = AuthManager::new(cfg.clone()).unwrap();
    let graph = GraphClient::new(cfg.clone(), auth)
        .unwrap()
        .with_my_user_id("user-a");
    let storage = Storage::open(&dir.join("cache.db")).unwrap();

    let state = Arc::new(serve::AppState {
        config: cfg,
        graph: graph.clone(),
        storage: storage.clone(),
        api_key: None,
    });
    let app = serve::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (addr, storage, graph, handle)
}

#[tokio::test]
async fn http_get_page_honours_section() {
    let (base_url, _mock, graph_server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (api_addr, _storage, _graph, api_server) = make_inprocess_serve(base_url, tmp.path()).await;
    let http = reqwest::Client::new();

    let r1 = http
        .get(format!(
            "http://{api_addr}/v1/onenote/pages/Architecture%20Spec?section=Specs"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r1.status(), StatusCode::OK);
    let v1: Value = r1.json().await.unwrap();
    assert_eq!(v1["ok"], true);
    assert_eq!(json_object_keys(&v1["page"]), full_page_key_set());

    let r2 = http
        .get(format!(
            "http://{api_addr}/v1/onenote/pages/Architecture%20Spec?section=Section%202"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r2.status(), StatusCode::NOT_FOUND);
    let v2: Value = r2.json().await.unwrap();
    assert!(v2["error"]
        .as_str()
        .unwrap()
        .contains("not found in section 'Section 2'"));

    let r2_off = http
        .get(format!(
            "http://{api_addr}/v1/onenote/pages/Architecture%20Spec?section=Section%202&offline=true"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r2_off.status(), StatusCode::NOT_FOUND);
    let v2_off: Value = r2_off.json().await.unwrap();
    assert!(v2_off["error"].as_str().unwrap().contains("offline cache"));

    let r3 = http
        .get(format!(
            "http://{api_addr}/v1/onenote/pages/Architecture%20Spec"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(r3.status(), StatusCode::OK);
    let v3: Value = r3.json().await.unwrap();
    assert_eq!(v3["ok"], true);
    assert_eq!(json_object_keys(&v3["page"]), full_page_key_set());

    // W5 (S1): PATCH honours `section` in request body.
    let r_patch_ok = http
        .patch(format!(
            "http://{api_addr}/v1/onenote/pages/Architecture%20Spec"
        ))
        .json(&json!({
            "section": "Specs",
            "title": "Architecture Spec Renamed"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r_patch_ok.status(), StatusCode::OK);
    let vp: Value = r_patch_ok.json().await.unwrap();
    assert_eq!(vp["ok"], true);

    let r_patch_miss = http
        .patch(format!(
            "http://{api_addr}/v1/onenote/pages/Architecture%20Spec"
        ))
        .json(&json!({
            "section": "Section 2",
            "title": "Architecture Spec Renamed"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r_patch_miss.status(), StatusCode::NOT_FOUND);

    api_server.abort();
    graph_server.abort();
}

#[tokio::test]
async fn http_list_pages_follows_cursor() {
    let (base_url, _mock, graph_server) = spawn_contract_mock(2, 5).await;
    let tmp = tempfile::tempdir().unwrap();
    let (api_addr, _storage, _graph, api_server) = make_inprocess_serve(base_url, tmp.path()).await;
    let http = reqwest::Client::new();
    let get_pages = |query: String| {
        let http = http.clone();
        async move {
            http.get(format!("http://{api_addr}/v1/onenote/pages?{query}"))
                .send()
                .await
                .unwrap()
        }
    };

    let mut keys = Vec::new();
    let mut cursor: Option<String> = None;
    let mut seen_pages = Vec::new();
    loop {
        let query = match &cursor {
            Some(c) => format!("section=Specs&limit=2&cursor={c}"),
            None => "section=Specs&limit=2".to_string(),
        };
        let resp = get_pages(query).await;
        assert_eq!(resp.status(), StatusCode::OK);
        let v: Value = resp.json().await.unwrap();
        assert_eq!(v["ok"], true);
        let returned = v["returned"].as_u64().unwrap();
        assert_eq!(v["pages"].as_array().unwrap().len() as u64, returned);
        seen_pages.push(returned);
        for p in v["pages"].as_array().unwrap() {
            keys.push(p["page_key"].as_str().unwrap().to_string());
        }
        match v["next_cursor"].as_str() {
            Some(next) => {
                assert_eq!(v["has_more"], true);
                cursor = Some(next.to_string());
            }
            None => {
                assert_eq!(v["has_more"], false);
                break;
            }
        }
    }
    assert_eq!(seen_pages, vec![2, 2, 1]);
    let uniq: BTreeSet<String> = keys.iter().cloned().collect();
    assert_eq!((keys.len(), uniq.len()), (5, 5), "{keys:?}");

    let bad = get_pages("section=Specs&cursor=abc".to_string()).await;
    assert_eq!(bad.status(), StatusCode::BAD_REQUEST);
    let bv: Value = bad.json().await.unwrap();
    assert!(
        bv["error"].as_str().unwrap().contains("Invalid cursor"),
        "{bv}"
    );

    api_server.abort();
    graph_server.abort();
}

#[tokio::test]
async fn cli_read_honours_section() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let ok_out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "Architecture Spec", "--section", "Specs", "--json"],
    )
    .await;
    assert!(
        ok_out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&ok_out.stderr)
    );
    let v: Value = serde_json::from_slice(&ok_out.stdout).unwrap();
    assert_eq!(v["page"]["page_key"], "me|page-1");

    let miss_out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &[
            "read",
            "Architecture Spec",
            "--section",
            "Section 2",
            "--json",
        ],
    )
    .await;
    assert_eq!(miss_out.status.code(), Some(1));
    let mv: Value = serde_json::from_slice(&miss_out.stdout).unwrap();
    assert_eq!(mv["ok"], false);
    assert_eq!(mv["error"]["code"], "not_found");
    assert!(mv["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not found in section 'Section 2'"));

    server.abort();
}

#[tokio::test]
async fn http_search_default_is_30() {
    let (base_url, _mock, graph_server) = spawn_contract_mock(2, 40).await;
    let tmp = tempfile::tempdir().unwrap();
    let (api_addr, storage, graph, api_server) = make_inprocess_serve(base_url, tmp.path()).await;

    // Seed 40 cached pages
    cli::list_onenote_pages_helper(&storage, &graph, "Engineering/Specs", 50, true, false)
        .await
        .unwrap();

    let http = reqwest::Client::new();
    let resp = http
        .get(format!(
            "http://{api_addr}/v1/onenote/search?q=Architecture"
        ))
        .send()
        .await
        .unwrap();
    assert_eq!(resp.status(), StatusCode::OK);
    let v: Value = resp.json().await.unwrap();
    assert_eq!(v["hits"].as_array().unwrap().len(), 30);
    let expected_keys: BTreeSet<String> = ["ok", "query", "hits"]
        .into_iter()
        .map(String::from)
        .collect();
    assert_eq!(json_object_keys(&v), expected_keys);

    api_server.abort();
    graph_server.abort();
}

/// ON-03: a title lookup with `--section` used to read only the first 100 pages.
#[tokio::test]
async fn read_title_with_section_resolves_past_the_first_100_pages() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 150).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &[
            "read",
            "Architecture Note 140 user-a",
            "--section",
            "Specs",
            "--json",
        ],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["page"]["page_key"], "me|page-140");

    // A title that exists nowhere keeps the plain not-found message.
    let miss = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "No Such Title", "--section", "Specs", "--json"],
    )
    .await;
    assert_eq!(miss.status.code(), Some(1));
    let mv: Value = serde_json::from_slice(&miss.stdout).unwrap();
    assert_eq!(mv["error"]["code"], "not_found");
    assert!(mv["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not found in section 'Specs'."));

    server.abort();
}

/// ON-10 over HTTP: `section=<plain name>` is applied before the limit.
#[tokio::test]
async fn http_search_plain_section_name_is_filtered_before_the_limit() {
    let (base_url, _mock, graph_server) = spawn_contract_mock(2, 40).await;
    let tmp = tempfile::tempdir().unwrap();
    let (api_addr, storage, graph, api_server) = make_inprocess_serve(base_url, tmp.path()).await;

    // 40 cached pages in "Specs" (sec-1) ...
    cli::list_onenote_pages_helper(&storage, &graph, "Engineering/Specs", 50, true, false)
        .await
        .unwrap();
    // ... and 300 newer matching pages in "Section 2" that fill any global window.
    let flood: Vec<onenote_cli::models::OnenotePageSummary> = (0..300)
        .map(|i| onenote_cli::models::OnenotePageSummary {
            page_key: format!("me|flood-{i:03}"),
            id: format!("flood-{i:03}"),
            scope: onenote_cli::models::OnenoteScope::Me,
            section_id: "sec-2".to_string(),
            section_name: Some("Section 2".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Engineering".to_string()),
            title: format!("Architecture Flood {i}"),
            level: 0,
            order: i,
            created_at: None,
            last_modified_at: Some("2026-11-22T10:00:00Z".to_string()),
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: false,
        })
        .collect();
    storage.upsert_onenote_pages(&flood).unwrap();

    let http = reqwest::Client::new();
    let search = |section: &'static str| {
        let http = http.clone();
        async move {
            let resp = http
                .get(format!("http://{api_addr}/v1/onenote/search"))
                .query(&[("q", "Architecture"), ("section", section), ("limit", "5")])
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK);
            let v: Value = resp.json().await.unwrap();
            v["hits"].as_array().unwrap().clone()
        }
    };

    let specs = search("Specs").await;
    assert_eq!(specs.len(), 5);
    assert!(specs.iter().all(|h| h["section_id"] == "sec-1"));

    let second = search("section 2").await;
    assert_eq!(second.len(), 5);
    assert!(second.iter().all(|h| h["section_id"] == "sec-2"));

    assert!(search("Nope").await.is_empty());

    api_server.abort();
    graph_server.abort();
}

/// ON-22: the refusal still comes before the runtime is opened, so nothing is sent.
#[tokio::test]
async fn delete_without_yes_is_refused_before_any_request() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));
    const REFUSAL: &str =
        "Refusing to delete page without `--yes` in JSON or non-interactive mode.";

    let out_json = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["delete", "Architecture Spec", "--json"],
    )
    .await;
    assert_eq!(out_json.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out_json.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["message"], REFUSAL);

    // Without a terminal on stdin the text mode refuses the same way.
    let out_text = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["delete", "Architecture Spec", "--section", "Specs"],
    )
    .await;
    assert_eq!(out_text.status.code(), Some(1));
    assert!(out_text.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out_text.stderr).contains(REFUSAL));

    assert_eq!(mock.total_count(), 0, "no Graph request before the refusal");
    server.abort();
}
