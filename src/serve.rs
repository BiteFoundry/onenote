//! Local HTTP JSON API server (`onenote serve`, default `127.0.0.1:8788`).
//!
//! Endpoints:
//! - `GET /health`
//! - `GET /v1/whoami`
//! - `GET/POST /v1/onenote/notebooks`
//! - `GET/POST /v1/onenote/sections`
//! - `GET/POST /v1/onenote/pages`
//! - `GET/PATCH/DELETE /v1/onenote/pages/{page_key}`
//! - `POST /v1/onenote/pages/{page_key}/append`
//! - `POST /v1/onenote/pages/{page_key}/toggle-todo`
//! - `POST /v1/onenote/pages/{page_key}/resources/download`
//! - `GET /v1/onenote/search`
//! (Also mounted under `/onenote/*` for ergonomic unversioned access.)

use std::{net::IpAddr, path::PathBuf, sync::Arc};

use anyhow::{bail, Context, Result};
use axum::{
    body::Body,
    extract::{Path as AxumPath, Query, State},
    http::{HeaderMap, Request, StatusCode},
    middleware::{self, Next},
    response::{IntoResponse, Response},
    routing::{get, post},
    Json, Router,
};
use serde::Deserialize;
use serde_json::{json, Value};
use tokio::net::TcpListener;
use tower_http::cors::{Any, CorsLayer};

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
    if lower.contains("not found") || lower.contains("404") {
        ApiError::not_found(msg)
    } else if lower.contains("empty")
        || lower.contains("ambiguous")
        || lower.contains("required")
        || lower.contains("out of range")
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

#[derive(Debug, Deserialize)]
pub struct PatchPageBody {
    #[serde(default)]
    pub title: Option<String>,
    #[serde(default)]
    pub markdown: Option<String>,
    #[serde(default)]
    pub block_id: Option<String>,
}

#[derive(Debug, Deserialize)]
pub struct AppendPageBody {
    #[serde(default)]
    pub markdown: String,
    #[serde(default)]
    pub image_paths: Vec<String>,
}

#[derive(Debug, Default, Deserialize)]
pub struct ToggleTodoBody {
    #[serde(default)]
    pub block_id: Option<String>,
    #[serde(default)]
    pub index: Option<usize>,
}

