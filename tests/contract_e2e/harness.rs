//! Shared contract mock Graph server, CLI runner, and MCP subprocess harness.

#![allow(dead_code, unused_imports)]

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


#[derive(Clone)]
pub(crate) struct ContractMock {
    pub(crate) base_url: String,
    pub(crate) num_sections: usize,
    pub(crate) num_pages: usize,
    pub(crate) me_calls: Arc<AtomicUsize>,
    pub(crate) total_calls: Arc<AtomicUsize>,
}

impl ContractMock {
    pub(crate) fn me_count(&self) -> usize {
        self.me_calls.load(Ordering::SeqCst)
    }

    pub(crate) fn total_count(&self) -> usize {
        self.total_calls.load(Ordering::SeqCst)
    }
}

pub(crate) fn make_unsigned_jwt(oid: &str) -> String {
    let header =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#);
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(json!({ "oid": oid, "exp": 4_102_444_800_i64 }).to_string());
    format!("{header}.{payload}.")
}

pub(crate) fn closed_graph_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}/v1.0")
}

pub(crate) fn account_from_headers(headers: &HeaderMap) -> Option<&'static str> {
    let auth = headers
        .get(axum::http::header::AUTHORIZATION)
        .and_then(|v| v.to_str().ok())?;
    let token = auth.strip_prefix("Bearer ")?.trim();
    match token {
        "opaque-a" => Some("user-a"),
        "opaque-b" => Some("user-b"),
        other => {
            let mut parts = other.split('.');
            let _hdr = parts.next()?;
            let payload_b64 = parts.next()?;
            let _sig = parts.next()?;
            let raw = base64::engine::general_purpose::URL_SAFE_NO_PAD
                .decode(payload_b64)
                .ok()?;
            let val: Value = serde_json::from_slice(&raw).ok()?;
            match val.get("oid").and_then(Value::as_str) {
                Some("user-a") => Some("user-a"),
                Some("user-b") => Some("user-b"),
                _ => None,
            }
        }
    }
}

pub(crate) fn unauthorized() -> axum::response::Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(
            json!({ "error": { "code": "InvalidAuthenticationToken", "message": "Unauthorized" } }),
        ),
    )
        .into_response()
}

pub(crate) fn page_odata_slice(
    items: Vec<Value>,
    query: &HashMap<String, String>,
    base_url: &str,
    rel_path: &str,
) -> Value {
    let skip: usize = query.get("$skip").and_then(|s| s.parse().ok()).unwrap_or(0);
    let top: usize = query
        .get("$top")
        .and_then(|s| s.parse().ok())
        .unwrap_or(100)
        .max(1);
    let total = items.len();
    let end = skip.saturating_add(top).min(total);
    let slice: Vec<Value> = if skip >= total {
        Vec::new()
    } else {
        items[skip..end].to_vec()
    };
    let mut resp = json!({ "value": slice });
    if end < total {
        resp["@odata.nextLink"] = json!(format!("{base_url}{rel_path}?$top={top}&$skip={end}"));
    }
    resp
}

pub(crate) fn build_sections(num_sections: usize) -> Vec<Value> {
    (1..=num_sections)
        .map(|i| {
            let display_name = if i == 1 {
                "Specs".to_string()
            } else {
                format!("Section {i}")
            };
            json!({
                "id": format!("sec-{i}"),
                "displayName": display_name,
                "isDefault": i == 1,
                "parentNotebook": { "id": "nb-1", "displayName": "Engineering" },
                "lastModifiedDateTime": "2026-11-20T09:00:00Z",
            })
        })
        .collect()
}

pub(crate) fn page_title(idx: usize, acct: &str) -> String {
    if idx == 1 {
        format!("Architecture Spec {acct}")
    } else {
        format!("Architecture Note {idx} {acct}")
    }
}

