//! Local HTTP JSON API server (`onenote serve`, default `127.0.0.1:8793`).
//!
//! Endpoints:
//! - `GET /health`
//! - `GET /v1/whoami`
//! - `GET/POST /v1/onenote/notebooks`
//! - `GET/POST /v1/onenote/sections`
//! - `GET/POST /v1/onenote/pages`
//! - `GET/PATCH/DELETE /v1/onenote/pages/{page_key}`
//! - `POST /v1/onenote/pages/{page_key}/append`
//! - `POST /v1/onenote/pages/{page_key}/todo` (alias: `/toggle-todo`)
//! - `POST /v1/onenote/resources/download` (alias: `/pages/{page_key}/resources/download`)
//! - `GET /v1/onenote/search`
//!
//! (Also mounted under `/onenote/*` for ergonomic unversioned access.)
//!
//! Access control (every route, including `/health`, passes the request guard):
//! - `Host` must be a loopback name, the `--bind` value or an `--allowed-host`
//!   (anything else is answered with `421`), which stops DNS-rebinding pages.
//! - A request carrying an `Origin` header is refused with `403` unless that
//!   exact origin was listed with `--cors-origin`. There is no wildcard CORS.
//! - `/v1/*` and `/onenote/*` need the API key. `run` generates a random key
//!   for loopback binds when none is given; `--allow-keyless` opts out.

use std::{net::IpAddr, path::PathBuf, sync::Arc};

use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
use rand::RngCore;
use sha2::{Digest, Sha256};

use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{
        header::{self, HeaderValue},
        HeaderMap, Method, Request, StatusCode,
    },
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;

use crate::{cli, config::AppConfig, graph::GraphClient, resolver, storage::Storage};

pub struct AppState {
    pub config: AppConfig,
    pub graph: GraphClient,
    pub storage: Storage,
    pub api_key: Option<String>,
}

#[derive(Debug)]
struct ApiError {
    status: StatusCode,
    message: String,
}

impl ApiError {
    fn bad_request(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::BAD_REQUEST,
            message: msg.into(),
        }
    }

    fn not_found(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::NOT_FOUND,
            message: msg.into(),
        }
    }

    fn conflict(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::CONFLICT,
            message: msg.into(),
        }
    }

    fn internal(msg: impl Into<String>) -> Self {
        Self {
            status: StatusCode::INTERNAL_SERVER_ERROR,
            message: msg.into(),
        }
    }
}

impl IntoResponse for ApiError {
    fn into_response(self) -> Response {
        (
            self.status,
            Json(json!({ "ok": false, "error": self.message })),
        )
            .into_response()
    }
}

fn onenote_api_error(err: anyhow::Error) -> ApiError {
    let msg = err.to_string();
    let lower = msg.to_ascii_lowercase();
    if lower.starts_with("ambiguous ") {
        ApiError::bad_request(msg)
    } else if lower.contains("conflict:") || lower.contains("conflict") {
        ApiError::conflict(msg)
    } else if lower.contains("not found") || lower.contains("404") {
        ApiError::not_found(msg)
    } else if lower.contains("empty")
        || lower.contains("ambiguous")
        || lower.contains("required")
        || lower.contains("out of range")
        || lower.contains("does not match")
        || lower.contains("no onenote to-do")
        || lower.contains("no embedded")
        || lower.contains("cannot specify both")
    {
        ApiError::bad_request(msg)
    } else {
        ApiError::internal(msg)
    }
}

