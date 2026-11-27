//! Operational CLI commands: sync, daemon lifecycle, and self-update.

use anyhow::Result;
use serde_json::json;

use crate::{config::AppConfig, daemon, sync, updater};

use super::{
    args::DaemonCommand,
    runtime::{nudge_live_daemon, open_runtime},
};

pub(super) async fn cmd_sync(cfg: &AppConfig, json_out: bool) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let summary = sync::bootstrap_onenote(&graph, &storage).await?;
    nudge_live_daemon(cfg, &graph, sync::SyncCommand::RefreshAll).await;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&summary)?);
    } else {
        println!(
            "Synced {} notebooks, {} section groups, {} sections, {} pages.",
            summary.notebooks, summary.section_groups, summary.sections, summary.pages
        );
    }
    Ok(())
}

pub(super) async fn cmd_daemon(cfg: &AppConfig, action: DaemonCommand) -> Result<()> {
    match action {
        DaemonCommand::Run => daemon::run_daemon_foreground(cfg.clone()).await,
        DaemonCommand::Start { json } => {
            let status = daemon::ensure_daemon_started(cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!(
                    "OneNote sync daemon running (pid={:?}, transport={:?}).",
                    status.pid, status.transport
                );
            }
            Ok(())
        }
        DaemonCommand::Status { json } => {
            let status = daemon::query_daemon_status(cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else if status.running {
                println!(
                    "Running (pid={:?}, clients={}, summary={})",
                    status.pid, status.attached_clients, status.sync_summary
                );
            } else {
                println!("Stopped.");
            }
            Ok(())
        }
        DaemonCommand::Stop { json } => {
            let stopped = daemon::stop_daemon(cfg).await?;
            if json {
                println!(
                    "{}",
                    serde_json::to_string_pretty(&json!({ "ok": true, "stopped": stopped }))?
                );
            } else if stopped {
                println!("Stopped OneNote sync daemon.");
            } else {
                println!("OneNote sync daemon was not running.");
            }
            Ok(())
        }
        DaemonCommand::Restart { json } => {
            let _ = daemon::stop_daemon(cfg).await;
            let status = daemon::ensure_daemon_started(cfg).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&status)?);
            } else {
                println!("Restarted OneNote sync daemon (pid={:?}).", status.pid);
            }
            Ok(())
        }
    }
}

pub(super) async fn cmd_self_update(
    cfg: &AppConfig,
    check_only: bool,
    force: bool,
    repo: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let target_repo = repo.unwrap_or(&cfg.github_repo);
    let res = updater::check_or_apply_update(target_repo, check_only, force).await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&res)?);
    } else if res.updated {
        println!(
            "Updated onenote {} -> {} ({}).",
            res.current_version,
            res.latest_version,
            res.binary_path.display()
        );
    } else if res.update_available {
        println!(
            "Update available: {} -> {} ({})",
            res.current_version, res.latest_version, res.release_url
        );
    } else {
        println!("onenote {} is already up to date.", res.current_version);
    }
    Ok(())
}
