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
    /// Failure injection: statuses answered (first in, first out) by the next
    /// `POST …/sections/{id}/pages` requests before the mock creates a page.
    page_post_failures: Arc<Mutex<std::collections::VecDeque<u16>>>,
    page_post_hits: Arc<Mutex<usize>>,
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
            page_post_failures: Arc::new(Mutex::new(std::collections::VecDeque::new())),
            page_post_hits: Arc::new(Mutex::new(0)),
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
        let mut arguments =
            json!({ "page": "me|page-1", "block_id": "p:{todo-1}", "compact": false });
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

    // W1 regression: retrying the same implicit-baseline update must keep failing with Conflict
    // and never overwrite remote edits until the caller explicitly re-reads (or passes --force).
    let stale_err_retry = cli::update_onenote_page_helper_with_guard(
        &storage,
        &graph,
        "me|p-1",
        None,
        Some("Local stale edit"),
        Some("p:{b1}"),
        None,
        false,
    )
    .await
    .unwrap_err();
    assert!(stale_err_retry.to_string().to_lowercase().contains("conflict"));
    assert_eq!(*st.patch_count.lock().unwrap(), patches_before_s3);
    assert!(st
        .page_xhtml
        .lock()
        .unwrap()
        .get("p-1")
        .unwrap()
        .contains("CRITICAL REMOTE EDIT BY ALICE"));

    // Same implicit-baseline protection for `check` (`change_onenote_todo_helper_with_guard`):
    // edit p:{todo1} remotely while keeping the earlier cached baseline, verify 2x Conflict + 0 PATCH.
    let saved_xhtml_before_todo_conflict = st.page_xhtml.lock().unwrap().get("p-1").unwrap().clone();
    st.page_xhtml.lock().unwrap().insert(
        "p-1".to_string(),
        r#"<!DOCTYPE html><html><head><title>Spec v3 LocalRename</title></head><body><div id="div:{root}"><p id="p:{b1}">CRITICAL REMOTE EDIT BY ALICE</p><p id="p:{todo1}" data-tag="to-do">Ship sync audit (REMOTE EDIT)</p></div></body></html>"#
            .to_string(),
    );
    for attempt in 1..=2 {
        let todo_conflict = cli::change_onenote_todo_helper_with_guard(
            &storage,
            &graph,
            "me|p-1",
            Some("p:{todo1}"),
            None,
            Some(true),
            None,
            false,
        )
        .await
        .unwrap_err();
        assert!(
            todo_conflict.to_string().to_lowercase().contains("conflict"),
            "attempt {attempt}: {todo_conflict}"
        );
        assert_eq!(
            *st.patch_count.lock().unwrap(),
            patches_before_s3,
            "attempt {attempt} must not send PATCH"
        );
    }
    // Restore remote p:{todo1} to the original text for the control group below, then verify
    // re-reading allows the update on p:{b1} to proceed after the control group.
    st.page_xhtml
        .lock()
        .unwrap()
        .insert("p-1".to_string(), saved_xhtml_before_todo_conflict);

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

    // After an explicit re-read refreshes the cache baseline, the same update succeeds.
    // Re-stage remote change on p:{b1}, confirm 2x Conflict, then re-read and succeed.
    st.page_xhtml.lock().unwrap().insert(
        "p-1".to_string(),
        r#"<!DOCTYPE html><html><head><title>Spec v3 LocalRename</title></head><body><div id="div:{root}"><p id="p:{b1}">REMOTE V2 BY ALICE</p><p id="p:{todo1}" data-tag="to-do:completed">Ship sync audit</p></div></body></html>"#
            .to_string(),
    );
    let patches_before_reread_check = *st.patch_count.lock().unwrap();
    for attempt in 1..=2 {
        let err = cli::update_onenote_page_helper_with_guard(
            &storage,
            &graph,
            "me|p-1",
            None,
            Some("Local edit after reread"),
            Some("p:{b1}"),
            None,
            false,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("conflict"),
            "attempt {attempt}: {err}"
        );
        assert_eq!(*st.patch_count.lock().unwrap(), patches_before_reread_check);
    }
    let _ = cli::load_onenote_page_detail(&storage, &graph, "me|p-1", false)
        .await
        .unwrap();
    let after_reread = cli::update_onenote_page_helper_with_guard(
        &storage,
        &graph,
        "me|p-1",
        None,
        Some("CRITICAL REMOTE EDIT BY ALICE"),
        Some("p:{b1}"),
        None,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        *st.patch_count.lock().unwrap(),
        patches_before_reread_check + 1
    );
    assert!(after_reread
        .raw_xhtml
        .contains("CRITICAL REMOTE EDIT BY ALICE"));

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
                    "expected_content": "HTTP fresh preflight edit",
                    "compact": false
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
            .route(
                "/v1.0/me/onenote/pages/{page_id}/copyToSection",
                axum::routing::post(
                    |State(st): State<CliMockState>| async move {
                        let op_url = format!("{}/me/onenote/operations/op-pending-1", st.base_url);
                        (
                            StatusCode::ACCEPTED,
                            [("Operation-Location", op_url)],
                            "",
                        )
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
    async fn e2e_real_cli_copy_page_pending_emits_single_json_object() {
        let (base_url, _state, server) = spawn_cli_mock_graph().await;
        let tmp = tempfile::tempdir().unwrap();
        let cfg_dir = tmp.path().join("cfg");
        let data_dir = tmp.path().join("data");

        let out = run_onenote_bin(
            &base_url,
            &cfg_dir,
            &data_dir,
            &[
                "copy-page",
                "me|page-hyphen",
                "--section",
                "me|sec-1",
                "--wait-seconds",
                "0",
                "--json",
            ],
        )
        .await;
        assert_eq!(out.status.code(), Some(1));
        let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "copy-page pending must emit a single JSON object ({e}), got: {}",
                String::from_utf8_lossy(&out.stdout)
            )
        });
        assert_eq!(v["ok"], false);
        assert_eq!(v["status"], "pending");
        assert_eq!(v["operation_id"], "op-pending-1");
        assert!(v["operation_url"]
            .as_str()
            .unwrap()
            .ends_with("/me/onenote/operations/op-pending-1"));
        assert_eq!(v["error"]["code"], "copy_pending");
        assert!(v["error"]["message"]
            .as_str()
            .unwrap()
            .contains("--operation"));
        assert!(!out.stderr.is_empty());

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

mod confirmed_bug_regressions {
    use super::*;
    use axum::{extract::Request, response::IntoResponse};

    #[tokio::test]
    async fn regression_onenote_bug_08_ambiguous_page_cli_and_http() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");
        let requests: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let deleted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let reqs_clone = requests.clone();
        let del_clone = deleted.clone();
        let base_clone = base_url.clone();

        let app = Router::new().fallback(move |req: Request| {
            let reqs = reqs_clone.clone();
            let dels = del_clone.clone();
            let base = base_clone.clone();
            async move {
                let method = req.method().as_str().to_string();
                let path = req.uri().path().to_string();
                reqs.lock().unwrap().push((method.clone(), path.clone()));

                match (method.as_str(), path.as_str()) {
                    ("GET", "/v1.0/me/onenote/notebooks") => Json(json!({
                        "value": [{
                            "id": "nb-1",
                            "displayName": "Engineering",
                            "isDefault": true,
                            "isShared": false
                        }]
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/onenote/notebooks/nb-1/sectionGroups")
                    | ("GET", "/v1.0/me/onenote/sectionGroups") => {
                        Json(json!({ "value": [] })).into_response()
                    }
                    ("GET", "/v1.0/me/onenote/sections")
                    | ("GET", "/v1.0/me/onenote/notebooks/nb-1/sections") => Json(json!({
                        "value": [
                            {
                                "id": "sec-alpha",
                                "displayName": "Alpha",
                                "isDefault": true,
                                "parentNotebook": { "id": "nb-1", "displayName": "Engineering" }
                            },
                            {
                                "id": "sec-beta",
                                "displayName": "Beta",
                                "isDefault": false,
                                "parentNotebook": { "id": "nb-1", "displayName": "Engineering" }
                            }
                        ]
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/onenote/sections/sec-alpha/pages") => Json(json!({
                        "value": [
                            {
                                "id": "page-standup-a",
                                "title": "Standup notes",
                                "level": 0,
                                "order": 0,
                                "contentUrl": format!("{base}/me/onenote/pages/page-standup-a/content"),
                                "parentSection": { "id": "sec-alpha", "displayName": "Alpha" }
                            },
                            {
                                "id": "page-unique",
                                "title": "Quarterly Roadmap",
                                "level": 0,
                                "order": 1,
                                "contentUrl": format!("{base}/me/onenote/pages/page-unique/content"),
                                "parentSection": { "id": "sec-alpha", "displayName": "Alpha" }
                            }
                        ]
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/onenote/sections/sec-beta/pages") => Json(json!({
                        "value": [
                            {
                                "id": "page-standup-b",
                                "title": "Standup notes",
                                "level": 0,
                                "order": 0,
                                "contentUrl": format!("{base}/me/onenote/pages/page-standup-b/content"),
                                "parentSection": { "id": "sec-beta", "displayName": "Beta" }
                            }
                        ]
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/onenote/pages/page-standup-a") => Json(json!({
                        "id": "page-standup-a",
                        "title": "Standup notes",
                        "parentSection": { "id": "sec-alpha", "displayName": "Alpha" }
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/onenote/pages/page-standup-a/content") => (
                        StatusCode::OK,
                        [("content-type", "text/html")],
                        r#"<!DOCTYPE html><html><head><title>Standup notes</title></head><body><div id="div:a"><p id="p:a">Alpha standup body</p></div></body></html>"#,
                    )
                        .into_response(),
                    ("GET", "/v1.0/me/onenote/pages/page-standup-b") => Json(json!({
                        "id": "page-standup-b",
                        "title": "Standup notes",
                        "parentSection": { "id": "sec-beta", "displayName": "Beta" }
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/onenote/pages/page-standup-b/content") => (
                        StatusCode::OK,
                        [("content-type", "text/html")],
                        r#"<!DOCTYPE html><html><head><title>Standup notes</title></head><body><div id="div:b"><p id="p:b">Beta standup body</p></div></body></html>"#,
                    )
                        .into_response(),
                    ("GET", "/v1.0/me/onenote/pages/page-unique") => Json(json!({
                        "id": "page-unique",
                        "title": "Quarterly Roadmap",
                        "parentSection": { "id": "sec-alpha", "displayName": "Alpha" }
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/onenote/pages/page-unique/content") => (
                        StatusCode::OK,
                        [("content-type", "text/html")],
                        r#"<!DOCTYPE html><html><head><title>Quarterly Roadmap</title></head><body><div id="div:u"><p id="p:u">Roadmap body</p></div></body></html>"#,
                    )
                        .into_response(),
                    ("GET", "/v1.0/me/onenote/pages/page-remote-only") => Json(json!({
                        "id": "page-remote-only",
                        "title": "Remote Only Note",
                        "parentSection": { "id": "sec-beta", "displayName": "Beta" }
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/onenote/pages/page-remote-only/content") => (
                        StatusCode::OK,
                        [("content-type", "text/html")],
                        r#"<!DOCTYPE html><html><head><title>Remote Only Note</title></head><body><div id="div:r"><p id="p:r">Remote only body</p></div></body></html>"#,
                    )
                        .into_response(),
                    ("DELETE", p) if p.starts_with("/v1.0/me/onenote/pages/") => {
                        let id = p.trim_start_matches("/v1.0/me/onenote/pages/").to_string();
                        dels.lock().unwrap().push(id);
                        StatusCode::NO_CONTENT.into_response()
                    }
                    ("PATCH", p) if p.ends_with("/content") => StatusCode::NO_CONTENT.into_response(),
                    _ => (
                        StatusCode::NOT_FOUND,
                        Json(json!({ "error": { "code": "ResourceNotFound", "message": format!("Not found: {path}") } })),
                    )
                        .into_response(),
                }
            }
        });
        let graph_server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempfile::tempdir().unwrap();
        let (cfg, graph, storage) = make_test_runtime(base_url, dir.path());

        // Populate cache with both sections and hydrate both duplicate pages
        cli::load_onenote_notebooks(&storage, &graph, false, true, false)
            .await
            .unwrap();
        cli::load_onenote_sections(&storage, &graph, Some("Engineering"), true, false)
            .await
            .unwrap();
        cli::list_onenote_pages_helper(&storage, &graph, "Alpha", 20, true, false)
            .await
            .unwrap();
        cli::list_onenote_pages_helper(&storage, &graph, "Beta", 20, true, false)
            .await
            .unwrap();
        cli::load_onenote_page_detail(&storage, &graph, "me|page-standup-a", false)
            .await
            .unwrap();
        cli::load_onenote_page_detail(&storage, &graph, "me|page-standup-b", false)
            .await
            .unwrap();

        // Start local HTTP API (`onenote serve`) sharing the same runtime
        let state = Arc::new(ServeState {
            config: cfg.clone(),
            graph: graph.clone(),
            storage: storage.clone(),
            api_key: None,
        });
        let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_addr = api_listener.local_addr().unwrap();
        let api_server = tokio::spawn(async move {
            let _ = axum::serve(api_listener, serve::build_router(state)).await;
        });
        let http = reqwest::Client::new();

        requests.lock().unwrap().clear();

        let assert_ambiguous_msg = |msg: &str, ctx: &str| {
            assert!(
                msg.to_ascii_lowercase().contains("ambiguous"),
                "{ctx}: expected ambiguity error, got: {msg}"
            );
            assert!(
                msg.contains("Standup notes")
                    && msg.contains("Alpha")
                    && msg.contains("Beta")
                    && msg.contains("me|page-standup-a")
                    && msg.contains("me|page-standup-b"),
                "{ctx}: error must include title, sections, and both scoped page keys, got: {msg}"
            );
        };

        for query in ["Standup notes", "Standup"] {
            // CLI helpers: read (online), read (offline), append, update, delete
            let err = cli::load_onenote_page_detail(&storage, &graph, query, false)
                .await
                .unwrap_err();
            assert_ambiguous_msg(&err.to_string(), &format!("read online '{query}'"));

            let err = cli::load_onenote_page_detail(&storage, &graph, query, true)
                .await
                .unwrap_err();
            assert_ambiguous_msg(&err.to_string(), &format!("read offline '{query}'"));

            let err = cli::append_onenote_page_helper(&storage, &graph, query, "Extra", &[])
                .await
                .unwrap_err();
            assert_ambiguous_msg(&err.to_string(), &format!("append '{query}'"));

            let err = cli::update_onenote_page_helper(
                &storage,
                &graph,
                query,
                Some("Renamed"),
                None,
                None,
            )
            .await
            .unwrap_err();
            assert_ambiguous_msg(&err.to_string(), &format!("update '{query}'"));

            let err = cli::delete_onenote_page_helper(&storage, &graph, query)
                .await
                .unwrap_err();
            assert_ambiguous_msg(&err.to_string(), &format!("delete '{query}'"));

            // HTTP API: GET, GET?offline=true, PATCH, POST /append, DELETE
            let encoded = query.replace(' ', "%20");
            let r_get = http
                .get(format!("http://{api_addr}/v1/onenote/pages/{encoded}"))
                .send()
                .await
                .unwrap();
            assert_eq!(r_get.status(), StatusCode::BAD_REQUEST, "HTTP GET '{query}'");
            let v_get: Value = r_get.json().await.unwrap();
            assert_ambiguous_msg(
                v_get["error"].as_str().unwrap_or(""),
                &format!("HTTP GET '{query}'"),
            );

            let r_off = http
                .get(format!("http://{api_addr}/v1/onenote/pages/{encoded}?offline=true"))
                .send()
                .await
                .unwrap();
            assert_eq!(r_off.status(), StatusCode::BAD_REQUEST, "HTTP GET offline '{query}'");
            let v_off: Value = r_off.json().await.unwrap();
            assert_ambiguous_msg(
                v_off["error"].as_str().unwrap_or(""),
                &format!("HTTP GET offline '{query}'"),
            );

            let r_del = http
                .delete(format!("http://{api_addr}/v1/onenote/pages/{encoded}"))
                .send()
                .await
                .unwrap();
            assert_eq!(r_del.status(), StatusCode::BAD_REQUEST, "HTTP DELETE '{query}'");
            let v_del: Value = r_del.json().await.unwrap();
            assert_ambiguous_msg(
                v_del["error"].as_str().unwrap_or(""),
                &format!("HTTP DELETE '{query}'"),
            );
        }

        // Zero Graph requests must have been sent for any ambiguous query
        assert!(
            requests.lock().unwrap().is_empty(),
            "ambiguous queries must send 0 Graph requests, got: {:?}",
            *requests.lock().unwrap()
        );
        assert!(deleted.lock().unwrap().is_empty());

        // Regression controls:
        // 1. Scoped key selects only the specified duplicate page
        let detail_a = cli::load_onenote_page_detail(&storage, &graph, "me|page-standup-a", false)
            .await
            .unwrap();
        assert_eq!(detail_a.summary.page_key, "me|page-standup-a");
        assert!(detail_a.body_plain.contains("Alpha standup body"));

        let detail_b = cli::load_onenote_page_detail(&storage, &graph, "me|page-standup-b", true)
            .await
            .unwrap();
        assert_eq!(detail_b.summary.page_key, "me|page-standup-b");
        assert!(detail_b.body_plain.contains("Beta standup body"));

        // 2. Unique title & unique substring continue working
        let by_title = cli::load_onenote_page_detail(&storage, &graph, "Quarterly Roadmap", false)
            .await
            .unwrap();
        assert_eq!(by_title.summary.page_key, "me|page-unique");

        let by_sub = cli::load_onenote_page_detail(&storage, &graph, "Roadmap", false)
            .await
            .unwrap();
        assert_eq!(by_sub.summary.page_key, "me|page-unique");

        // 3. Unseen real remote ID still uses online fallback
        let remote_only =
            cli::load_onenote_page_detail(&storage, &graph, "page-remote-only", false)
                .await
                .unwrap();
        assert_eq!(remote_only.summary.page_key, "me|page-remote-only");
        assert!(remote_only.body_plain.contains("Remote only body"));

        // 4. Deleting via explicit scoped key deletes only that page
        let del_key = cli::delete_onenote_page_helper(&storage, &graph, "me|page-standup-a")
            .await
            .unwrap();
        assert_eq!(del_key, "me|page-standup-a");
        assert_eq!(*deleted.lock().unwrap(), vec!["page-standup-a".to_string()]);

        api_server.abort();
        graph_server.abort();
    }

    #[tokio::test]
    async fn regression_onenote_bug_01_full_body_repeated_chars_and_failure_safety() {
        use scraper::{Html, Selector};

        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");

        let mut initial_xhtml = std::collections::HashMap::new();
        initial_xhtml.insert(
            "page-abs".to_string(),
            r#"<!DOCTYPE html><html><head><title>Abs Page</title></head><body><div id="div:abs" style="position:absolute;left:48px;top:120px"><p id="p:agenda" data-id="agenda-1" style="font-size:12pt">Meeting agenda</p><p id="p:score" data-id="score-1" style="color:#333333">Total 1000 vs 2000: 40 points</p><p id="p:sib" data-id="sib-1">Sibling untouched</p></div><div id="div:side" style="position:absolute;left:500px;top:120px"><p id="p:side">Side region</p></div></body></html>"#.to_string(),
        );
        initial_xhtml.insert(
            "page-plain".to_string(),
            r#"<!DOCTYPE html><html><head><title>Plain Page</title></head><body><div id="div:plain"><p id="p:agenda" data-id="agenda-2" style="font-size:12pt">Meeting agenda</p><p id="p:score" data-id="score-2" style="color:#333333">Total 1000 vs 2000: 40 points</p><p id="p:sib" data-id="sib-2">Sibling untouched</p></div></body></html>"#.to_string(),
        );
        initial_xhtml.insert(
            "page-ambig-plain".to_string(),
            r#"<!DOCTYPE html><html><head><title>Ambig Plain</title></head><body><div id="div:plain-amb"><p id="p:fmt" data-id="fmt-1" style="color:#111111"><b>alpha</b> alpha</p><p id="p:keep" data-id="keep-1">Keep sibling</p></div></body></html>"#.to_string(),
        );
        initial_xhtml.insert(
            "page-ambig-abs".to_string(),
            r#"<!DOCTYPE html><html><head><title>Ambig Abs</title></head><body><div id="div:abs-amb" style="position:absolute;left:48px"><p id="p:fmt" data-id="fmt-2" style="color:#111111"><b>alpha</b> alpha</p><p id="p:keep" data-id="keep-2">Keep sibling</p></div></body></html>"#.to_string(),
        );
        initial_xhtml.insert(
            "page-ctrl".to_string(),
            r#"<!DOCTYPE html><html><head><title>Control Title</title></head><body><div id="div:ctrl" style="position:absolute;left:48px"><p id="p:status" data-id="st-1" style="color:green">Status READY</p></div></body></html>"#.to_string(),
        );

        let page_xhtml = Arc::new(Mutex::new(initial_xhtml));
        let patches: Arc<Mutex<Vec<(String, Vec<Value>)>>> = Arc::new(Mutex::new(Vec::new()));
        let xhtml_clone = page_xhtml.clone();
        let patches_clone = patches.clone();

        let app = Router::new().fallback(move |req: Request| {
            let xhtml_map = xhtml_clone.clone();
            let patch_log = patches_clone.clone();
            async move {
                let method = req.method().as_str().to_string();
                let path = req.uri().path().to_string();
                let body = axum::body::to_bytes(req.into_body(), 1024 * 1024)
                    .await
                    .unwrap();
                if method == "GET" && path.starts_with("/v1.0/me/onenote/pages/") {
                    let rest = path.trim_start_matches("/v1.0/me/onenote/pages/");
                    if let Some(id) = rest.strip_suffix("/content") {
                        if let Some(html) = xhtml_map.lock().unwrap().get(id).cloned() {
                            return (StatusCode::OK, [("content-type", "text/html")], html)
                                .into_response();
                        }
                    } else if let Some(html) = xhtml_map.lock().unwrap().get(rest).cloned() {
                        let title = html
                            .split("<title>")
                            .nth(1)
                            .and_then(|s| s.split("</title>").next())
                            .unwrap_or("Page")
                            .to_string();
                        return Json(json!({
                            "id": rest,
                            "title": title,
                            "parentSection": { "id": "sec-1", "displayName": "Specs" }
                        }))
                        .into_response();
                    }
                } else if method == "PATCH" && path.ends_with("/content") {
                    let id = path
                        .trim_start_matches("/v1.0/me/onenote/pages/")
                        .trim_end_matches("/content")
                        .to_string();
                    let cmds: Vec<Value> = serde_json::from_slice(&body).unwrap();
                    patch_log.lock().unwrap().push((id.clone(), cmds.clone()));
                    if let Some(html) = xhtml_map.lock().unwrap().get_mut(&id) {
                        for cmd in &cmds {
                            let target = cmd["target"].as_str().unwrap_or("");
                            let action = cmd["action"].as_str().unwrap_or("");
                            let content = cmd["content"].as_str().unwrap_or("");
                            if action == "replace" && target == "title" {
                                if let (Some(s), Some(e)) =
                                    (html.find("<title>"), html.find("</title>"))
                                {
                                    html.replace_range(
                                        s..e + "</title>".len(),
                                        &format!("<title>{content}</title>"),
                                    );
                                }
                            } else if action == "replace" {
                                let needle = format!(r#"id="{target}""#);
                                if let Some(id_pos) = html.find(&needle) {
                                    if let Some(open_lt) = html[..id_pos].rfind('<') {
                                        let tag: String = html[open_lt + 1..]
                                            .chars()
                                            .take_while(|c| c.is_ascii_alphanumeric())
                                            .collect();
                                        let close_tag = format!("</{tag}>");
                                        if let Some(rel_end) = html[open_lt..].find(&close_tag) {
                                            html.replace_range(
                                                open_lt..open_lt + rel_end + close_tag.len(),
                                                content,
                                            );
                                        }
                                    }
                                }
                            }
                        }
                    }
                    return StatusCode::NO_CONTENT.into_response();
                }
                StatusCode::NOT_FOUND.into_response()
            }
        });
        let graph_server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempfile::tempdir().unwrap();
        let (_cfg, graph, storage) = make_test_runtime(base_url, dir.path());

        let assert_p_dom = |html: &str,
                            expected_id: &str,
                            expected_data_id: &str,
                            expected_style: &str,
                            expected_text: &str| {
            let frag = Html::parse_fragment(html);
            let p_sel = Selector::parse("p").unwrap();
            let p = frag.select(&p_sel).next().expect("patch must contain <p>");
            assert_eq!(p.value().attr("id"), Some(expected_id));
            assert_eq!(p.value().attr("data-id"), Some(expected_data_id));
            assert_eq!(p.value().attr("style"), Some(expected_style));
            let text: String = p.text().collect();
            assert_eq!(text, expected_text);
        };

        // 1 & 2. Both position:absolute container and plain div:
        //    - "Meeting agenda" -> "Meeting agendas"
        //    - "40" -> "45" in sentence containing repeated '0's
        for (page_id, agenda_did, score_did, sib_did) in [
            ("page-abs", "agenda-1", "score-1", "sib-1"),
            ("page-plain", "agenda-2", "score-2", "sib-2"),
        ] {
            let key = format!("me|{page_id}");
            let before = cli::load_onenote_page_detail(&storage, &graph, &key, false)
                .await
                .unwrap();

            // Edit 1: "Meeting agenda" -> "Meeting agendas"
            let md1 = before
                .body_markdown
                .replace("Meeting agenda", "Meeting agendas");
            let count_before = patches.lock().unwrap().len();
            let after1 = cli::update_onenote_page_helper(&storage, &graph, &key, None, Some(&md1), None)
                .await
                .unwrap_or_else(|e| panic!("{page_id} agenda->agendas failed: {e}"));
            {
                let log = patches.lock().unwrap();
                assert_eq!(log.len(), count_before + 1, "{page_id} expected 1 PATCH");
                let (pid, cmds) = log.last().unwrap();
                assert_eq!(pid, page_id);
                assert_eq!(cmds.len(), 1, "{page_id} expected 1 command");
                assert_eq!(cmds[0]["target"], "p:agenda", "{page_id} must target p:agenda, not parent div");
                assert_eq!(cmds[0]["action"], "replace");
                assert_p_dom(
                    cmds[0]["content"].as_str().unwrap(),
                    "p:agenda",
                    agenda_did,
                    "font-size:12pt",
                    "Meeting agendas",
                );
            }

            // Edit 2: "40" -> "45" in sentence with repeated 0s
            let md2 = after1.body_markdown.replace("40 points", "45 points");
            let count_before = patches.lock().unwrap().len();
            let after2 = cli::update_onenote_page_helper(&storage, &graph, &key, None, Some(&md2), None)
                .await
                .unwrap_or_else(|e| panic!("{page_id} 40->45 failed: {e}"));
            {
                let log = patches.lock().unwrap();
                assert_eq!(log.len(), count_before + 1, "{page_id} expected 1 PATCH for 40->45");
                let (_, cmds) = log.last().unwrap();
                assert_eq!(cmds.len(), 1);
                assert_eq!(cmds[0]["target"], "p:score", "{page_id} must target p:score, not parent div");
                assert_p_dom(
                    cmds[0]["content"].as_str().unwrap(),
                    "p:score",
                    score_did,
                    "color:#333333",
                    "Total 1000 vs 2000: 45 points",
                );
            }

            // Verify DOM preserves sibling <p> and child order
            let parsed_doc = Html::parse_document(&after2.raw_xhtml);
            let p_sel = Selector::parse("p").unwrap();
            let p_ids: Vec<_> = parsed_doc
                .select(&p_sel)
                .filter_map(|el| el.value().attr("id"))
                .collect();
            assert!(p_ids.starts_with(&["p:agenda", "p:score", "p:sib"]));
            let sib = parsed_doc
                .select(&Selector::parse(r#"p[id="p:sib"]"#).unwrap())
                .next()
                .unwrap();
            assert_eq!(sib.value().attr("data-id"), Some(sib_did));
            assert_eq!(sib.text().collect::<String>(), "Sibling untouched");
        }

        // 3. Ambiguous edit across formatting nodes (BOTH plain div and position:absolute div)
        //    must fail with edit guidance and send 0 PATCH requests (must NOT replace parent div!).
        for page_id in ["page-ambig-plain", "page-ambig-abs"] {
            let key = format!("me|{page_id}");
            let before = cli::load_onenote_page_detail(&storage, &graph, &key, false)
                .await
                .unwrap();
            let count_before = patches.lock().unwrap().len();
            let ambig_md = before
                .body_markdown
                .replacen("**alpha** alpha", "**alphas** alpha", 1);
            let err = cli::update_onenote_page_helper(&storage, &graph, &key, None, Some(&ambig_md), None)
                .await
                .unwrap_err();
            assert!(
                err.to_string().contains("--block-id"),
                "{page_id} ambiguous cross-node edit must return edit guidance, got: {err}"
            );
            assert_eq!(
                patches.lock().unwrap().len(),
                count_before,
                "{page_id} ambiguous edit must send 0 PATCH requests"
            );

            // Also crossing formatting boundary ("**alpha** alpha" -> "replaced") must send 0 PATCH requests
            let cross_md = before
                .body_markdown
                .replacen("**alpha** alpha", "replaced", 1);
            let err2 = cli::update_onenote_page_helper(&storage, &graph, &key, None, Some(&cross_md), None)
                .await
                .unwrap_err();
            assert!(
                err2.to_string().contains("--block-id"),
                "{page_id} cross-boundary edit must return edit guidance, got: {err2}"
            );
            assert_eq!(
                patches.lock().unwrap().len(),
                count_before,
                "{page_id} cross-boundary edit must send 0 PATCH requests"
            );
        }

        // 4. Regression controls: unique word replacement & title-only update
        let ctrl_before = cli::load_onenote_page_detail(&storage, &graph, "me|page-ctrl", false)
            .await
            .unwrap();
        let count_before = patches.lock().unwrap().len();
        let ctrl_md = ctrl_before.body_markdown.replace("READY", "APPROVED");
        cli::update_onenote_page_helper(&storage, &graph, "me|page-ctrl", None, Some(&ctrl_md), None)
            .await
            .unwrap();
        {
            let log = patches.lock().unwrap();
            assert_eq!(log.len(), count_before + 1);
            assert_eq!(log.last().unwrap().1[0]["target"], "p:status");
        }

        let count_before = patches.lock().unwrap().len();
        cli::update_onenote_page_helper(&storage, &graph, "me|page-ctrl", Some("New Control Title"), None, None)
            .await
            .unwrap();
        {
            let log = patches.lock().unwrap();
            assert_eq!(log.len(), count_before + 1);
            assert_eq!(log.last().unwrap().1[0]["target"], "title");
            assert_eq!(log.last().unwrap().1[0]["content"], "New Control Title");
        }

        graph_server.abort();
    }

    #[tokio::test]
    async fn regression_onenote_bug_05_nested_table_cell_order_and_list_todos() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");

        let complex_xhtml = format!(
            r#"<!DOCTYPE html><html><head><title>Nested And Todos</title></head><body>
<div id="div:x" style="position:absolute;left:48px;top:115px">
  <table id="table:o" style="border:1px solid">
    <tr>
      <td id="td:1"><p>앞</p><img id="img:c" alt="Logo" src="{base_url}/me/onenote/resources/res-9/$value" data-src-type="image/png" /><table id="table:i"><tr><td>INNERCELLZEBRA</td><td>77</td></tr></table><p>뒤</p></td>
    </tr>
    <tr>
      <td id="td:2">Row2A</td>
    </tr>
  </table>
  <table id="table:simple">
    <tr><th>Module</th><th>Owner</th></tr>
    <tr><td>Parser</td><td>Core</td></tr>
  </table>
  <ul id="ul:1">
    <li id="li:span1"><span id="span:1" data-tag="to-do">Order paint</span></li>
    <li id="li:p1"><p id="p:la" data-tag="to-do" style="margin-top:0pt">Buy filters</p></li>
    <li id="li:ctrl" data-tag="to-do">Control li tag</li>
    <li id="li:span2"><span data-tag="to-do:completed">Sand wall</span></li>
  </ul>
</div>
</body></html>"#
        );

        let page_xhtml = Arc::new(Mutex::new(complex_xhtml));
        let patches: Arc<Mutex<Vec<Vec<Value>>>> = Arc::new(Mutex::new(Vec::new()));
        let xhtml_clone = page_xhtml.clone();
        let patches_clone = patches.clone();

        let app = Router::new().fallback(move |req: Request| {
            let xhtml_ref = xhtml_clone.clone();
            let patch_log = patches_clone.clone();
            async move {
                let method = req.method().as_str().to_string();
                let path = req.uri().path().to_string();
                let body = axum::body::to_bytes(req.into_body(), 1024 * 1024)
                    .await
                    .unwrap();
                match (method.as_str(), path.as_str()) {
                    ("GET", "/v1.0/me/onenote/pages/pg-nested") => Json(json!({
                        "id": "pg-nested",
                        "title": "Nested And Todos",
                        "parentSection": { "id": "sec-1", "displayName": "Specs" }
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/onenote/pages/pg-nested/content") => (
                        StatusCode::OK,
                        [("content-type", "text/html")],
                        xhtml_ref.lock().unwrap().clone(),
                    )
                        .into_response(),
                    ("PATCH", "/v1.0/me/onenote/pages/pg-nested/content") => {
                        let cmds: Vec<Value> = serde_json::from_slice(&body).unwrap();
                        patch_log.lock().unwrap().push(cmds.clone());
                        let mut html = xhtml_ref.lock().unwrap();
                        for cmd in &cmds {
                            let target = cmd["target"].as_str().unwrap_or("");
                            let content = cmd["content"].as_str().unwrap_or("");
                            let needle = format!(r#"id="{target}""#);
                            if let Some(id_pos) = html.find(&needle) {
                                if let Some(open_lt) = html[..id_pos].rfind('<') {
                                    let tag: String = html[open_lt + 1..]
                                        .chars()
                                        .take_while(|c| c.is_ascii_alphanumeric())
                                        .collect();
                                    let close_tag = format!("</{tag}>");
                                    if let Some(rel_end) = html[open_lt..].find(&close_tag) {
                                        html.replace_range(
                                            open_lt..open_lt + rel_end + close_tag.len(),
                                            content,
                                        );
                                    }
                                }
                            }
                        }
                        StatusCode::NO_CONTENT.into_response()
                    }
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
            }
        });
        let graph_server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempfile::tempdir().unwrap();
        let (_cfg, graph, storage) = make_test_runtime(base_url, dir.path());

        // 1. Read & verify DOM order in cell, nested table non-duplication, simple table control, and search
        let detail = cli::load_onenote_page_detail(&storage, &graph, "me|pg-nested", false)
            .await
            .unwrap();
        assert!(
            patches.lock().unwrap().is_empty(),
            "read must not modify remote page"
        );

        // First block must be outer table (NOT hoisted img:c!)
        assert_eq!(detail.blocks[0].element_id.as_deref(), Some("table:o"));
        if let onenote_cli::models::OnenoteBlockKind::Table { headers, rows } =
            &detail.blocks[0].kind
        {
            assert!(headers.is_empty());
            assert_eq!(
                rows.len(),
                2,
                "outer table must have 2 rows (not absorb inner table <tr>), got {}",
                rows.len()
            );
        } else {
            panic!("expected outer Table block first, got {:?}", detail.blocks[0]);
        }

        // Verify resource for cell image is still preserved
        assert_eq!(detail.resources.len(), 1);
        assert!(detail.resources[0].is_image);
        assert_eq!(detail.resources[0].alt_text.as_deref(), Some("Logo"));

        // Verify reading order in both Markdown and plain text: 앞 < Logo < INNERCELLZEBRA < 77 < 뒤 < Row2A
        for (label, text) in [
            ("body_markdown", detail.body_markdown.as_str()),
            ("body_plain", detail.body_plain.as_str()),
        ] {
            let pos_front = text
                .find("앞")
                .unwrap_or_else(|| panic!("{label} missing '앞': {text}"));
            let pos_logo = text
                .find("Logo")
                .unwrap_or_else(|| panic!("{label} missing 'Logo': {text}"));
            let pos_zebra = text
                .find("INNERCELLZEBRA")
                .unwrap_or_else(|| panic!("{label} missing 'INNERCELLZEBRA': {text}"));
            // The preceding image URL can also contain 77 in its ephemeral port.
            let pos_77 = text[pos_zebra..]
                .find("77")
                .map(|offset| pos_zebra + offset)
                .unwrap_or_else(|| panic!("{label} missing table cell '77' after INNERCELLZEBRA: {text}"));
            let pos_back = text
                .find("뒤")
                .unwrap_or_else(|| panic!("{label} missing '뒤': {text}"));
            let pos_row2 = text
                .find("Row2A")
                .unwrap_or_else(|| panic!("{label} missing 'Row2A': {text}"));
            assert!(
                pos_front < pos_logo
                    && pos_logo < pos_zebra
                    && pos_zebra < pos_77
                    && pos_77 < pos_back
                    && pos_back < pos_row2,
                "{label} did not preserve DOM order (앞={pos_front}, Logo={pos_logo}, INNERCELLZEBRA={pos_zebra}, 77={pos_77}, 뒤={pos_back}, Row2A={pos_row2}):\n{text}"
            );
            assert_eq!(
                text.matches("INNERCELLZEBRA").count(),
                1,
                "{label} must include nested table cell text exactly once:\n{text}"
            );
        }

        // Control: simple table still parsed cleanly
        assert!(detail.body_markdown.contains("| Module | Owner |"));
        assert!(detail.body_markdown.contains("| Parser | Core |"));

        // Search finds INNERCELLZEBRA once and sends 0 remote writes
        let hits = cli::search_onenote_helper(&storage, &graph, "INNERCELLZEBRA", None, false, 10)
            .await
            .unwrap();
        assert_eq!(hits.len(), 1, "search for INNERCELLZEBRA must return 1 hit");
        assert_eq!(hits[0].page_key, "me|pg-nested");
        assert!(patches.lock().unwrap().is_empty());

        // 2. Verify li>span, li>p, and control li[data-tag] todos appear in blocks/markdown/has_todo
        assert!(detail.summary.has_todo);
        let todo_blocks: Vec<_> = detail
            .blocks
            .iter()
            .filter(|b| b.tag.as_ref().is_some_and(|t| t.is_todo()))
            .collect();
        assert_eq!(
            todo_blocks.len(),
            4,
            "expected 4 todo items (li>span, li>p, li, li>span:completed), got: {todo_blocks:?}"
        );
        assert!(detail.body_markdown.contains("- [ ] Order paint"));
        assert!(detail.body_markdown.contains("- [ ] Buy filters"));
        assert!(detail.body_markdown.contains("- [ ] Control li tag"));
        assert!(detail.body_markdown.contains("- [x] Sand wall"));

        // 3. Check index 0 (li>span) -> targets li:span1 (Graph-replaceable) and strips stale child tag
        let (after0, bid0, done0) =
            cli::toggle_onenote_todo_helper(&storage, &graph, "me|pg-nested", None, Some(0))
                .await
                .unwrap();
        assert!(done0);
        assert_eq!(bid0, "li:span1");
        assert!(after0.body_markdown.contains("- [x] Order paint"));
        {
            let log = patches.lock().unwrap();
            let cmd = &log.last().unwrap()[0];
            assert_eq!(cmd["target"], "li:span1");
            let content = cmd["content"].as_str().unwrap();
            assert!(content.contains(r#"data-tag="to-do:completed""#));
            assert!(
                !content.contains(r#"data-tag="to-do""#),
                "stale unchecked tag must not remain on child span: {content}"
            );
        }

        // 4. Check index 1 (li>p with id=p:la) -> targets p:la (actual tagged replaceable <p>)
        let (after1, bid1, done1) =
            cli::toggle_onenote_todo_helper(&storage, &graph, "me|pg-nested", None, Some(1))
                .await
                .unwrap();
        assert!(done1);
        assert_eq!(bid1, "p:la");
        assert!(after1.body_markdown.contains("- [x] Buy filters"));
        {
            let log = patches.lock().unwrap();
            let cmd = &log.last().unwrap()[0];
            assert_eq!(cmd["target"], "p:la");
            let content = cmd["content"].as_str().unwrap();
            assert!(content.contains(r#"data-tag="to-do:completed""#));
            assert!(content.contains(r#"style="margin-top:0pt""#));
        }

        // 5. Check index 2 (control li[data-tag]) -> targets li:ctrl
        let (after2, bid2, done2) =
            cli::toggle_onenote_todo_helper(&storage, &graph, "me|pg-nested", None, Some(2))
                .await
                .unwrap();
        assert!(done2);
        assert_eq!(bid2, "li:ctrl");
        assert!(after2.body_markdown.contains("- [x] Control li tag"));

        // 6. Check index 3 (li>span already completed) -> toggles to unchecked on li:span2
        let (after3, bid3, done3) =
            cli::toggle_onenote_todo_helper(&storage, &graph, "me|pg-nested", None, Some(3))
                .await
                .unwrap();
        assert!(!done3);
        assert_eq!(bid3, "li:span2");
        assert!(after3.body_markdown.contains("- [ ] Sand wall"));
        {
            let log = patches.lock().unwrap();
            let cmd = &log.last().unwrap()[0];
            assert_eq!(cmd["target"], "li:span2");
            let content = cmd["content"].as_str().unwrap();
            assert!(content.contains(r#"data-tag="to-do""#));
            assert!(!content.contains("to-do:completed"));
        }

        // 7. Check --block-id using outer li:p1 resolves to the tagged p:la target
        let (after4, bid4, done4) = cli::change_onenote_todo_helper_with_guard(
            &storage,
            &graph,
            "me|pg-nested",
            Some("li:p1"),
            None,
            Some(false),
            None,
            true,
        )
        .await
        .unwrap();
        assert!(!done4);
        assert_eq!(bid4, "p:la");
        assert!(after4.body_markdown.contains("- [ ] Buy filters"));

        graph_server.abort();
    }

    #[tokio::test]
    async fn regression_onenote_bug_03_http_api_contract_and_compat() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let base_url = format!("http://{addr}/v1.0");

        let png_a = sample_png_bytes();
        let png_b = {
            let mut out = Vec::new();
            let img = image::RgbaImage::from_pixel(3, 3, image::Rgba([20, 180, 90, 255]));
            let mut cursor = std::io::Cursor::new(&mut out);
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut cursor, image::ImageFormat::Png)
                .unwrap();
            out
        };
        let png_beta = {
            let mut out = Vec::new();
            let img = image::RgbaImage::from_pixel(4, 2, image::Rgba([10, 40, 220, 255]));
            let mut cursor = std::io::Cursor::new(&mut out);
            image::DynamicImage::ImageRgba8(img)
                .write_to(&mut cursor, image::ImageFormat::Png)
                .unwrap();
            out
        };

        let res1_url = format!("{base_url}/me/onenote/resources/res-1/content");
        let res2_url = format!("{base_url}/me/onenote/resources/res-2/content");
        let res_alpha_url = format!("{base_url}/me/onenote/resources/res-alpha/content");
        let res_beta_url = format!("{base_url}/me/onenote/resources/res-beta/content");

        let mut xhtml_map = std::collections::HashMap::new();
        xhtml_map.insert(
            "page-1".to_string(),
            format!(
                r#"<!DOCTYPE html><html><head><title>API Page</title></head><body><div id="div:1"><p id="p:todo-1" data-tag="to-do">Ship API parity</p><img id="img:1" src="{res1_url}" data-src-type="image/png" alt="first" /><img id="img:2" src="{res2_url}" data-src-type="image/png" alt="second" /></div></body></html>"#
            ),
        );
        xhtml_map.insert(
            "page-sa".to_string(),
            format!(
                r#"<!DOCTYPE html><html><head><title>Standup notes</title></head><body><div id="div:a"><img id="img:a" src="{res_alpha_url}" data-src-type="image/png" alt="alpha" /></div></body></html>"#
            ),
        );
        xhtml_map.insert(
            "page-sb".to_string(),
            format!(
                r#"<!DOCTYPE html><html><head><title>Standup notes</title></head><body><div id="div:b"><img id="img:b" src="{res_beta_url}" data-src-type="image/png" alt="beta" /></div></body></html>"#
            ),
        );

        let page_xhtml = Arc::new(Mutex::new(xhtml_map));
        let patches: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
        let resource_hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

        let xhtml_c = page_xhtml.clone();
        let patches_c = patches.clone();
        let res_hits_c = resource_hits.clone();
        let base_c = base_url.clone();
        let png_a_c = png_a.clone();
        let png_b_c = png_b.clone();
        let png_beta_c = png_beta.clone();

        let app = Router::new().fallback(move |req: Request| {
            let xhtml = xhtml_c.clone();
            let patch_log = patches_c.clone();
            let res_hits = res_hits_c.clone();
            let base = base_c.clone();
            let bytes_a = png_a_c.clone();
            let bytes_b = png_b_c.clone();
            let bytes_beta = png_beta_c.clone();
            async move {
                let method = req.method().as_str().to_string();
                let path = req.uri().path().to_string();
                let ct = req
                    .headers()
                    .get("content-type")
                    .and_then(|v| v.to_str().ok())
                    .unwrap_or("")
                    .to_string();
                let body = axum::body::to_bytes(req.into_body(), 2 * 1024 * 1024)
                    .await
                    .unwrap();

                match (method.as_str(), path.as_str()) {
                    ("GET", "/v1.0/me/onenote/notebooks") => Json(json!({
                        "value": [{
                            "id": "nb-personal",
                            "displayName": "Personal",
                            "isDefault": true,
                            "isShared": false
                        }]
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/joinedTeams") => Json(json!({
                        "value": [{ "id": "grp-ops", "displayName": "Ops Team" }]
                    }))
                    .into_response(),
                    ("GET", "/v1.0/groups/grp-ops/onenote/notebooks") => Json(json!({
                        "value": [{
                            "id": "nb-ops",
                            "displayName": "Ops Notebook",
                            "isDefault": false,
                            "isShared": true
                        }]
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/onenote/notebooks/nb-personal/sectionGroups")
                    | ("GET", "/v1.0/groups/grp-ops/onenote/notebooks/nb-ops/sectionGroups")
                    | ("GET", "/v1.0/me/onenote/sectionGroups") => {
                        Json(json!({ "value": [] })).into_response()
                    }
                    ("GET", "/v1.0/me/onenote/sections")
                    | ("GET", "/v1.0/me/onenote/notebooks/nb-personal/sections") => Json(json!({
                        "value": [
                            {
                                "id": "sec-alpha",
                                "displayName": "Alpha",
                                "isDefault": true,
                                "parentNotebook": { "id": "nb-personal", "displayName": "Personal" }
                            },
                            {
                                "id": "sec-beta",
                                "displayName": "Beta",
                                "isDefault": false,
                                "parentNotebook": { "id": "nb-personal", "displayName": "Personal" }
                            }
                        ]
                    }))
                    .into_response(),
                    ("GET", "/v1.0/groups/grp-ops/onenote/sections")
                    | ("GET", "/v1.0/groups/grp-ops/onenote/notebooks/nb-ops/sections") => {
                        Json(json!({ "value": [] })).into_response()
                    }
                    ("GET", "/v1.0/me/onenote/sections/sec-alpha/pages") => Json(json!({
                        "value": [
                            {
                                "id": "page-1",
                                "title": "API Page",
                                "contentUrl": format!("{base}/me/onenote/pages/page-1/content"),
                                "parentSection": { "id": "sec-alpha", "displayName": "Alpha" }
                            },
                            {
                                "id": "page-sa",
                                "title": "Standup notes",
                                "contentUrl": format!("{base}/me/onenote/pages/page-sa/content"),
                                "parentSection": { "id": "sec-alpha", "displayName": "Alpha" }
                            }
                        ]
                    }))
                    .into_response(),
                    ("GET", "/v1.0/me/onenote/sections/sec-beta/pages") => Json(json!({
                        "value": [{
                            "id": "page-sb",
                            "title": "Standup notes",
                            "contentUrl": format!("{base}/me/onenote/pages/page-sb/content"),
                            "parentSection": { "id": "sec-beta", "displayName": "Beta" }
                        }]
                    }))
                    .into_response(),
                    ("GET", p) if p.starts_with("/v1.0/me/onenote/pages/") && p.ends_with("/content") => {
                        let id = p
                            .trim_start_matches("/v1.0/me/onenote/pages/")
                            .trim_end_matches("/content");
                        match xhtml.lock().unwrap().get(id).cloned() {
                            Some(html) => (StatusCode::OK, [("content-type", "text/html")], html)
                                .into_response(),
                            None => StatusCode::NOT_FOUND.into_response(),
                        }
                    }
                    ("GET", p) if p.starts_with("/v1.0/me/onenote/pages/") => {
                        let id = p.trim_start_matches("/v1.0/me/onenote/pages/");
                        let (title, sec_id, sec_name) = match id {
                            "page-1" => ("API Page", "sec-alpha", "Alpha"),
                            "page-sa" => ("Standup notes", "sec-alpha", "Alpha"),
                            "page-sb" => ("Standup notes", "sec-beta", "Beta"),
                            _ => return StatusCode::NOT_FOUND.into_response(),
                        };
                        Json(json!({
                            "id": id,
                            "title": title,
                            "parentSection": { "id": sec_id, "displayName": sec_name }
                        }))
                        .into_response()
                    }
                    ("PATCH", p) if p.starts_with("/v1.0/me/onenote/pages/") && p.ends_with("/content") => {
                        let id = p
                            .trim_start_matches("/v1.0/me/onenote/pages/")
                            .trim_end_matches("/content")
                            .to_string();
                        let raw = String::from_utf8_lossy(&body).into_owned();
                        patch_log.lock().unwrap().push((id.clone(), ct, raw.clone()));
                        let mut map = xhtml.lock().unwrap();
                        if let Some(html) = map.get_mut(&id) {
                            if raw.contains("to-do:completed") {
                                *html = html.replace(r#"data-tag="to-do""#, r#"data-tag="to-do:completed""#);
                            } else if raw.contains(r#"data-tag=\"to-do\""#) {
                                *html = html.replace(r#"data-tag="to-do:completed""#, r#"data-tag="to-do""#);
                            }
                        }
                        StatusCode::NO_CONTENT.into_response()
                    }
                    ("GET", p) if p.starts_with("/v1.0/me/onenote/resources/") => {
                        res_hits.lock().unwrap().push(p.to_string());
                        let data = if p.contains("res-1") {
                            bytes_a
                        } else if p.contains("res-2") {
                            bytes_b
                        } else if p.contains("res-beta") {
                            bytes_beta
                        } else {
                            bytes_a
                        };
                        (StatusCode::OK, [("content-type", "image/png")], data).into_response()
                    }
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
            }
        });
        let graph_server = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let dir = tempfile::tempdir().unwrap();
        let (cfg, graph, storage) = make_test_runtime(base_url.clone(), dir.path());
        let state = Arc::new(ServeState {
            config: cfg.clone(),
            graph: graph.clone(),
            storage: storage.clone(),
            api_key: None,
        });
        let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let api_addr = api_listener.local_addr().unwrap();
        let api_server = tokio::spawn(async move {
            let _ = axum::serve(api_listener, serve::build_router(state)).await;
        });
        let http = reqwest::Client::new();

        // 1. Both documented `/todo` and legacy `/toggle-todo`:
        //    send [true, true, false, false] starting from unchecked -> returns [true, true, false, false] with 2 actual PATCHes
        for route in [
            format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/todo"),
            format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/toggle-todo"),
        ] {
            let writes_before = patches.lock().unwrap().len();
            let mut results = Vec::new();
            for desired in [true, true, false, false] {
                let resp = http
                    .post(&route)
                    .json(&json!({ "block_id": "p:todo-1", "completed": desired }))
                    .send()
                    .await
                    .unwrap();
                assert_eq!(resp.status(), StatusCode::OK, "route={route} desired={desired}");
                let val: Value = resp.json().await.unwrap();
                assert_eq!(val["ok"], true);
                assert_eq!(val["block_id"], "p:todo-1");
                results.push(val["completed"].as_bool().unwrap());
            }
            assert_eq!(results, vec![true, true, false, false], "route={route}");
            assert_eq!(
                patches.lock().unwrap().len() - writes_before,
                2,
                "route={route}: idempotent true,true,false,false must perform exactly 2 writes"
            );
        }

        // Omitting `completed` still toggles, and existing index/expected_content/force still work
        let writes_before = patches.lock().unwrap().len();
        let tog1: Value = http
            .post(format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/todo"))
            .json(&json!({ "index": 0 }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(tog1["completed"], true);
        assert_eq!(patches.lock().unwrap().len(), writes_before + 1);

        let conflict_resp = http
            .post(format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/todo"))
            .json(&json!({
                "block_id": "p:todo-1",
                "completed": false,
                "expected_content": "Wrong baseline text"
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(conflict_resp.status(), StatusCode::CONFLICT);
        assert_eq!(patches.lock().unwrap().len(), writes_before + 1);

        let forced_resp = http
            .post(format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/todo"))
            .json(&json!({
                "block_id": "p:todo-1",
                "completed": false,
                "expected_content": "Wrong baseline text",
                "force": true
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(forced_resp.status(), StatusCode::OK);
        assert_eq!(patches.lock().unwrap().len(), writes_before + 2);

        // 2. Resource download: legacy route vs documented `/v1/onenote/resources/download`
        let out_legacy = dir.path().join("dl_legacy");
        let out_doc_idx = dir.path().join("dl_doc_idx");
        let out_doc_url = dir.path().join("dl_doc_url");
        let out_doc_both = dir.path().join("dl_doc_both");
        let out_reject = dir.path().join("dl_reject");
        let out_beta = dir.path().join("dl_beta");
        std::fs::create_dir_all(&out_reject).unwrap();

        let legacy_dl: Value = http
            .post(format!(
                "http://{api_addr}/v1/onenote/pages/me%7Cpage-1/resources/download"
            ))
            .json(&json!({ "index": 1, "out_dir": out_legacy.to_string_lossy() }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(legacy_dl["ok"], true);
        let legacy_bytes = std::fs::read(legacy_dl["path"].as_str().unwrap()).unwrap();
        assert_eq!(legacy_bytes, png_b);

        let doc_idx_resp = http
            .post(format!("http://{api_addr}/v1/onenote/resources/download"))
            .json(&json!({
                "page": "me|page-1",
                "index": 1,
                "out": out_doc_idx.to_string_lossy()
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(doc_idx_resp.status(), StatusCode::OK);
        let doc_idx_dl: Value = doc_idx_resp.json().await.unwrap();
        assert_eq!(doc_idx_dl["ok"], true);
        assert_eq!(doc_idx_dl["mime_type"], legacy_dl["mime_type"]);
        assert_eq!(doc_idx_dl["size_bytes"], legacy_dl["size_bytes"]);
        assert_eq!(doc_idx_dl["is_image"], legacy_dl["is_image"]);
        assert_eq!(
            std::fs::read(doc_idx_dl["path"].as_str().unwrap()).unwrap(),
            legacy_bytes
        );

        let doc_url_dl: Value = http
            .post(format!("http://{api_addr}/v1/onenote/resources/download"))
            .json(&json!({
                "page": "me|page-1",
                "url": res2_url,
                "out": out_doc_url.to_string_lossy()
            }))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        assert_eq!(doc_url_dl["ok"], true);
        assert_eq!(
            std::fs::read(doc_url_dl["path"].as_str().unwrap()).unwrap(),
            legacy_bytes
        );

        // Matching index + url succeeds
        let doc_both_resp = http
            .post(format!("http://{api_addr}/v1/onenote/resources/download"))
            .json(&json!({
                "page": "me|page-1",
                "index": 1,
                "url": res2_url,
                "out": out_doc_both.to_string_lossy()
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(doc_both_resp.status(), StatusCode::OK);
        let doc_both_dl: Value = doc_both_resp.json().await.unwrap();
        assert_eq!(
            std::fs::read(doc_both_dl["path"].as_str().unwrap()).unwrap(),
            legacy_bytes
        );

        // Mismatched index + url, unknown url, and duplicate out+out_dir are rejected with 0 downloads & 0 files written
        let hits_before_reject = resource_hits.lock().unwrap().len();
        let mismatch = http
            .post(format!("http://{api_addr}/v1/onenote/resources/download"))
            .json(&json!({
                "page": "me|page-1",
                "index": 0,
                "url": res2_url,
                "out": out_reject.to_string_lossy()
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);

        let missing_url = http
            .post(format!("http://{api_addr}/v1/onenote/resources/download"))
            .json(&json!({
                "page": "me|page-1",
                "url": format!("{base_url}/me/onenote/resources/not-on-page/content"),
                "out": out_reject.to_string_lossy()
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(missing_url.status(), StatusCode::NOT_FOUND);

        let dup_out = http
            .post(format!("http://{api_addr}/v1/onenote/resources/download"))
            .json(&json!({
                "page": "me|page-1",
                "index": 0,
                "out": out_reject.to_string_lossy(),
                "out_dir": out_reject.to_string_lossy()
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(dup_out.status(), StatusCode::BAD_REQUEST);

        assert_eq!(
            resource_hits.lock().unwrap().len(),
            hits_before_reject,
            "rejected download requests must perform 0 resource downloads"
        );
        assert_eq!(
            std::fs::read_dir(&out_reject).unwrap().count(),
            0,
            "rejected download requests must write 0 files"
        );

        // Section disambiguation for duplicate page titles ("Standup notes" in Beta)
        let beta_dl_resp = http
            .post(format!("http://{api_addr}/v1/onenote/resources/download"))
            .json(&json!({
                "page": "Standup notes",
                "section": "Beta",
                "index": 0,
                "out": out_beta.to_string_lossy()
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(beta_dl_resp.status(), StatusCode::OK);
        let beta_dl: Value = beta_dl_resp.json().await.unwrap();
        assert_eq!(
            std::fs::read(beta_dl["path"].as_str().unwrap()).unwrap(),
            png_beta
        );

        // 3. Append `images` alias vs `image_paths` and duplicate-field 400
        let sample_img_file = dir.path().join("attach.png");
        std::fs::write(&sample_img_file, &png_a).unwrap();
        let img_str = sample_img_file.to_string_lossy().into_owned();

        let p_before = patches.lock().unwrap().len();
        let r_images = http
            .post(format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/append"))
            .json(&json!({ "markdown": "Photo via images", "images": [img_str] }))
            .send()
            .await
            .unwrap();
        assert_eq!(r_images.status(), StatusCode::OK);

        let r_image_paths = http
            .post(format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/append"))
            .json(&json!({ "markdown": "Photo via image_paths", "image_paths": [img_str] }))
            .send()
            .await
            .unwrap();
        assert_eq!(r_image_paths.status(), StatusCode::OK);

        {
            let log = patches.lock().unwrap();
            assert_eq!(log.len(), p_before + 2);
            let (_, ct1, body1) = &log[p_before];
            let (_, ct2, body2) = &log[p_before + 1];
            assert!(
                ct1.starts_with("multipart/form-data; boundary="),
                "images alias must send multipart, got: {ct1}"
            );
            assert!(
                ct2.starts_with("multipart/form-data; boundary="),
                "image_paths must send multipart, got: {ct2}"
            );
            assert!(body1.contains("filename=\"attach.png\"") && body1.contains("name:img1"));
            assert!(body2.contains("filename=\"attach.png\"") && body2.contains("name:img1"));
        }

        let p_before_dup = patches.lock().unwrap().len();
        let r_dup = http
            .post(format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/append"))
            .json(&json!({
                "markdown": "Both keys",
                "images": [img_str],
                "image_paths": [img_str]
            }))
            .send()
            .await
            .unwrap();
        assert_eq!(r_dup.status(), StatusCode::BAD_REQUEST);
        assert_eq!(patches.lock().unwrap().len(), p_before_dup);

        // 4. Notebooks `include_teams`: omitted and false return personal only; true includes group notebooks
        let nb_default: Value = http
            .get(format!("http://{api_addr}/v1/onenote/notebooks?refresh=true"))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let default_ids: Vec<&str> = nb_default["notebooks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["id"].as_str())
            .collect();
        assert_eq!(default_ids, vec!["nb-personal"]);

        let nb_false: Value = http
            .get(format!(
                "http://{api_addr}/v1/onenote/notebooks?include_teams=false&refresh=true"
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let false_ids: Vec<&str> = nb_false["notebooks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["id"].as_str())
            .collect();
        assert_eq!(false_ids, vec!["nb-personal"]);

        let nb_true: Value = http
            .get(format!(
                "http://{api_addr}/v1/onenote/notebooks?include_teams=true&refresh=true"
            ))
            .send()
            .await
            .unwrap()
            .json()
            .await
            .unwrap();
        let mut true_ids: Vec<&str> = nb_true["notebooks"]
            .as_array()
            .unwrap()
            .iter()
            .filter_map(|n| n["id"].as_str())
            .collect();
        true_ids.sort();
        assert_eq!(true_ids, vec!["nb-ops", "nb-personal"]);

        // Verify docs/API.md and docs/openapi.yaml reflect include_teams default: false
        let api_md = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/API.md"))
            .unwrap();
        assert!(
            api_md.contains("?include_teams=false"),
            "docs/API.md must document include_teams=false default"
        );
        let openapi_yaml =
            std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/openapi.yaml"))
                .unwrap();
        let include_teams_block = openapi_yaml
            .split("name: include_teams")
            .nth(1)
            .expect("openapi.yaml must define include_teams");
        let block_prefix: String = include_teams_block.chars().take(120).collect();
        assert!(
            block_prefix.contains("default: false"),
            "docs/openapi.yaml must document include_teams default: false, got: {block_prefix}"
        );

        api_server.abort();
        graph_server.abort();
    }
}

/// NEW-01: the HTTP API must not be reachable from a web page (cross-origin
/// requests, DNS rebinding) and must not act without the API key.
#[tokio::test]
async fn e2e_http_serve_guard_blocks_cross_site_and_rebinding() {
    let (base_url, mock, graph_server) = spawn_mock_graph().await;
    let dir = tempfile::tempdir().unwrap();
    let (cfg, graph, storage) = make_test_runtime(base_url, dir.path());
    let state = |api_key: Option<&str>| {
        Arc::new(ServeState {
            config: cfg.clone(),
            graph: graph.clone(),
            storage: storage.clone(),
            api_key: api_key.map(str::to_string),
        })
    };
    async fn spawn_api(app: Router) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, handle)
    }
    let http = reqwest::Client::new();

    // --- Default (loopback) guard with a key ---
    let (addr, keyed_server) = spawn_api(serve::build_router(state(Some("secret-key")))).await;

    // (a) Preflight from a foreign origin gets no CORS grant.
    let preflight = http
        .request(
            reqwest::Method::OPTIONS,
            format!("http://{addr}/v1/onenote/pages/me%7Cpage-1"),
        )
        .header("Origin", "https://evil.example")
        .header("Access-Control-Request-Method", "DELETE")
        .header("Access-Control-Request-Headers", "authorization")
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), StatusCode::FORBIDDEN);
    assert!(preflight
        .headers()
        .get("access-control-allow-origin")
        .is_none());

    // (b) DNS rebinding: an attacker-controlled Host is refused, key or not.
    for host in ["evil.example", "127.0.0.1.evil.example:8793"] {
        let rebound = http
            .get(format!("http://{addr}/v1/onenote/notebooks"))
            .header("Host", host)
            .header("x-api-key", "secret-key")
            .send()
            .await
            .unwrap();
        assert_eq!(rebound.status(), StatusCode::MISDIRECTED_REQUEST, "{host}");
        let body: Value = rebound.json().await.unwrap();
        assert_eq!(body["ok"], false);
    }
    let rebound_health = http
        .get(format!("http://{addr}/health"))
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(rebound_health.status(), StatusCode::MISDIRECTED_REQUEST);

    // (c) A "simple" cross-site GET is refused even with a valid key, and
    // never reaches Graph.
    let hits_before = *mock.notebook_list_hits.lock().unwrap();
    let cross_site = http
        .get(format!("http://{addr}/v1/onenote/notebooks?refresh=true"))
        .header("Origin", "https://evil.example")
        .header("x-api-key", "secret-key")
        .send()
        .await
        .unwrap();
    assert_eq!(cross_site.status(), StatusCode::FORBIDDEN);
    assert!(cross_site
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    assert_eq!(*mock.notebook_list_hits.lock().unwrap(), hits_before);

    // (d) Destructive call without (or with a wrong) key: 401, nothing deleted.
    for key in [None, Some("secret-kez"), Some("")] {
        let mut request = http.delete(format!("http://{addr}/v1/onenote/pages/me%7Cpage-1"));
        if let Some(key) = key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert!(mock.deleted_pages.lock().unwrap().is_empty());

    // (e) /health stays key-free for a valid Host (IP and name).
    let health = http
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    assert!(health
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    let health_by_name = http
        .get(format!("http://{addr}/health"))
        .header("Host", format!("LOCALHOST:{}", addr.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(health_by_name.status(), StatusCode::OK);
    keyed_server.abort();

    // An empty configured key must not mean "no key".
    let (addr, empty_key_server) = spawn_api(serve::build_router(state(Some("")))).await;
    let empty = http
        .get(format!("http://{addr}/v1/onenote/notebooks"))
        .header("x-api-key", "")
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), StatusCode::UNAUTHORIZED);
    empty_key_server.abort();

    // A keyless router (`--allow-keyless`) keeps the Host and Origin checks.
    let (addr, keyless_server) = spawn_api(serve::build_router(state(None))).await;
    let keyless_ok = http
        .get(format!("http://{addr}/v1/onenote/notebooks"))
        .send()
        .await
        .unwrap();
    assert_eq!(keyless_ok.status(), StatusCode::OK);
    let keyless_rebound = http
        .get(format!("http://{addr}/v1/onenote/notebooks"))
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(keyless_rebound.status(), StatusCode::MISDIRECTED_REQUEST);
    let keyless_cross_site = http
        .delete(format!("http://{addr}/v1/onenote/pages/me%7Cpage-1"))
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(keyless_cross_site.status(), StatusCode::FORBIDDEN);
    assert!(mock.deleted_pages.lock().unwrap().is_empty());
    keyless_server.abort();

    // (f) `--cors-origin` echoes exactly the configured origin and nothing else.
    let guard = serve::ServeGuard::new(
        "127.0.0.1",
        &["notes.corp.example".to_string()],
        &["https://app.example.com".to_string()],
    )
    .unwrap();
    let (addr, cors_server) = spawn_api(serve::build_router_with_guard(
        state(Some("secret-key")),
        guard,
    ))
    .await;
    let allowed = http
        .get(format!("http://{addr}/v1/onenote/notebooks"))
        .header("Origin", "https://app.example.com")
        .header("x-api-key", "secret-key")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(
        allowed.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );
    let allowed_preflight = http
        .request(
            reqwest::Method::OPTIONS,
            format!("http://{addr}/v1/onenote/pages/me%7Cpage-1"),
        )
        .header("Origin", "https://app.example.com")
        .header("Access-Control-Request-Method", "DELETE")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed_preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        allowed_preflight.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );
    assert!(allowed_preflight.headers()["access-control-allow-methods"]
        .to_str()
        .unwrap()
        .contains("DELETE"));
    // The allowed origin still needs the key.
    let allowed_no_key = http
        .get(format!("http://{addr}/v1/onenote/notebooks"))
        .header("Origin", "https://app.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed_no_key.status(), StatusCode::UNAUTHORIZED);
    let other_origin = http
        .get(format!("http://{addr}/health"))
        .header("Origin", "https://app.example.com.evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(other_origin.status(), StatusCode::FORBIDDEN);
    let extra_host = http
        .get(format!("http://{addr}/health"))
        .header("Host", "notes.corp.example:8793")
        .send()
        .await
        .unwrap();
    assert_eq!(extra_host.status(), StatusCode::OK);
    cors_server.abort();

    graph_server.abort();
}

mod discovery_completeness {
    use super::*;
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
}

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
