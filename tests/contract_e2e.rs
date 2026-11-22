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
struct ContractMock {
    base_url: String,
    num_sections: usize,
    num_pages: usize,
    me_calls: Arc<AtomicUsize>,
    total_calls: Arc<AtomicUsize>,
}

impl ContractMock {
    fn me_count(&self) -> usize {
        self.me_calls.load(Ordering::SeqCst)
    }

    fn total_count(&self) -> usize {
        self.total_calls.load(Ordering::SeqCst)
    }
}

fn make_unsigned_jwt(oid: &str) -> String {
    let header =
        base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(r#"{"alg":"none","typ":"JWT"}"#);
    let payload = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .encode(json!({ "oid": oid, "exp": 4_102_444_800_i64 }).to_string());
    format!("{header}.{payload}.")
}

fn closed_graph_url() -> String {
    let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
    let addr = listener.local_addr().unwrap();
    drop(listener);
    format!("http://{addr}/v1.0")
}

fn account_from_headers(headers: &HeaderMap) -> Option<&'static str> {
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

fn unauthorized() -> axum::response::Response {
    (
        StatusCode::UNAUTHORIZED,
        Json(
            json!({ "error": { "code": "InvalidAuthenticationToken", "message": "Unauthorized" } }),
        ),
    )
        .into_response()
}

fn page_odata_slice(
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

fn build_sections(num_sections: usize) -> Vec<Value> {
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

fn page_title(idx: usize, acct: &str) -> String {
    if idx == 1 {
        format!("Architecture Spec {acct}")
    } else {
        format!("Architecture Note {idx} {acct}")
    }
}

fn build_page_meta(base_url: &str, idx: usize, acct: &str) -> Value {
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

fn build_page_xhtml(base_url: &str, idx: usize, acct: &str) -> String {
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

async fn spawn_contract_mock(
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

fn configure_cmd(
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

async fn run_onenote(
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

struct McpProc {
    child: tokio::process::Child,
    stdin: tokio::process::ChildStdin,
    stdout: BufReader<tokio::process::ChildStdout>,
}

impl McpProc {
    fn spawn(base_url: &str, cfg_dir: &Path, data_dir: &Path, token: Option<&str>) -> Self {
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

    async fn call(&mut self, req: Value) -> Value {
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

    fn is_alive(&mut self) -> bool {
        self.child.try_wait().unwrap().is_none()
    }

    async fn close(mut self) {
        drop(self.stdin);
        let _ = self.child.wait().await;
    }
}

fn identity_map_json_count(data_dir: &Path) -> usize {
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

fn full_page_key_set() -> BTreeSet<String> {
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

fn json_object_keys(v: &Value) -> BTreeSet<String> {
    v.as_object()
        .expect("expected JSON object")
        .keys()
        .cloned()
        .collect()
}

// ===========================================================================
// Identity (U1)
// ===========================================================================

#[tokio::test]
async fn unmapped_search_errors_without_network() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["search", "Architecture", "--json"],
    )
    .await;
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
        panic!(
            "stdout must be valid JSON error object ({e}), got: {}",
            String::from_utf8_lossy(&out.stdout)
        )
    });
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "unknown_identity");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("Unknown identity"));
    let stderr = String::from_utf8_lossy(&out.stderr);
    assert!(stderr.contains("Unknown identity"), "stderr={stderr}");
    assert!(stderr.contains("onenote whoami"), "stderr={stderr}");
    assert_eq!(mock.total_count(), 0);
    assert!(!data_dir.join("default").exists());

    server.abort();
}

#[tokio::test]
async fn whoami_writes_binding_then_local_paths_work() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let who = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["whoami", "--json"],
    )
    .await;
    assert!(
        who.status.success(),
        "whoami stderr={}",
        String::from_utf8_lossy(&who.stderr)
    );
    assert_eq!(identity_map_json_count(&data_dir), 1);

    let read = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--json"],
    )
    .await;
    assert!(
        read.status.success(),
        "read stderr={}",
        String::from_utf8_lossy(&read.stderr)
    );

    let calls_before = mock.total_count();

    for _ in 0..2 {
        let srch = run_onenote(
            &base_url,
            &cfg_dir,
            &data_dir,
            Some("opaque-a"),
            &["search", "Architecture", "--json"],
        )
        .await;
        assert!(
            srch.status.success(),
            "search stderr={}",
            String::from_utf8_lossy(&srch.stderr)
        );
        let v: Value = serde_json::from_slice(&srch.stdout).unwrap();
        assert!(!v["hits"].as_array().unwrap().is_empty());
    }

    let off = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--offline", "--json"],
    )
    .await;
    assert!(
        off.status.success(),
        "offline read stderr={}",
        String::from_utf8_lossy(&off.stderr)
    );

    assert_eq!(mock.total_count(), calls_before);
    assert!(data_dir.join("user-a").join("cache.db").is_file());
    assert!(!data_dir.join("default").exists());

    server.abort();
}

#[tokio::test]
async fn mapped_search_survives_closed_graph() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    assert!(run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["whoami", "--json"]
    )
    .await
    .status
    .success());
    assert!(run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["pages", "Engineering/Specs", "--json"],
    )
    .await
    .status
    .success());

    let online_srch = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["search", "Architecture", "--json"],
    )
    .await;
    assert!(online_srch.status.success());
    let online_hits = serde_json::from_slice::<Value>(&online_srch.stdout).unwrap()["hits"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(online_hits, 3);

    server.abort();
    let closed = closed_graph_url();
    let closed_srch = run_onenote(
        &closed,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["search", "Architecture", "--json"],
    )
    .await;
    assert!(
        closed_srch.status.success(),
        "closed search stderr={}",
        String::from_utf8_lossy(&closed_srch.stderr)
    );
    let closed_hits = serde_json::from_slice::<Value>(&closed_srch.stdout).unwrap()["hits"]
        .as_array()
        .unwrap()
        .len();
    assert_eq!(closed_hits, online_hits);
}

