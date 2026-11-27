//! Standalone Microsoft OneNote (`onenote` / `note`) CLI parser, command dispatcher,
//! and shared domain helpers used by CLI, HTTP API (`serve`), and MCP (`mcp`).

mod args;
mod auth_cmds;
mod format;
mod helpers_read;
mod helpers_write;
mod notebooks;
mod ops;
mod pages;
mod parse;
mod runtime;
#[cfg(test)]
mod tests;

use anyhow::Result;
use serde_json::json;

use crate::{
    config::AppConfig,
    safe_text::sanitize_terminal_inline,
    serve, sync,
};

pub use args::{Cli, Command, DaemonCommand};
pub(crate) use format::{page_content_json, write_result_json, PageSurface};
pub use helpers_read::{
    list_onenote_pages_helper, list_onenote_pages_with_cache, load_onenote_notebooks,
    load_onenote_notebooks_with_cache, load_onenote_notebooks_with_warnings,
    load_onenote_page_detail, load_onenote_page_detail_with_cache, load_onenote_sections,
    load_onenote_sections_with_cache, load_onenote_sections_with_warnings,
    resolve_onenote_section_query, resolve_onenote_section_query_with_refresh,
};
pub use helpers_write::{
    append_onenote_page_helper, change_onenote_todo_helper_with_guard, create_onenote_page_helper,
    delete_onenote_page_helper, download_onenote_resource_helper,
    download_onenote_resource_helper_with_options, load_inline_images_from_paths,
    rename_onenote_section_helper, resolve_page_query_in_section, search_onenote_helper,
    search_onenote_with_offset, select_onenote_resource,
    select_onenote_resource_by_index_or_url, toggle_onenote_todo_guarded,
    toggle_onenote_todo_helper, toggle_onenote_todo_helper_with_guard,
    update_onenote_content_helper_with_guard, update_onenote_page_guarded,
    update_onenote_page_helper, update_onenote_page_helper_with_guard, GuardedTodoResult,
    GuardedUpdateResult,
};
pub use parse::try_fast_help_or_version;
pub use runtime::{build_onenote_scope_from_args, open_runtime};
pub(crate) use runtime::{
    next_cursor, notebooks_cache_synced_at, nudge_live_daemon, open_mcp_runtime, page_slice,
    pages_cache_synced_at, parse_mcp_cursor, resolve_cache_user_id, sections_cache_synced_at,
    IdentityMode,
};

use auth_cmds::{cmd_doctor, cmd_login, cmd_logout, cmd_tui, cmd_whoami};
use notebooks::{
    cmd_create_notebook, cmd_create_section, cmd_notebooks, cmd_pages, cmd_rename_section,
    cmd_sections,
};
use ops::{cmd_daemon, cmd_self_update, cmd_sync};
use pages::{
    cmd_append, cmd_check, cmd_create, cmd_delete, cmd_preview_file, cmd_read, cmd_resource,
    cmd_search, cmd_update_page,
};

#[derive(Debug)]
struct CliFailure {
    code: String,
    message: String,
    extra: serde_json::Value,
}

impl std::fmt::Display for CliFailure {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.message)
    }
}

impl std::error::Error for CliFailure {}

fn error_code(err: &anyhow::Error) -> (&'static str, Option<serde_json::Value>) {
    let message = format!("{err:#}");
    let lower = message.to_ascii_lowercase();
    if let Some(status) = crate::graph::client::graph_http_status(err) {
        return match status {
            401 => ("unauthorized", None),
            404 | 410 => ("not_found", None),
            other => ("graph_error", Some(json!({ "http_status": other }))),
        };
    }
    if message.starts_with("Conflict:") {
        return ("conflict", None);
    }
    if message.starts_with("Ambiguous ") {
        return ("ambiguous", None);
    }
    if message.starts_with("Refusing to delete") {
        return ("confirmation_required", None);
    }
    if message.starts_with("Unknown identity") {
        return ("unknown_identity", None);
    }
    if lower.contains("not found") {
        return ("not_found", None);
    }
    ("error", None)
}

fn cli_error_json(err: &anyhow::Error) -> serde_json::Value {
    if let Some(failure) = err
        .chain()
        .find_map(|cause| cause.downcast_ref::<CliFailure>())
    {
        let mut map = serde_json::Map::new();
        map.insert("ok".to_string(), json!(false));
        if let Some(extra) = failure.extra.as_object() {
            for (k, v) in extra {
                map.insert(k.clone(), v.clone());
            }
        }
        map.insert(
            "error".to_string(),
            json!({
                "code": failure.code,
                "message": failure.message,
            }),
        );
        return serde_json::Value::Object(map);
    }
    let message = format!("{err:#}");
    let (code, details) = error_code(err);
    let mut error_obj = json!({
        "code": code,
        "message": message,
    });
    if let Some(details) = details {
        error_obj["details"] = details;
    }
    json!({
        "ok": false,
        "error": error_obj,
    })
}

