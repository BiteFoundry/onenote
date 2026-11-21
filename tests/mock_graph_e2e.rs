//! End-to-end integration tests for standalone `@bitefoundry/onenote` (`onenote-cli`)
//! against an in-process mock Microsoft Graph v1.0 server:
//! - Notebooks, sections, and pages CRUD
//! - OneNote XHTML (`?includeIDs=true`) block parsing, GFM Markdown, surgical PATCH block updates & to-do toggles
//! - Inline image multipart page creation & append
//! - Local SQLite WAL + FTS5 hybrid search
//! - Resource download returning `{ ok, path, file_name, mime_type, size_bytes, is_image }`
//! - Local Axum HTTP API (`/health`, `/v1/whoami`, `/v1/onenote/*`, API key guard)
//! - Stdio MCP JSON-RPC 2.0 server initialization, tool discovery, and tool calls

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

mod native_copy {
    use super::*;
    use axum::{extract::Request, response::IntoResponse};
    use onenote_cli::graph::copy::CopyPageStatus;

    const SOURCE_XHTML: &str =
        "<html><head><title>Release note</title></head><body><p>Keep this.</p></body></html>";
    const SOURCE_LINK: &str = "https://onenote.example/source-page";

    #[derive(Clone, Copy, PartialEq, Eq)]
    enum CopyBehavior {
        Complete,
        Fail,
        GatewayError,
        RedirectSubmission,
        ForeignOperation,
        RedirectOperation,
        WrongDestination,
    }

    #[derive(Default)]
    struct CopyLog {
        requests: Vec<(String, String)>,
        polls: u32,
        copy_exists: bool,
    }

    async fn fixture(behavior: CopyBehavior) -> (GraphClient, Arc<Mutex<CopyLog>>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let base = format!("http://{}/v1.0", listener.local_addr().unwrap());
        let log = Arc::new(Mutex::new(CopyLog::default()));
        let app = Router::new().fallback({
            let base = base.clone();
            let log = log.clone();
            move |request: Request| {
                let base = base.clone();
                let log = log.clone();
                async move {
                    let method = request.method().as_str().to_owned();
                    let path = request.uri().path().to_owned();
                    assert_eq!(request.headers()["Authorization"], "Bearer e2e-test-token");
                    let body = axum::body::to_bytes(request.into_body(), 4096).await.unwrap();
                    let mut log = log.lock().unwrap();
                    log.requests.push((method.clone(), path.clone()));
                    match (method.as_str(), path.as_str()) {
                        ("POST", "/v1.0/me/onenote/pages/source/copyToSection") => {
                            assert_eq!(
                                serde_json::from_slice::<Value>(&body).unwrap(),
                                json!({"id":"target", "groupId":"team"})
                            );
                            match behavior {
                                CopyBehavior::GatewayError => StatusCode::BAD_GATEWAY.into_response(),
                                CopyBehavior::RedirectSubmission => (
                                    StatusCode::TEMPORARY_REDIRECT,
                                    [("Location", format!("{base}/unexpected"))],
                                ).into_response(),
                                CopyBehavior::ForeignOperation => (
                                    StatusCode::ACCEPTED,
                                    [("Operation-Location", "http://127.0.0.1:1/v1.0/me/onenote/operations/job"), ("Retry-After", "0")],
                                ).into_response(),
                                _ => (
                                    StatusCode::ACCEPTED,
                                    [("Operation-Location", "/v1.0/me/onenote/operations/job"), ("Retry-After", "0")],
                                ).into_response(),
                            }
                        }
                        ("GET", "/v1.0/me/onenote/operations/job") => {
                            log.polls += 1;
                            if behavior == CopyBehavior::RedirectOperation {
                                return (
                                    StatusCode::TEMPORARY_REDIRECT,
                                    [("Location", format!("{base}/unexpected"))],
                                ).into_response();
                            }
                            let payload = if log.polls == 1 {
                                json!({"id":"job","status":"Running"})
                            } else if behavior == CopyBehavior::Fail {
                                json!({"id":"job","status":"Failed","error":{"code":"copyDenied","message":"Permission denied"}})
                            } else {
                                log.copy_exists = true;
                                json!({
                                    "id":"job", "status":"Completed",
                                    "resourceLocation":format!("{base}/groups/team/onenote/pages/copy"),
                                    "resourceId":"copy"
                                })
                            };
                            ([("Retry-After", "0")], Json(payload)).into_response()
                        }
                        ("GET", "/v1.0/me/onenote/pages/source") => Json(json!({
                            "id":"source","title":"Release note","parentSection":{"id":"original"},
                            "links":{"oneNoteWebUrl":{"href":SOURCE_LINK}}
                        })).into_response(),
                        ("GET", "/v1.0/me/onenote/pages/source/content") => SOURCE_XHTML.into_response(),
                        ("GET", "/v1.0/groups/team/onenote/pages/copy") if log.copy_exists => {
                            let section = if behavior == CopyBehavior::WrongDestination {"other"} else {"target"};
                            Json(json!({"id":"copy","title":"Release note","parentSection":{"id":section}})).into_response()
                        }
                        ("GET", "/v1.0/groups/team/onenote/pages/copy/content") if log.copy_exists => SOURCE_XHTML.into_response(),
                        _ => StatusCode::NOT_FOUND.into_response(),
                    }
                }
            }
        });
        tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
        let dir = tempfile::tempdir().unwrap();
        let (_, graph, _) = make_test_runtime(base, dir.path());
        (graph, log)
    }

    #[tokio::test]
    async fn copy_resumes_and_preserves_source_link_title_and_body() {
        let (graph, log) = fixture(CopyBehavior::Complete).await;
        let source = OnenoteScope::Me;
        let destination = OnenoteScope::from_scope_key("group:team", None).unwrap();
        let original = graph
            .get_onenote_page_summary(&source, "source", None)
            .await
            .unwrap();
        let original_html = graph
            .get_onenote_page_content_xhtml(&source, "source")
            .await
            .unwrap();
        assert_eq!(original.web_url.as_deref(), Some(SOURCE_LINK));

        let pending = graph
            .copy_onenote_page("me|source", "group:team|target", None, 0)
            .await
            .unwrap();
        assert!(matches!(pending.status, CopyPageStatus::Pending));
        assert_eq!(pending.operation_id, "job");
        assert!(!pending.ok);
        assert_eq!(log.lock().unwrap().polls, 0);

        let copied = graph
            .copy_onenote_page(
                "me|source",
                "group:team|target",
                Some(&pending.operation_url),
                2,
            )
            .await
            .unwrap();
        assert!(matches!(copied.status, CopyPageStatus::Completed));
        assert!(copied.ok);
        assert_eq!(copied.source_page_id, "source");
        assert_eq!(copied.destination_section_id, "target");
        assert_eq!(copied.new_page_key.as_deref(), Some("group:team|copy"));
        assert_eq!(copied.new_page_id.as_deref(), Some("copy"));
        assert_eq!(copied.operation_url, pending.operation_url);

        let retained = graph
            .get_onenote_page_summary(&source, "source", None)
            .await
            .unwrap();
        let duplicate = graph
            .get_onenote_page_summary(&destination, "copy", None)
            .await
            .unwrap();
        assert_eq!(retained, original);
        assert_eq!(duplicate.title, original.title);
        assert_eq!(duplicate.section_id, "target");
        assert_eq!(
            graph
                .get_onenote_page_content_xhtml(&source, "source")
                .await
                .unwrap(),
            original_html
        );
        assert_eq!(
            graph
                .get_onenote_page_content_xhtml(&destination, "copy")
                .await
                .unwrap(),
            original_html
        );
        let log = log.lock().unwrap();
        assert_eq!(log.polls, 2);
        assert_eq!(
            log.requests
                .iter()
                .filter(|(method, _)| method != "GET")
                .count(),
            1
        );
    }

    #[tokio::test]
    async fn copy_reports_terminal_failure_and_verification_failure_with_handles() {
        for (behavior, expected, error) in [
            (
                CopyBehavior::Fail,
                "failed",
                "copyDenied: Permission denied",
            ),
            (
                CopyBehavior::WrongDestination,
                "verification_failed",
                "expected page copy in section target",
            ),
            (CopyBehavior::RedirectOperation, "status_unavailable", "307"),
        ] {
            let (graph, log) = fixture(behavior).await;
            let outcome = graph
                .copy_onenote_page("source", "group:team|target", None, 2)
                .await
                .unwrap();
            assert_eq!(serde_json::to_value(&outcome).unwrap()["status"], expected);
            assert!(!outcome.ok);
            assert_eq!(outcome.operation_id, "job");
            assert!(outcome.operation_url.ends_with("/operations/job"));
            assert!(
                outcome.error.as_deref().unwrap().contains(error),
                "{outcome:?}"
            );
            assert!(outcome.new_page_id.is_none());
            assert_eq!(
                graph
                    .get_onenote_page_content_xhtml(&OnenoteScope::Me, "source")
                    .await
                    .unwrap(),
                SOURCE_XHTML
            );
            let log = log.lock().unwrap();
            assert_eq!(
                log.requests
                    .iter()
                    .filter(|(method, _)| method != "GET")
                    .count(),
                1
            );
            assert!(!log
                .requests
                .iter()
                .any(|(_, path)| path.ends_with("/unexpected")));
        }
    }