#[tokio::test]
async fn whoami_with_closed_graph_writes_nothing() {
    let closed = closed_graph_url();
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let who = run_onenote(
        &closed,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["whoami", "--json"],
    )
    .await;
    assert_eq!(who.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&who.stderr).contains("GET /me failed"),
        "stderr={}",
        String::from_utf8_lossy(&who.stderr)
    );
    assert_eq!(identity_map_json_count(&data_dir), 0);

    let srch = run_onenote(
        &closed,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["search", "x", "--json"],
    )
    .await;
    assert_eq!(srch.status.code(), Some(1));
    assert!(
        String::from_utf8_lossy(&srch.stderr).contains("Unknown identity"),
        "stderr={}",
        String::from_utf8_lossy(&srch.stderr)
    );
    assert_eq!(identity_map_json_count(&data_dir), 0);
}

#[tokio::test]
async fn search_refresh_bootstraps_identity() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let first = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["search", "Architecture", "--refresh", "--json"],
    )
    .await;
    assert!(
        first.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&first.stderr)
    );
    assert_eq!(mock.me_count(), 1);

    let total_after_first = mock.total_count();
    let second = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["search", "Architecture", "--json"],
    )
    .await;
    assert!(
        second.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&second.stderr)
    );
    assert_eq!(mock.total_count(), total_after_first);

    server.abort();
}

