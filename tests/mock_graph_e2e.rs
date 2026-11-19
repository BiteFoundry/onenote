//! End-to-end integration tests for standalone `@bitefoundry/onenote` (`onenote-cli`)
//! against an in-process mock Microsoft Graph v1.0 server:
//! - Notebooks, sections, and pages CRUD
//! - OneNote XHTML (`?includeIDs=true`) block parsing, GFM Markdown, surgical PATCH block updates & to-do toggles
//! - Inline image multipart page creation & append
//! - Local SQLite WAL + FTS5 hybrid search
//! - Resource download returning `{ ok, path, file_name, mime_type, size_bytes, is_image }`
//! - Local Axum HTTP API (`/health`, `/v1/whoami`, `/v1/onenote/*`, API key guard)
//! - Stdio MCP JSON-RPC 2.0 server (`initialize`, `tools/list`, all 13 OneNote tools + `whoami`)

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
struct MockGraphState {
    base_url: String,
    notebooks: Arc<Mutex<Vec<Value>>>,
    sections: Arc<Mutex<Vec<Value>>>,
    pages: Arc<Mutex<Vec<Value>>>,
    page_xhtml: Arc<Mutex<std::collections::HashMap<String, String>>>,
    patches_received: Arc<Mutex<Vec<String>>>,
    deleted_pages: Arc<Mutex<Vec<String>>>,
}

impl MockGraphState {
    fn new(base_url: String) -> Self {
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
        }
    }
}

/// Minimal valid 1x1 PNG image bytes for resource download & sniffing verification.
fn sample_png_bytes() -> Vec<u8> {
    let mut out = Vec::new();
    let img = image::RgbaImage::from_pixel(2, 2, image::Rgba([119, 25, 170, 255]));
    let mut cursor = std::io::Cursor::new(&mut out);
    image::DynamicImage::ImageRgba8(img)
        .write_to(&mut cursor, image::ImageFormat::Png)
        .unwrap();
    out
}

async fn spawn_mock_graph() -> (String, MockGraphState, tokio::task::JoinHandle<()>) {
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
            get(|| async { Json(json!({ "value": [] })) }),
        )
        .route(
            "/v1.0/me/onenote/notebooks",
            get(|State(st): State<MockGraphState>| async move {
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
                    (StatusCode::CREATED, Json(created))
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
            .patch(|| async { StatusCode::NO_CONTENT })
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

fn make_test_runtime(base_url: String, dl_dir: &std::path::Path) -> (AppConfig, GraphClient, Storage) {
    let mut cfg = AppConfig::default();
    cfg.graph_base_url = base_url;
    cfg.download_dir = dl_dir.to_string_lossy().into_owned();
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

#[tokio::test]
async fn e2e_onenote_crud_xhtml_fts5_resource_download() {
    let (base_url, mock_state, server) = spawn_mock_graph().await;
    let dir = tempfile::tempdir().unwrap();
    let (cfg, graph, storage) = make_test_runtime(base_url, dir.path());

    // 1. Notebooks, sections, pages
    let notebooks = cli::load_onenote_notebooks(&storage, &graph, false, true, false)
        .await
        .unwrap();
    assert_eq!(notebooks.len(), 1);
    assert_eq!(notebooks[0].display_name, "Engineering");

    let (_nb, sections) = cli::load_onenote_sections(&storage, &graph, Some("Engineering"), true, false)
        .await
        .unwrap();
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].display_name, "Specs");

    let (_sec, pages) = cli::list_onenote_pages_helper(&storage, &graph, "Engineering/Specs", 20, true, false)
        .await
        .unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].title, "Architecture Spec");

    // 2. Read page detail with parsed blocks, todo, and embedded image resource
    let detail = cli::load_onenote_page_detail(&storage, &graph, "me|page-1", false)
        .await
        .unwrap();
    assert_eq!(detail.summary.title, "Architecture Spec");
    assert!(detail.summary.has_todo);
    assert_eq!(detail.resources.len(), 1);
    assert!(detail.resources[0].is_image);

    // 3. FTS5 search finds cached page body
    let hits = cli::search_onenote_helper(&storage, &graph, "standalone", None, false, 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].page_id, "page-1");

    // 4. Surgical to-do toggle PATCH
    let (after_todo, block_id, completed) =
        cli::toggle_onenote_todo_helper(&storage, &graph, "me|page-1", None, Some(0))
            .await
            .unwrap();
    assert!(completed);
    assert!(block_id.contains("todo-1"));
    assert!(after_todo.raw_xhtml.contains("to-do:completed"));

    // 5. Surgical block update & append
    let after_update = cli::update_onenote_page_helper(
        &storage,
        &graph,
        "me|page-1",
        None,
        Some("Updated Block Copy"),
        Some("p:{body-1}"),
    )
    .await
    .unwrap();
    assert!(after_update.body_plain.contains("Updated Block Copy"));

    let after_append = cli::append_onenote_page_helper(
        &storage,
        &graph,
        "me|page-1",
        "Appended Section",
        &[],
    )
    .await
    .unwrap();
    assert!(after_append.body_plain.contains("Appended Section"));

    // 6. Resource download returns canonical `{ ok, path, file_name, mime_type, size_bytes, is_image }`
    let (_d, _idx, dl) =
        cli::download_onenote_resource_helper(&cfg, &storage, &graph, "me|page-1", Some(0), None)
            .await
            .unwrap();
    assert!(dl.ok);
    assert!(dl.is_image);
    assert_eq!(dl.mime_type, "image/png");
    assert!(dl.size_bytes > 0);
    assert!(std::path::Path::new(&dl.path).is_file());

    // 7. Create notebook, section, page, and delete page
    let created_nb = graph
        .create_onenote_notebook(&OnenoteScope::Me, "Personal")
        .await
        .unwrap();
    assert_eq!(created_nb.display_name, "Personal");

    let (_sec2, created_page) = cli::create_onenote_page_helper(
        &storage,
        &graph,
        "Specs",
        "Created E2E Page",
        "## Notes\n- [ ] Verify CI",
        Vec::new(),
        Some(1),
    )
    .await
    .unwrap();
    let del_key = cli::delete_onenote_page_helper(&storage, &graph, &created_page.page_key)
        .await
        .unwrap();
    assert_eq!(del_key, created_page.page_key);
    assert!(mock_state
        .deleted_pages
        .lock()
        .unwrap()
        .contains(&created_page.id));

    server.abort();
}