    #[tokio::test]
    async fn copy_submission_does_not_replay_or_follow_foreign_and_redirected_locations() {
        for behavior in [
            CopyBehavior::GatewayError,
            CopyBehavior::RedirectSubmission,
            CopyBehavior::ForeignOperation,
        ] {
            let (graph, log) = fixture(behavior).await;
            let error = graph
                .copy_onenote_page("source", "group:team|target", None, 2)
                .await
                .unwrap_err();
            assert!(error
                .to_string()
                .contains("Check the destination before submitting another copy"));
            assert_eq!(log.lock().unwrap().requests.len(), 1);
        }

        let (graph, log) = fixture(CopyBehavior::Complete).await;
        let error = graph
            .copy_onenote_page(
                "source",
                "group:team|target",
                Some("http://127.0.0.1:1/v1.0/me/onenote/operations/job"),
                2,
            )
            .await
            .unwrap_err();
        assert!(error.to_string().contains("Untrusted Graph"));
        assert!(log.lock().unwrap().requests.is_empty());
    }
}

#[derive(Clone)]
struct MockGraphState {
    base_url: String,
    notebooks: Arc<Mutex<Vec<Value>>>,
    joined_teams: Arc<Mutex<Vec<Value>>>,
    group_notebooks: Arc<Mutex<std::collections::HashMap<String, Vec<Value>>>>,
    sections: Arc<Mutex<Vec<Value>>>,
    pages: Arc<Mutex<Vec<Value>>>,
    page_xhtml: Arc<Mutex<std::collections::HashMap<String, String>>>,
    patches_received: Arc<Mutex<Vec<String>>>,
    deleted_pages: Arc<Mutex<Vec<String>>>,
    notebook_list_hits: Arc<Mutex<usize>>,
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
        Some(0),
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
    let (base_url, mock, graph_server) = spawn_mock_graph().await;
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