#[tokio::test]
async fn unmapped_offline_errors() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    for args in [
        &["notebooks", "--offline", "--json"][..],
        &["read", "me|page-1", "--offline", "--json"][..],
    ] {
        let out = run_onenote(&base_url, &cfg_dir, &data_dir, Some("opaque-a"), args).await;
        assert_eq!(out.status.code(), Some(1), "args={args:?}");
        let v: Value = serde_json::from_slice(&out.stdout).unwrap_or_else(|e| {
            panic!(
                "args={args:?} stdout must be JSON error ({e}), got: {}",
                String::from_utf8_lossy(&out.stdout)
            )
        });
        assert_eq!(v["ok"], false, "args={args:?}");
        assert_eq!(v["error"]["code"], "unknown_identity", "args={args:?}");
        assert!(
            String::from_utf8_lossy(&out.stderr).contains("Unknown identity"),
            "args={args:?}, stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(mock.total_count(), 0);
    assert!(!data_dir.join("default").exists());

    server.abort();
}

#[tokio::test]
async fn two_accounts_do_not_share_cache() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    for tok in ["opaque-a", "opaque-b"] {
        assert!(run_onenote(
            &base_url,
            &cfg_dir,
            &data_dir,
            Some(tok),
            &["whoami", "--json"]
        )
        .await
        .status
        .success());
        assert!(run_onenote(
            &base_url,
            &cfg_dir,
            &data_dir,
            Some(tok),
            &["pages", "Engineering/Specs", "--json"],
        )
        .await
        .status
        .success());
    }

    assert!(data_dir.join("user-a").is_dir());
    assert!(data_dir.join("user-b").is_dir());
    assert_eq!(identity_map_json_count(&data_dir), 2);

    for (tok, own, other) in [
        ("opaque-a", "user-a", "user-b"),
        ("opaque-b", "user-b", "user-a"),
    ] {
        let pages_out = run_onenote(
            &base_url,
            &cfg_dir,
            &data_dir,
            Some(tok),
            &["pages", "Engineering/Specs", "--offline", "--json"],
        )
        .await;
        assert!(pages_out.status.success());
        let pv: Value = serde_json::from_slice(&pages_out.stdout).unwrap();
        let pages = pv["pages"].as_array().unwrap();
        assert_eq!(pages.len(), 3);
        for p in pages {
            let t = p["title"].as_str().unwrap();
            assert!(t.contains(own), "expected {own} in {t}");
            assert!(!t.contains(other), "unexpected {other} in {t}");
        }

        let srch_out = run_onenote(
            &base_url,
            &cfg_dir,
            &data_dir,
            Some(tok),
            &["search", "Architecture", "--json"],
        )
        .await;
        assert!(srch_out.status.success());
        let sv: Value = serde_json::from_slice(&srch_out.stdout).unwrap();
        let hits = sv["hits"].as_array().unwrap();
        assert_eq!(hits.len(), 3);
        for h in hits {
            let t = h["title"].as_str().unwrap();
            assert!(t.contains(own), "expected {own} in {t}");
            assert!(!t.contains(other), "unexpected {other} in {t}");
        }
    }

    server.abort();
}

#[tokio::test]
async fn second_account_without_binding_is_refused() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    assert!(run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["whoami", "--json"]
    )
    .await
    .status
    .success());
    assert!(run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["pages", "Engineering/Specs", "--json"],
    )
    .await
    .status
    .success());

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-b"),
        &["search", "Architecture", "--json"],
    )
    .await;
    assert_eq!(out.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "unknown_identity");
    assert!(String::from_utf8_lossy(&out.stderr).contains("Unknown identity"));

    server.abort();
}

#[tokio::test]
async fn concurrent_first_use_two_accounts() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;

    for iter in 0..5 {
        let tmp = tempfile::tempdir().unwrap();
        let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));
        let (out_a, out_b) = tokio::join!(
            run_onenote(
                &base_url,
                &cfg_dir,
                &data_dir,
                Some("opaque-a"),
                &["whoami", "--json"]
            ),
            run_onenote(
                &base_url,
                &cfg_dir,
                &data_dir,
                Some("opaque-b"),
                &["whoami", "--json"]
            ),
        );
        assert!(
            out_a.status.success(),
            "iter {iter} A failed: {}",
            String::from_utf8_lossy(&out_a.stderr)
        );
        assert!(
            out_b.status.success(),
            "iter {iter} B failed: {}",
            String::from_utf8_lossy(&out_b.stderr)
        );
        assert_eq!(identity_map_json_count(&data_dir), 2, "iter {iter}");

        for (tok, expected_dir) in [("opaque-a", "user-a"), ("opaque-b", "user-b")] {
            let doc = run_onenote(
                &base_url,
                &cfg_dir,
                &data_dir,
                Some(tok),
                &["doctor", "--json"],
            )
            .await;
            assert!(doc.status.success());
            let dv: Value = serde_json::from_slice(&doc.stdout).unwrap();
            assert_eq!(dv["cache_identity"], "identity_map");
            let db_file = dv["db_file"].as_str().unwrap();
            assert!(
                db_file.contains(expected_dir),
                "iter {iter}: expected {expected_dir} in {db_file}"
            );
        }
    }

    server.abort();
}

