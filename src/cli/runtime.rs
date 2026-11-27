//! Runtime initialization, cache identity resolution, cursor freshness, and pagination helpers.

use anyhow::{bail, Context, Result};

use crate::{
    auth::AuthManager,
    config::{self, AppConfig},
    daemon,
    graph::GraphClient,
    models::{OnenoteNotebook, OnenoteScope},
    storage::Storage,
    sync,
};

pub(super) const CURSOR_NOTEBOOKS_PERSONAL: &str = "onenote:notebooks:personal";
pub(super) const CURSOR_NOTEBOOKS_ALL: &str = "onenote:notebooks:all";
pub(super) const CURSOR_SECTIONS_ALL: &str = "onenote:sections:all";
pub(super) const CURSOR_HIERARCHY: &str = "onenote:hierarchy";

pub(super) const UNKNOWN_IDENTITY_MSG: &str = "Unknown identity: the current token carries no user ID and no identity is recorded for it, so the local cache cannot be selected. Run `onenote whoami` once while online (MCP: call the whoami tool), then retry.";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityMode {
    LocalOnly,
    AllowNetwork,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentitySource {
    NoToken,
    Token,
    IdentityMap,
    GraphMe,
}

/// Selects the cache bucket for the current token: token claim, then the local
/// identity map, then `GET /me`. Shared by the CLI/TUI/MCP runtime and the daemon
/// so they all open the same database file.
pub(crate) async fn resolve_cache_user_id(
    auth: &AuthManager,
    graph: &GraphClient,
    mode: IdentityMode,
) -> Result<(Option<String>, IdentitySource)> {
    let Some(tok) = auth.current_tokens().await else {
        return Ok((None, IdentitySource::NoToken));
    };
    if let Some(id) = tok
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return Ok((Some(id.to_string()), IdentitySource::Token));
    }
    if let Some(id) = config::lookup_identity(&tok.access_token) {
        return Ok((Some(id), IdentitySource::IdentityMap));
    }
    if mode == IdentityMode::LocalOnly {
        bail!("{UNKNOWN_IDENTITY_MSG}");
    }
    let id = graph
        .require_my_user_id()
        .await
        .context(
            "Cannot select a cache for the current token: GET /me failed, so the signed-in account is unknown.",
        )?;
    let trimmed = id.trim();
    if trimmed.is_empty() {
        bail!(
            "Cannot select a cache for the current token: GET /me failed, so the signed-in account is unknown."
        );
    }
    let _ = config::record_identity(&tok.access_token, trimmed);
    Ok((Some(trimmed.to_string()), IdentitySource::GraphMe))
}

pub async fn open_runtime(
    cfg: &AppConfig,
    require_auth: bool,
) -> Result<(AuthManager, GraphClient, Storage)> {
    open_runtime_inner(cfg, require_auth, IdentityMode::AllowNetwork).await
}

pub(crate) async fn open_mcp_runtime(
    cfg: &AppConfig,
    mode: IdentityMode,
) -> Result<(GraphClient, Storage)> {
    let (_auth, graph, storage) = open_runtime_inner(cfg, false, mode).await?;
    Ok((graph, storage))
}

pub(super) async fn open_runtime_inner(
    cfg: &AppConfig,
    require_auth: bool,
    mode: IdentityMode,
) -> Result<(AuthManager, GraphClient, Storage)> {
    let auth = AuthManager::new(cfg.clone())?;
    if require_auth {
        let _ = auth.valid_access_token().await?;
    }
    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let (user_id, _source) = resolve_cache_user_id(&auth, &graph, mode).await?;
    let graph = if let Some(ref uid) = user_id {
        graph.with_my_user_id(uid.clone())
    } else {
        graph
    };
    let db_file = config::db_path(user_id.as_deref());
    let storage = Storage::open(&db_file)?;
    Ok((auth, graph, storage))
}

pub(super) fn is_cursor_fresh(storage: &Storage, scope_key: &str, ttl_secs: u64) -> bool {
    if ttl_secs == 0 {
        return false;
    }
    let Ok(Some(cursor)) = storage.get_sync_cursor(scope_key) else {
        return false;
    };
    let Ok(synced_at) = chrono::DateTime::parse_from_rfc3339(&cursor.last_synced_at) else {
        return false;
    };
    let age_secs = chrono::Utc::now()
        .signed_duration_since(synced_at.with_timezone(&chrono::Utc))
        .num_seconds();
    age_secs < (ttl_secs as i64)
}

