//! Authentication, identity diagnostics, doctor, and TUI entry commands.

use std::path::PathBuf;

use anyhow::{Context, Result};
use serde_json::json;

use crate::{
    auth::{diagnose_access_token_scopes, AuthManager, LoginOptions},
    config::{self, AppConfig, Persona},
    daemon,
    graph::GraphClient,
    img_preview,
    models::OnenoteScope,
    safe_text::sanitize_terminal_inline,
    storage::Storage,
    tui,
};

use super::{
    format::format_unix_ts,
    runtime::{open_runtime, resolve_cache_user_id, IdentityMode, IdentitySource},
};

pub(super) async fn cmd_tui(cfg: &AppConfig) -> Result<()> {
    let (auth, graph, storage) = open_runtime(cfg, true).await?;
    tui::run_with_persona(cfg.clone(), Persona::detect(), auth, graph, storage).await
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_login(
    cfg: &mut AppConfig,
    client_id: Option<String>,
    tenant_id: Option<String>,
    device_code: bool,
    client_secret: Option<String>,
    access_token: Option<String>,
    token_file: Option<PathBuf>,
    port: Option<u16>,
    json_out: bool,
) -> Result<()> {
    if let Some(ref tf) = token_file {
        cfg.token_file = tf.to_string_lossy().into_owned();
        cfg.save()?;
        let auth = AuthManager::new(cfg.clone())?;
        let tok = auth.valid_access_token().await?;
        let diag = diagnose_access_token_scopes(&tok);
        if json_out {
            println!(
                "{}",
                serde_json::to_string_pretty(&json!({
                    "ok": true,
                    "auth_mode": "token_file",
                    "token_file": cfg.token_file,
                    "scopes": diag.scopes,
                    "warnings": diag.warnings,
                }))?
            );
        } else {
            println!("Configured external token file: {}", cfg.token_file);
        }
        return Ok(());
    }

    if let Some(ref cid) = client_id {
        cfg.client_id = cid.clone();
    }
    if let Some(ref tid) = tenant_id {
        cfg.tenant_id = tid.clone();
    }
    if client_id.is_some() || tenant_id.is_some() {
        let _ = cfg.save();
    }

    let auth = AuthManager::new(cfg.clone())?;
    let tokens = auth
        .login(&LoginOptions {
            client_id,
            tenant_id,
            device_code,
            client_secret,
            access_token,
            port,
        })
        .await?;

    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let profile = graph.get_me().await.ok();
    if let Some(ref me) = profile {
        let _ = auth
            .update_identity(me.id.clone(), me.display_name.clone(), me.email.clone())
            .await;
    }

    let diag = diagnose_access_token_scopes(&tokens.access_token);
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "auth_mode": tokens.auth_mode,
                "user": profile,
                "expires_at": tokens.expires_at,
                "scopes": diag.scopes,
                "warnings": diag.warnings,
            }))?
        );
    } else {
        let who = profile
            .as_ref()
            .map(|p| p.display_name.as_str())
            .or(tokens.user_display_name.as_deref())
            .unwrap_or("Signed-in user");
        println!(
            "Signed in to Microsoft OneNote as {} ({}).",
            sanitize_terminal_inline(who),
            sanitize_terminal_inline(&tokens.auth_mode),
        );
        for w in &diag.warnings {
            eprintln!("Warning: {w}");
        }
    }
    Ok(())
}

pub(super) async fn cmd_logout(cfg: &AppConfig, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    auth.logout().await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "ok": true }))?);
    } else {
        println!("Logged out and removed stored OneNote credentials.");
    }
    Ok(())
}

pub(super) async fn cmd_whoami(cfg: &AppConfig, json_out: bool) -> Result<()> {
    let (auth, graph, _storage) = open_runtime(cfg, true).await?;
    let tok = auth.current_tokens().await;
    let me = graph.get_me().await?;
    if let Some(t) = tok
        .as_ref()
        .filter(|t| t.user_id.as_deref().is_none_or(|u| u.trim().is_empty()))
    {
        config::record_identity(&t.access_token, &me.id)
            .context("Signed in, but the identity binding for this token could not be saved")?;
    }
    let diag = tok
        .as_ref()
        .map(|t| diagnose_access_token_scopes(&t.access_token))
        .unwrap_or_default();

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "user": me,
                "auth_mode": tok.as_ref().map(|t| t.auth_mode.as_str()),
                "expires_at": tok.as_ref().map(|t| t.expires_at),
                "scopes": diag.scopes,
                "roles": diag.roles,
                "can_read_notes": diag.can_read_notes,
                "can_write_notes": diag.can_write_notes,
                "can_create_notes": diag.can_create_notes,
                "warnings": diag.warnings,
            }))?
        );
    } else {
        println!(
            "User:   {} ({})",
            sanitize_terminal_inline(&me.display_name),
            sanitize_terminal_inline(&me.id),
        );
        if let Some(ref email) = me.email {
            println!("Email:  {}", sanitize_terminal_inline(email));
        }
        if let Some(ref t) = tok {
            println!("Auth:   {} (expires {})", t.auth_mode, format_unix_ts(t.expires_at));
        }
        if !diag.scopes.is_empty() {
            println!("Scopes: {}", diag.scopes.join(" "));
        }
        for w in &diag.warnings {
            eprintln!("Warning: {w}");
        }
    }
    Ok(())
}