#[tokio::test]
async fn jwt_token_never_calls_me() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));
    let jwt = make_unsigned_jwt("user-a");

    for args in [
        &["search", "x", "--json"][..],
        &["read", "me|page-1", "--json"][..],
        &["read", "me|page-1", "--offline", "--json"][..],
    ] {
        let out = run_onenote(&base_url, &cfg_dir, &data_dir, Some(&jwt), args).await;
        assert!(
            out.status.success(),
            "args={args:?}, stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
    }
    assert_eq!(mock.me_count(), 0);
    assert!(!data_dir.join("identity-map").exists());

    server.abort();
}

#[tokio::test]
async fn no_token_uses_default_bucket() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        None,
        &["search", "x", "--json"],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["hits"].as_array().unwrap().len(), 0);
    assert_eq!(v["cache"]["pages"], 0);
    assert_eq!(mock.total_count(), 0);

    server.abort();
}

#[tokio::test]
async fn doctor_reports_cache_identity() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let before = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["doctor", "--json"],
    )
    .await;
    assert!(before.status.success());
    let bv: Value = serde_json::from_slice(&before.stdout).unwrap();
    assert_eq!(bv["cache_identity"], "unresolved");
    assert!(bv["db_file"].is_null());
    assert!(!data_dir.join("default").exists());

    assert!(run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["whoami", "--json"]
    )
    .await
    .status
    .success());

    let after = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["doctor", "--json"],
    )
    .await;
    assert!(after.status.success());
    let av: Value = serde_json::from_slice(&after.stdout).unwrap();
    assert_eq!(av["cache_identity"], "identity_map");
    assert!(av["db_file"].as_str().unwrap().contains("user-a"));

    server.abort();
}

#[tokio::test]
async fn mcp_catalogue_without_identity() {
    let closed = closed_graph_url();
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let mut proc = McpProc::spawn(&closed, &cfg_dir, &data_dir, Some("opaque-a"));

    let r1 = proc
        .call(json!({"jsonrpc":"2.0","id":1,"method":"initialize","params":{}}))
        .await;
    assert!(r1["result"]["serverInfo"].is_object());
    assert!(proc.is_alive());

    let r2 = proc
        .call(json!({"jsonrpc":"2.0","id":2,"method":"tools/list","params":{}}))
        .await;
    assert!(!r2["result"]["tools"].as_array().unwrap().is_empty());
    assert!(proc.is_alive());

    let r3 = proc
        .call(json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"search_pages","arguments":{"query":"x"}}
        }))
        .await;
    assert_eq!(r3["result"]["isError"], true);
    assert!(r3["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("Unknown identity"));
    assert!(proc.is_alive());

    let r4 = proc
        .call(json!({
            "jsonrpc":"2.0","id":4,"method":"tools/call",
            "params":{"name":"list_notebooks","arguments":{}}
        }))
        .await;
    assert_eq!(r4["result"]["isError"], true);
    assert!(r4["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("GET /me failed"));
    assert!(proc.is_alive());

    proc.close().await;
}

#[tokio::test]
async fn mcp_first_online_call_binds_then_search_is_local() {
    let (base_url, mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let mut proc = McpProc::spawn(&base_url, &cfg_dir, &data_dir, Some("opaque-a"));

    let r1 = proc
        .call(json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"whoami","arguments":{}}
        }))
        .await;
    assert_eq!(r1["result"]["isError"], false);

    let r2 = proc
        .call(json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"get_page","arguments":{"page":"me|page-1"}}
        }))
        .await;
    assert_eq!(r2["result"]["isError"], false);
    assert_eq!(identity_map_json_count(&data_dir), 1);

    let calls_before = mock.total_count();
    let r3 = proc
        .call(json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"search_pages","arguments":{"query":"Architecture"}}
        }))
        .await;
    assert_eq!(r3["result"]["isError"], false);
    assert!(!r3["result"]["structuredContent"]["hits"]
        .as_array()
        .unwrap()
        .is_empty());
    assert_eq!(mock.total_count(), calls_before);

    proc.close().await;
    server.abort();
}