#[tokio::test]
async fn e2e_http_serve_and_mcp_tools() {
    let (base_url, _mock_state, graph_server) = spawn_mock_graph().await;
    let dir = tempfile::tempdir().unwrap();
    let (cfg, graph, storage) = make_test_runtime(base_url, dir.path());

    // --- Part A: HTTP API (`onenote serve`) ---
    let state = Arc::new(ServeState {
        config: cfg.clone(),
        graph: graph.clone(),
        storage: storage.clone(),
        api_key: Some("secret-key".to_string()),
    });
    let app = serve::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();
    let health: Value = http
        .get(format!("http://{api_addr}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["service"], "onenote-cli");

    // Unauthorized without API key
    let unauth = http
        .get(format!("http://{api_addr}/v1/onenote/notebooks"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // Authorized notebooks, sections, pages, resource download
    let nb_resp: Value = http
        .get(format!("http://{api_addr}/v1/onenote/notebooks?refresh=true"))
        .header("x-api-key", "secret-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(nb_resp["notebooks"].as_array().unwrap().len(), 1);

    let dl_resp: Value = http
        .post(format!(
            "http://{api_addr}/v1/onenote/pages/me%7Cpage-1/resources/download"
        ))
        .header("Authorization", "Bearer secret-key")
        .json(&json!({ "index": 0 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(dl_resp["ok"], true);
    assert_eq!(dl_resp["is_image"], true);
    assert_eq!(dl_resp["mime_type"], "image/png");
    assert!(dl_resp["size_bytes"].as_u64().unwrap() > 0);
    assert!(dl_resp["path"].as_str().is_some());
    assert!(dl_resp["file_name"].as_str().is_some());

    // --- Part B: MCP JSON-RPC 2.0 server ---
    let init = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
    )
    .await
    .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "onenote-cli");

    let listed = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await
    .unwrap();
    let tool_names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in [
        "whoami",
        "list_notebooks",
        "list_sections",
        "list_pages",
        "get_page",
        "create_notebook",
        "create_section",
        "create_page",
        "append_page",
        "update_block",
        "toggle_todo",
        "delete_page",
        "search_pages",
        "download_resource",
    ] {
        assert!(
            tool_names.contains(&expected),
            "missing required MCP tool: {expected}"
        );
    }

    // Call MCP `download_resource` and check canonical JSON fields
    let mcp_dl = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "download_resource",
                "arguments": { "page": "me|page-1", "index": 0 }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_dl["result"]["isError"], false);
    let sc = &mcp_dl["result"]["structuredContent"];
    assert_eq!(sc["ok"], true);
    assert_eq!(sc["is_image"], true);
    assert_eq!(sc["mime_type"], "image/png");
    assert!(sc["size_bytes"].as_u64().unwrap() > 0);
    assert!(std::path::Path::new(sc["path"].as_str().unwrap()).is_file());

    // Call MCP `search_pages`
    let mcp_search = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "search_pages",
                "arguments": { "query": "Architecture" }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_search["result"]["isError"], false);
    assert!(
        !mcp_search["result"]["structuredContent"]["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Call MCP `delete_page` without confirm: true -> must return isError: true
    let mcp_del_guard = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "delete_page",
                "arguments": { "page": "me|page-1", "confirm": false }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_del_guard["result"]["isError"], true);

    api_server.abort();
    graph_server.abort();
}