pub(crate) fn build_page_meta(base_url: &str, idx: usize, acct: &str) -> Value {
    let id = format!("page-{idx}");
    let day = 20usize.saturating_sub((idx % 18) + 1).max(1);
    json!({
        "id": id,
        "title": page_title(idx, acct),
        "level": 0,
        "order": (idx - 1) as i32,
        "createdDateTime": "2026-11-01T00:00:00Z",
        "lastModifiedDateTime": format!("2026-11-{day:02}T10:00:00Z"),
        "contentUrl": format!("{base_url}/me/onenote/pages/{id}/content"),
        "links": {
            "oneNoteWebUrl": { "href": format!("https://example.invalid/{id}") }
        },
        "parentSection": { "id": "sec-1", "displayName": "Specs" },
        "parentNotebook": { "id": "nb-1", "displayName": "Engineering" },
    })
}

pub(crate) fn build_page_xhtml(base_url: &str, idx: usize, acct: &str) -> String {
    let title = page_title(idx, acct);
    if idx == 1 {
        format!(
            r#"<!DOCTYPE html><html><head><title>{title}</title></head><body><div id="div:root"><p id="p:todo1" data-tag="to-do">Review cache design</p><p id="p:todo2" data-tag="to-do:completed">Ship parser</p><p id="p:img1"><img id="img:1" src="{base_url}/me/onenote/resources/img-1/$value" data-fullres-src="{base_url}/me/onenote/resources/img-1-full/$value" data-src-type="image/png" alt="Diagram" /></p><object id="obj:1" data="{base_url}/me/onenote/resources/att-1/$value" data-attachment="spec.pdf" type="application/pdf" /></div></body></html>"#
        )
    } else {
        format!(
            r#"<!DOCTYPE html><html><head><title>{title}</title></head><body><div id="div:root"><p id="p:body-{idx}">Notes for page {idx} ({acct}).</p></div></body></html>"#
        )
    }
}