// ===========================================================================
// Read responses (U2)
// ===========================================================================

#[tokio::test]
async fn read_json_is_compact_with_scope_key() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--json"],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    let page = &v["page"];
    assert_eq!(page["page_key"], "me|page-1");
    assert_eq!(page["id"], "page-1");
    assert_eq!(page["scope"]["scope_type"], "me");
    assert!(page["title"]
        .as_str()
        .unwrap()
        .contains("Architecture Spec"));
    assert!(page["last_modified_at"].is_string());
    assert!(page["body_markdown"].is_string());
    assert_eq!(page["omitted"]["resources"], 2);
    assert_eq!(
        page["omitted"]["fields"],
        json!([
            "raw_xhtml",
            "body_plain",
            "primary_div_id",
            "summary.created_at",
            "summary.content_url",
            "summary.order",
            "summary.preview_text"
        ])
    );
    assert!(page.get("blocks").is_none());
    assert!(page.get("raw_xhtml").is_none());
    assert!(page.get("resources").is_none());
    assert!(page["hint"].as_str().unwrap().contains("--full"));

    server.abort();
}

#[tokio::test]
async fn read_json_full_matches_previous_shape() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--json", "--full"],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(json_object_keys(&v["page"]), full_page_key_set());

    server.abort();
}

#[tokio::test]
async fn read_explicit_format_json_stays_full() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    for args in [
        &["read", "me|page-1", "--format", "xhtml", "--json"][..],
        &["read", "me|page-1", "--format", "md", "--json"][..],
        &[
            "read",
            "me|page-1",
            "--format",
            "blocks",
            "--json",
            "--full",
        ][..],
    ] {
        let out = run_onenote(&base_url, &cfg_dir, &data_dir, Some("opaque-a"), args).await;
        assert!(
            out.status.success(),
            "args={args:?}, stderr={}",
            String::from_utf8_lossy(&out.stderr)
        );
        let v: Value = serde_json::from_slice(&out.stdout).unwrap();
        assert_eq!(
            json_object_keys(&v["page"]),
            full_page_key_set(),
            "args={args:?}"
        );
        assert!(
            !v["page"]["raw_xhtml"].as_str().unwrap().is_empty(),
            "args={args:?}"
        );
    }

    server.abort();
}

#[tokio::test]
async fn read_explicit_format_json_compact() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &[
            "read",
            "me|page-1",
            "--format",
            "xhtml",
            "--json",
            "--compact",
        ],
    )
    .await;
    assert!(out.status.success());
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert!(v["page"].get("omitted").is_some());
    assert!(v["page"].get("raw_xhtml").is_none());

    server.abort();
}

#[tokio::test]
async fn read_compact_alias_equals_default() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out1 = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--json"],
    )
    .await;
    let out2 = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--json", "--compact"],
    )
    .await;
    assert!(out1.status.success() && out2.status.success());
    let mut v1: Value = serde_json::from_slice(&out1.stdout).unwrap();
    let mut v2: Value = serde_json::from_slice(&out2.stdout).unwrap();
    v1["page"].as_object_mut().unwrap().remove("synced_at");
    v2["page"].as_object_mut().unwrap().remove("synced_at");
    assert_eq!(v1, v2);

    server.abort();
}

#[tokio::test]
async fn read_selector_errors() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    for args in [
        &["read", "me|page-1", "--json", "--compact", "--full"][..],
        &["read", "me|page-1", "--full"][..],
    ] {
        let out = run_onenote(&base_url, &cfg_dir, &data_dir, Some("opaque-a"), args).await;
        assert_eq!(out.status.code(), Some(2), "args={args:?}");
        assert!(out.stdout.is_empty(), "args={args:?}");
    }

    // W6: cold-cache title returning 404 explains the cause and next actions (--section / pages).
    let cold = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "Cold Cache Spec"],
    )
    .await;
    assert_eq!(cold.status.code(), Some(1));
    assert!(cold.stdout.is_empty());
    let cold_err = String::from_utf8_lossy(&cold.stderr);
    assert!(
        cold_err.contains("OneNote page not found: 'Cold Cache Spec' is neither a cached title nor a page ID."),
        "stderr={cold_err}"
    );
    assert!(cold_err.contains("--section"), "stderr={cold_err}");

    server.abort();
}