    let original_pages = mock.pages.lock().unwrap().clone();
    let original_xhtml = mock.page_xhtml.lock().unwrap().clone();
    let unsupported = http
        .post(format!("http://{api_addr}/v1/onenote/pages"))
        .header("Authorization", "Bearer secret-key")
        .json(&json!({ "section": "me|sec-1", "title": "Subpage", "level": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(unsupported.status(), StatusCode::BAD_REQUEST);
    let error: Value = unsupported.json().await.unwrap();
    assert!(error["error"].as_str().unwrap().contains("top-level"));

    let unsupported = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({
            "jsonrpc": "2.0", "id": 10, "method": "tools/call",
            "params": { "name": "create_page", "arguments": {
                "section": "me|sec-1", "title": "Subpage", "level": 2
            }}
        }),
    )
    .await
    .unwrap();
    assert_eq!(unsupported["result"]["isError"], true);
    assert!(unsupported["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("top-level"));
    assert_eq!(*mock.pages.lock().unwrap(), original_pages);
    assert_eq!(*mock.page_xhtml.lock().unwrap(), original_xhtml);
    assert!(mock.patches_received.lock().unwrap().is_empty());

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
        "rename_section",
        "create_page",
        "append_page",
        "update_block",
        "update_page",
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

    let patch_start = mock.patches_received.lock().unwrap().len();
    for (requested, expected, writes) in [
        (Some(true), true, 1),
        (Some(true), true, 1),
        (Some(false), false, 2),
        (None, true, 3),
    ] {
        let mut arguments = json!({ "page": "me|page-1", "block_id": "p:{todo-1}" });
        if let Some(state) = requested {
            arguments["completed"] = json!(state);
        }
        let response = mcp::handle_request_value(
            &cfg,
            &graph,
            &storage,
            json!({
                "jsonrpc": "2.0", "id": 9, "method": "tools/call",
                "params": { "name": "toggle_todo", "arguments": arguments }
            }),
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["isError"], false, "{response}");
        let value = &response["result"]["structuredContent"];
        assert_eq!(value["completed"], expected);
        assert_eq!(
            value["page"]["body_markdown"]
                .as_str()
                .unwrap()
                .contains("- [x]"),
            expected
        );
        assert_eq!(
            mock.patches_received.lock().unwrap().len() - patch_start,
            writes
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

#[tokio::test]
async fn e2e_cli_default_zero_ttl_fetches_fresh_and_cached_flag_uses_cache() {
    let (base_url, mock_state, server) = spawn_mock_graph().await;
    let dir = tempfile::tempdir().unwrap();
    let (cfg, graph, storage) = make_test_runtime(base_url.clone(), dir.path());
    assert_eq!(cfg.cli_cache_ttl_secs, 0);

    // 1. Default invocation (cli_cache_ttl_secs = 0, refresh = false, cached = false) fetches 1 notebook
    let first = cli::load_onenote_notebooks_with_cache(&storage, &graph, false, false, false, false)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(*mock_state.notebook_list_hits.lock().unwrap(), 1);

    // 2. Add a 2nd personal notebook on MockGraph; next default call MUST fetch fresh without --refresh
    mock_state.notebooks.lock().unwrap().push(json!({
        "id": "nb-2",
        "displayName": "Product Specs",
        "isDefault": false,
        "isShared": false,
        "userRole": "Owner",
        "createdDateTime": "2026-11-19T00:00:00Z",
        "lastModifiedDateTime": "2026-11-19T10:00:00Z"
    }));
    let second =
        cli::load_onenote_notebooks_with_cache(&storage, &graph, false, false, false, false)
            .await
            .unwrap();
    assert_eq!(second.len(), 2);
    assert_eq!(*mock_state.notebook_list_hits.lock().unwrap(), 2);

    // 3. Add a 3rd notebook on MockGraph; --cached returns cached 2 immediately without hitting Graph
    mock_state.notebooks.lock().unwrap().push(json!({
        "id": "nb-3",
        "displayName": "Design Notes",
        "isDefault": false,
        "isShared": false,
        "userRole": "Owner"
    }));
    let from_cache =
        cli::load_onenote_notebooks_with_cache(&storage, &graph, false, false, false, true)
            .await
            .unwrap();
    assert_eq!(from_cache.len(), 2);
    assert_eq!(*mock_state.notebook_list_hits.lock().unwrap(), 2);

    // 4. --cached on an EMPTY cache falls back to Graph (unlike --offline which stays network-free)
    let empty_storage = Storage::open_in_memory().unwrap();
    let offline_empty = cli::load_onenote_notebooks_with_cache(
        &empty_storage,
        &graph,
        false,
        false,
        true,
        false,
    )
    .await
    .unwrap();
    assert!(offline_empty.is_empty());
    assert_eq!(*mock_state.notebook_list_hits.lock().unwrap(), 2);

    let cached_fallback = cli::load_onenote_notebooks_with_cache(
        &empty_storage,
        &graph,
        false,
        false,
        false,
        true,
    )
    .await
    .unwrap();
    assert_eq!(cached_fallback.len(), 3);
    assert_eq!(*mock_state.notebook_list_hits.lock().unwrap(), 3);

    // 5. Non-zero TTL serves personal cache, while --include-teams still fetches group notebooks!
    mock_state.joined_teams.lock().unwrap().push(json!({
        "id": "grp-1",
        "displayName": "Platform Team"
    }));
    mock_state.group_notebooks.lock().unwrap().insert(
        "grp-1".to_string(),
        vec![json!({
            "id": "nb-team-1",
            "displayName": "Platform Team Notebook",
            "isDefault": false,
            "isShared": true,
            "userRole": "Contributor"
        })],
    );

    let mut ttl_cfg = cfg.clone();
    ttl_cfg.cli_cache_ttl_secs = 300;
    let ttl_tokens = StoredTokens {
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
    let ttl_auth = AuthManager::from_tokens(ttl_cfg.clone(), ttl_tokens);
    let ttl_graph = GraphClient::new(ttl_cfg, ttl_auth)
        .unwrap()
        .with_my_user_id("user-1");
    let ttl_storage = Storage::open_in_memory().unwrap();

    // Fetch personal notebooks (populates personal cursor only)
    let personal_nbs =
        cli::load_onenote_notebooks(&ttl_storage, &ttl_graph, false, false, false)
            .await
            .unwrap();
    assert_eq!(personal_nbs.len(), 3);
    let hits_after_personal = *mock_state.notebook_list_hits.lock().unwrap();

    // Repeat personal call within TTL -> cache hit (no extra Graph request)
    let personal_cached =
        cli::load_onenote_notebooks(&ttl_storage, &ttl_graph, false, false, false)
            .await
            .unwrap();
    assert_eq!(personal_cached.len(), 3);
    assert_eq!(
        *mock_state.notebook_list_hits.lock().unwrap(),
        hits_after_personal
    );

    // Now request --include-teams -> must fetch group notebooks instead of returning personal-only cache
    let with_teams =
        cli::load_onenote_notebooks(&ttl_storage, &ttl_graph, true, false, false)
            .await
            .unwrap();
    assert_eq!(with_teams.len(), 4);
    assert!(with_teams.iter().any(|n| n.id == "nb-team-1"));
    assert_eq!(
        *mock_state.notebook_list_hits.lock().unwrap(),
        hits_after_personal + 1
    );

    server.abort();
}

#[tokio::test]
async fn e2e_bidirectional_sync_fixes_s1_s3_s4_s6_and_section4() {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Clone)]
    struct SyncFixState {
        fail_group_notebooks: Arc<AtomicBool>,
        notebooks: Arc<Mutex<Vec<Value>>>,
        joined_teams: Arc<Mutex<Vec<Value>>>,
        group_notebooks: Arc<Mutex<HashMap<String, Vec<Value>>>>,
        group_sections: Arc<Mutex<HashMap<String, Vec<Value>>>>,
        section_groups: Arc<Mutex<Vec<Value>>>,
        sections: Arc<Mutex<Vec<Value>>>,
        pages: Arc<Mutex<HashMap<String, Value>>>,
        page_xhtml: Arc<Mutex<HashMap<String, String>>>,
        patch_count: Arc<Mutex<usize>>,
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let mut pages_map = HashMap::new();
    pages_map.insert(
        "p-1".to_string(),
        json!({
            "id": "p-1",
            "title": "Spec v1",
            "level": 0,
            "order": 0,
            "lastModifiedDateTime": "2026-11-18T10:00:00Z",
            "parentSection": { "id": "sec-1", "displayName": "Specs" }
        }),
    );
    pages_map.insert(
        "p-2".to_string(),
        json!({
            "id": "p-2",
            "title": "Ghost Candidate",
            "level": 0,
            "order": 1,
            "lastModifiedDateTime": "2026-11-18T10:01:00Z",
            "parentSection": { "id": "sec-1", "displayName": "Specs" }
        }),
    );
    pages_map.insert(
        "p-team-1".to_string(),
        json!({
            "id": "p-team-1",
            "title": "Team Runbook",
            "level": 0,
            "order": 0,
            "lastModifiedDateTime": "2026-11-18T10:00:00Z",
            "parentSection": { "id": "sec-team-1", "displayName": "OnCall" }
        }),
    );

    let mut xhtml_map = HashMap::new();
    xhtml_map.insert(
        "p-1".to_string(),
        r#"<!DOCTYPE html><html><head><title>Spec v1</title></head><body><div id="div:{root}"><p id="p:{b1}">Alpha initial body text</p><p id="p:{todo1}" data-tag="to-do">Ship sync audit</p></div></body></html>"#
            .to_string(),
    );
    xhtml_map.insert(
        "p-2".to_string(),
        r#"<!DOCTYPE html><html><head><title>Ghost Candidate</title></head><body><div id="div:{r2}"><p id="p:{b2}">GhostBodyTokenXYZ</p></div></body></html>"#
            .to_string(),
    );
    xhtml_map.insert(
        "p-team-1".to_string(),
        r#"<!DOCTYPE html><html><head><title>Team Runbook</title></head><body><div id="div:{tr}"><p id="p:{tb1}">Team incident steps</p></div></body></html>"#
            .to_string(),
    );

    let mut group_nbs = HashMap::new();
    group_nbs.insert(
        "team-1".to_string(),
        vec![json!({
            "id": "nb-team-1",
            "displayName": "Team Runbooks",
            "isDefault": false,
            "isShared": true,
            "lastModifiedDateTime": "2026-11-18T10:00:00Z"
        })],
    );
    let mut group_secs = HashMap::new();
    group_secs.insert(
        "team-1".to_string(),
        vec![json!({
            "id": "sec-team-1",
            "displayName": "OnCall",
            "isDefault": false,
            "parentNotebook": { "id": "nb-team-1", "displayName": "Team Runbooks" },
            "lastModifiedDateTime": "2026-11-18T10:00:00Z"
        })],
    );

    let st = SyncFixState {
        fail_group_notebooks: Arc::new(AtomicBool::new(false)),
        notebooks: Arc::new(Mutex::new(vec![
            json!({
                "id": "nb-1",
                "displayName": "Engineering",
                "isDefault": true,
                "isShared": false,
                "lastModifiedDateTime": "2026-11-18T10:00:00Z"
            }),
            json!({
                "id": "nb-2",
                "displayName": "ArchiveNB",
                "isDefault": false,
                "isShared": false,
                "lastModifiedDateTime": "2026-11-18T10:00:00Z"
            }),
        ])),
        joined_teams: Arc::new(Mutex::new(vec![json!({
            "id": "team-1",
            "displayName": "Platform Team"
        })])),
        group_notebooks: Arc::new(Mutex::new(group_nbs)),
        group_sections: Arc::new(Mutex::new(group_secs)),
        section_groups: Arc::new(Mutex::new(vec![json!({
            "id": "sg-1",
            "displayName": "Q4 Group",
            "parentNotebook": { "id": "nb-1", "displayName": "Engineering" }
        })])),
        sections: Arc::new(Mutex::new(vec![
            json!({
                "id": "sec-1",
                "displayName": "Specs",
                "isDefault": true,
                "parentNotebook": { "id": "nb-1", "displayName": "Engineering" },
                "parentSectionGroup": { "id": "sg-1", "displayName": "Q4 Group" },
                "lastModifiedDateTime": "2026-11-18T10:00:00Z"
            }),
            json!({
                "id": "sec-2",
                "displayName": "DeprecatedSec",
                "isDefault": false,
                "parentNotebook": { "id": "nb-2", "displayName": "ArchiveNB" },
                "lastModifiedDateTime": "2026-11-18T10:00:00Z"
            }),
            json!({
                "id": "sec-3",
                "displayName": "TargetSec",
                "isDefault": false,
                "parentNotebook": { "id": "nb-1", "displayName": "Engineering" },
                "lastModifiedDateTime": "2026-11-18T10:00:00Z"
            }),
        ])),
        pages: Arc::new(Mutex::new(pages_map)),
        page_xhtml: Arc::new(Mutex::new(xhtml_map)),
        patch_count: Arc::new(Mutex::new(0)),
    };

    let app = Router::new()
        .route(
            "/v1.0/me/joinedTeams",
            get(|State(s): State<SyncFixState>| async move {
                Json(json!({ "value": s.joined_teams.lock().unwrap().clone() }))
            }),
        )
        .route(
            "/v1.0/me/onenote/notebooks",
            get(|State(s): State<SyncFixState>| async move {
                Json(json!({ "value": s.notebooks.lock().unwrap().clone() }))
            }),
        )
        .route(
            "/v1.0/groups/{gid}/onenote/notebooks",
            get(
                |State(s): State<SyncFixState>, AxumPath(gid): AxumPath<String>| async move {
                    if s.fail_group_notebooks.load(Ordering::SeqCst) {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": { "code": "InternalServerError" } })),
                        );
                    }
                    let items = s
                        .group_notebooks
                        .lock()
                        .unwrap()
                        .get(&gid)
                        .cloned()
                        .unwrap_or_default();
                    (StatusCode::OK, Json(json!({ "value": items })))
                },
            ),
        )
        .route(
            "/v1.0/sites/{sid}/onenote/notebooks",
            axum::routing::post(
                |AxumPath(sid): AxumPath<String>, Json(body): Json<Value>| async move {
                    let nb = json!({
                        "id": format!("nb-site-{sid}"),
                        "displayName": body["displayName"].as_str().unwrap_or("SiteNB"),
                        "isDefault": false,
                        "isShared": true,
                        "lastModifiedDateTime": "2026-11-19T12:00:00Z"
                    });
                    (StatusCode::CREATED, Json(nb))
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/sectionGroups",
            get(|State(s): State<SyncFixState>| async move {
                Json(json!({ "value": s.section_groups.lock().unwrap().clone() }))
            }),
        )
        .route(
            "/v1.0/groups/{gid}/onenote/sectionGroups",
            get(|| async { Json(json!({ "value": [] })) }),
        )
        .route(
            "/v1.0/me/onenote/sections",
            get(|State(s): State<SyncFixState>| async move {
                Json(json!({ "value": s.sections.lock().unwrap().clone() }))
            }),
        )
        .route(
            "/v1.0/groups/{gid}/onenote/sections",
            get(
                |State(s): State<SyncFixState>, AxumPath(gid): AxumPath<String>| async move {
                    let items = s
                        .group_sections
                        .lock()
                        .unwrap()
                        .get(&gid)
                        .cloned()
                        .unwrap_or_default();
                    Json(json!({ "value": items }))
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/sections/{sec_id}/pages",
            get(
                |State(s): State<SyncFixState>, AxumPath(sec_id): AxumPath<String>| async move {
                    let exists = s
                        .sections
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|sec| sec["id"].as_str() == Some(sec_id.as_str()));
                    if !exists {
                        return (
                            StatusCode::NOT_FOUND,
                            Json(json!({ "error": { "code": "ResourceNotFound", "message": "Section not found" } })),
                        );
                    }
                    let items: Vec<Value> = s
                        .pages
                        .lock()
                        .unwrap()
                        .values()
                        .filter(|p| p["parentSection"]["id"].as_str() == Some(sec_id.as_str()))
                        .cloned()
                        .collect();
                    (StatusCode::OK, Json(json!({ "value": items })))
                },
            ),
        )
        .route(
            "/v1.0/groups/{gid}/onenote/sections/{sec_id}/pages",
            get(
                |State(s): State<SyncFixState>, AxumPath((_gid, sec_id)): AxumPath<(String, String)>| async move {
                    let items: Vec<Value> = s
                        .pages
                        .lock()
                        .unwrap()
                        .values()
                        .filter(|p| p["parentSection"]["id"].as_str() == Some(sec_id.as_str()))
                        .cloned()
                        .collect();
                    Json(json!({ "value": items }))
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/pages/{pid}",
            get(
                |State(s): State<SyncFixState>, AxumPath(pid): AxumPath<String>| async move {
                    match s.pages.lock().unwrap().get(&pid).cloned() {
                        Some(p) => (StatusCode::OK, Json(p)),
                        None => (
                            StatusCode::NOT_FOUND,
                            Json(json!({ "error": { "code": "ResourceNotFound", "message": "Page not found" } })),
                        ),
                    }
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/pages/{pid}/content",
            get(
                |State(s): State<SyncFixState>, AxumPath(pid): AxumPath<String>| async move {
                    match s.page_xhtml.lock().unwrap().get(&pid).cloned() {
                        Some(x) => (StatusCode::OK, [("content-type", "text/html")], x),
                        None => (
                            StatusCode::NOT_FOUND,
                            [("content-type", "application/json")],
                            r#"{"error":{"code":"ResourceNotFound","message":"Page not found"}}"#.to_string(),
                        ),
                    }
                },
            )
            .patch(
                |State(s): State<SyncFixState>, AxumPath(pid): AxumPath<String>, body: Bytes| async move {
                    *s.patch_count.lock().unwrap() += 1;
                    let cmds: Vec<Value> = serde_json::from_slice(&body).unwrap_or_default();
                    for cmd in cmds {
                        let target = cmd["target"].as_str().unwrap_or("");
                        let content = cmd["content"].as_str().unwrap_or("");
                        if target == "title" {
                            if let Some(p) = s.pages.lock().unwrap().get_mut(&pid) {
                                let old_t = p["title"].as_str().unwrap_or("").to_string();
                                p["title"] = json!(content);
                                p["lastModifiedDateTime"] = json!("2026-11-20T12:11:00Z");
                                if let Some(x) = s.page_xhtml.lock().unwrap().get_mut(&pid) {
                                    *x = x.replace(&format!("<title>{old_t}</title>"), &format!("<title>{content}</title>"));
                                }
                            }
                        } else if target == "p:{b1}" {
                            if let Some(x) = s.page_xhtml.lock().unwrap().get_mut(&pid) {
                                if let Some(start) = x.find(r#"<p id="p:{b1}""#) {
                                    if let Some(rel_end) = x[start..].find("</p>") {
                                        let repl = content.replacen("<p>", r#"<p id="p:{b1}">"#, 1);
                                        x.replace_range(start..start + rel_end + 4, &repl);
                                    }
                                }
                            }
                            if let Some(p) = s.pages.lock().unwrap().get_mut(&pid) {
                                p["lastModifiedDateTime"] = json!("2026-11-20T12:12:00Z");
                            }
                        } else if target == "p:{todo1}" {
                            if let Some(x) = s.page_xhtml.lock().unwrap().get_mut(&pid) {
                                if let Some(start) = x.find(r#"<p id="p:{todo1}""#) {
                                    if let Some(rel_end) = x[start..].find("</p>") {
                                        x.replace_range(start..start + rel_end + 4, content);
                                    }
                                }
                            }
                            if let Some(p) = s.pages.lock().unwrap().get_mut(&pid) {
                                p["lastModifiedDateTime"] = json!("2026-11-20T12:13:00Z");
                            }
                        }
                    }
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let (cfg, graph, storage) = make_test_runtime(base_url, dir.path());

    // Initial bootstrap + read p-1 and p-2
    onenote_cli::sync::bootstrap_onenote(&graph, &storage)
        .await
        .unwrap();
    let _ = cli::load_onenote_page_detail(&storage, &graph, "me|p-1", false)
        .await
        .unwrap();
    let _ = cli::load_onenote_page_detail(&storage, &graph, "me|p-2", false)
        .await
        .unwrap();

    // S1: Remote edit title + lastModifiedDateTime + body -> direct read updates all fields + FTS5
    {
        let mut pm = st.pages.lock().unwrap();
        let p1 = pm.get_mut("p-1").unwrap();
        p1["title"] = json!("Spec v2 RemoteTitle");
        p1["lastModifiedDateTime"] = json!("2026-11-19T15:00:00Z");
        st.page_xhtml.lock().unwrap().insert(
            "p-1".to_string(),
            r#"<!DOCTYPE html><html><head><title>Spec v2 RemoteTitle</title></head><body><div id="div:{root}"><p id="p:{b1}">Beta remote content edited natively</p><p id="p:{todo1}" data-tag="to-do">Ship sync audit</p></div></body></html>"#
                .to_string(),
        );
    }
    let s1 = cli::load_onenote_page_detail(&storage, &graph, "me|p-1", false)
        .await
        .unwrap();
    assert_eq!(s1.summary.title, "Spec v2 RemoteTitle");
    assert_eq!(
        s1.summary.last_modified_at.as_deref(),
        Some("2026-11-19T15:00:00Z")
    );
    assert!(s1
        .summary
        .preview_text
        .as_deref()
        .is_some_and(|t| t.contains("Beta remote content")));
    assert_eq!(
        storage
            .search_onenote_pages("Beta remote content", None, 10)
            .unwrap()
            .len(),
        1
    );

    // S2: Local title update returns live title + timestamp and preserves FTS5 hits after sync
    let s2 = cli::update_onenote_page_helper(
        &storage,
        &graph,
        "me|p-1",
        Some("Spec v3 LocalRename"),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(s2.summary.title, "Spec v3 LocalRename");
    assert_eq!(
        s2.summary.last_modified_at.as_deref(),
        Some("2026-11-20T12:11:00Z")
    );
    onenote_cli::sync::bootstrap_onenote(&graph, &storage)
        .await
        .unwrap();
    assert_eq!(
        storage
            .search_onenote_pages("Beta remote content", None, 10)
            .unwrap()
            .len(),
        1,
        "FTS5 body hits must survive sync after local update"
    );

    // S3: Same-block remote edit blocks stale overwrite; different-block edit succeeds!
    let _ = cli::load_onenote_page_detail(&storage, &graph, "me|p-1", false)
        .await
        .unwrap();
    {
        let mut pm = st.pages.lock().unwrap();
        pm.get_mut("p-1").unwrap()["lastModifiedDateTime"] = json!("2026-11-20T09:00:00Z");
        st.page_xhtml.lock().unwrap().insert(
            "p-1".to_string(),
            r#"<!DOCTYPE html><html><head><title>Spec v3 LocalRename</title></head><body><div id="div:{root}"><p id="p:{b1}">CRITICAL REMOTE EDIT BY ALICE</p><p id="p:{todo1}" data-tag="to-do">Ship sync audit</p></div></body></html>"#
                .to_string(),
        );
    }
    let patches_before_s3 = *st.patch_count.lock().unwrap();
    let stale_err = cli::update_onenote_page_helper(
        &storage,
        &graph,
        "me|p-1",
        None,
        Some("Local stale edit"),
        Some("p:{b1}"),
    )
    .await
    .unwrap_err();
    assert!(stale_err.to_string().to_lowercase().contains("conflict"));
    assert_eq!(*st.patch_count.lock().unwrap(), patches_before_s3);
    assert!(st
        .page_xhtml
        .lock()
        .unwrap()
        .get("p-1")
        .unwrap()
        .contains("CRITICAL REMOTE EDIT BY ALICE"));

    // Different-paragraph concurrent edit (toggle p:{todo1} while p:{b1} changed remotely) MUST succeed!
    let (after_todo, _, completed) = cli::toggle_onenote_todo_helper_with_guard(
        &storage,
        &graph,
        "me|p-1",
        Some("p:{todo1}"),
        None,
        Some("- [ ] Ship sync audit"),
        false,
    )
    .await
    .unwrap();
    assert!(completed);
    assert!(after_todo.raw_xhtml.contains("CRITICAL REMOTE EDIT BY ALICE"));
    assert!(after_todo.raw_xhtml.contains("to-do:completed"));

    // S4: Remote deletions pruned by --refresh on sections, notebooks, pages, and 404 read
    st.notebooks
        .lock()
        .unwrap()
        .retain(|n| n["id"].as_str() != Some("nb-2"));
    st.sections
        .lock()
        .unwrap()
        .retain(|s| s["id"].as_str() != Some("sec-2"));
    st.pages.lock().unwrap().remove("p-2");
    st.page_xhtml.lock().unwrap().remove("p-2");

    let (_, secs_refreshed) = cli::load_onenote_sections(&storage, &graph, None, true, false)
        .await
        .unwrap();
    assert!(!secs_refreshed.iter().any(|s| s.id == "sec-2"));

    let nbs_refreshed = cli::load_onenote_notebooks(&storage, &graph, false, true, false)
        .await
        .unwrap();
    assert!(!nbs_refreshed.iter().any(|n| n.id == "nb-2"));
    assert!(!storage
        .list_onenote_notebooks()
        .unwrap()
        .iter()
        .any(|n| n.id == "nb-2"));

    let (_, pages_refreshed) =
        cli::list_onenote_pages_helper(&storage, &graph, "Engineering/Specs", 50, true, false)
            .await
            .unwrap();
    assert!(!pages_refreshed.iter().any(|p| p.id == "p-2"));
    assert!(storage
        .search_onenote_pages("GhostBodyTokenXYZ", None, 10)
        .unwrap()
        .is_empty());

    // S6: Partial group 500 failure preserves group:team-1 AND site:site-99 caches
    let site_nb = graph
        .create_onenote_notebook(
            &OnenoteScope::Site {
                site_id: "site-99".to_string(),
                site_name: None,
            },
            "SharePoint Site NB",
        )
        .await
        .unwrap();
    storage
        .upsert_onenote_notebooks(std::slice::from_ref(&site_nb))
        .unwrap();
    st.fail_group_notebooks.store(true, Ordering::SeqCst);
    onenote_cli::sync::bootstrap_onenote(&graph, &storage)
        .await
        .unwrap();
    st.fail_group_notebooks.store(false, Ordering::SeqCst);
    let nbs_after_s6 = storage.list_onenote_notebooks().unwrap();
    assert!(nbs_after_s6
        .iter()
        .any(|n| n.notebook_key == "site:site-99|nb-site-site-99"));
    assert!(nbs_after_s6
        .iter()
        .any(|n| n.notebook_key == "group:team-1|nb-team-1"));
    assert!(!storage
        .list_onenote_pages("group:team-1|sec-team-1")
        .unwrap()
        .is_empty());

    // Section 4.1 & 4.2: Section moved out of sectionGroup, Page moved across sections,
    // and Recreated section with same name ("TargetSec": sec-3 -> sec-3-new)
    {
        let mut secs = st.sections.lock().unwrap();
        for s in secs.iter_mut() {
            if s["id"].as_str() == Some("sec-1") {
                s.as_object_mut().unwrap().remove("parentSectionGroup");
            }
        }
        secs.retain(|s| s["id"].as_str() != Some("sec-3"));
        secs.push(json!({
            "id": "sec-3-new",
            "displayName": "TargetSec",
            "isDefault": false,
            "parentNotebook": { "id": "nb-1", "displayName": "Engineering" },
            "lastModifiedDateTime": "2026-11-20T10:30:00Z"
        }));
        let mut pm = st.pages.lock().unwrap();
        let p1 = pm.get_mut("p-1").unwrap();
        p1["parentSection"] = json!({ "id": "sec-3-new", "displayName": "TargetSec" });
    }
    // Resolve recreated section by name with refresh=true -> must find sec-3-new and return p-1
    let (resolved_sec, moved_pages) =
        cli::list_onenote_pages_helper(&storage, &graph, "TargetSec", 50, true, false)
            .await
            .unwrap();
    assert_eq!(resolved_sec.id, "sec-3-new");
    assert_eq!(moved_pages.len(), 1);
    assert_eq!(moved_pages[0].id, "p-1");

    // Verify get_onenote_page_detail reflects moved section_id/section_name
    let cached_detail = storage.get_onenote_page_detail("me|p-1").unwrap().unwrap();
    assert_eq!(cached_detail.summary.section_id, "sec-3-new");
    assert_eq!(
        cached_detail.summary.section_name.as_deref(),
        Some("TargetSec")
    );

    // Verify sec-1 parent_section_group_id was cleared to None on hierarchy sync
    onenote_cli::sync::bootstrap_onenote(&graph, &storage)
        .await
        .unwrap();
    let sec1 = storage
        .list_onenote_sections(None)
        .unwrap()
        .into_iter()
        .find(|s| s.id == "sec-1")
        .unwrap();
    assert_eq!(sec1.parent_section_group_id, None);
    assert_eq!(sec1.parent_section_group_name, None);

    // HTTP 409 Conflict & MCP conflict verification
    let serve_router = serve::build_router(Arc::new(ServeState {
        config: cfg.clone(),
        graph: graph.clone(),
        storage: storage.clone(),
        api_key: Some("k".to_string()),
    }));
    let serve_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let serve_addr = serve_listener.local_addr().unwrap();
    let serve_handle = tokio::spawn(async move {
        let _ = axum::serve(serve_listener, serve_router).await;
    });

    let http = reqwest::Client::new();
    let conflict_resp = http
        .patch(format!("http://{serve_addr}/v1/onenote/pages/me%7Cp-1"))
        .header("Authorization", "Bearer k")
        .json(&json!({
            "block_id": "p:{b1}",
            "markdown": "HTTP stale overwrite",
            "expected_content": "Old stale baseline text"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict_resp.status(), reqwest::StatusCode::CONFLICT);

    let mcp_conflict = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": {
                "name": "update_block",
                "arguments": {
                    "page": "me|p-1",
                    "block_id": "p:{b1}",
                    "markdown": "MCP stale overwrite",
                    "expected_content": "Old stale baseline text"
                }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_conflict["result"]["isError"], true);
    assert!(mcp_conflict["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_lowercase()
        .contains("conflict"));

    // Matching/new baseline preflight succeeds on HTTP (200) and MCP (isError: false)
    let ok_http = http
        .patch(format!("http://{serve_addr}/v1/onenote/pages/me%7Cp-1"))
        .header("Authorization", "Bearer k")
        .json(&json!({
            "block_id": "p:{b1}",
            "markdown": "HTTP fresh preflight edit",
            "expected_content": "CRITICAL REMOTE EDIT BY ALICE"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok_http.status(), reqwest::StatusCode::OK);

    let ok_mcp = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "tools/call",
            "params": {
                "name": "update_block",
                "arguments": {
                    "page": "me|p-1",
                    "block_id": "p:{b1}",
                    "markdown": "MCP fresh preflight edit",
                    "expected_content": "HTTP fresh preflight edit"
                }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(ok_mcp["result"]["isError"], false);
    assert!(ok_mcp["result"]["structuredContent"]["page"]["body_plain"]
        .as_str()
        .unwrap_or("")
        .contains("MCP fresh preflight edit"));

    serve_handle.abort();
    server.abort();
}

#[tokio::test]
async fn e2e_non_404_with_404_text_preserves_cached_page_while_404_and_410_evict() {
    use std::sync::atomic::{AtomicU16, Ordering};

    let status_code = Arc::new(AtomicU16::new(200));
    let sc_meta = status_code.clone();
    let sc_content = status_code.clone();

    let app = Router::new()
        .route(
            "/v1.0/me/onenote/pages/p-keep",
            get(move || {
                let sc = sc_meta.clone();
                async move {
                    match sc.load(Ordering::SeqCst) {
                        200 => (
                            StatusCode::OK,
                            Json(json!({
                                "id": "p-keep",
                                "title": "Preserved Page",
                                "level": 0,
                                "order": 0,
                                "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                                "parentSection": { "id": "sec-1", "displayName": "Specs" }
                            })),
                        ),
                        503 => (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({
                                "error": {
                                    "code": "ResourceNotFound",
                                    "message": "transient gateway 404 backend shard not found"
                                }
                            })),
                        ),
                        410 => (
                            StatusCode::GONE,
                            Json(json!({
                                "error": { "code": "Gone", "message": "Page permanently removed" }
                            })),
                        ),
                        _ => (
                            StatusCode::NOT_FOUND,
                            Json(json!({
                                "error": { "code": "ResourceNotFound", "message": "Page not found" }
                            })),
                        ),
                    }
                }
            }),
        )
        .route(
            "/v1.0/me/onenote/pages/p-keep/content",
            get(move || {
                let sc = sc_content.clone();
                async move {
                    match sc.load(Ordering::SeqCst) {
                        200 => (
                            StatusCode::OK,
                            [("Retry-After", "0")],
                            r#"<!DOCTYPE html><html><head><title>Preserved Page</title></head><body><div id="div:1"><p id="p:1">PreserveCacheToken999</p></div></body></html>"#,
                        ),
                        503 => (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("Retry-After", "0")],
                            r#"{"error":{"code":"ResourceNotFound","message":"transient gateway 404 backend shard not found"}}"#,
                        ),
                        410 => (
                            StatusCode::GONE,
                            [("Retry-After", "0")],
                            r#"{"error":{"code":"Gone","message":"Page permanently removed"}}"#,
                        ),
                        _ => (
                            StatusCode::NOT_FOUND,
                            [("Retry-After", "0")],
                            r#"{"error":{"code":"ResourceNotFound","message":"Page not found"}}"#,
                        ),
                    }
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let (_cfg, graph, storage) = make_test_runtime(format!("http://{addr}/v1.0"), dir.path());

    // 1. Seed cache via 200 OK read
    let detail = cli::load_onenote_page_detail(&storage, &graph, "me|p-keep", false)
        .await
        .unwrap();
    assert_eq!(detail.summary.title, "Preserved Page");
    assert_eq!(
        storage
            .search_onenote_pages("PreserveCacheToken999", None, 10)
            .unwrap()
            .len(),
        1
    );

    // 2. Non-404 (503) whose error body mentions "404" / "ResourceNotFound" / "not found"
    //    must fail the request WITHOUT deleting the cached page or its FTS5 entry.
    status_code.store(503, Ordering::SeqCst);
    let err_503 = cli::load_onenote_page_detail(&storage, &graph, "me|p-keep", false)
        .await
        .unwrap_err();
    assert!(err_503.to_string().contains("503"));
    assert!(
        storage.get_onenote_page_detail("me|p-keep").unwrap().is_some(),
        "503 with 404/not-found text must preserve cached page detail"
    );
    assert_eq!(
        storage
            .search_onenote_pages("PreserveCacheToken999", None, 10)
            .unwrap()
            .len(),
        1,
        "503 with 404/not-found text must preserve FTS5 search hit"
    );

    // 3. Authoritative HTTP 410 Gone evicts the cached page & FTS5 entry
    status_code.store(410, Ordering::SeqCst);
    let _ = cli::load_onenote_page_detail(&storage, &graph, "me|p-keep", false)
        .await
        .unwrap_err();
    assert!(storage
        .get_onenote_page_detail("me|p-keep")
        .unwrap()
        .is_none());
    assert!(storage
        .search_onenote_pages("PreserveCacheToken999", None, 10)
        .unwrap()
        .is_empty());

    srv.abort();
}

mod cli_feedback_regression {
    use super::*;
    use std::collections::HashMap;
    use tokio::process::Command;

    #[derive(Clone, Debug)]
    struct RecordedWrite {
        method: String,
        path: String,
        body: String,
    }

    #[derive(Clone)]
    struct CliMockState {
        base_url: String,
        sections: Arc<Mutex<HashMap<String, Value>>>,
        pages: Arc<Mutex<HashMap<String, Value>>>,
        page_xhtml: Arc<Mutex<HashMap<String, String>>>,
        writes: Arc<Mutex<Vec<RecordedWrite>>>,
    }

    impl CliMockState {
        fn new(base_url: String) -> Self {
            let mut sections = HashMap::new();
            sections.insert(
                "sec-1".to_string(),
                json!({
                    "id": "sec-1",
                    "displayName": "Specs",
                    "isDefault": true,
                    "parentNotebook": { "id": "nb-1", "displayName": "Engineering" },
                    "lastModifiedDateTime": "2026-11-20T09:00:00Z"
                }),
            );

            let mut pages = HashMap::new();
            let mut page_xhtml = HashMap::new();
            let seed_pages = [
                (
                    "page-hyphen",
                    "Hyphen Page",
                    r#"<!DOCTYPE html><html><head><title>Hyphen Page</title></head><body><div id="div:root"><p id="p:test">- old</p></div></body></html>"#,
                ),
                (
                    "page-single",
                    "Single Region",
                    r#"<!DOCTYPE html><html><head><title>Single Region</title></head><body><div id="div:single"><p id="p:only">- 기존 항목</p></div></body></html>"#,
                ),
                (
                    "page-unique",
                    "Unique Layout",
                    r#"<!DOCTYPE html><html><head><title>Unique Layout</title></head><body><div id="div:left" style="position:absolute;left:0"><p id="p:test"><a href="https://example.invalid">ref</a> <b>alpha</b> beta</p></div><div id="div:right" style="position:absolute;left:100px"><p id="p:side">untouched</p></div></body></html>"#,
                ),
                (
                    "page-both",
                    "Duplicate Layout",
                    r#"<!DOCTYPE html><html><head><title>Duplicate Layout</title></head><body><div id="div:left" style="position:absolute;left:0"><p id="p:test">alpha beta / alpha <b>beta</b></p></div><div id="div:right" style="position:absolute;left:100px"><p id="p:side">untouched</p></div></body></html>"#,
                ),
                (
                    "page-split",
                    "Split Layout",
                    r#"<!DOCTYPE html><html><head><title>Split Layout</title></head><body><div id="div:left" style="position:absolute;left:0"><p id="p:test">alpha <b>beta</b></p></div><div id="div:right" style="position:absolute;left:100px"><p id="p:side">untouched</p></div></body></html>"#,
                ),
                (
                    "page-boundaries",
                    "Boundaries Layout",
                    r#"<!DOCTYPE html><html><head><title>Boundaries Layout</title></head><body><div id="div:left" style="position:absolute;left:0"><p id="p:br">alpha beta / alpha<br />beta</p><table id="table:dup"><tr><td>alpha beta</td><td>alpha <b>beta</b></td></tr></table></div><div id="div:right" style="position:absolute;left:100px"><p id="p:side">untouched</p></div></body></html>"#,
                ),
                (
                    "page-multi",
                    "Multi Region",
                    r#"<!DOCTYPE html><html><head><title>Multi Region</title></head><body><div id="div:left" style="position:absolute;left:0"><p id="p:status"><span style="color:#008800">READY</span> <a href="https://example.invalid">Evidence</a></p></div><div id="div:right" style="position:absolute;left:100px"><p id="p:side">untouched</p></div></body></html>"#,
                ),
            ];
            for (id, title, xhtml) in seed_pages {
                pages.insert(
                    id.to_string(),
                    json!({
                        "id": id,
                        "title": title,
                        "level": 0,
                        "order": 0,
                        "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                        "contentUrl": format!("{base_url}/me/onenote/pages/{id}/content"),
                        "parentSection": { "id": "sec-1", "displayName": "Specs" }
                    }),
                );
                page_xhtml.insert(id.to_string(), xhtml.to_string());
            }

            Self {
                base_url,
                sections: Arc::new(Mutex::new(sections)),
                pages: Arc::new(Mutex::new(pages)),
                page_xhtml: Arc::new(Mutex::new(page_xhtml)),
                writes: Arc::new(Mutex::new(Vec::new())),
            }
        }
    }

    fn apply_mock_patch(html: &mut String, title_out: &mut Option<String>, cmd: &Value) {
        let target = cmd["target"].as_str().unwrap_or("");
        let action = cmd["action"].as_str().unwrap_or("");
        let content = cmd["content"].as_str().unwrap_or("");
        if action == "append" {
            if let Some(pos) = html.rfind("</div>") {
                html.insert_str(pos, content);
            } else if let Some(pos) = html.rfind("</body>") {
                html.insert_str(pos, content);
            }
            return;
        }
        if action == "replace" && target == "title" {
            *title_out = Some(content.to_string());
            if let (Some(s), Some(e)) = (html.find("<title>"), html.find("</title>")) {
                html.replace_range(s..e + "</title>".len(), &format!("<title>{content}</title>"));
            }
            return;
        }
        if action == "replace" {
            let needle = format!(r#"id="{target}""#);
            if let Some(id_pos) = html.find(&needle) {
                if let Some(open_lt) = html[..id_pos].rfind('<') {
                    let tag_name: String = html[open_lt + 1..]
                        .chars()
                        .take_while(|c| c.is_ascii_alphanumeric())
                        .collect();
                    let close_tag = format!("</{tag_name}>");
                    if let Some(rel_end) = html[open_lt..].find(&close_tag) {
                        html.replace_range(open_lt..open_lt + rel_end + close_tag.len(), content);
                    }
                }
            }
        }
    }

    async fn spawn_cli_mock_graph() -> (String, CliMockState, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");
        let state = CliMockState::new(base_url.clone());

        let app = Router::new()
            .route(
                "/v1.0/me",
                get(|| async {
                    Json(json!({
                        "id": "user-1",
                        "displayName": "Alice Engineer",
                        "mail": "alice@contoso.com"
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/notebooks",
                get(|| async {
                    Json(json!({
                        "value": [{
                            "id": "nb-1",
                            "displayName": "Engineering",
                            "isDefault": true,
                            "isShared": false
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups",
                get(|| async { Json(json!({ "value": [] })) }),
            )
            .route(
                "/v1.0/me/onenote/notebooks/{nb_id}/sectionGroups",
                get(|| async { Json(json!({ "value": [] })) }),
            )
            .route(
                "/v1.0/me/onenote/sections",
                get(|State(st): State<CliMockState>| async move {
                    let items: Vec<Value> = st.sections.lock().unwrap().values().cloned().collect();
                    Json(json!({ "value": items }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sections/{sec_id}",
                get(
                    |State(st): State<CliMockState>, AxumPath(sec_id): AxumPath<String>| async move {
                        match st.sections.lock().unwrap().get(&sec_id).cloned() {
                            Some(sec) => (StatusCode::OK, Json(sec)),
                            None => (StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "ResourceNotFound" } }))),
                        }
                    },
                )
                .patch(
                    |State(st): State<CliMockState>,
                     AxumPath(sec_id): AxumPath<String>,
                     body: Bytes| async move {
                        let raw = String::from_utf8_lossy(&body).into_owned();
                        st.writes.lock().unwrap().push(RecordedWrite {
                            method: "PATCH".to_string(),
                            path: format!("/v1.0/me/onenote/sections/{sec_id}"),
                            body: raw.clone(),
                        });
                        if let Ok(parsed) = serde_json::from_str::<Value>(&raw) {
                            if let Some(name) = parsed["displayName"].as_str() {
                                if let Some(sec) = st.sections.lock().unwrap().get_mut(&sec_id) {
                                    sec["displayName"] = json!(name);
                                }
                            }
                        }
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .route(
                "/v1.0/me/onenote/sections/{sec_id}/pages",
                get(|State(st): State<CliMockState>| async move {
                    let items: Vec<Value> = st.pages.lock().unwrap().values().cloned().collect();
                    Json(json!({ "value": items }))
                })
                .post(
                    |State(st): State<CliMockState>,
                     AxumPath(sec_id): AxumPath<String>,
                     body: Bytes| async move {
                        let raw = String::from_utf8_lossy(&body).into_owned();
                        st.writes.lock().unwrap().push(RecordedWrite {
                            method: "POST".to_string(),
                            path: format!("/v1.0/me/onenote/sections/{sec_id}/pages"),
                            body: raw.clone(),
                        });
                        let title = raw
                            .split("<title>")
                            .nth(1)
                            .and_then(|s| s.split("</title>").next())
                            .unwrap_or("Untitled")
                            .to_string();
                        let id = format!("page-created-{}", st.pages.lock().unwrap().len() + 1);
                        let created = json!({
                            "id": id,
                            "title": title,
                            "level": 0,
                            "order": 1,
                            "lastModifiedDateTime": "2026-11-21T01:00:00Z",
                            "contentUrl": format!("{}/me/onenote/pages/{id}/content", st.base_url),
                            "parentSection": { "id": sec_id, "displayName": "Specs" }
                        });
                        let stored_xhtml = raw
                            .replacen("<body>", r#"<body><div id="div:created">"#, 1)
                            .replacen("</body>", "</div></body>", 1);
                        st.pages.lock().unwrap().insert(id.clone(), created.clone());
                        st.page_xhtml.lock().unwrap().insert(id, stored_xhtml);
                        (StatusCode::CREATED, Json(created))
                    },
                ),
            )
            .route(
                "/v1.0/me/onenote/pages/{page_id}",
                get(
                    |State(st): State<CliMockState>, AxumPath(page_id): AxumPath<String>| async move {
                        match st.pages.lock().unwrap().get(&page_id).cloned() {
                            Some(p) => (StatusCode::OK, Json(p)),
                            None => (StatusCode::NOT_FOUND, Json(json!({ "error": { "code": "ResourceNotFound" } }))),
                        }
                    },
                ),
            )
            .route(
                "/v1.0/me/onenote/pages/{page_id}/content",
                get(
                    |State(st): State<CliMockState>, AxumPath(page_id): AxumPath<String>| async move {
                        match st.page_xhtml.lock().unwrap().get(&page_id).cloned() {
                            Some(x) => (StatusCode::OK, [("content-type", "text/html")], x),
                            None => (StatusCode::NOT_FOUND, [("content-type", "text/plain")], "Not found".to_string()),
                        }
                    },
                )
                .patch(
                    |State(st): State<CliMockState>,
                     AxumPath(page_id): AxumPath<String>,
                     body: Bytes| async move {
                        let raw = String::from_utf8_lossy(&body).into_owned();
                        st.writes.lock().unwrap().push(RecordedWrite {
                            method: "PATCH".to_string(),
                            path: format!("/v1.0/me/onenote/pages/{page_id}/content"),
                            body: raw.clone(),
                        });
                        if let Ok(cmds) = serde_json::from_str::<Vec<Value>>(&raw) {
                            let mut new_title = None;
                            if let Some(html) = st.page_xhtml.lock().unwrap().get_mut(&page_id) {
                                for cmd in &cmds {
                                    apply_mock_patch(html, &mut new_title, cmd);
                                }
                            }
                            if let Some(title) = new_title {
                                if let Some(page) = st.pages.lock().unwrap().get_mut(&page_id) {
                                    page["title"] = json!(title);
                                }
                            }
                        }
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(state.clone());

        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (base_url, state, handle)
    }

    async fn run_onenote_bin(
        base_url: &str,
        cfg_dir: &std::path::Path,
        data_dir: &std::path::Path,
        args: &[&str],
    ) -> std::process::Output {
        Command::new(env!("CARGO_BIN_EXE_onenote"))
            .args(args)
            .env("ONENOTE_GRAPH_BASE_URL", base_url)
            .env("ONENOTE_CONFIG_DIR", cfg_dir)
            .env("ONENOTE_DATA_DIR", data_dir)
            .env("ONENOTE_ACCESS_TOKEN", "e2e-cli-mock-token")
            .env("ONENOTE_DISABLE_KEYRING", "1")
            .env("ONENOTE_SHARE_M365_AUTH", "0")
            .env("ONENOTE_AUTO_DAEMON", "0")
            .env_remove("ONENOTE_TOKEN_FILE")
            .env_remove("ONENOTE_TOKEN_REFRESH_CMD")
            .env_remove("TEAMS_TOKEN_FILE")
            .env_remove("TEAMS_ACCESS_TOKEN")
            .output()
            .await
            .expect("spawn built onenote binary")
    }

    #[tokio::test]
    async fn e2e_real_cli_hyphen_inputs_and_payloads() {
        let (base_url, state, server) = spawn_cli_mock_graph().await;
        let tmp = tempfile::tempdir().unwrap();
        let cfg_dir = tmp.path().join("cfg");
        let data_dir = tmp.path().join("data");

        // 1. `create` with hyphenated --title and hyphenated two-line positional Markdown list
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &[
                "create",
                "me|sec-1",
                "--title",
                "- 목록 항목",
                "- first\n- second",
                "--json",
            ],
        )
        .await;
        assert!(
            out.status.success(),
            "create failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let created_json: Value = serde_json::from_slice(&out.stdout).unwrap();
        let created_key = created_json["page"]["page_key"].as_str().unwrap().to_string();
        {
            let writes = state.writes.lock().unwrap();
            assert_eq!(writes.len(), 1);
            assert_eq!(writes[0].method, "POST");
            assert_eq!(writes[0].path, "/v1.0/me/onenote/sections/sec-1/pages");
            assert!(writes[0].body.contains("<title>- 목록 항목</title>"));
            assert!(writes[0].body.contains("<ul><li>first</li><li>second</li></ul>"));
        }

        // 2. `append` with hyphenated two-line Korean list
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &["append", &created_key, "- 첫째 항목\n- 둘째 항목", "--json"],
        )
        .await;
        assert!(
            out.status.success(),
            "append failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        {
            let writes = state.writes.lock().unwrap();
            assert_eq!(writes.len(), 2);
            assert_eq!(writes[1].method, "PATCH");
            let cmds: Vec<Value> = serde_json::from_str(&writes[1].body).unwrap();
            assert_eq!(cmds.len(), 1);
            assert_eq!(cmds[0]["action"], "append");
            assert_eq!(cmds[0]["content"], "<ul><li>첫째 항목</li><li>둘째 항목</li></ul>");
        }

        // 3. `update --block-id --replace-text --with-text` with hyphen values
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &[
                "update",
                "me|page-hyphen",
                "--block-id",
                "p:test",
                "--replace-text",
                "- old",
                "--with-text",
                "- 목록 항목",
                "--json",
            ],
        )
        .await;
        assert!(
            out.status.success(),
            "update replace-text failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        {
            let writes = state.writes.lock().unwrap();
            assert_eq!(writes.len(), 3);
            assert_eq!(writes[2].path, "/v1.0/me/onenote/pages/page-hyphen/content");
            let cmds: Vec<Value> = serde_json::from_str(&writes[2].body).unwrap();
            assert_eq!(cmds.len(), 1);
            assert_eq!(cmds[0]["target"], "p:test");
            assert!(cmds[0]["content"].as_str().unwrap().contains("- 목록 항목"));
        }

        // 4. `update --body` + `--expected-content` with hyphen values
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &[
                "update",
                "me|page-single",
                "--body",
                "- 목록 항목",
                "--expected-content",
                "- 기존 항목",
                "--json",
            ],
        )
        .await;
        assert!(
            out.status.success(),
            "update --body --expected-content failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(state.writes.lock().unwrap().len(), 4);

        // 5. `rename-section` with hyphenated --name and explicit `--name=--dry-run`
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &["rename-section", "me|sec-1", "--name", "- 목록 항목", "--json"],
        )
        .await;
        assert!(
            out.status.success(),
            "rename-section failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        {
            let writes = state.writes.lock().unwrap();
            assert_eq!(writes.len(), 5);
            assert_eq!(writes[4].path, "/v1.0/me/onenote/sections/sec-1");
            assert_eq!(
                serde_json::from_str::<Value>(&writes[4].body).unwrap(),
                json!({ "displayName": "- 목록 항목" })
            );
        }

        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &["rename-section", "me|sec-1", "--name=--dry-run", "--json"],
        )
        .await;
        assert!(
            out.status.success(),
            "rename-section --name=--dry-run failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        {
            let writes = state.writes.lock().unwrap();
            assert_eq!(writes.len(), 6);
            assert_eq!(
                serde_json::from_str::<Value>(&writes[5].body).unwrap(),
                json!({ "displayName": "--dry-run" })
            );
        }

        // 6. Positional `--` passes flag-looking body literally
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &["append", "me|page-hyphen", "--", "--dry-run"],
        )
        .await;
        assert!(
            out.status.success(),
            "append -- --dry-run failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        {
            let writes = state.writes.lock().unwrap();
            assert_eq!(writes.len(), 7);
            let cmds: Vec<Value> = serde_json::from_str(&writes[6].body).unwrap();
            assert_eq!(cmds[0]["content"], "<p>--dry-run</p>");
        }

        // 7. `search` with hyphenated Korean query hits cached created page
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &["search", "- 목록 항목", "--json"],
        )
        .await;
        assert!(
            out.status.success(),
            "search failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let search_json: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(search_json["query"], "- 목록 항목");
        assert!(!search_json["hits"].as_array().unwrap().is_empty());

        // 8. Missing values, unknown flags, and --help produce 0 writes
        let writes_before = state.writes.lock().unwrap().len();
        for bad_args in [
            &["update", "me|page-hyphen", "--body", "--json"][..],
            &["update", "me|page-hyphen", "--body", "--dry-run"][..],
            &["update", "me|page-hyphen", "--body", "--help"][..],
            &["append", "me|page-hyphen", "--dry-run"][..],
            &["create", "me|sec-1", "--title", "--json"][..],
            &["rename-section", "me|sec-1", "--name", "--json"][..],
            &["search", "--json"][..],
        ] {
            let out = run_onenote_bin(&base_url, &cfg_dir, &data_dir, bad_args).await;
            assert!(
                !out.status.success(),
                "expected failure for {bad_args:?}, got stdout={}",
                String::from_utf8_lossy(&out.stdout)
            );
        }
        for help_args in [
            &["create", "me|sec-1", "--title", "- 목록 항목", "--help"][..],
            &["append", "me|page-hyphen", "- 목록 항목", "--help"][..],
            &["update", "me|page-hyphen", "--body", "- 목록 항목", "--help"][..],
            &["update", "--help"][..],
            &["search", "- 목록 항목", "--help"][..],
        ] {
            let out = run_onenote_bin(&base_url, &cfg_dir, &data_dir, help_args).await;
            assert!(
                out.status.success(),
                "expected help exit 0 for {help_args:?}, stderr={}",
                String::from_utf8_lossy(&out.stderr)
            );
            assert!(String::from_utf8_lossy(&out.stdout).contains("Usage:"));
        }
        assert_eq!(state.writes.lock().unwrap().len(), writes_before);

        server.abort();
    }

    #[tokio::test]
    async fn e2e_real_cli_block_replace_visible_duplicates_and_layout() {
        let (base_url, state, server) = spawn_cli_mock_graph().await;
        let tmp = tempfile::tempdir().unwrap();
        let cfg_dir = tmp.path().join("cfg");
        let data_dir = tmp.path().join("data");

        // 1. Unique phrase: 1 PATCH targeting p:test, formatting & side panel preserved
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &[
                "update",
                "me|page-unique",
                "--block-id",
                "p:test",
                "--replace-text",
                "alpha",
                "--with-text",
                "NEW",
                "--json",
            ],
        )
        .await;
        assert!(
            out.status.success(),
            "unique replace failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(state.writes.lock().unwrap().len(), 1);
        {
            let xhtml = state.page_xhtml.lock().unwrap()["page-unique"].clone();
            assert!(xhtml.contains(r#"<a href="https://example.invalid">ref</a> <b>NEW</b> beta"#));
            assert!(xhtml.contains(r#"<div id="div:right" style="position:absolute;left:100px"><p id="p:side">untouched</p></div>"#));
        }

        // 2. Unchanged input (no-op): exit 0, 0 writes
        let writes_before = state.writes.lock().unwrap().len();
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &[
                "update",
                "me|page-unique",
                "--block-id",
                "p:test",
                "--replace-text",
                "NEW",
                "--with-text",
                "NEW",
                "--json",
            ],
        )
        .await;
        assert!(out.status.success());
        assert_eq!(state.writes.lock().unwrap().len(), writes_before);

        // 3. Duplicate where second match crosses <b>: fails with 0 writes
        let before_both_html = state.page_xhtml.lock().unwrap()["page-both"].clone();
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &[
                "update",
                "me|page-both",
                "--block-id",
                "p:test",
                "--replace-text",
                "alpha beta",
                "--with-text",
                "NEW",
                "--json",
            ],
        )
        .await;
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("ambiguous"));
        assert_eq!(state.writes.lock().unwrap().len(), writes_before);
        assert_eq!(state.page_xhtml.lock().unwrap()["page-both"], before_both_html);

        // 4. Title change + ambiguous replacement together: fails with 0 writes (title NOT saved!)
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &[
                "update",
                "me|page-both",
                "--title",
                "Title Must Not Save",
                "--block-id",
                "p:test",
                "--replace-text",
                "alpha beta",
                "--with-text",
                "NEW",
                "--json",
            ],
        )
        .await;
        assert!(!out.status.success());
        assert_eq!(state.writes.lock().unwrap().len(), writes_before);
        assert_eq!(state.pages.lock().unwrap()["page-both"]["title"], "Duplicate Layout");
        assert_eq!(state.page_xhtml.lock().unwrap()["page-both"], before_both_html);

        // 5. Single phrase spanning formatting boundary: fails with 0 writes
        let before_split_html = state.page_xhtml.lock().unwrap()["page-split"].clone();
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &[
                "update",
                "me|page-split",
                "--block-id",
                "p:test",
                "--replace-text",
                "alpha beta",
                "--with-text",
                "NEW",
                "--json",
            ],
        )
        .await;
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("single text node"));
        assert_eq!(state.writes.lock().unwrap().len(), writes_before);
        assert_eq!(state.page_xhtml.lock().unwrap()["page-split"], before_split_html);

        // 6. Boundaries: <br /> does not merge lines into duplicate; table duplicate across cells fails
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &[
                "update",
                "me|page-boundaries",
                "--block-id",
                "p:br",
                "--replace-text",
                "alpha beta",
                "--with-text",
                "NEW",
                "--json",
            ],
        )
        .await;
        assert!(
            out.status.success(),
            "line-break boundary unique replace failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(state.writes.lock().unwrap().len(), writes_before + 1);

        let writes_after_br = state.writes.lock().unwrap().len();
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &[
                "update",
                "me|page-boundaries",
                "--block-id",
                "table:dup",
                "--replace-text",
                "alpha beta",
                "--with-text",
                "NEW",
                "--json",
            ],
        )
        .await;
        assert!(!out.status.success());
        assert!(String::from_utf8_lossy(&out.stderr).contains("ambiguous"));
        assert_eq!(state.writes.lock().unwrap().len(), writes_after_br);

        // 7. Full-page edit regression (distinct from --block-id --replace-text):
        //    aligned multi-region text edit succeeds in 1 PATCH preserving side region;
        //    flattening multi-region edit with --title fails with 0 writes.
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &[
                "update",
                "me|page-multi",
                "--body",
                "APPROVED [Evidence](https://example.invalid)\n\nuntouched",
                "--json",
            ],
        )
        .await;
        assert!(
            out.status.success(),
            "full-page aligned multi-region edit failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        assert_eq!(state.writes.lock().unwrap().len(), writes_after_br + 1);
        {
            let xhtml = state.page_xhtml.lock().unwrap()["page-multi"].clone();
            assert!(xhtml.contains(r#"<span style="color:#008800">APPROVED</span>"#));
            assert!(xhtml.contains(r#"<div id="div:right" style="position:absolute;left:100px"><p id="p:side">untouched</p></div>"#));
        }

        let writes_before_flatten = state.writes.lock().unwrap().len();
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &[
                "update",
                "me|page-multi",
                "--title",
                "Flatten Must Not Save",
                "--body",
                "Flattened replacement",
                "--json",
            ],
        )
        .await;
        assert!(!out.status.success());
        assert_eq!(state.writes.lock().unwrap().len(), writes_before_flatten);
        assert_eq!(state.pages.lock().unwrap()["page-multi"]["title"], "Multi Region");

        // 8. Implicit full-page update on page-both keeps prior exact-node alignment on p:test
        //    even though explicit --block-id p:test --replace-text "alpha beta" was rejected above.
        let writes_before_implicit_both = state.writes.lock().unwrap().len();
        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &[
                "update",
                "me|page-both",
                "--body",
                "NEW / alpha **beta**\n\nuntouched",
                "--json",
            ],
        )
        .await;
        assert!(
            out.status.success(),
            "implicit full-page edit on page-both failed: stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        {
            let writes = state.writes.lock().unwrap();
            assert_eq!(writes.len(), writes_before_implicit_both + 1);
            let cmds: Vec<Value> = serde_json::from_str(&writes.last().unwrap().body).unwrap();
            assert_eq!(cmds.len(), 1);
            assert_eq!(cmds[0]["target"], "p:test");
        }
        {
            let xhtml = state.page_xhtml.lock().unwrap()["page-both"].clone();
            assert!(xhtml.contains(r#"<p id="p:test">NEW / alpha <b>beta</b></p>"#));
            assert!(xhtml.contains(r#"<div id="div:right" style="position:absolute;left:100px"><p id="p:side">untouched</p></div>"#));
        }

        server.abort();
    }
}
