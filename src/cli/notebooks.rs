//! Notebook, section, and page listing/management CLI commands.

use anyhow::Result;
use serde_json::json;

use crate::{
    config::AppConfig,
    resolver,
    safe_text::sanitize_terminal_inline,
    sync,
};

use super::{
    helpers_read::{
        list_onenote_pages_with_cache, load_onenote_notebooks_with_warnings,
        load_onenote_sections_with_warnings,
    },
    helpers_write::rename_onenote_section_helper,
    runtime::{
        build_onenote_scope_from_args, next_cursor, notebooks_cache_synced_at, nudge_live_daemon,
        open_runtime, open_runtime_inner, page_slice, pages_cache_synced_at,
        sections_cache_synced_at, IdentityMode,
    },
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_notebooks(
    cfg: &AppConfig,
    include_teams: bool,
    limit: Option<u64>,
    cursor: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    let mode = if offline {
        IdentityMode::LocalOnly
    } else {
        IdentityMode::AllowNetwork
    };
    let (_auth, graph, storage) = open_runtime_inner(cfg, !offline && !cached, mode).await?;
    let (all_notebooks, warnings) =
        load_onenote_notebooks_with_warnings(&storage, &graph, include_teams, refresh, offline, cached)
            .await?;
    for w in &warnings {
        eprintln!("Warning: {w}");
    }
    let effective = limit.map(|n| n as usize).or(json_out.then_some(50));
    let (notebooks, has_more, next) = match effective {
        Some(n) => {
            let (sliced, more) = page_slice(all_notebooks, cursor, n);
            let next = next_cursor(cursor, n, Some(more));
            (sliced, more, next)
        }
        None => {
            let sliced = if cursor == 0 {
                all_notebooks
            } else {
                all_notebooks.into_iter().skip(cursor).collect()
            };
            (sliced, false, None)
        }
    };
    if json_out {
        let mut out = json!({
            "notebooks": notebooks,
            "returned": notebooks.len(),
            "has_more": has_more,
            "next_cursor": next,
        });
        if offline {
            out["cache_synced_at"] = json!(notebooks_cache_synced_at(&storage));
        }
        if !warnings.is_empty() {
            out["warnings"] = json!(warnings);
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if notebooks.is_empty() {
        println!("No OneNote notebooks found.");
    } else {
        for nb in &notebooks {
            let def = if nb.is_default { " [default]" } else { "" };
            let owner = nb.scope.owner_label();
            println!(
                "{}  ({}){def}  [{}]",
                sanitize_terminal_inline(&nb.display_name),
                sanitize_terminal_inline(&owner),
                sanitize_terminal_inline(&nb.notebook_key),
            );
        }
        if let Some(next) = next {
            eprintln!("More results: rerun with --cursor {next}");
        }
    }
    Ok(())
}

pub(super) async fn cmd_create_notebook(
    cfg: &AppConfig,
    name: &str,
    group: Option<&str>,
    site: Option<&str>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let scope = build_onenote_scope_from_args(group, site)?;
    let created = graph.create_onenote_notebook(&scope, name.trim()).await?;
    storage.upsert_onenote_notebooks(std::slice::from_ref(&created))?;
    nudge_live_daemon(cfg, &graph, sync::SyncCommand::RefreshHierarchy).await;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "notebook": created }))?
        );
    } else {
        println!(
            "Created notebook '{}' ({})",
            sanitize_terminal_inline(&created.display_name),
            sanitize_terminal_inline(&created.notebook_key),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_sections(
    cfg: &AppConfig,
    notebook: Option<&str>,
    limit: Option<u64>,
    cursor: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    let mode = if offline {
        IdentityMode::LocalOnly
    } else {
        IdentityMode::AllowNetwork
    };
    let (_auth, graph, storage) = open_runtime_inner(cfg, !offline && !cached, mode).await?;
    let (nb, all_sections, warnings) =
        load_onenote_sections_with_warnings(&storage, &graph, notebook, refresh, offline, cached)
            .await?;
    for w in &warnings {
        eprintln!("Warning: {w}");
    }
    let effective = limit.map(|n| n as usize).or(json_out.then_some(50));
    let (sections, has_more, next) = match effective {
        Some(n) => {
            let (sliced, more) = page_slice(all_sections, cursor, n);
            let next = next_cursor(cursor, n, Some(more));
            (sliced, more, next)
        }
        None => {
            let sliced = if cursor == 0 {
                all_sections
            } else {
                all_sections.into_iter().skip(cursor).collect()
            };
            (sliced, false, None)
        }
    };
    if json_out {
        let mut out = json!({
            "notebook": nb,
            "sections": sections,
            "returned": sections.len(),
            "has_more": has_more,
            "next_cursor": next,
        });
        if offline {
            out["cache_synced_at"] = json!(sections_cache_synced_at(&storage, nb.as_ref()));
        }
        if !warnings.is_empty() {
            out["warnings"] = json!(warnings);
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if sections.is_empty() {
        println!("No OneNote sections found.");
    } else {
        for sec in &sections {
            let nb_label = sec.notebook_name.as_deref().unwrap_or("Notebook");
            println!(
                "{} › {}  [{}]",
                sanitize_terminal_inline(nb_label),
                sanitize_terminal_inline(&sec.group_qualified_name()),
                sanitize_terminal_inline(&sec.section_key),
            );
        }
        if let Some(next) = next {
            eprintln!("More results: rerun with --cursor {next}");
        }
    }
    Ok(())
}

pub(super) async fn cmd_create_section(
    cfg: &AppConfig,
    notebook_query: &str,
    name: &str,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let nb = resolver::resolve_notebook(&graph, &storage, notebook_query, false).await?;
    let created = graph
        .create_onenote_section(&nb.scope, &nb.id, name.trim())
        .await?;
    storage.upsert_onenote_sections(std::slice::from_ref(&created))?;
    nudge_live_daemon(cfg, &graph, sync::SyncCommand::RefreshHierarchy).await;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "notebook": nb, "section": created }))?
        );
    } else {
        println!(
            "Created section '{} › {}' ({})",
            sanitize_terminal_inline(&nb.display_name),
            sanitize_terminal_inline(&created.display_name),
            sanitize_terminal_inline(&created.section_key),
        );
    }
    Ok(())
}