#[tokio::test]
async fn runtime_json_errors_emit_single_stdout_object() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let out_json = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|nonexistent-999", "--json"],
    )
    .await;
    assert_eq!(out_json.status.code(), Some(1));
    let v: Value = serde_json::from_slice(&out_json.stdout).unwrap_or_else(|e| {
        panic!(
            "expected single JSON error object ({e}), got: {}",
            String::from_utf8_lossy(&out_json.stdout)
        )
    });
    assert_eq!(v["ok"], false);
    assert_eq!(v["error"]["code"], "not_found");
    assert!(v["error"]["message"]
        .as_str()
        .unwrap()
        .contains("not found"));
    assert!(!out_json.stderr.is_empty());

    let out_text = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|nonexistent-999"],
    )
    .await;
    assert_eq!(out_text.status.code(), Some(1));
    assert!(out_text.stdout.is_empty());
    assert!(String::from_utf8_lossy(&out_text.stderr).contains("OneNote page not found"));

    server.abort();
}

#[tokio::test]
async fn read_text_formats_unchanged() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let md = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1"],
    )
    .await;
    assert!(md.status.success());
    let md_text = String::from_utf8_lossy(&md.stdout);
    assert!(md_text.starts_with("# "), "md_text={md_text}");
    assert!(serde_json::from_str::<Value>(&md_text).is_err());

    let xhtml = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["read", "me|page-1", "--format", "xhtml"],
    )
    .await;
    assert!(xhtml.status.success());
    let xhtml_text = String::from_utf8_lossy(&xhtml.stdout);
    assert!(xhtml_text.contains("<html"), "xhtml_text={xhtml_text}");
    assert!(serde_json::from_str::<Value>(&xhtml_text).is_err());

    server.abort();
}

#[tokio::test]
async fn write_json_is_compact_by_default_and_supports_full() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    // Default append --json is compact: no `page`, has `ok`, `page_key`, `title`, `last_modified_at`.
    let out = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["append", "me|page-1", "hello", "--json"],
    )
    .await;
    assert!(
        out.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out.stderr)
    );
    let v: Value = serde_json::from_slice(&out.stdout).unwrap();
    assert_eq!(v["ok"], true);
    assert_eq!(v["page_key"], "me|page-1");
    assert!(v["title"].is_string());
    assert!(v.get("last_modified_at").is_some());
    assert!(v.get("page").is_none());

    // append --json --full includes the full page with blocks.
    let out_full = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["append", "me|page-1", "hello", "--json", "--full"],
    )
    .await;
    assert!(
        out_full.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out_full.stderr)
    );
    let vf: Value = serde_json::from_slice(&out_full.stdout).unwrap();
    assert_eq!(vf["ok"], true);
    assert_eq!(vf["page_key"], "me|page-1");
    assert!(vf["page"].get("summary").is_some());
    assert!(vf["page"].get("blocks").is_some());
    assert!(vf["page"].get("omitted").is_none());

    // update --block-id default compact includes block_id, omits page.
    let out_upd = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &[
            "update",
            "me|page-2",
            "--block-id",
            "p:body-2",
            "--body",
            "Updated body",
            "--json",
        ],
    )
    .await;
    assert!(
        out_upd.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out_upd.stderr)
    );
    let vu: Value = serde_json::from_slice(&out_upd.stdout).unwrap();
    assert_eq!(vu["ok"], true);
    assert_eq!(vu["page_key"], "me|page-2");
    assert_eq!(vu["block_id"], "p:body-2");
    assert!(vu.get("page").is_none());

    // check default compact includes block_id & completed, omits page.
    let out_chk = run_onenote(
        &base_url,
        &cfg_dir,
        &data_dir,
        Some("opaque-a"),
        &["check", "me|page-1", "--block-id", "p:todo1", "--done", "--json"],
    )
    .await;
    assert!(
        out_chk.status.success(),
        "stderr={}",
        String::from_utf8_lossy(&out_chk.stderr)
    );
    let vc: Value = serde_json::from_slice(&out_chk.stdout).unwrap();
    assert_eq!(vc["ok"], true);
    assert_eq!(vc["page_key"], "me|page-1");
    assert_eq!(vc["block_id"], "p:todo1");
    assert_eq!(vc["completed"], true);
    assert!(vc.get("page").is_none());

    server.abort();
}