pub(crate) async fn spawn_contract_mock(
    num_sections: usize,
    num_pages: usize,
) -> (String, ContractMock, tokio::task::JoinHandle<()>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");
    let state = ContractMock {
        base_url: base_url.clone(),
        num_sections,
        num_pages,
        me_calls: Arc::new(AtomicUsize::new(0)),
        total_calls: Arc::new(AtomicUsize::new(0)),
    };

    let app = Router::new()
        .route(
            "/v1.0/me",
            get(
                |State(st): State<ContractMock>, headers: HeaderMap| async move {
                    st.total_calls.fetch_add(1, Ordering::SeqCst);
                    st.me_calls.fetch_add(1, Ordering::SeqCst);
                    let Some(acct) = account_from_headers(&headers) else {
                        return unauthorized();
                    };
                    Json(json!({
                        "id": acct,
                        "displayName": format!("User {acct}"),
                        "mail": format!("{acct}@example.invalid"),
                    }))
                    .into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/notebooks",
            get(
                |State(st): State<ContractMock>, headers: HeaderMap| async move {
                    st.total_calls.fetch_add(1, Ordering::SeqCst);
                    let Some(_acct) = account_from_headers(&headers) else {
                        return unauthorized();
                    };
                    Json(json!({
                        "value": [{
                            "id": "nb-1",
                            "displayName": "Engineering",
                            "isDefault": true,
                            "isShared": false,
                        }]
                    }))
                    .into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/sectionGroups",
            get(
                |State(st): State<ContractMock>, headers: HeaderMap| async move {
                    st.total_calls.fetch_add(1, Ordering::SeqCst);
                    if account_from_headers(&headers).is_none() {
                        return unauthorized();
                    }
                    Json(json!({ "value": [] })).into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/notebooks/{nb_id}/sectionGroups",
            get(
                |State(st): State<ContractMock>, headers: HeaderMap| async move {
                    st.total_calls.fetch_add(1, Ordering::SeqCst);
                    if account_from_headers(&headers).is_none() {
                        return unauthorized();
                    }
                    Json(json!({ "value": [] })).into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/sections",
            get(
                |State(st): State<ContractMock>,
                 headers: HeaderMap,
                 Query(q): Query<HashMap<String, String>>| async move {
                    st.total_calls.fetch_add(1, Ordering::SeqCst);
                    if account_from_headers(&headers).is_none() {
                        return unauthorized();
                    }
                    let items = build_sections(st.num_sections);
                    Json(page_odata_slice(
                        items,
                        &q,
                        &st.base_url,
                        "/me/onenote/sections",
                    ))
                    .into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/notebooks/{nb_id}/sections",
            get(
                |State(st): State<ContractMock>,
                 headers: HeaderMap,
                 AxumPath(nb_id): AxumPath<String>,
                 Query(q): Query<HashMap<String, String>>| async move {
                    st.total_calls.fetch_add(1, Ordering::SeqCst);
                    if account_from_headers(&headers).is_none() {
                        return unauthorized();
                    }
                    let items = build_sections(st.num_sections);
                    let rel = format!("/me/onenote/notebooks/{nb_id}/sections");
                    Json(page_odata_slice(items, &q, &st.base_url, &rel)).into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/sections/{sec_id}/pages",
            get(
                |State(st): State<ContractMock>,
                 headers: HeaderMap,
                 AxumPath(sec_id): AxumPath<String>,
                 Query(q): Query<HashMap<String, String>>| async move {
                    st.total_calls.fetch_add(1, Ordering::SeqCst);
                    let Some(acct) = account_from_headers(&headers) else {
                        return unauthorized();
                    };
                    if sec_id != "sec-1" {
                        return Json(json!({ "value": [] })).into_response();
                    }
                    let items: Vec<Value> = (1..=st.num_pages)
                        .map(|i| build_page_meta(&st.base_url, i, acct))
                        .collect();
                    let rel = format!("/me/onenote/sections/{sec_id}/pages");
                    Json(page_odata_slice(items, &q, &st.base_url, &rel)).into_response()
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/pages/{page_id}",
            get(
                |State(st): State<ContractMock>,
                 headers: HeaderMap,
                 AxumPath(page_id): AxumPath<String>| async move {
                    st.total_calls.fetch_add(1, Ordering::SeqCst);
                    let Some(acct) = account_from_headers(&headers) else {
                        return unauthorized();
                    };
                    let idx = page_id
                        .strip_prefix("page-")
                        .and_then(|s| s.parse::<usize>().ok());
                    match idx {
                        Some(i) if (1..=st.num_pages).contains(&i) => {
                            Json(build_page_meta(&st.base_url, i, acct)).into_response()
                        }
                        _ => (
                            StatusCode::NOT_FOUND,
                            Json(json!({ "error": { "code": "ResourceNotFound" } })),
                        )
                            .into_response(),
                    }
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/pages/{page_id}/content",
            get(
                |State(st): State<ContractMock>,
                 headers: HeaderMap,
                 AxumPath(page_id): AxumPath<String>| async move {
                    st.total_calls.fetch_add(1, Ordering::SeqCst);
                    let Some(acct) = account_from_headers(&headers) else {
                        return unauthorized();
                    };
                    let idx = page_id
                        .strip_prefix("page-")
                        .and_then(|s| s.parse::<usize>().ok());
                    match idx {
                        Some(i) if (1..=st.num_pages).contains(&i) => (
                            StatusCode::OK,
                            [("content-type", "text/html")],
                            build_page_xhtml(&st.base_url, i, acct),
                        )
                            .into_response(),
                        _ => (StatusCode::NOT_FOUND, "Not found").into_response(),
                    }
                },
            )
            .patch(
                |State(st): State<ContractMock>, headers: HeaderMap| async move {
                    st.total_calls.fetch_add(1, Ordering::SeqCst);
                    if account_from_headers(&headers).is_none() {
                        return unauthorized();
                    }
                    StatusCode::NO_CONTENT.into_response()
                },
            ),
        )
        .with_state(state.clone());

    let handle = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });
    (base_url, state, handle)
}

pub(crate) fn configure_cmd(
    cmd: &mut Command,
    base_url: &str,
    cfg_dir: &Path,
    data_dir: &Path,
    token: Option<&str>,
) {
    cmd.env("ONENOTE_GRAPH_BASE_URL", base_url)
        .env("ONENOTE_CONFIG_DIR", cfg_dir)
        .env("ONENOTE_DATA_DIR", data_dir)
        .env("ONENOTE_DISABLE_KEYRING", "1")
        .env("ONENOTE_SHARE_M365_AUTH", "0")
        .env("ONENOTE_AUTO_DAEMON", "0")
        .env_remove("ONENOTE_TOKEN_FILE")
        .env_remove("ONENOTE_TOKEN_REFRESH_CMD")
        .env_remove("TEAMS_TOKEN_FILE")
        .env_remove("TEAMS_ACCESS_TOKEN")
        .env_remove("TEAMS_CONFIG_DIR")
        .env_remove("TEAMS_DATA_DIR");
    match token {
        Some(t) => {
            cmd.env("ONENOTE_ACCESS_TOKEN", t);
        }
        None => {
            cmd.env_remove("ONENOTE_ACCESS_TOKEN");
        }
    }
}

pub(crate) async fn run_onenote(
    base_url: &str,
    cfg_dir: &Path,
    data_dir: &Path,
    token: Option<&str>,
    args: &[&str],
) -> std::process::Output {
    let mut cmd = Command::new(env!("CARGO_BIN_EXE_onenote"));
    cmd.args(args);
    configure_cmd(&mut cmd, base_url, cfg_dir, data_dir, token);
    cmd.output().await.expect("spawn onenote binary")
}

pub(crate) struct McpProc {
    pub(crate) child: tokio::process::Child,
    pub(crate) stdin: tokio::process::ChildStdin,
    pub(crate) stdout: BufReader<tokio::process::ChildStdout>,
}

impl McpProc {
    pub(crate) fn spawn(base_url: &str, cfg_dir: &Path, data_dir: &Path, token: Option<&str>) -> Self {
        let mut cmd = Command::new(env!("CARGO_BIN_EXE_onenote"));
        cmd.arg("mcp")
            .stdin(Stdio::piped())
            .stdout(Stdio::piped())
            .stderr(Stdio::piped());
        configure_cmd(&mut cmd, base_url, cfg_dir, data_dir, token);
        let mut child = cmd.spawn().expect("spawn onenote mcp");
        let stdin = child.stdin.take().unwrap();
        let stdout = BufReader::new(child.stdout.take().unwrap());
        Self {
            child,
            stdin,
            stdout,
        }
    }

    pub(crate) async fn call(&mut self, req: Value) -> Value {
        let line = serde_json::to_string(&req).unwrap();
        self.stdin.write_all(line.as_bytes()).await.unwrap();
        self.stdin.write_all(b"\n").await.unwrap();
        self.stdin.flush().await.unwrap();
        let mut resp_line = String::new();
        let n = self.stdout.read_line(&mut resp_line).await.unwrap();
        assert!(n > 0, "MCP process closed stdout unexpectedly");
        serde_json::from_str(&resp_line)
            .unwrap_or_else(|e| panic!("invalid MCP JSON line {resp_line:?}: {e}"))
    }

    pub(crate) fn is_alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    pub(crate) async fn close(mut self) {
        drop(self.stdin);
        let _ = self.child.wait().await;
    }
}

pub(crate) fn identity_map_json_count(data_dir: &Path) -> usize {
    let dir = data_dir.join("identity-map");
    if !dir.exists() {
        return 0;
    }
    std::fs::read_dir(dir)
        .unwrap()
        .filter_map(|e| e.ok())
        .filter(|e| e.file_name().to_string_lossy().ends_with(".json"))
        .count()
}

pub(crate) fn full_page_key_set() -> BTreeSet<String> {
    [
        "blocks",
        "body_markdown",
        "body_plain",
        "primary_div_id",
        "raw_xhtml",
        "resources",
        "summary",
        "synced_at",
    ]
    .into_iter()
    .map(String::from)
    .collect()
}

pub(crate) fn json_object_keys(v: &Value) -> BTreeSet<String> {
    v.as_object()
        .expect("expected JSON object")
        .keys()
        .cloned()
        .collect()
}

// ===========================================================================
// Identity (U1)
// ===========================================================================
