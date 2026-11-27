#![allow(unused_imports)]

use std::sync::{Arc, Mutex};

use axum::{
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use onenote_cli::{
    auth::{AuthManager, StoredTokens},
    cli,
    config::AppConfig,
    graph::GraphClient,
    mcp,
    models::OnenoteScope,
    serve::{self, AppState as ServeState},
    storage::Storage,
};

use crate::server::*;

fn search_fixture_section(id: &str, name: &str) -> onenote_cli::models::OnenoteSection {
    onenote_cli::models::OnenoteSection {
        section_key: format!("me|{id}"),
        id: id.to_string(),
        scope: OnenoteScope::Me,
        notebook_id: Some("nb-1".to_string()),
        notebook_name: Some("Work".to_string()),
        parent_section_group_id: None,
        parent_section_group_name: None,
        display_name: name.to_string(),
        color: None,
        is_default: false,
        pages_url: None,
        created_at: None,
        last_modified_at: None,
        web_url: None,
    }
}

fn search_fixture_page(
    id: String,
    section_id: &str,
    section_name: &str,
    modified_at: &str,
) -> onenote_cli::models::OnenotePageSummary {
    onenote_cli::models::OnenotePageSummary {
        page_key: format!("me|{id}"),
        title: format!("Kickoff notes {id}"),
        id,
        scope: OnenoteScope::Me,
        section_id: section_id.to_string(),
        section_name: Some(section_name.to_string()),
        notebook_id: Some("nb-1".to_string()),
        notebook_name: Some("Work".to_string()),
        level: 0,
        order: 0,
        created_at: None,
        last_modified_at: Some(modified_at.to_string()),
        content_url: None,
        web_url: None,
        preview_text: None,
        has_todo: false,
    }
}

/// ON-10: a plain `--section` name used to be post-filtered over a small window of
/// global hits, so a section whose hits ranked below that window returned nothing.
#[tokio::test]
async fn search_with_plain_section_name_is_filtered_before_the_limit() {
    let tmp = tempfile::tempdir().unwrap();
    // The search is local: the Graph URL is never contacted.
    let (_cfg, graph, storage) =
        make_test_runtime("http://127.0.0.1:9/v1.0".to_string(), tmp.path());

    storage
        .upsert_onenote_sections(&[
            search_fixture_section("sec-alpha", "Alpha"),
            search_fixture_section("sec-beta", "Beta"),
            search_fixture_section("sec-ko", "회의록 DEV"),
        ])
        .unwrap();
    let mut pages = Vec::new();
    for i in 0..300 {
        pages.push(search_fixture_page(
            format!("alpha-{i:03}"),
            "sec-alpha",
            "Alpha",
            "2026-11-22T09:00:00Z",
        ));
    }
    for i in 0..3 {
        pages.push(search_fixture_page(
            format!("beta-{i}"),
            "sec-beta",
            "Beta",
            "2026-11-02T09:00:00Z",
        ));
    }
    for i in 0..25 {
        pages.push(search_fixture_page(
            format!("ko-{i:02}"),
            "sec-ko",
            "회의록 DEV",
            "2026-11-03T09:00:00Z",
        ));
    }
    // Pages whose section row is missing from the sections table.
    for i in 0..2 {
        pages.push(search_fixture_page(
            format!("ghost-{i}"),
            "sec-ghost",
            "Ghost Notes",
            "2026-11-01T09:00:00Z",
        ));
    }
    storage.upsert_onenote_pages(&pages).unwrap();

    // 300 newer hits in "Alpha" no longer hide the 3 in "Beta".
    let (hits, has_more) =
        cli::search_onenote_with_offset(&storage, &graph, "kickoff", Some("Beta"), false, 10, 0)
            .await
            .unwrap();
    assert_eq!(hits.len(), 3);
    assert!(hits.iter().all(|h| h.section_id == "sec-beta"));
    assert_eq!(has_more, Some(false));

    // A raw section ID keeps working as a plain filter.
    let (by_id, by_id_more) =
        cli::search_onenote_with_offset(&storage, &graph, "kickoff", Some("sec-beta"), false, 10, 0)
            .await
            .unwrap();
    assert_eq!(by_id.len(), 3);
    assert_eq!(by_id_more, Some(false));

    // Offset/limit paging across a 25-hit section, matched case-insensitively.
    let mut seen = std::collections::BTreeSet::new();
    for (offset, expect_len, expect_more) in [(0, 10, true), (10, 10, true), (20, 5, false)] {
        let (page, more) = cli::search_onenote_with_offset(
            &storage,
            &graph,
            "kickoff",
            Some("회의록 dev"),
            false,
            10,
            offset,
        )
        .await
        .unwrap();
        assert_eq!(page.len(), expect_len, "offset {offset}");
        assert_eq!(more, Some(expect_more), "offset {offset}");
        assert!(page.iter().all(|h| h.section_id == "sec-ko"));
        seen.extend(page.into_iter().map(|h| h.page_key));
    }
    assert_eq!(seen.len(), 25);

    // No matching section: an empty result that is known to be complete.
    let (none, none_more) =
        cli::search_onenote_with_offset(&storage, &graph, "kickoff", Some("Nope"), false, 10, 0)
            .await
            .unwrap();
    assert!(none.is_empty());
    assert_eq!(none_more, Some(false));

    // Hits whose section is missing from the sections table are still matched by name.
    let (ghost, ghost_more) =
        cli::search_onenote_with_offset(&storage, &graph, "kickoff", Some("ghost"), false, 10, 0)
            .await
            .unwrap();
    assert_eq!(ghost.len(), 2);
    assert_eq!(ghost_more, Some(false));

    // Scoped keys and the unfiltered search are unchanged.
    let (scoped, scoped_more) = cli::search_onenote_with_offset(
        &storage,
        &graph,
        "kickoff",
        Some("me|sec-beta"),
        false,
        10,
        0,
    )
    .await
    .unwrap();
    assert_eq!(scoped.len(), 3);
    assert_eq!(scoped_more, Some(false));
    let (all, all_more) =
        cli::search_onenote_with_offset(&storage, &graph, "kickoff", None, false, 10, 0)
            .await
            .unwrap();
    assert_eq!(all.len(), 10);
    assert_eq!(all_more, Some(true));
}

/// ON-20: a page creation answered with 504 may already have been applied, so it
/// is not sent again; a 429 was rejected before processing and is retried.
#[tokio::test]
async fn e2e_create_page_is_not_replayed_after_gateway_timeout() {
    let (base_url, mock_state, server) = spawn_mock_graph().await;
    let tmp = tempfile::tempdir().unwrap();
    let (_cfg, graph, storage) = make_test_runtime(base_url, tmp.path());
    cli::load_onenote_sections(&storage, &graph, None, true, false)
        .await
        .unwrap();
    let pages_before = mock_state.pages.lock().unwrap().len();

    mock_state.page_post_failures.lock().unwrap().push_back(504);
    let err = cli::create_onenote_page_helper(
        &storage,
        &graph,
        "Specs",
        "Gateway timeout page",
        "body",
        Vec::new(),
        None,
    )
    .await
    .expect_err("a 504 on page creation must fail instead of being replayed");
    assert_eq!(*mock_state.page_post_hits.lock().unwrap(), 1);
    assert!(
        format!("{err:#}").contains("The request may have been applied; check before retrying."),
        "unexpected error: {err:#}"
    );
    assert_eq!(mock_state.pages.lock().unwrap().len(), pages_before);

    mock_state.page_post_failures.lock().unwrap().push_back(429);
    let (_sec, created) = cli::create_onenote_page_helper(
        &storage,
        &graph,
        "Specs",
        "Throttled page",
        "body",
        Vec::new(),
        None,
    )
    .await
    .unwrap();
    assert!(!created.id.is_empty());
    // One throttled attempt plus the retry that created the page.
    assert_eq!(*mock_state.page_post_hits.lock().unwrap(), 3);
    assert_eq!(mock_state.pages.lock().unwrap().len(), pages_before + 1);

    server.abort();
}

#[tokio::test]
async fn append_uses_cached_primary_div_and_retries_once_on_stale_target() {
    use std::sync::atomic::{AtomicUsize, Ordering};

    let content_get_hits = Arc::new(AtomicUsize::new(0));
    let patch_hits = Arc::new(AtomicUsize::new(0));
    let current_div = Arc::new(Mutex::new("div:{div-v1}".to_string()));
    let body_text = Arc::new(Mutex::new("Initial".to_string()));

    let app = Router::new()
        .route(
            "/v1.0/me/onenote/pages/p-1",
            get(|| async {
                Json(serde_json::json!({
                    "id": "p-1",
                    "title": "Fast Append",
                    "level": 0,
                    "order": 0,
                    "parentSection": { "id": "sec-1", "displayName": "Notes" }
                }))
            }),
        )
        .route(
            "/v1.0/me/onenote/pages/p-1/content",
            get({
                let gets = content_get_hits.clone();
                let div = current_div.clone();
                let body = body_text.clone();
                move || async move {
                    gets.fetch_add(1, Ordering::SeqCst);
                    let d = div.lock().unwrap().clone();
                    let b = body.lock().unwrap().clone();
                    (
                        StatusCode::OK,
                        [("content-type", "text/html")],
                        format!(
                            "<!DOCTYPE html><html><head><title>Fast Append</title></head><body><div id=\"{d}\"><p id=\"p:{{1}}\">{b}</p></div></body></html>"
                        ),
                    )
                }
            })
            .patch({
                let patches = patch_hits.clone();
                let div = current_div.clone();
                let body = body_text.clone();
                move |raw_body: axum::body::Bytes| async move {
                    patches.fetch_add(1, Ordering::SeqCst);
                    let req_str = String::from_utf8_lossy(&raw_body);
                    let expected_div = div.lock().unwrap().clone();
                    if !req_str.contains(&expected_div) {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "error": { "code": "20132", "message": "target div does not exist" }
                            })),
                        );
                    }
                    body.lock().unwrap().push_str(" + appended");
                    (StatusCode::NO_CONTENT, Json(serde_json::json!({})))
                }
            }),
        );

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let cfg = AppConfig {
        graph_base_url: format!("http://{addr}/v1.0"),
        ..AppConfig::default()
    };
    let auth = AuthManager::from_tokens(
        cfg.clone(),
        StoredTokens {
            access_token: "test-token".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("u1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".to_string(),
        },
    );
    let graph = GraphClient::new(cfg, auth).unwrap();
    let storage = Storage::open_in_memory().unwrap();

    // Seed SQLite cache with initial detail (div:{div-v1}).
    let seeded = cli::load_onenote_page_detail(&storage, &graph, "me|p-1", false)
        .await
        .unwrap();
    assert_eq!(seeded.primary_div_id.as_deref(), Some("div:{div-v1}"));
    content_get_hits.store(0, Ordering::SeqCst);
    patch_hits.store(0, Ordering::SeqCst);

    // 1. Cached append: skips pre-fetch GET (only 1 PATCH + 1 post-write GET).
    let updated = cli::append_onenote_page_helper(&storage, &graph, "me|p-1", "Line 1", &[])
        .await
        .unwrap();
    assert!(updated.body_plain.contains("appended"));
    assert_eq!(patch_hits.load(Ordering::SeqCst), 1);
    assert_eq!(
        content_get_hits.load(Ordering::SeqCst),
        1,
        "cached append must skip redundant pre-fetch GET"
    );

    // 2. Stale cached primary_div_id: PATCH 400 triggers 1 remote refresh + retry.
    *current_div.lock().unwrap() = "div:{div-v2}".to_string();
    content_get_hits.store(0, Ordering::SeqCst);
    patch_hits.store(0, Ordering::SeqCst);
    let retried = cli::append_onenote_page_helper(&storage, &graph, "me|p-1", "Line 2", &[])
        .await
        .unwrap();
    assert_eq!(retried.primary_div_id.as_deref(), Some("div:{div-v2}"));
    assert_eq!(patch_hits.load(Ordering::SeqCst), 2);
    assert_eq!(content_get_hits.load(Ordering::SeqCst), 2);

    srv.abort();
}
