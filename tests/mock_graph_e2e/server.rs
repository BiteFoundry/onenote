//! In-process mock Microsoft Graph v1.0 server and runtime test helpers.

#![allow(dead_code, unused_imports)]

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

#[derive(Clone)]
pub(crate) struct MockGraphState {
    pub(crate) base_url: String,
    pub(crate) notebooks: Arc<Mutex<Vec<Value>>>,
    pub(crate) joined_teams: Arc<Mutex<Vec<Value>>>,
    pub(crate) group_notebooks: Arc<Mutex<std::collections::HashMap<String, Vec<Value>>>>,
    pub(crate) sections: Arc<Mutex<Vec<Value>>>,
    pub(crate) pages: Arc<Mutex<Vec<Value>>>,
    pub(crate) page_xhtml: Arc<Mutex<std::collections::HashMap<String, String>>>,
    pub(crate) patches_received: Arc<Mutex<Vec<String>>>,
    pub(crate) deleted_pages: Arc<Mutex<Vec<String>>>,
    pub(crate) notebook_list_hits: Arc<Mutex<usize>>,
    /// Failure injection: statuses answered (first in, first out) by the next
    /// `POST …/sections/{id}/pages` requests before the mock creates a page.
    pub(crate) page_post_failures: Arc<Mutex<std::collections::VecDeque<u16>>>,
    pub(crate) page_post_hits: Arc<Mutex<usize>>,
}

impl MockGraphState {
    pub(crate) fn new(base_url: String) -> Self {
        let initial_xhtml = format!(
            r#"<!DOCTYPE html>
<html lang="en-US">
<head><title>Architecture Spec</title></head>
<body>
  <div id="div:{{root-1}}">
    <h1 id="h1:{{1}}">System Overview</h1>
    <p id="p:{{todo-1}}" data-tag="to-do">Ship OneNote standalone v0.1.0</p>
    <p id="p:{{body-1}}">See <a href="https://docs.example.com/onenote">OneNote Docs</a> for details.</p>
    <img id="img:{{1}}" src="{base_url}/me/onenote/resources/res-img-1/content" data-fullres-src="{base_url}/me/onenote/resources/res-img-1/content" alt="diagram" width="320" height="200" />
  </div>
</body>
</html>"#
        );

        let mut xhtml_map = std::collections::HashMap::new();
        xhtml_map.insert("page-1".to_string(), initial_xhtml);

        Self {
            base_url: base_url.clone(),
            notebooks: Arc::new(Mutex::new(vec![json!({
                "id": "nb-1",
                "displayName": "Engineering",
                "isDefault": true,
                "isShared": false,
                "userRole": "Owner",
                "createdDateTime": "2026-11-01T00:00:00Z",
                "lastModifiedDateTime": "2026-11-18T10:00:00Z",
                "links": { "oneNoteWebUrl": { "href": "https://onedrive.live.com/nb-1" } }
            })])),
            joined_teams: Arc::new(Mutex::new(Vec::new())),
            group_notebooks: Arc::new(Mutex::new(std::collections::HashMap::new())),
            sections: Arc::new(Mutex::new(vec![json!({
                "id": "sec-1",
                "displayName": "Specs",
                "isDefault": true,
                "parentNotebook": { "id": "nb-1", "displayName": "Engineering" },
                "createdDateTime": "2026-11-01T00:00:00Z",
                "lastModifiedDateTime": "2026-11-18T10:00:00Z"
            })])),
            pages: Arc::new(Mutex::new(vec![json!({
                "id": "page-1",
                "title": "Architecture Spec",
                "level": 0,
                "order": 0,
                "createdDateTime": "2026-11-01T00:00:00Z",
                "lastModifiedDateTime": "2026-11-18T10:00:00Z",
                "contentUrl": format!("{base_url}/me/onenote/pages/page-1/content"),
                "parentSection": { "id": "sec-1", "displayName": "Specs" },
                "links": { "oneNoteWebUrl": { "href": "https://onedrive.live.com/page-1" } }
            })])),
            page_xhtml: Arc::new(Mutex::new(xhtml_map)),
            patches_received: Arc::new(Mutex::new(Vec::new())),
            deleted_pages: Arc::new(Mutex::new(Vec::new())),
            notebook_list_hits: Arc::new(Mutex::new(0)),
            page_post_failures: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            page_post_hits: Arc::new(Mutex::new(0)),
        }
    }
}