pub(super) async fn cmd_rename_section(
    cfg: &AppConfig,
    section_id: &str,
    name: &str,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let section = rename_onenote_section_helper(&storage, &graph, section_id, name).await?;
    nudge_live_daemon(cfg, &graph, sync::SyncCommand::RefreshHierarchy).await;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "section": section }))?
        );
    } else {
        println!(
            "Renamed section to '{}' ({})",
            sanitize_terminal_inline(&section.display_name),
            sanitize_terminal_inline(&section.section_key),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_pages(
    cfg: &AppConfig,
    section_query: &str,
    limit: usize,
    cursor: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    let mode = if offline {
        IdentityMode::LocalOnly
    } else {
        IdentityMode::AllowNetwork
    };
    let (_auth, graph, storage) = open_runtime_inner(cfg, !offline && !cached, mode).await?;
    let (sec, pages, has_more) = list_onenote_pages_with_cache(
        &storage,
        &graph,
        section_query,
        limit,
        cursor,
        refresh,
        offline,
        cached,
    )
    .await?;
    let next = next_cursor(cursor, limit, has_more);
    if json_out {
        let mut out = json!({
            "section": sec,
            "pages": pages,
            "returned": pages.len(),
            "has_more": has_more,
            "next_cursor": next,
        });
        if offline {
            out["cache_synced_at"] = json!(pages_cache_synced_at(&storage, &sec.section_key));
        }
        println!("{}", serde_json::to_string_pretty(&out)?);
    } else if pages.is_empty() {
        println!(
            "No pages in section '{}'.",
            sanitize_terminal_inline(&sec.display_name),
        );
    } else {
        for p in &pages {
            let indent = "  ".repeat(p.level.clamp(0, 2) as usize);
            let todo_mark = if p.has_todo { " [todo]" } else { "" };
            println!(
                "{indent}{}{}  [{}]",
                sanitize_terminal_inline(&p.title),
                todo_mark,
                sanitize_terminal_inline(&p.page_key),
            );
        }
        if let Some(next) = next {
            eprintln!("More pages: rerun with --cursor {next}");
        }
    }
    Ok(())
}