pub(crate) fn latest_sync_at(storage: &Storage, scope_keys: &[&str]) -> Option<String> {
    let mut best: Option<(chrono::DateTime<chrono::FixedOffset>, String)> = None;
    for &key in scope_keys {
        let Ok(Some(cursor)) = storage.get_sync_cursor(key) else {
            continue;
        };
        let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&cursor.last_synced_at) else {
            continue;
        };
        if best.as_ref().is_none_or(|(cur, _)| ts > *cur) {
            best = Some((ts, cursor.last_synced_at));
        }
    }
    best.map(|(_, raw)| raw)
}

pub(crate) fn notebooks_cache_synced_at(storage: &Storage) -> Option<String> {
    latest_sync_at(
        storage,
        &[
            CURSOR_NOTEBOOKS_PERSONAL,
            CURSOR_NOTEBOOKS_ALL,
            CURSOR_HIERARCHY,
        ],
    )
}

pub(crate) fn sections_cache_synced_at(
    storage: &Storage,
    notebook: Option<&OnenoteNotebook>,
) -> Option<String> {
    if let Some(nb) = notebook {
        let nb_cursor = format!("onenote:sections:{}", nb.notebook_key);
        latest_sync_at(
            storage,
            &[nb_cursor.as_str(), CURSOR_SECTIONS_ALL, CURSOR_HIERARCHY],
        )
    } else {
        latest_sync_at(storage, &[CURSOR_SECTIONS_ALL, CURSOR_HIERARCHY])
    }
}

pub(crate) fn pages_cache_synced_at(storage: &Storage, section_key: &str) -> Option<String> {
    let page_cursor = format!("onenote:pages:{section_key}");
    latest_sync_at(storage, &[page_cursor.as_str()])
}

pub(super) fn record_sync_cursor(storage: &Storage, scope_key: impl Into<String>, delta_link: Option<String>) {
    let _ = storage.upsert_sync_cursor(&crate::models::SyncCursor {
        scope_key: scope_key.into(),
        delta_link,
        history_next_link: None,
        last_synced_at: chrono::Utc::now().to_rfc3339(),
    });
}

pub(crate) fn page_slice<T>(items: Vec<T>, offset: usize, limit: usize) -> (Vec<T>, bool) {
    let has_more = items.len() > offset.saturating_add(limit);
    (items.into_iter().skip(offset).take(limit).collect(), has_more)
}

pub(crate) fn next_cursor(offset: usize, limit: usize, has_more: Option<bool>) -> Option<String> {
    (has_more == Some(true)).then(|| offset.saturating_add(limit).to_string())
}

pub(crate) fn parse_mcp_cursor(args: &serde_json::Value) -> Result<usize> {
    match args.get("cursor") {
        None | Some(serde_json::Value::Null) => Ok(0),
        Some(serde_json::Value::String(s)) => s.trim().parse::<usize>().map_err(|_| {
            anyhow::anyhow!("Invalid cursor: pass the next_cursor value from the previous response.")
        }),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Invalid cursor: pass the next_cursor value from the previous response."
                )
            }),
        Some(_) => bail!("Invalid cursor: pass the next_cursor value from the previous response."),
    }
}

pub(crate) async fn nudge_live_daemon(cfg: &AppConfig, graph: &GraphClient, cmd: sync::SyncCommand) {
    if daemon::is_daemon_alive_fast(cfg, graph.my_user_id()) {
        let _ = daemon::nudge_daemon(cfg, Some(cmd), None).await;
    }
}

pub fn build_onenote_scope_from_args(
    group_id: Option<&str>,
    site_id: Option<&str>,
) -> Result<OnenoteScope> {
    let g = group_id.map(str::trim).filter(|s| !s.is_empty());
    let s = site_id.map(str::trim).filter(|s| !s.is_empty());
    match (g, s) {
        (Some(_), Some(_)) => bail!("Cannot specify both --group and --site at the same time."),
        (Some(gid), None) => Ok(OnenoteScope::Group {
            group_id: gid.to_string(),
            group_name: None,
        }),
        (None, Some(sid)) => Ok(OnenoteScope::Site {
            site_id: sid.to_string(),
            site_name: None,
        }),
        (None, None) => Ok(OnenoteScope::Me),
    }
}
