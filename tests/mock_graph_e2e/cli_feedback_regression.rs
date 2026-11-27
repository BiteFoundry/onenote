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