/// Minimal valid 1x1 PNG image bytes for resource download & sniffing verification.
pub(crate) fn sample_png_bytes() -> Vec<u8> {
    let mut out = Vec::new();
    let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([119, 25, 170, 255]));
    let mut cursor = std::io::Cursor::new(&mut out);
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .unwrap();
    out
}

pub(crate) async fn spawn_mock_graph() -> (String, MockGraphState, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");
    let state = MockGraphState::new(base_url.clone());

    let app = Router::new()
        .route(
            "/v1.0/me",
            get(|| async {
                Json(json!({
                    "id": "user-1",
                    "displayName": "Alice Engineer",
                    "mail": "alice@contoso.com",
                    "jobTitle": "Principal Engineer"
                }))
            }),
        )
        .route(
            "/v1.0/me/joinedTeams",
            get(|State(st): State<MockGraphState>| async move {
                let items = st.joined_teams.lock().unwrap().clone();
                Json(json!({ "value": items }))
            }),
        )
        .route(
            "/v1.0/groups/{group_id}/onenote/notebooks",
            get(
                |State(st): State<MockGraphState>, AxumPath(group_id): AxumPath<String>| async move {
                    let items = st
                        .group_notebooks
                        .lock()
                        .unwrap()
                        .get(&group_id)
                        .cloned()
                        .unwrap_or_default();
                    Json(json!({ "value": items }))
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/notebooks",
            get(|State(st): State<MockGraphState>| async move {
                *st.notebook_list_hits.lock().unwrap() += 1;
                let items = st.notebooks.lock().unwrap().clone();
                Json(json!({ "value": items }))
            })
            .post(|State(st): State<MockGraphState>, Json(body): Json<Value>| async move {
                let name = body["displayName"].as_str().unwrap_or("New Notebook");
                let created = json!({
                    "id": "nb-new",
                    "displayName": name,
                    "isDefault": false,
                    "isShared": false,
                    "userRole": "Owner"
                });
                st.notebooks.lock().unwrap().push(created.clone());
                (StatusCode::CREATED, Json(created))
            }),
        )
        .route(
            "/v1.0/me/onenote/notebooks/{nb_id}/sectionGroups",
            get(|| async { Json(json!({ "value": [] })) }),
        )
        .route(
            "/v1.0/me/onenote/sections",
            get(|State(st): State<MockGraphState>| async move {
                let items = st.sections.lock().unwrap().clone();
                Json(json!({ "value": items }))
            }),
        )
        .route(
            "/v1.0/me/onenote/notebooks/{nb_id}/sections",
            get(|State(st): State<MockGraphState>| async move {
                let items = st.sections.lock().unwrap().clone();
                Json(json!({ "value": items }))
            })
            .post(
                |State(st): State<MockGraphState>,
                 AxumPath(nb_id): AxumPath<String>,
                 Json(body): Json<Value>| async move {
                    let name = body["displayName"].as_str().unwrap_or("New Section");
                    let created = json!({
                        "id": "sec-new",
                        "displayName": name,
                        "isDefault": false,
                        "parentNotebook": { "id": nb_id, "displayName": "Engineering" }
                    });
                    st.sections.lock().unwrap().push(created.clone());
                    (StatusCode::CREATED, Json(created))
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/sections/{sec_id}/pages",
            get(|State(st): State<MockGraphState>| async move {
                let items = st.pages.lock().unwrap().clone();
                Json(json!({ "value": items }))
            })
            .post(
                |State(st): State<MockGraphState>,
                 AxumPath(sec_id): AxumPath<String>,
                 headers: HeaderMap,
                 body: Bytes| async move {
                    use axum::response::IntoResponse;
                    *st.page_post_hits.lock().unwrap() += 1;
                    if let Some(code) = st.page_post_failures.lock().unwrap().pop_front() {
                        return (
                            StatusCode::from_u16(code).unwrap(),
                            [("Retry-After", "0")],
                            Json(json!({ "error": { "code": "Injected", "message": "injected failure" } })),
                        )
                            .into_response();
                    }
                    let raw = String::from_utf8_lossy(&body).into_owned();
                    let ct = headers
                        .get("content-type")
                        .and_then(|v| v.to_str().ok())
                        .unwrap_or("");
                    assert!(
                        ct.contains("application/xhtml+xml")
                            || ct.contains("text/html")
                            || ct.contains("multipart/form-data"),
                        "unexpected content-type: {ct}"
                    );
                    let id = format!("page-{}", st.pages.lock().unwrap().len() + 1);
                    let created = json!({
                        "id": id,
                        "title": "Created E2E Page",
                        "level": 0,
                        "order": 1,
                        "contentUrl": format!("{}/me/onenote/pages/{id}/content", st.base_url),
                        "parentSection": { "id": sec_id, "displayName": "Specs" }
                    });
                    st.pages.lock().unwrap().push(created.clone());
                    st.page_xhtml.lock().unwrap().insert(id, raw);
                    (StatusCode::CREATED, Json(created)).into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/pages/{page_id}",
            get(
                |State(st): State<MockGraphState>, AxumPath(page_id): AxumPath<String>| async move {
                    let found = st
                        .pages
                        .lock()
                        .unwrap()
                        .iter()
                        .find(|p| p["id"].as_str() == Some(page_id.as_str()))
                        .cloned();
                    match found {
                        Some(p) => (StatusCode::OK, Json(p)),
                        None => (
                            StatusCode::NOT_FOUND,
                            Json(json!({ "error": { "code": "ResourceNotFound", "message": "Page not found" } })),
                        ),
                    }
                },
            )
            .delete(
                |State(st): State<MockGraphState>, AxumPath(page_id): AxumPath<String>| async move {
                    st.deleted_pages.lock().unwrap().push(page_id.clone());
                    st.pages
                        .lock()
                        .unwrap()
                        .retain(|p| p["id"].as_str() != Some(page_id.as_str()));
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/pages/{page_id}/content",
            get(
                |State(st): State<MockGraphState>, AxumPath(page_id): AxumPath<String>| async move {
                    let xhtml = st
                        .page_xhtml
                        .lock()
                        .unwrap()
                        .get(&page_id)
                        .cloned()
                        .unwrap_or_else(|| "<html><body><p>Empty</p></body></html>".to_string());
                    (StatusCode::OK, [("content-type", "text/html")], xhtml)
                },
            )
            .patch(
                |State(st): State<MockGraphState>,
                 AxumPath(page_id): AxumPath<String>,
                 body: Bytes| async move {
                    let raw = String::from_utf8_lossy(&body).into_owned();
                    st.patches_received.lock().unwrap().push(raw.clone());
                    if raw.contains("to-do:completed") {
                        let mut map = st.page_xhtml.lock().unwrap();
                        if let Some(html) = map.get_mut(&page_id) {
                            *html = html.replace(r#"data-tag="to-do""#, r#"data-tag="to-do:completed""#);
                        }
                    } else if raw.contains(r#"data-tag=\"to-do\""#) {
                        let mut map = st.page_xhtml.lock().unwrap();
                        if let Some(html) = map.get_mut(&page_id) {
                            *html = html.replace(r#"data-tag="to-do:completed""#, r#"data-tag="to-do""#);
                        }
                    } else if raw.contains("Updated Block Copy") {
                        let mut map = st.page_xhtml.lock().unwrap();
                        if let Some(html) = map.get_mut(&page_id) {
                            *html = html.replace("OneNote Docs", "Updated Block Copy");
                        }
                    } else if raw.contains("Appended Section") {
                        let mut map = st.page_xhtml.lock().unwrap();
                        if let Some(html) = map.get_mut(&page_id) {
                            *html = html.replace("</div>", "<p id=\"p:{appended}\">Appended Section</p></div>");
                        }
                    }
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/resources/{res_id}/content",
            get(|| async {
                (
                    StatusCode::OK,
                    [("content-type", "image/png")],
                    sample_png_bytes(),
                )
            }),
        )
        .with_state(state.clone());

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    (base_url, state, handle)
}

pub(crate) fn make_test_runtime(base_url: String, dl_dir: &std::path::Path) -> (AppConfig, GraphClient, Storage) {
    let cfg = AppConfig {
        graph_base_url: base_url,
        download_dir: dl_dir.to_string_lossy().into_owned(),
        ..AppConfig::default()
    };
    let tokens = StoredTokens {
        access_token: "e2e-test-token".to_string(),
        refresh_token: None,
        expires_at: chrono::Utc::now().timestamp() + 3600,
        client_id: "cid".to_string(),
        tenant_id: "tid".to_string(),
        user_id: Some("user-1".to_string()),
        user_display_name: Some("Alice Engineer".to_string()),
        user_email: Some("alice@contoso.com".to_string()),
        auth_mode: "access_token".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tokens);
    let graph = GraphClient::new(cfg.clone(), auth)
        .unwrap()
        .with_my_user_id("user-1");
    let storage = Storage::open_in_memory().unwrap();
    (cfg, graph, storage)
}