#[tokio::test]
async fn mcp_get_page_compact_default_and_full() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let mut proc = McpProc::spawn(&base_url, &cfg_dir, &data_dir, Some("opaque-a"));

    let r1 = proc
        .call(json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"get_page","arguments":{"page":"me|page-1"}}
        }))
        .await;
    assert_eq!(r1["result"]["isError"], false);
    let sc1 = &r1["result"]["structuredContent"];
    assert!(sc1["page"].get("omitted").is_some());
    assert!(sc1["page"].get("blocks").is_none());
    let parsed_text1: Value =
        serde_json::from_str(r1["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(parsed_text1, *sc1);

    let r2 = proc
        .call(json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"get_page","arguments":{"page":"me|page-1","compact":false}}
        }))
        .await;
    assert_eq!(r2["result"]["isError"], false);
    let sc2 = &r2["result"]["structuredContent"];
    assert!(sc2["page"].get("blocks").is_some());
    let parsed_text2: Value =
        serde_json::from_str(r2["result"]["content"][0]["text"].as_str().unwrap()).unwrap();
    assert_eq!(parsed_text2, *sc2);

    proc.close().await;
    server.abort();
}

#[tokio::test]
async fn mcp_write_tool_is_compact_by_default_and_supports_full() {
    let (base_url, _mock, server) = spawn_contract_mock(2, 3).await;
    let tmp = tempfile::tempdir().unwrap();
    let (cfg_dir, data_dir) = (tmp.path().join("cfg"), tmp.path().join("data"));

    let mut proc = McpProc::spawn(&base_url, &cfg_dir, &data_dir, Some("opaque-a"));
    let r = proc
        .call(json!({
            "jsonrpc":"2.0","id":1,"method":"tools/call",
            "params":{"name":"append_page","arguments":{"page":"me|page-1","markdown":"hello"}}
        }))
        .await;
    assert_eq!(r["result"]["isError"], false, "{r}");
    let sc = &r["result"]["structuredContent"];
    assert_eq!(sc["ok"], true);
    assert_eq!(sc["page_key"], "me|page-1");
    assert!(sc["title"].is_string());
    assert!(sc.get("page").is_none());

    let r_full = proc
        .call(json!({
            "jsonrpc":"2.0","id":2,"method":"tools/call",
            "params":{"name":"append_page","arguments":{"page":"me|page-1","markdown":"hello","compact":false}}
        }))
        .await;
    assert_eq!(r_full["result"]["isError"], false, "{r_full}");
    let sc_full = &r_full["result"]["structuredContent"];
    assert_eq!(sc_full["ok"], true);
    assert_eq!(sc_full["page_key"], "me|page-1");
    assert!(sc_full["page"].get("summary").is_some());
    assert!(sc_full["page"].get("blocks").is_some());
    assert!(sc_full["page"].get("omitted").is_none());

    let r_todo = proc
        .call(json!({
            "jsonrpc":"2.0","id":3,"method":"tools/call",
            "params":{"name":"toggle_todo","arguments":{"page":"me|page-1","block_id":"p:todo1","completed":true}}
        }))
        .await;
    assert_eq!(r_todo["result"]["isError"], false, "{r_todo}");
    let sc_todo = &r_todo["result"]["structuredContent"];
    assert_eq!(sc_todo["ok"], true);
    assert_eq!(sc_todo["page_key"], "me|page-1");
    assert_eq!(sc_todo["block_id"], "p:todo1");
    assert_eq!(sc_todo["completed"], true);
    assert!(sc_todo.get("page").is_none());

    proc.close().await;
    server.abort();
}

// ===========================================================================
// Lists (U3), whoami (U4), HTTP (U5)
// ===========================================================================

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