#[derive(Debug, Default, Deserialize)]
pub struct DownloadResourceBody {
    #[serde(default)]
    pub index: Option<usize>,
    #[serde(default)]
    pub out_dir: Option<String>,
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

pub fn build_router(state: Arc<AppState>) -> Router {
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
        .layer(
            CorsLayer::new()
                .allow_origin(Any)
                .allow_methods(Any)
                .allow_headers(Any),
        )
        .with_state(state)
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
        .route("/pages/{page_key}/toggle-todo", post(toggle_todo))
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
    if let Some(expected) = state.api_key.as_deref().filter(|k| !k.is_empty()) {
        let bearer = headers
            .get(axum::http::header::AUTHORIZATION)
            .and_then(|v| v.to_str().ok())
            .and_then(|s| s.strip_prefix("Bearer "))
            .map(str::trim);
        let x_api_key = headers
            .get("x-api-key")
            .and_then(|v| v.to_str().ok())
            .map(str::trim);
        if bearer != Some(expected) && x_api_key != Some(expected) {
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
    bind: &str,
    port: u16,
    api_key: Option<String>,
) -> Result<()> {
    let is_loopback = bind
        .parse::<IpAddr>()
        .map(|ip| ip.is_loopback())
        .unwrap_or_else(|_| bind.eq_ignore_ascii_case("localhost"));
    let allow_insecure = std::env::var("ONENOTE_ALLOW_INSECURE_SERVE")
        .ok()
        .is_some_and(|v| matches!(v.trim(), "1" | "true" | "TRUE"));

    if !is_loopback && api_key.as_deref().unwrap_or("").is_empty() && !allow_insecure {
        bail!(
            "Refusing to bind non-loopback address '{bind}' without `--api-key`. Pass `--api-key <SECRET>` or set ONENOTE_ALLOW_INSECURE_SERVE=1."
        );
    }

    let state = Arc::new(AppState {
        config,
        graph,
        storage,
        api_key,
    });
    let app = build_router(state);
    let addr = format!("{bind}:{port}");
    let listener = TcpListener::bind(&addr)
        .await
        .with_context(|| format!("Failed to bind HTTP server on {addr}"))?;
    eprintln!(
        "onenote HTTP API server listening on http://{addr} (v{})",
        env!("CARGO_PKG_VERSION")
    );
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
    let _ = state
        .storage
        .upsert_onenote_notebooks(std::slice::from_ref(&created));
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
    let _ = state
        .storage
        .upsert_onenote_sections(std::slice::from_ref(&created));
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
    let (section, pages) = cli::list_onenote_pages_helper(
        &state.storage,
        &state.graph,
        &query.section,
        query.limit,
        query.refresh,
        query.offline,
    )
    .await
    .map_err(onenote_api_error)?;
    Ok(Json(json!({
        "ok": true,
        "section": section,
        "pages": pages,
    })))
}

async fn create_page(
    State(state): State<Arc<AppState>>,
    Json(body): Json<CreatePageBody>,
) -> Result<Json<Value>, ApiError> {
    if body.title.trim().is_empty() {
        return Err(ApiError::bad_request("Page title is required."));
    }
    let paths: Vec<PathBuf> = body.image_paths.iter().map(PathBuf::from).collect();
    let inline_images = cli::load_inline_images_from_paths(&paths).map_err(onenote_api_error)?;
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
    let page = cli::load_onenote_page_detail(&state.storage, &state.graph, &page_key, query.offline)
        .await
        .map_err(onenote_api_error)?;
    Ok(Json(json!({ "ok": true, "page": page })))
}

async fn patch_page(
    State(state): State<Arc<AppState>>,
    AxumPath(page_key): AxumPath<String>,
    Json(body): Json<PatchPageBody>,
) -> Result<Json<Value>, ApiError> {
    let updated = cli::update_onenote_page_helper(
        &state.storage,
        &state.graph,
        &page_key,
        body.title.as_deref(),
        body.markdown.as_deref(),
        body.block_id.as_deref(),
    )
    .await
    .map_err(onenote_api_error)?;
    Ok(Json(json!({ "ok": true, "page": updated })))
}

async fn delete_page(
    State(state): State<Arc<AppState>>,
    AxumPath(page_key): AxumPath<String>,
) -> Result<Json<Value>, ApiError> {
    let deleted = cli::delete_onenote_page_helper(&state.storage, &state.graph, &page_key)
        .await
        .map_err(onenote_api_error)?;
    Ok(Json(json!({ "ok": true, "deleted": deleted })))
}

async fn append_page(
    State(state): State<Arc<AppState>>,
    AxumPath(page_key): AxumPath<String>,
    Json(body): Json<AppendPageBody>,
) -> Result<Json<Value>, ApiError> {
    let paths: Vec<PathBuf> = body.image_paths.iter().map(PathBuf::from).collect();
    let inline_images = cli::load_inline_images_from_paths(&paths).map_err(onenote_api_error)?;
    let updated = cli::append_onenote_page_helper(
        &state.storage,
        &state.graph,
        &page_key,
        &body.markdown,
        &inline_images,
    )
    .await
    .map_err(onenote_api_error)?;
    Ok(Json(json!({ "ok": true, "page": updated })))
}

async fn toggle_todo(
    State(state): State<Arc<AppState>>,
    AxumPath(page_key): AxumPath<String>,
    Json(body): Json<ToggleTodoBody>,
) -> Result<Json<Value>, ApiError> {
    let (updated, block_id, completed) = cli::toggle_onenote_todo_helper(
        &state.storage,
        &state.graph,
        &page_key,
        body.block_id.as_deref(),
        body.index,
    )
    .await
    .map_err(onenote_api_error)?;
    Ok(Json(json!({
        "ok": true,
        "page_key": updated.summary.page_key,
        "block_id": block_id,
        "completed": completed,
        "page": updated,
    })))
}

async fn download_resource(
    State(state): State<Arc<AppState>>,
    AxumPath(page_key): AxumPath<String>,
    Json(body): Json<DownloadResourceBody>,
) -> Result<Json<Value>, ApiError> {
    let out_dir = body.out_dir.as_deref().map(PathBuf::from);
    let (_detail, _idx, downloaded) = cli::download_onenote_resource_helper(
        &state.config,
        &state.storage,
        &state.graph,
        &page_key,
        body.index,
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