pub(super) async fn cmd_doctor(cfg: &AppConfig, online: bool, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    let tok = auth.current_tokens().await;
    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let (db_file, cache_identity) =
        match resolve_cache_user_id(&auth, &graph, IdentityMode::LocalOnly).await {
            Ok((user_id, source)) => {
                let label = match source {
                    IdentitySource::NoToken => "no_token",
                    IdentitySource::Token => "token",
                    IdentitySource::IdentityMap => "identity_map",
                    IdentitySource::GraphMe => "graph_me",
                };
                (Some(config::db_path(user_id.as_deref())), label)
            }
            Err(_) => (None, "unresolved"),
        };
    let storage = db_file.as_ref().and_then(|p| Storage::open(p).ok());
    let nb_count = storage
        .as_ref()
        .and_then(|s| s.list_onenote_notebooks().ok())
        .map(|v| v.len())
        .unwrap_or(0);
    let sec_count = storage
        .as_ref()
        .and_then(|s| s.list_onenote_sections(None).ok())
        .map(|v| v.len())
        .unwrap_or(0);
    let mut diag = tok
        .as_ref()
        .map(|t| diagnose_access_token_scopes(&t.access_token))
        .unwrap_or_default();
    if tok.is_some() {
        if let Some(hint) = crate::graph::onenote::teams_skipped_warning(tok.as_ref()) {
            diag.warnings.push(hint.to_string());
        }
    }
    let img_caps = img_preview::probe_terminal_graphics_caps(cfg.image_protocol);
    let daemon_st = daemon::query_daemon_status(cfg).await.unwrap_or_default();

    let mut online_ok: Option<bool> = None;
    let mut online_error: Option<String> = None;
    let mut online_hint: Option<&'static str> = None;
    if online {
        match graph.list_onenote_notebooks(&OnenoteScope::Me).await {
            Ok(_) => online_ok = Some(true),
            Err(e) => {
                online_ok = Some(false);
                online_hint = crate::http::tls_hint_for(&e);
                online_error = Some(e.to_string());
            }
        }
    }

    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "version": env!("CARGO_PKG_VERSION"),
                "config_file": config::config_file_path(),
                "credentials_file": config::credential_fallback_path(),
                "db_file": db_file,
                "cache_identity": cache_identity,
                "download_dir": cfg.resolved_download_dir(),
                "graph_base_url": cfg.graph_base_url,
                "logged_in": tok.is_some(),
                "auth_mode": tok.as_ref().map(|t| t.auth_mode.as_str()),
                "scopes": diag.scopes,
                "warnings": diag.warnings,
                "cached_notebooks": nb_count,
                "cached_sections": sec_count,
                "image_protocol": format!("{:?}", img_caps.protocol),
                "daemon": daemon_st,
                "online_probe_ok": online_ok,
                "online_probe_error": online_error,
                "online_probe_hint": online_hint,
            }))?
        );
    } else {
        println!("onenote-cli v{}", env!("CARGO_PKG_VERSION"));
        println!("Config:         {}", config::config_file_path().display());
        match db_file.as_ref() {
            Some(path) => println!("Database:       {}", path.display()),
            None => println!("Database:       (unresolved)"),
        }
        println!("Cache identity: {cache_identity}");
        println!("Downloads:      {}", cfg.resolved_download_dir().display());
        println!(
            "Auth:           {}",
            tok.as_ref()
                .map(|t| format!("logged in ({})", t.auth_mode))
                .unwrap_or_else(|| "not logged in".to_string())
        );
        println!("Cache:          {nb_count} notebooks, {sec_count} sections");
        println!("Image protocol: {:?}", img_caps.protocol);
        println!(
            "Daemon:         {}",
            if daemon_st.running { "running" } else { "stopped" }
        );
        if let Some(ok) = online_ok {
            if ok {
                println!("Online probe:   OK (/me/onenote/notebooks)");
            } else {
                println!(
                    "Online probe:   FAILED ({})",
                    online_error.as_deref().unwrap_or("unknown error")
                );
                if let Some(hint) = online_hint {
                    if !online_error.as_deref().is_some_and(|e| e.contains(hint)) {
                        eprintln!("hint: {hint}");
                    }
                }
            }
        }
        for w in &diag.warnings {
            eprintln!("Warning: {w}");
        }
    }
    Ok(())
}