#[derive(Debug, Default, Deserialize)]
pub struct NotebooksQuery {
    #[serde(default)]
    pub include_teams: bool,
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateNotebookBody {
    pub name: String,
    #[serde(default)]
    pub group_id: Option<String>,
    #[serde(default)]
    pub site_id: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct SectionsQuery {
    #[serde(default)]
    pub notebook: Option<String>,
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreateSectionBody {
    pub notebook: String,
    pub name: String,
}

#[derive(Debug, Deserialize)]
pub struct PagesQuery {
    pub section: String,
    #[serde(default = "default_page_limit")]
    pub limit: usize,
    /// `next_cursor` from the previous response (an offset into the section).
    #[serde(default)]
    pub cursor: Option<String>,
    #[serde(default)]
    pub refresh: bool,
    #[serde(default)]
    pub offline: bool,
}

fn default_page_limit() -> usize {
    50
}

#[derive(Debug, Default, Deserialize)]
pub struct GetPageQuery {
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default)]
    pub offline: bool,
}

#[derive(Debug, Deserialize)]
pub struct CreatePageBody {
    pub section: String,
    pub title: String,
    #[serde(default)]
    pub markdown: String,
    #[serde(default)]
    pub image_paths: Vec<String>,
    #[serde(default)]
    pub level: Option<i32>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DeletePageQuery {
    #[serde(default)]
    pub section: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct PatchPageBody {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub markdown: Option<String>,
    #[serde(default)]
    pub block_id: Option<String>,
    #[serde(default, alias = "base_content")]
    pub expected_content: Option<String>,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub section: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AppendPageBody {
    #[serde(default)]
    pub markdown: String,
    #[serde(default)]
    pub image_paths: Option<Vec<String>>,
    #[serde(default)]
    pub images: Option<Vec<String>>,
    #[serde(default)]
    pub section: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ToggleTodoBody {
    #[serde(default)]
    pub block_id: Option<String>,
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub completed: Option<bool>,
    #[serde(default, alias = "base_content")]
    pub expected_content: Option<String>,
    #[serde(default)]
    pub force: bool,
    #[serde(default)]
    pub section: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DownloadResourceBody {
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub out_dir: Option<String>,
    #[serde(default)]
    pub out: Option<String>,
    #[serde(default)]
    pub section: Option<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DownloadResourceByBody {
    #[serde(default)]
    pub page: String,
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub url: Option<String>,
    #[serde(default)]
    pub out_dir: Option<String>,
    #[serde(default)]
    pub out: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct SearchQuery {
    pub q: String,
    #[serde(default)]
    pub section: Option<String>,
    #[serde(default)]
    pub refresh: bool,
    #[serde(default = "default_search_limit")]
    pub limit: usize,
}

fn default_search_limit() -> usize {
    30
}

/// Request guard for the HTTP API: which `Host` values the server answers to
/// and which browser origins may call it. Both lists are exact matches.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ServeGuard {
    /// Normalised host names (lower-case, no port, IPv6 in brackets).
    pub allowed_hosts: Vec<String>,
    /// Normalised origins (`scheme://host[:port]`). Empty means no browser
    /// origin is accepted.
    pub allowed_origins: Vec<String>,
}

impl Default for ServeGuard {
    fn default() -> Self {
        Self::loopback()
    }
}

impl ServeGuard {
    /// Loopback names only, no browser origins.
    pub fn loopback() -> Self {
        Self {
            allowed_hosts: vec![
                "127.0.0.1".to_string(),
                "localhost".to_string(),
                "[::1]".to_string(),
            ],
            allowed_origins: Vec::new(),
        }
    }

    /// Loopback names plus the literal `--bind` value, extra `--allowed-host`
    /// names and the exact `--cors-origin` list.
    pub fn new(bind: &str, extra_hosts: &[String], cors_origins: &[String]) -> Result<Self> {
        let mut guard = Self::loopback();
        for raw in std::iter::once(bind).chain(extra_hosts.iter().map(String::as_str)) {
            let host = normalize_allowed_host(raw)
                .with_context(|| format!("Invalid host name '{raw}' for the HTTP API server"))?;
            if !guard.allowed_hosts.contains(&host) {
                guard.allowed_hosts.push(host);
            }
        }
        for raw in cors_origins {
            let origin = normalize_origin(raw).with_context(|| {
                format!(
                    "Invalid --cors-origin '{raw}': expected an exact origin such as https://app.example.com (wildcards are not supported)"
                )
            })?;
            if !guard.allowed_origins.contains(&origin) {
                guard.allowed_origins.push(origin);
            }
        }
        Ok(guard)
    }

    fn allows_host(&self, host_header: &str) -> bool {
        host_without_port(host_header).is_some_and(|host| self.allowed_hosts.contains(&host))
    }

    fn allows_origin(&self, origin_header: &str) -> bool {
        normalize_origin(origin_header).is_ok_and(|origin| self.allowed_origins.contains(&origin))
    }
}

/// Extracts the host part of a `Host` header (or URI authority): lower-case,
/// port and one trailing dot removed, IPv6 literals kept in brackets.
/// Returns `None` for anything that is not `host`, `host:port`, `[v6]` or
/// `[v6]:port`.
fn host_without_port(raw: &str) -> Option<String> {
    let value = raw.trim().to_ascii_lowercase();
    if value.is_empty() || value.contains(['/', '@', ' ', '\\']) {
        return None;
    }
    if let Some(rest) = value.strip_prefix('[') {
        let (addr, tail) = rest.split_once(']')?;
        if !is_port_suffix(tail) {
            return None;
        }
        let ip: std::net::Ipv6Addr = addr.parse().ok()?;
        return Some(format!("[{ip}]"));
    }
    let host = match value.rsplit_once(':') {
        Some((host, port)) => {
            if host.contains(':') || port.is_empty() || !port.bytes().all(|b| b.is_ascii_digit()) {
                return None;
            }
            host
        }
        None => value.as_str(),
    };
    let host = host.strip_suffix('.').unwrap_or(host);
    if host.is_empty() {
        return None;
    }
    Some(host.to_string())
}

fn is_port_suffix(tail: &str) -> bool {
    match tail.strip_prefix(':') {
        Some(port) => !port.is_empty() && port.bytes().all(|b| b.is_ascii_digit()),
        None => tail.is_empty(),
    }
}

/// Normalises a configured host (`--bind` / `--allowed-host`). A bare IPv6
/// address is accepted and stored in its bracketed form.
fn normalize_allowed_host(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    if let Ok(ip) = trimmed.parse::<std::net::Ipv6Addr>() {
        return Ok(format!("[{ip}]"));
    }
    host_without_port(trimmed).context("expected a host name or IP address")
}

/// Normalises an origin to `scheme://host[:port]`. Only `http`/`https` origins
/// without a path, query or credentials are accepted.
fn normalize_origin(raw: &str) -> Result<String> {
    let trimmed = raw.trim();
    let parsed = url::Url::parse(trimmed).context("not a valid URL")?;
    if !matches!(parsed.scheme(), "http" | "https") {
        bail!("only http and https origins are supported");
    }
    if parsed
        .host_str()
        .is_none_or(|h| h.is_empty() || h.contains('*'))
    {
        bail!("origin has no concrete host");
    }
    if !matches!(parsed.path(), "" | "/")
        || parsed.query().is_some()
        || parsed.fragment().is_some()
        || !parsed.username().is_empty()
        || parsed.password().is_some()
    {
        bail!("an origin has no path, query or credentials");
    }
    Ok(parsed.origin().ascii_serialization())
}

/// Compares two secrets without an early exit on the first differing byte.
/// Both sides are hashed first so the comparison length is fixed.
fn constant_time_eq(a: &str, b: &str) -> bool {
    let da = Sha256::digest(a.as_bytes());
    let db = Sha256::digest(b.as_bytes());
    da.iter()
        .zip(db.iter())
        .fold(0u8, |acc, (x, y)| acc | (x ^ y))
        == 0
}

/// How the API key for this run was decided.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum KeyPolicy {
    /// `--api-key` or `ONENOTE_API_KEY`.
    Provided(String),
    /// Loopback bind with no key: a random key is generated for this run.
    Generated,
    /// Loopback bind with `--allow-keyless`.
    Keyless,
}

/// Decides the key policy. Pure so it can be tested without binding a socket.
///
/// `ONENOTE_ALLOW_INSECURE_SERVE` is deliberately not an input: a non-loopback
/// bind always needs a key.
pub fn resolve_key_policy(
    bind: &str,
    is_loopback: bool,
    cli_key: Option<&str>,
    env_key: Option<&str>,
    allow_keyless: bool,
) -> Result<KeyPolicy> {
    let provided = match (cli_key, env_key) {
        (Some(key), _) => Some(("--api-key", key)),
        (None, Some(key)) => Some(("ONENOTE_API_KEY", key)),
        (None, None) => None,
    };
    if let Some((source, key)) = provided {
        let key = key.trim();
        if key.is_empty() {
            bail!(
                "{source} is empty. Pass a non-empty secret, or use `--allow-keyless` on a loopback bind to run without a key."
            );
        }
        if allow_keyless {
            bail!("`--allow-keyless` cannot be combined with an API key ({source} is set).");
        }
        return Ok(KeyPolicy::Provided(key.to_string()));
    }
    if !is_loopback {
        bail!(
            "Refusing to bind non-loopback address '{bind}' without an API key. Pass `--api-key <SECRET>` or set ONENOTE_API_KEY. (ONENOTE_ALLOW_INSECURE_SERVE and `--allow-keyless` do not apply to non-loopback binds.)"
        );
    }
    if allow_keyless {
        Ok(KeyPolicy::Keyless)
    } else {
        Ok(KeyPolicy::Generated)
    }
}

fn generate_api_key() -> String {
    let mut buf = [0u8; 32];
    rand::thread_rng().fill_bytes(&mut buf);
    URL_SAFE_NO_PAD.encode(buf)
}

fn is_loopback_bind(bind: &str) -> bool {
    let bare = bind.trim().trim_start_matches('[').trim_end_matches(']');
    bare.parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or_else(|_| bare.eq_ignore_ascii_case("localhost"))
}

/// Options for [`run`].
#[derive(Debug, Clone)]
pub struct ServeOptions {
    pub bind: String,
    pub port: u16,
    /// `--api-key` / `--token`.
    pub api_key: Option<String>,
    /// `--allow-keyless`: run a loopback server without a key.
    pub allow_keyless: bool,
    /// Extra `Host` values to answer to (`--allowed-host`).
    pub allowed_hosts: Vec<String>,
    /// Exact browser origins allowed to call the API (`--cors-origin`).
    pub cors_origins: Vec<String>,
}

/// Router with the loopback-default guard: loopback `Host` names only and no
/// browser origins. The API key is whatever `state.api_key` holds; the
/// "key by default" rule lives in [`run`].
pub fn build_router(state: Arc<AppState>) -> Router {
    build_router_with_guard(state, ServeGuard::loopback())
}

pub fn build_router_with_guard(state: Arc<AppState>, guard: ServeGuard) -> Router {
    let protected = Router::new()
        .route("/v1/whoami", get(whoami))
        .nest("/v1/onenote", onenote_routes())
        .nest("/onenote", onenote_routes())
        .route_layer(middleware::from_fn_with_state(
            state.clone(),
            require_api_key,
        ));

    Router::new()
        .route("/health", get(health))
        .merge(protected)
        // Outermost layer, so it also covers `/health`, unknown paths and
        // CORS preflight requests.
        .layer(middleware::from_fn_with_state(
            Arc::new(guard),
            guard_request,
        ))
        .with_state(state)
}

fn guard_error(status: StatusCode, message: &str) -> Response {
    (status, Json(json!({ "ok": false, "error": message }))).into_response()
}

/// `Host` allow-list and `Origin` check. Runs before routing and before the
/// API-key check.
async fn guard_request(
    State(guard): State<Arc<ServeGuard>>,
    req: Request<Body>,
    next: Next,
) -> Response {
    // HTTP/1.1 clients send `Host`; HTTP/2 carries it in the URI authority.
    // A request with neither is not a browser request (browsers always send
    // one), so it cannot be a DNS-rebinding attempt and is let through.
    let host = req
        .headers()
        .get(header::HOST)
        .map(|v| v.to_str().unwrap_or("\u{0}").to_string())
        .or_else(|| req.uri().authority().map(|a| a.as_str().to_string()));
    if let Some(host) = host {
        if !guard.allows_host(&host) {
            return guard_error(
                StatusCode::MISDIRECTED_REQUEST,
                "Misdirected request: this Host is not served. Use the address the server listens on, or start it with --allowed-host <HOST>.",
            );
        }
    }

    let Some(origin_value) = req.headers().get(header::ORIGIN).cloned() else {
        return next.run(req).await;
    };
    let allowed = origin_value
        .to_str()
        .is_ok_and(|origin| guard.allows_origin(origin));
    if !allowed {
        return guard_error(
            StatusCode::FORBIDDEN,
            "Forbidden: cross-origin requests are not allowed. Start the server with --cors-origin <ORIGIN> to allow this origin.",
        );
    }

    let is_preflight = req.method() == Method::OPTIONS
        && req
            .headers()
            .contains_key(header::ACCESS_CONTROL_REQUEST_METHOD);
    let mut response = if is_preflight {
        let mut preflight = StatusCode::NO_CONTENT.into_response();
        let headers = preflight.headers_mut();
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_METHODS,
            HeaderValue::from_static("GET, POST, PATCH, DELETE, OPTIONS"),
        );
        headers.insert(
            header::ACCESS_CONTROL_ALLOW_HEADERS,
            HeaderValue::from_static("authorization, content-type, x-api-key"),
        );
        headers.insert(
            header::ACCESS_CONTROL_MAX_AGE,
            HeaderValue::from_static("600"),
        );
        preflight
    } else {
        next.run(req).await
    };
    // Echo exactly the configured origin that matched; never `*`.
    response
        .headers_mut()
        .insert(header::ACCESS_CONTROL_ALLOW_ORIGIN, origin_value);
    response
        .headers_mut()
        .append(header::VARY, HeaderValue::from_static("Origin"));
    response
}

fn onenote_routes() -> Router<Arc<AppState>> {
    Router::new()
        .route("/notebooks", get(list_notebooks).post(create_notebook))
        .route("/sections", get(list_sections).post(create_section))
        .route("/pages", get(list_pages).post(create_page))
        .route(
            "/pages/{page_key}",
            get(get_page).patch(patch_page).delete(delete_page),
        )
        .route("/pages/{page_key}/append", post(append_page))
        .route("/pages/{page_key}/todo", post(toggle_todo))
        .route("/pages/{page_key}/toggle-todo", post(toggle_todo))
        .route("/resources/download", post(download_resource_by_body))
        .route(
            "/pages/{page_key}/resources/download",
            post(download_resource),
        )
        .route("/search", get(search_pages))
}

async fn require_api_key(
    State(state): State<Arc<AppState>>,
    headers: HeaderMap,
    req: Request<Body>,
    next: Next,
) -> Response {
    if let Some(expected) = state.api_key.as_deref() {
        let bearer = headers
            .get(header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(str::trim);
        let x_api_key = headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(str::trim);
        // An empty configured key never matches: it must not mean "no key".
        // Both headers are always compared so timing does not reveal which
        // one was used.
        let bearer_ok = bearer.is_some_and(|got| constant_time_eq(got, expected));
        let x_api_key_ok = x_api_key.is_some_and(|got| constant_time_eq(got, expected));
        if expected.is_empty() || !(bearer_ok | x_api_key_ok) {
            return (
                StatusCode::UNAUTHORIZED,
                Json(json!({ "ok": false, "error": "Unauthorized: invalid or missing API key" })),
            )
                .into_response();
        }
    }
    next.run(req).await
}

pub async fn run(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    options: ServeOptions,
) -> Result<()> {
    let bind = options.bind.trim();
    let port = options.port;
    let env_key = std::env::var("ONENOTE_API_KEY").ok();
    let policy = resolve_key_policy(
        bind,
        is_loopback_bind(bind),
        options.api_key.as_deref(),
        env_key.as_deref(),
        options.allow_keyless,
    )?;
    let guard = ServeGuard::new(bind, &options.allowed_hosts, &options.cors_origins)?;

    let (api_key, generated) = match policy {
        KeyPolicy::Provided(key) => (Some(key), false),
        KeyPolicy::Generated => (Some(generate_api_key()), true),
        KeyPolicy::Keyless => (None, false),
    };

    let state = Arc::new(AppState {
        config,
        graph,
        storage,
        api_key: api_key.clone(),
    });
    let app = build_router_with_guard(state, guard);
    let addr = if bind.contains(':') && !bind.starts_with('[') {
        format!("[{bind}]:{port}")
    } else {
        format!("{bind}:{port}")
    };
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind HTTP server on {addr}"))?;
    // Everything below goes to stderr; stdout stays free for callers.
    eprintln!(
        "onenote HTTP API server listening on http://{addr} (v{})",
        env!("CARGO_PKG_VERSION")
    );
    match (&api_key, generated) {
        (Some(key), true) => {
            eprintln!(
                "API key generated for this run (shown once). Send it with every /v1 request:"
            );
            eprintln!("  Authorization: Bearer {key}");
            eprintln!(
                "Use --api-key <SECRET> or ONENOTE_API_KEY to choose the key, or --allow-keyless to run without one."
            );
        }
        (Some(_), false) => {
            eprintln!("API key required (Authorization: Bearer <key> or x-api-key).");
        }
        (None, _) => {
            eprintln!(
                "Warning: running without an API key (--allow-keyless). Any local process can read, edit and delete your notebooks through this server."
            );
        }
    }
    axum::serve(listener, app).await?;
    Ok(())
}

async fn health() -> Json<Value> {
    Json(json!({
        "ok": true,
        "service": "onenote-cli",
        "version": env!("CARGO_PKG_VERSION"),
    }))
}

async fn whoami(State(state): State<Arc<AppState>>) -> Result<Json<Value>, ApiError> {
    let me = state.graph.get_me().await.map_err(onenote_api_error)?;
    Ok(Json(json!({ "ok": true, "user": me })))
}

async fn list_notebooks(
    State(state): State<Arc<AppState>>,
    Query(query): Query<NotebooksQuery>,
) -> Result<Json<Value>, ApiError> {
    let notebooks = cli::load_onenote_notebooks(
        &state.storage,
        &state.graph,
        query.include_teams,
        query.refresh,
        query.offline,
    )
    .await
    .map_err(onenote_api_error)?;
    Ok(Json(json!({ "ok": true, "notebooks": notebooks })))
}

async fn create_notebook(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateNotebookBody>,
) -> Result<Json<Value>, ApiError> {
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("Notebook name is required."));
    }
    let scope = cli::build_onenote_scope_from_args(body.group_id.as_deref(), body.site_id.as_deref())
        .map_err(onenote_api_error)?;
    let created = state
        .graph
        .create_onenote_notebook(&scope, body.name.trim())
        .await
        .map_err(onenote_api_error)?;
    let created_for_db = created.clone();
    let _ = state
        .storage
        .blocking(move |s| s.upsert_onenote_notebooks(std::slice::from_ref(&created_for_db)))
        .await;
    Ok(Json(json!({ "ok": true, "notebook": created })))
}

async fn list_sections(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SectionsQuery>,
) -> Result<Json<Value>, ApiError> {
    let (notebook, sections) = cli::load_onenote_sections(
        &state.storage,
        &state.graph,
        query.notebook.as_deref(),
        query.refresh,
        query.offline,
    )
    .await
    .map_err(onenote_api_error)?;
    Ok(Json(json!({
        "ok": true,
        "notebook": notebook,
        "sections": sections,
    })))
}

async fn create_section(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreateSectionBody>,
) -> Result<Json<Value>, ApiError> {
    if body.name.trim().is_empty() {
        return Err(ApiError::bad_request("Section name is required."));
    }
    let nb = resolver::resolve_notebook(&state.graph, &state.storage, &body.notebook, false)
        .await
        .map_err(onenote_api_error)?;
    let created = state
        .graph
        .create_onenote_section(&nb.scope, &nb.id, body.name.trim())
        .await
        .map_err(onenote_api_error)?;
    let created_for_db = created.clone();
    let _ = state
        .storage
        .blocking(move |s| s.upsert_onenote_sections(std::slice::from_ref(&created_for_db)))
        .await;
    Ok(Json(json!({
        "ok": true,
        "notebook": nb,
        "section": created,
    })))
}

async fn list_pages(
    State(state): State<Arc<AppState>>,
    Query(query): Query<PagesQuery>,
) -> Result<Json<Value>, ApiError> {
    let offset = match query.cursor.as_deref().map(str::trim) {
        None | Some("") => 0,
        Some(raw) => raw.parse::<usize>().map_err(|_| {
            ApiError::bad_request(
                "Invalid cursor: pass the next_cursor value from the previous response.",
            )
        })?,
    };
    let (section, pages, has_more) = cli::list_onenote_pages_with_cache(
        &state.storage,
        &state.graph,
        &query.section,
        query.limit,
        offset,
        query.refresh,
        query.offline,
        false,
    )
    .await
    .map_err(onenote_api_error)?;
    Ok(Json(json!({
        "ok": true,
        "section": section,
        "pages": pages,
        "returned": pages.len(),
        "has_more": has_more,
        "next_cursor": cli::next_cursor(offset, query.limit, has_more),
    })))
}

async fn create_page(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreatePageBody>,
) -> Result<Json<Value>, ApiError> {
    if body.title.trim().is_empty() {
        return Err(ApiError::bad_request("Page title is required."));
    }
    crate::graph::onenote::validate_page_creation_level(body.level)
        .map_err(|err| ApiError::bad_request(err.to_string()))?;
    let paths: Vec<PathBuf> = body.image_paths.iter().map(PathBuf::from).collect();
    let inline_images = tokio::task::spawn_blocking(move || cli::load_inline_images_from_paths(&paths))
        .await
        .map_err(|err| ApiError::internal(format!("Failed to load inline images: {err}")))?
        .map_err(onenote_api_error)?;
    let (section, page) = cli::create_onenote_page_helper(
        &state.storage,
        &state.graph,
        &body.section,
        &body.title,
        &body.markdown,
        inline_images,
        body.level,
    )
    .await
    .map_err(onenote_api_error)?;
    Ok(Json(json!({
        "ok": true,
        "section": section,
        "page": page,
    })))
}

async fn get_page(
    State(state): State<Arc<AppState>>,
    AxumPath(page_key): AxumPath<String>,
    Query(query): Query<GetPageQuery>,
) -> Result<Json<Value>, ApiError> {
    let resolved = cli::resolve_page_query_in_section(
        &state.storage,
        &state.graph,
        &page_key,
        query.section.as_deref(),
        query.offline,
    )
    .await
    .map_err(onenote_api_error)?;
    let page = cli::load_onenote_page_detail(&state.storage, &state.graph, &resolved, query.offline)
        .await
        .map_err(onenote_api_error)?;
    Ok(Json(json!({ "ok": true, "page": page })))
}

async fn patch_page(
    State(state): State<Arc<AppState>>,
    AxumPath(page_key): AxumPath<String>,
    Json(body): Json<PatchPageBody>,
) -> Result<Json<Value>, ApiError> {
    let target_page = cli::resolve_page_query_in_section(
        &state.storage,
        &state.graph,
        &page_key,
        body.section.as_deref(),
        false,
    )
    .await
    .map_err(onenote_api_error)?;
    let updated = cli::update_onenote_page_helper_with_guard(
        &state.storage,
        &state.graph,
        &target_page,
        body.title.as_deref(),
        body.markdown.as_deref(),
        body.block_id.as_deref(),
        body.expected_content.as_deref(),
        body.force,
    )
    .await
    .map_err(onenote_api_error)?;
    cli::nudge_live_daemon(
        &state.config,
        &state.graph,
        crate::sync::SyncCommand::HydratePage {
            page_key: updated.summary.page_key.clone(),
        },
    )
    .await;
    if !updated.summary.section_id.trim().is_empty() {
        cli::nudge_live_daemon(
            &state.config,
            &state.graph,
            crate::sync::SyncCommand::RefreshSectionPages {
                section_key: updated
                    .summary
                    .scope
                    .make_entity_key(&updated.summary.section_id),
            },
        )
        .await;
    }
    Ok(Json(json!({ "ok": true, "page": updated })))
}

async fn delete_page(
    State(state): State<Arc<AppState>>,
    AxumPath(page_key): AxumPath<String>,
    Query(query): Query<DeletePageQuery>,
) -> Result<Json<Value>, ApiError> {
    let target_page = cli::resolve_page_query_in_section(
        &state.storage,
        &state.graph,
        &page_key,
        query.section.as_deref(),
        false,
    )
    .await
    .map_err(onenote_api_error)?;
    let target_for_resolve = target_page.clone();
    let section_key = state
        .storage
        .blocking(move |s| {
            resolver::resolve_page_cached(s, &target_for_resolve, None)
                .and_then(resolver::PageResolution::into_option)
        })
        .await
        .map_err(onenote_api_error)?
        .map(|s| s.scope.make_entity_key(&s.section_id));
    let deleted = cli::delete_onenote_page_helper(&state.storage, &state.graph, &target_page)
        .await
        .map_err(onenote_api_error)?;
    let nudge_cmd = match section_key {
        Some(sec_key) => crate::sync::SyncCommand::RefreshSectionPages {
            section_key: sec_key,
        },
        None => crate::sync::SyncCommand::RefreshAll,
    };
    cli::nudge_live_daemon(&state.config, &state.graph, nudge_cmd).await;
    Ok(Json(json!({ "ok": true, "deleted": deleted })))
}

async fn append_page(
    State(state): State<Arc<AppState>>,
    AxumPath(page_key): AxumPath<String>,
    Json(body): Json<AppendPageBody>,
) -> Result<Json<Value>, ApiError> {
    if body.image_paths.is_some() && body.images.is_some() {
        return Err(ApiError::bad_request(
            "Cannot specify both 'image_paths' and 'images'; provide only one.",
        ));
    }
    let raw_image_paths = body.image_paths.or(body.images).unwrap_or_default();
    let paths: Vec<PathBuf> = raw_image_paths.iter().map(PathBuf::from).collect();
    let inline_images = tokio::task::spawn_blocking(move || cli::load_inline_images_from_paths(&paths))
        .await
        .map_err(|err| ApiError::internal(format!("Failed to load inline images: {err}")))?
        .map_err(onenote_api_error)?;
    let target_page = cli::resolve_page_query_in_section(
        &state.storage,
        &state.graph,
        &page_key,
        body.section.as_deref(),
        false,
    )
    .await
    .map_err(onenote_api_error)?;
    let updated = cli::append_onenote_page_helper(
        &state.storage,
        &state.graph,
        &target_page,
        &body.markdown,
        &inline_images,
    )
    .await
    .map_err(onenote_api_error)?;
    cli::nudge_live_daemon(
        &state.config,
        &state.graph,
        crate::sync::SyncCommand::HydratePage {
            page_key: updated.summary.page_key.clone(),
        },
    )
    .await;
    Ok(Json(json!({ "ok": true, "page": updated })))
}

async fn toggle_todo(
    State(state): State<Arc<AppState>>,
    AxumPath(page_key): AxumPath<String>,
    Json(body): Json<ToggleTodoBody>,
) -> Result<Json<Value>, ApiError> {
    let target_page = cli::resolve_page_query_in_section(
        &state.storage,
        &state.graph,
        &page_key,
        body.section.as_deref(),
        false,
    )
    .await
    .map_err(onenote_api_error)?;
    let (updated, block_id, completed) = cli::change_onenote_todo_helper_with_guard(
        &state.storage,
        &state.graph,
        &target_page,
        body.block_id.as_deref(),
        body.index,
        body.completed,
        body.expected_content.as_deref(),
        body.force,
    )
    .await
    .map_err(onenote_api_error)?;
    cli::nudge_live_daemon(
        &state.config,
        &state.graph,
        crate::sync::SyncCommand::HydratePage {
            page_key: updated.summary.page_key.clone(),
        },
    )
    .await;
    Ok(Json(json!({
        "ok": true,
        "page_key": updated.summary.page_key,
        "block_id": block_id,
        "completed": completed,
        "page": updated,
    })))
}

fn resolve_out_dir_alias(
    out_dir: Option<String>,
    out: Option<String>,
) -> Result<Option<PathBuf>, ApiError> {
    if out_dir.is_some() && out.is_some() {
        return Err(ApiError::bad_request(
            "Cannot specify both 'out_dir' and 'out'; provide only one.",
        ));
    }
    Ok(out_dir
        .or(out)
        .as_deref()
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(PathBuf::from))
}

async fn download_resource(
    State(state): State<Arc<AppState>>,
    AxumPath(page_key): AxumPath<String>,
    Json(body): Json<DownloadResourceBody>,
) -> Result<Json<Value>, ApiError> {
    let out_dir = resolve_out_dir_alias(body.out_dir, body.out)?;
    let (_detail, _idx, downloaded) = cli::download_onenote_resource_helper_with_options(
        &state.config,
        &state.storage,
        &state.graph,
        &page_key,
        body.section.as_deref(),
        body.index,
        body.url.as_deref(),
        out_dir.as_deref(),
    )
    .await
    .map_err(onenote_api_error)?;
    Ok(Json(serde_json::to_value(downloaded).unwrap_or_else(|_| json!({ "ok": true }))))
}

async fn download_resource_by_body(
    State(state): State<Arc<AppState>>,
    Json(body): Json<DownloadResourceByBody>,
) -> Result<Json<Value>, ApiError> {
    if body.page.trim().is_empty() {
        return Err(ApiError::bad_request("Page ID or title is required."));
    }
    let out_dir = resolve_out_dir_alias(body.out_dir, body.out)?;
    let (_detail, _idx, downloaded) = cli::download_onenote_resource_helper_with_options(
        &state.config,
        &state.storage,
        &state.graph,
        &body.page,
        body.section.as_deref(),
        body.index,
        body.url.as_deref(),
        out_dir.as_deref(),
    )
    .await
    .map_err(onenote_api_error)?;
    Ok(Json(serde_json::to_value(downloaded).unwrap_or_else(|_| json!({ "ok": true }))))
}

async fn search_pages(
    State(state): State<Arc<AppState>>,
    Query(query): Query<SearchQuery>,
) -> Result<Json<Value>, ApiError> {
    let hits = cli::search_onenote_helper(
        &state.storage,
        &state.graph,
        &query.q,
        query.section.as_deref(),
        query.refresh,
        query.limit,
    )
    .await
    .map_err(onenote_api_error)?;
    Ok(Json(json!({
        "ok": true,
        "query": query.q,
        "hits": hits,
    })))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn host_without_port_parses_exact_hosts() {
        assert_eq!(
            host_without_port("127.0.0.1:8793").as_deref(),
            Some("127.0.0.1")
        );
        assert_eq!(host_without_port("localhost").as_deref(), Some("localhost"));
        assert_eq!(
            host_without_port("LocalHost.:8793").as_deref(),
            Some("localhost")
        );
        assert_eq!(host_without_port("[::1]:8793").as_deref(), Some("[::1]"));
        assert_eq!(host_without_port("[::1]").as_deref(), Some("[::1]"));
        assert_eq!(
            host_without_port("[0:0:0:0:0:0:0:1]:80").as_deref(),
            Some("[::1]")
        );
        assert_eq!(
            host_without_port("evil.example:8793").as_deref(),
            Some("evil.example")
        );
        assert_eq!(
            host_without_port("127.0.0.1.evil.example").as_deref(),
            Some("127.0.0.1.evil.example")
        );
        for bad in [
            "",
            "::1",
            "[::1",
            "[::1]x",
            "host:port",
            "a:1:2",
            "user@localhost",
            "localhost:",
        ] {
            assert_eq!(host_without_port(bad), None, "{bad:?} must be rejected");
        }
    }

    #[test]
    fn loopback_guard_matches_hosts_exactly() {
        let guard = ServeGuard::loopback();
        for ok in [
            "127.0.0.1:8793",
            "localhost:8793",
            "LOCALHOST",
            "[::1]:8793",
            "localhost.",
        ] {
            assert!(guard.allows_host(ok), "{ok} should be allowed");
        }
        for bad in [
            "evil.example:8793",
            "127.0.0.1.evil.example",
            "localhost.evil.example:8793",
            "evil-localhost",
            "127.0.0.2",
            "",
        ] {
            assert!(!guard.allows_host(bad), "{bad} should be refused");
        }
    }

    #[test]
    fn guard_new_adds_bind_extra_hosts_and_exact_origins() {
        let guard = ServeGuard::new(
            "0.0.0.0",
            &["Notes.Corp.Example".to_string(), "::1".to_string()],
            &["https://App.Example.com/".to_string()],
        )
        .unwrap();
        assert!(guard.allows_host("0.0.0.0:8793"));
        assert!(guard.allows_host("notes.corp.example:8793"));
        assert!(!guard.allows_host("other.corp.example"));
        assert!(guard.allows_origin("https://app.example.com"));
        assert!(!guard.allows_origin("http://app.example.com"));
        assert!(!guard.allows_origin("https://app.example.com:8443"));
        assert!(!guard.allows_origin("https://app.example.com.evil.example"));
        assert!(!guard.allows_origin("null"));

        assert!(ServeGuard::loopback().allowed_origins.is_empty());
        assert!(!ServeGuard::loopback().allows_origin("http://127.0.0.1:8793"));

        for bad in [
            "*",
            "https://*.example.com",
            "app.example.com",
            "https://a.example/path",
            "file:///x",
        ] {
            assert!(
                ServeGuard::new("127.0.0.1", &[], &[bad.to_string()]).is_err(),
                "--cors-origin {bad} must be rejected"
            );
        }
    }

    #[test]
    fn constant_time_eq_compares_whole_values() {
        assert!(constant_time_eq("secret-key", "secret-key"));
        assert!(!constant_time_eq("secret-key", "secret-kez"));
        assert!(!constant_time_eq("secret-key", "secret"));
        assert!(!constant_time_eq("", "secret"));
        assert!(constant_time_eq("", ""));
    }

    #[test]
    fn key_policy_generates_a_key_on_loopback_by_default() {
        assert_eq!(
            resolve_key_policy("127.0.0.1", true, None, None, false).unwrap(),
            KeyPolicy::Generated
        );
        assert_eq!(
            resolve_key_policy("127.0.0.1", true, None, None, true).unwrap(),
            KeyPolicy::Keyless
        );
        assert_eq!(
            resolve_key_policy("127.0.0.1", true, Some(" abc "), None, false).unwrap(),
            KeyPolicy::Provided("abc".to_string())
        );
        assert_eq!(
            resolve_key_policy("127.0.0.1", true, None, Some("from-env"), false).unwrap(),
            KeyPolicy::Provided("from-env".to_string())
        );
        // The flag wins over the environment.
        assert_eq!(
            resolve_key_policy("127.0.0.1", true, Some("flag"), Some("env"), false).unwrap(),
            KeyPolicy::Provided("flag".to_string())
        );
    }

    #[test]
    fn key_policy_requires_a_key_off_loopback() {
        // No input of this function reads ONENOTE_ALLOW_INSECURE_SERVE, so the
        // old bypass cannot apply; `--allow-keyless` does not help either.
        for allow_keyless in [false, true] {
            let err = resolve_key_policy("0.0.0.0", false, None, None, allow_keyless)
                .unwrap_err()
                .to_string();
            assert!(err.contains("non-loopback"), "{err}");
            assert!(err.contains("ONENOTE_API_KEY"), "{err}");
        }
        assert_eq!(
            resolve_key_policy("0.0.0.0", false, Some("k"), None, false).unwrap(),
            KeyPolicy::Provided("k".to_string())
        );
    }

    #[test]
    fn key_policy_rejects_empty_and_conflicting_keys() {
        assert!(resolve_key_policy("127.0.0.1", true, Some(""), None, false).is_err());
        assert!(resolve_key_policy("127.0.0.1", true, Some("   "), None, false).is_err());
        assert!(resolve_key_policy("127.0.0.1", true, None, Some(""), false).is_err());
        // An empty flag is an error even when the environment has a key.
        assert!(resolve_key_policy("127.0.0.1", true, Some(""), Some("env"), false).is_err());
        assert!(resolve_key_policy("127.0.0.1", true, Some("k"), None, true).is_err());
    }

    #[test]
    fn generated_keys_are_32_random_bytes() {
        let a = generate_api_key();
        let b = generate_api_key();
        assert_ne!(a, b);
        assert_eq!(URL_SAFE_NO_PAD.decode(&a).unwrap().len(), 32);
    }

    #[test]
    fn loopback_bind_detection() {
        assert!(is_loopback_bind("127.0.0.1"));
        assert!(is_loopback_bind("::1"));
        assert!(is_loopback_bind("[::1]"));
        assert!(is_loopback_bind("LOCALHOST"));
        assert!(!is_loopback_bind("0.0.0.0"));
        assert!(!is_loopback_bind("192.168.0.10"));
        assert!(!is_loopback_bind("example.com"));
    }
}