pub async fn run_from_env() -> Result<()> {
    let mut cfg = AppConfig::load();
    let cli = Cli::parse();
    let wants_json = cli.command.as_ref().is_some_and(Command::wants_json);
    let res = dispatch_command(&mut cfg, cli.command).await;
    if let Err(ref err) = res {
        if wants_json {
            if let Ok(rendered) = serde_json::to_string_pretty(&cli_error_json(err)) {
                println!("{rendered}");
            }
        }
    }
    res
}

pub async fn dispatch_command(cfg: &mut AppConfig, cmd: Option<Command>) -> Result<()> {
    match cmd {
        None | Some(Command::Tui) => Box::pin(cmd_tui(cfg)).await,
        Some(Command::Login {
            client_id,
            tenant_id,
            device_code,
            client_secret,
            access_token,
            token_file,
            port,
            json,
        }) => {
            cmd_login(
                cfg,
                client_id,
                tenant_id,
                device_code,
                client_secret,
                access_token,
                token_file,
                port,
                json,
            )
            .await
        }
        Some(Command::Logout { json }) => cmd_logout(cfg, json).await,
        Some(Command::Whoami { json }) => cmd_whoami(cfg, json).await,
        Some(Command::Doctor { online, json }) => cmd_doctor(cfg, online, json).await,
        Some(Command::Notebooks {
            include_teams,
            limit,
            cursor,
            refresh,
            offline,
            cached,
            json,
        }) => cmd_notebooks(cfg, include_teams, limit, cursor, refresh, offline, cached, json).await,
        Some(Command::CreateNotebook {
            name,
            group,
            site,
            json,
        }) => cmd_create_notebook(cfg, &name, group.as_deref(), site.as_deref(), json).await,
        Some(Command::RenameNotebook {
            drive_id,
            item_id,
            name,
            json,
        }) => {
            let (_auth, graph, _storage) = open_runtime(cfg, true).await?;
            let item = graph.rename_notebook_storage(&drive_id, &item_id, &name).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&json!({ "ok": true, "drive_item": item }))?);
            } else {
                println!(
                    "Notebook storage item '{}' is named '{}'.",
                    sanitize_terminal_inline(&item.item_id),
                    sanitize_terminal_inline(&item.name),
                );
            }
            Ok(())
        }
        Some(Command::Sections {
            notebook,
            limit,
            cursor,
            refresh,
            offline,
            cached,
            json,
        }) => cmd_sections(cfg, notebook.as_deref(), limit, cursor, refresh, offline, cached, json).await,
        Some(Command::CreateSection {
            notebook,
            name,
            json,
        }) => cmd_create_section(cfg, &notebook, &name, json).await,
        Some(Command::RenameSection {
            section,
            name,
            json,
        }) => {
            cmd_rename_section(cfg, &section, &name, json).await
        }
        #[cfg(windows)]
        Some(Command::DesktopSetPageLevel { page_id, level, json }) => {
            let result = crate::desktop::set_page_level(&page_id, level).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!(
                    "OneNote desktop page '{}' is at level {level}.",
                    sanitize_terminal_inline(&page_id),
                );
            }
            Ok(())
        }
        #[cfg(windows)]
        Some(Command::DesktopRenameSection { section_id, name, json }) => {
            let result = crate::desktop::rename_section(&section_id, &name).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!(
                    "OneNote desktop section '{}' is named '{}'.",
                    sanitize_terminal_inline(&section_id),
                    sanitize_terminal_inline(name.trim()),
                );
            }
            Ok(())
        }
        Some(Command::Pages {
            section,
            limit,
            cursor,
            refresh,
            offline,
            cached,
            json,
        }) => cmd_pages(cfg, &section, limit, cursor, refresh, offline, cached, json).await,
        Some(Command::Read {
            page,
            section,
            format,
            preview_images,
            offline,
            cached,
            json,
            full,
            compact,
        }) => {
            cmd_read(
                cfg,
                &page,
                section.as_deref(),
                format.as_deref(),
                preview_images,
                offline,
                cached,
                json,
                compact,
                full,
            )
            .await
        }
        Some(Command::Create {
            section,
            title,
            markdown_body,
            file,
            image,
            level,
            json,
        }) => {
            cmd_create(
                cfg,
                &section,
                &title,
                markdown_body.as_deref(),
                file.as_deref(),
                &image,
                level,
                json,
            )
            .await
        }
        Some(Command::CopyPage {
            page,
            section,
            operation,
            wait_seconds,
            json,
        }) => {
            let (_auth, graph, _storage) = open_runtime(cfg, true).await?;
            let outcome = graph
                .copy_onenote_page(&page, &section, operation.as_deref(), wait_seconds)
                .await?;
            if !outcome.ok {
                let detail = outcome
                    .error
                    .as_deref()
                    .unwrap_or("Graph has not confirmed completion");
                let message = format!(
                    "Copy {:?} ({}): {detail}. Check the existing operation with --operation '{}'.",
                    outcome.status, outcome.operation_id, outcome.operation_url
                );
                let mut extra = serde_json::to_value(&outcome)?;
                let status_str = extra
                    .get("status")
                    .and_then(serde_json::Value::as_str)
                    .unwrap_or("failed")
                    .to_string();
                if let Some(obj) = extra.as_object_mut() {
                    obj.remove("ok");
                    obj.remove("error");
                }
                return Err(anyhow::Error::new(CliFailure {
                    code: format!("copy_{status_str}"),
                    message,
                    extra,
                }));
            }
            nudge_live_daemon(
                cfg,
                &graph,
                sync::SyncCommand::RefreshSectionPages {
                    section_key: outcome.destination_section_key.clone(),
                },
            )
            .await;
            if let Some(ref new_page_key) = outcome.new_page_key {
                nudge_live_daemon(
                    cfg,
                    &graph,
                    sync::SyncCommand::HydratePage {
                        page_key: new_page_key.clone(),
                    },
                )
                .await;
            }
            if json {
                println!("{}", serde_json::to_string_pretty(&outcome)?);
            } else {
                println!(
                    "Copied {} to {} as {}",
                    sanitize_terminal_inline(&outcome.source_page_key),
                    sanitize_terminal_inline(&outcome.destination_section_key),
                    sanitize_terminal_inline(outcome.new_page_key.as_deref().unwrap_or_default()),
                );
            }
            Ok(())
        }
        Some(Command::Append {
            page,
            markdown_body,
            section,
            file,
            image,
            json,
            full,
        }) => {
            cmd_append(
                cfg,
                &page,
                section.as_deref(),
                markdown_body.as_deref(),
                file.as_deref(),
                &image,
                json,
                full,
            )
            .await
        }
        Some(Command::Update {
            page,
            section,
            title,
            body,
            file,
            block_id,
            replace_text,
            with_text,
            expected_content,
            check,
            force,
            repo,
            json,
            full,
        }) => {
            if let Some(page_query) = page {
                cmd_update_page(
                    cfg,
                    &page_query,
                    section.as_deref(),
                    title.as_deref(),
                    body.as_deref(),
                    file.as_deref(),
                    block_id.as_deref(),
                    replace_text.as_deref().zip(with_text.as_deref()),
                    expected_content.as_deref(),
                    force,
                    json,
                    full,
                )
                .await
            } else {
                cmd_self_update(cfg, check, force, repo.as_deref(), json).await
            }
        }
        Some(Command::SelfUpdate {
            check,
            force,
            repo,
            json,
        }) => cmd_self_update(cfg, check, force, repo.as_deref(), json).await,
        Some(Command::Check {
            page,
            section,
            block_id,
            index,
            done,
            undone,
            expected_content,
            force,
            json,
            full,
        }) => {
            cmd_check(
                cfg,
                &page,
                section.as_deref(),
                block_id.as_deref(),
                index,
                (done || undone).then_some(done),
                expected_content.as_deref(),
                force,
                json,
                full,
            )
            .await
        }
        Some(Command::Delete {
            page,
            section,
            yes,
            json,
        }) => cmd_delete(cfg, &page, section.as_deref(), yes, json).await,
        Some(Command::Search {
            query,
            section,
            refresh,
            limit,
            cursor,
            json,
        }) => cmd_search(cfg, &query, section.as_deref(), refresh, limit, cursor, json).await,
        Some(Command::Resource {
            page,
            index,
            out,
            preview,
            protocol,
            cols,
            rows,
            json,
        }) => {
            cmd_resource(
                cfg,
                &page,
                index,
                out.as_deref(),
                preview,
                protocol.as_deref(),
                cols,
                rows,
                json,
            )
            .await
        }
        Some(Command::PreviewFile {
            path,
            protocol,
            cols,
            rows,
        }) => cmd_preview_file(cfg, &path, protocol.as_deref(), cols, rows),
        Some(Command::Sync { json }) => cmd_sync(cfg, json).await,
        Some(Command::Daemon { action }) => cmd_daemon(cfg, action).await,
        Some(Command::Serve {
            bind,
            port,
            api_key,
            allow_keyless,
            allowed_hosts,
            cors_origins,
        }) => {
            let (_auth, graph, storage) = open_runtime(cfg, false).await?;
            serve::run(
                cfg.clone(),
                graph,
                storage,
                serve::ServeOptions {
                    bind,
                    port,
                    api_key,
                    allow_keyless,
                    allowed_hosts,
                    cors_origins,
                },
            )
            .await
        }
        Some(Command::Mcp) => crate::mcp::run(cfg.clone()).await,
    }
}

