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

use axum::{extract::Request, response::IntoResponse};
use base64::Engine as _;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

fn unsigned_jwt(payload: Value) -> String {
    let b64 = base64::engine::general_purpose::URL_SAFE_NO_PAD;
    format!(
        "{}.{}.sig",
        b64.encode(r#"{"alg":"RS256","typ":"JWT"}"#),
        b64.encode(payload.to_string())
    )
}

fn runtime_with_token(
    base_url: String,
    access_token: String,
    tenant_id: &str,
) -> (GraphClient, Storage) {
    let cfg = AppConfig {
        graph_base_url: base_url,
        tenant_id: tenant_id.to_string(),
        ..AppConfig::default()
    };
    let tokens = StoredTokens {
        access_token,
        refresh_token: None,
        expires_at: chrono::Utc::now().timestamp() + 3600,
        client_id: "cid".to_string(),
        tenant_id: tenant_id.to_string(),
        user_id: Some("user-1".to_string()),
        user_display_name: Some("Alice Engineer".to_string()),
        user_email: Some("alice@contoso.com".to_string()),
        auth_mode: "pkce".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tokens);
    let graph = GraphClient::new(cfg, auth).unwrap().with_my_user_id("user-1");
    (graph, Storage::open_in_memory().unwrap())
}

/// ON-01: a section group that fails to load must not delete its cached
/// sections and pages; a later complete fetch still prunes what is gone.
#[tokio::test]
async fn partial_section_fetch_keeps_cached_sections_and_pages() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1.0", listener.local_addr().unwrap());
    let group_sections_fail = Arc::new(AtomicBool::new(false));
    let grouped_section_exists = Arc::new(AtomicBool::new(true));
    let app = Router::new().fallback({
        let fail = group_sections_fail.clone();
        let exists = grouped_section_exists.clone();
        move |req: Request| {
            let fail = fail.clone();
            let exists = exists.clone();
            async move {
                let path = req.uri().path().to_string();
                match (req.method().as_str(), path.as_str()) {
                    ("GET", "/v1.0/me/onenote/notebooks") => Json(json!({
                        "value": [{ "id": "nb-1", "displayName": "Engineering", "isDefault": true }]
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/joinedTeams") => Json(json!({ "value": [] })).into_response(),
                    ("GET", "/v1.0/me/onenote/notebooks/nb-1/sections") => Json(json!({
                        "value": [{
                            "id": "sec-direct",
                            "displayName": "Direct",
                            "parentNotebook": { "id": "nb-1", "displayName": "Engineering" }
                        }]
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/onenote/notebooks/nb-1/sectionGroups") => Json(json!({
                        "value": [{
                            "id": "sg-1",
                            "displayName": "Archive",
                            "parentNotebook": { "id": "nb-1", "displayName": "Engineering" }
                        }]
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/onenote/sectionGroups/sg-1/sectionGroups") => {
                        Json(json!({ "value": [] })).into_response()
                    }
                    ("GET", "/v1.0/me/onenote/sectionGroups/sg-1/sections") => {
                        if fail.load(Ordering::SeqCst) {
                            return (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(json!({
                                    "error": { "code": "InternalServerError", "message": "backend down" }
                                })),
                            )
                                .into_response();
                        }
                        let value = if exists.load(Ordering::SeqCst) {
                            json!([{
                                "id": "sec-grouped",
                                "displayName": "Grouped",
                                "parentNotebook": { "id": "nb-1", "displayName": "Engineering" },
                                "parentSectionGroup": { "id": "sg-1", "displayName": "Archive" }
                            }])
                        } else {
                            json!([])
                        };
                        Json(json!({ "value": value })).into_response()
                    }
                    ("GET", "/v1.0/me/onenote/sections/sec-grouped/pages") => Json(json!({
                        "value": [{
                            "id": "page-grouped",
                            "title": "Grouped runbook",
                            "level": 0,
                            "order": 0,
                            "parentSection": { "id": "sec-grouped", "displayName": "Grouped" }
                        }]
                    }))
                    .into_response(),
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
            }
        }
    });
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    let dir = tempfile::tempdir().unwrap();
    let (_cfg, graph, storage) = make_test_runtime(base_url, dir.path());
    let section_keys = |sections: &[onenote_cli::models::OnenoteSection]| {
        let mut keys: Vec<String> = sections.iter().map(|s| s.section_key.clone()).collect();
        keys.sort();
        keys
    };

    // 1. Complete fetch caches both sections; cache a page of the grouped one.
    let (_nb, sections, warnings) = cli::load_onenote_sections_with_warnings(
        &storage,
        &graph,
        Some("me|nb-1"),
        true,
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(section_keys(&sections), ["me|sec-direct", "me|sec-grouped"]);
    assert!(warnings.is_empty(), "{warnings:?}");
    let (_sec, pages) =
        cli::list_onenote_pages_helper(&storage, &graph, "me|sec-grouped", 50, true, false)
            .await
            .unwrap();
    assert_eq!(pages.len(), 1);
    let hits = storage.search_onenote_pages("runbook", None, 10).unwrap();
    assert_eq!(hits.len(), 1, "page title must be searchable before the partial fetch");

    // 2. The section group's sections fail (HTTP 500): nothing is pruned.
    group_sections_fail.store(true, Ordering::SeqCst);
    let (_nb, sections, warnings) = cli::load_onenote_sections_with_warnings(
        &storage,
        &graph,
        Some("me|nb-1"),
        true,
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(section_keys(&sections), ["me|sec-direct", "me|sec-grouped"]);
    assert_eq!(warnings.len(), 1);
    assert!(warnings[0].contains("could not be fetched"), "{warnings:?}");
    assert_eq!(storage.list_onenote_pages("me|sec-grouped").unwrap().len(), 1);
    assert_eq!(storage.search_onenote_pages("runbook", None, 10).unwrap().len(), 1);
    // The plain helper used by HTTP/MCP behaves the same way.
    let (_nb, sections) =
        cli::load_onenote_sections(&storage, &graph, Some("me|nb-1"), true, false)
            .await
            .unwrap();
    assert_eq!(sections.len(), 2);

    // 3. Complete fetch after the section really was deleted: pruned with its pages.
    group_sections_fail.store(false, Ordering::SeqCst);
    grouped_section_exists.store(false, Ordering::SeqCst);
    let (_nb, sections, warnings) = cli::load_onenote_sections_with_warnings(
        &storage,
        &graph,
        Some("me|nb-1"),
        true,
        false,
        false,
    )
    .await
    .unwrap();
    assert_eq!(section_keys(&sections), ["me|sec-direct"]);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert!(storage.list_onenote_pages("me|sec-grouped").unwrap().is_empty());
    assert!(storage.search_onenote_pages("runbook", None, 10).unwrap().is_empty());

    server.abort();
}

/// ON-05: a work sign-in without Team.ReadBasic.All gets its personal
/// notebooks plus a warning, and /me/joinedTeams is never requested.
#[tokio::test]
async fn include_teams_without_org_scopes_warns_and_skips_joined_teams() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base_url = format!("http://{}/v1.0", listener.local_addr().unwrap());
    let joined_teams_hits = Arc::new(AtomicUsize::new(0));
    let app = Router::new().fallback({
        let hits = joined_teams_hits.clone();
        move |req: Request| {
            let hits = hits.clone();
            async move {
                match req.uri().path() {
                    "/v1.0/me/onenote/notebooks" => Json(json!({
                        "value": [{ "id": "nb-1", "displayName": "Personal", "isDefault": true }]
                    }))
                    .into_response(),
                    "/v1.0/me/joinedTeams" => {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(json!({ "value": [{ "id": "team-1", "displayName": "Team" }] }))
                            .into_response()
                    }
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
            }
        }
    });
    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let notes_only = unsigned_jwt(json!({
        "tid": "org-tenant-123",
        "scp": "User.Read Notes.Read Notes.ReadWrite Notes.Create"
    }));
    let (graph, storage) = runtime_with_token(base_url.clone(), notes_only, "common");
    let (notebooks, warnings) =
        cli::load_onenote_notebooks_with_warnings(&storage, &graph, true, true, false, false)
            .await
            .unwrap();
    assert_eq!(notebooks.len(), 1);
    assert_eq!(notebooks[0].notebook_key, "me|nb-1");
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert!(warnings[0].contains("Teams notebooks were skipped"));
    assert!(warnings[0].contains("onenote login --tenant-id"));
    assert_eq!(joined_teams_hits.load(Ordering::SeqCst), 0);

    // Served from the cache: the hint is repeated, still without a Teams request.
    let (cached, warnings) =
        cli::load_onenote_notebooks_with_warnings(&storage, &graph, true, false, false, false)
            .await
            .unwrap();
    assert_eq!(cached.len(), 1);
    assert_eq!(warnings.len(), 1, "{warnings:?}");
    assert_eq!(joined_teams_hits.load(Ordering::SeqCst), 0);

    // Not asking for Teams: no warning.
    let (_personal, warnings) =
        cli::load_onenote_notebooks_with_warnings(&storage, &graph, false, true, false, false)
            .await
            .unwrap();
    assert!(warnings.is_empty(), "{warnings:?}");

    // A personal Microsoft account stays silent.
    let consumer = unsigned_jwt(json!({
        "tid": "9188040d-6c67-4c5b-b112-36a304b66dad",
        "scp": "User.Read Notes.ReadWrite"
    }));
    let (graph, storage) = runtime_with_token(base_url, consumer, "consumers");
    let (notebooks, warnings) =
        cli::load_onenote_notebooks_with_warnings(&storage, &graph, true, true, false, false)
            .await
            .unwrap();
    assert_eq!(notebooks.len(), 1);
    assert!(warnings.is_empty(), "{warnings:?}");
    assert_eq!(joined_teams_hits.load(Ordering::SeqCst), 0);

    server.abort();
}
