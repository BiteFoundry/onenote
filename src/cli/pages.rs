//! Page reading, creation, editing, to-do toggling, deletion, search, and resource CLI commands.

use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use serde_json::json;

use crate::{
    config::{AppConfig, ImageProtocolPreference},
    img_preview, parser, resolver,
    safe_text::{sanitize_terminal_inline, sanitize_terminal_text},
    sync,
};

use super::{
    format::{
        default_preview_dimensions, delete_prompt_text, page_content_json, read_json_is_compact,
        resolve_markdown_input, truncate_single_line, write_result_json, PageSurface,
    },
    helpers_read::{load_onenote_page_detail, load_onenote_page_detail_with_cache},
    helpers_write::{
        append_onenote_page_helper, change_onenote_todo_helper_with_guard,
        create_onenote_page_helper, delete_onenote_page_helper, download_onenote_resource_helper,
        load_inline_images_from_paths, resolve_page_query_in_section, search_onenote_with_offset,
        select_onenote_resource, update_onenote_content_helper_with_guard,
    },
    runtime::{next_cursor, nudge_live_daemon, open_runtime, open_runtime_inner, IdentityMode},
};

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_read(
    cfg: &AppConfig,
    page: &str,
    section: Option<&str>,
    format: Option<&str>,
    preview_images: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
    compact: bool,
    full: bool,
) -> Result<()> {
    let mode = if offline {
        IdentityMode::LocalOnly
    } else {
        IdentityMode::AllowNetwork
    };
    let (_auth, graph, storage) = open_runtime_inner(cfg, !offline && !cached, mode).await?;
    let resolved_page =
        resolve_page_query_in_section(&storage, &graph, page, section, offline || cached).await?;
    let detail =
        load_onenote_page_detail_with_cache(&storage, &graph, &resolved_page, offline, cached)
            .await?;
    if json_out {
        let is_compact = read_json_is_compact(format.is_some(), compact, full);
        let page = page_content_json(&detail, is_compact, PageSurface::Cli);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "page": page }))?
        );
        return Ok(());
    }

    match format.unwrap_or("md") {
        "xhtml" => println!("{}", detail.raw_xhtml),
        "plain" => {
            println!("# {}\n", sanitize_terminal_inline(&detail.summary.title));
            println!("{}", sanitize_terminal_text(&detail.body_plain));
        }
        "blocks" => {
            println!(
                "# {} [{}]\n",
                sanitize_terminal_inline(&detail.summary.title),
                sanitize_terminal_inline(&detail.summary.page_key),
            );
            for (idx, blk) in detail.blocks.iter().enumerate() {
                let id = sanitize_terminal_inline(blk.element_id.as_deref().unwrap_or("-"));
                let tag = blk
                    .tag
                    .as_ref()
                    .map(|t| format!(" [{}]", sanitize_terminal_inline(&t.data_tag_value())))
                    .unwrap_or_default();
                let preview = truncate_single_line(&parser::onenote_blocks_to_plain(std::slice::from_ref(blk)), 90);
                println!("[{idx}] id={id}{tag}  {preview}");
            }
        }
        _ => {
            println!("# {}\n", sanitize_terminal_inline(&detail.summary.title));
            println!("{}", sanitize_terminal_text(&detail.body_markdown));
        }
    }

    if preview_images && !offline {
        for res in detail.resources.iter().filter(|r| r.is_image) {
            let url = res.fullres_url.as_deref().unwrap_or(&res.resource_url);
            if let Ok((_, bytes)) = graph.fetch_onenote_resource_bytes(url).await {
                let (cols, rows) = default_preview_dimensions(None, None);
                if let Ok(rendered) =
                    img_preview::render_ansi_stdout_with_pref(&bytes, cols, rows, cfg.image_protocol)
                {
                    print!("{rendered}");
                    if !rendered.ends_with('\n') {
                        println!();
                    }
                }
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_create(
    cfg: &AppConfig,
    section: &str,
    title: &str,
    markdown_body: Option<&str>,
    file: Option<&Path>,
    images: &[PathBuf],
    level: Option<i32>,
    json_out: bool,
) -> Result<()> {
    crate::graph::onenote::validate_page_creation_level(level)?;
    let body = resolve_markdown_input(markdown_body, file)?;
    let inline_images = load_inline_images_from_paths(images)?;
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let (sec, created) =
        create_onenote_page_helper(&storage, &graph, section, title, &body, inline_images, level)
            .await?;
    nudge_live_daemon(
        cfg,
        &graph,
        sync::SyncCommand::RefreshSectionPages {
            section_key: sec.section_key.clone(),
        },
    )
    .await;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "section": sec, "page": created }))?
        );
    } else {
        println!(
            "Created page '{}' in '{}' ({})",
            sanitize_terminal_inline(&created.title),
            sanitize_terminal_inline(&sec.display_name),
            sanitize_terminal_inline(&created.page_key),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_append(
    cfg: &AppConfig,
    page: &str,
    section: Option<&str>,
    markdown_body: Option<&str>,
    file: Option<&Path>,
    images: &[PathBuf],
    json_out: bool,
    full: bool,
) -> Result<()> {
    let body = resolve_markdown_input(markdown_body, file)?;
    let inline_images = load_inline_images_from_paths(images)?;
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved_page =
        resolve_page_query_in_section(&storage, &graph, page, section, false).await?;
    let updated =
        append_onenote_page_helper(&storage, &graph, &resolved_page, &body, &inline_images).await?;
    nudge_live_daemon(
        cfg,
        &graph,
        sync::SyncCommand::HydratePage {
            page_key: updated.summary.page_key.clone(),
        },
    )
    .await;
    if !updated.summary.section_id.trim().is_empty() {
        nudge_live_daemon(
            cfg,
            &graph,
            sync::SyncCommand::RefreshSectionPages {
                section_key: updated
                    .summary
                    .scope
                    .make_entity_key(&updated.summary.section_id),
            },
        )
        .await;
    }
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&write_result_json(&updated, None, None, full))?
        );
    } else {
        println!(
            "Appended content to page '{}' ({}).",
            sanitize_terminal_inline(&updated.summary.title),
            sanitize_terminal_inline(&updated.summary.page_key),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_update_page(
    cfg: &AppConfig,
    page: &str,
    section: Option<&str>,
    title: Option<&str>,
    body: Option<&str>,
    file: Option<&Path>,
    block_id: Option<&str>,
    replace_text: Option<(&str, &str)>,
    expected_content: Option<&str>,
    force: bool,
    json_out: bool,
    full: bool,
) -> Result<()> {
    let loaded_body = match (body, file) {
        (_, Some(path)) => Some(
            std::fs::read_to_string(path)
                .with_context(|| format!("Failed to read markdown file: {}", path.display()))?,
        ),
        (Some(b), None) => Some(b.to_string()),
        (None, None) => None,
    };
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved_page =
        resolve_page_query_in_section(&storage, &graph, page, section, false).await?;
    let updated = update_onenote_content_helper_with_guard(
        &storage,
        &graph,
        &resolved_page,
        title,
        loaded_body.as_deref(),
        block_id,
        replace_text,
        expected_content,
        force,
    )
    .await?;
    nudge_live_daemon(
        cfg,
        &graph,
        sync::SyncCommand::HydratePage {
            page_key: updated.summary.page_key.clone(),
        },
    )
    .await;
    if !updated.summary.section_id.trim().is_empty() {
        nudge_live_daemon(
            cfg,
            &graph,
            sync::SyncCommand::RefreshSectionPages {
                section_key: updated
                    .summary
                    .scope
                    .make_entity_key(&updated.summary.section_id),
            },
        )
        .await;
    }
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&write_result_json(&updated, block_id, None, full))?
        );
    } else {
        println!(
            "Updated page '{}' ({}).",
            sanitize_terminal_inline(&updated.summary.title),
            sanitize_terminal_inline(&updated.summary.page_key),
        );
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_check(
    cfg: &AppConfig,
    page: &str,
    section: Option<&str>,
    block_id: Option<&str>,
    index: Option<usize>,
    completed: Option<bool>,
    expected_content: Option<&str>,
    force: bool,
    json_out: bool,
    full: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved_page =
        resolve_page_query_in_section(&storage, &graph, page, section, false).await?;
    let (updated, target_id, completed) = change_onenote_todo_helper_with_guard(
        &storage,
        &graph,
        &resolved_page,
        block_id,
        index,
        completed,
        expected_content,
        force,
    )
    .await?;
    nudge_live_daemon(
        cfg,
        &graph,
        sync::SyncCommand::HydratePage {
            page_key: updated.summary.page_key.clone(),
        },
    )
    .await;
    if !updated.summary.section_id.trim().is_empty() {
        nudge_live_daemon(
            cfg,
            &graph,
            sync::SyncCommand::RefreshSectionPages {
                section_key: updated
                    .summary
                    .scope
                    .make_entity_key(&updated.summary.section_id),
            },
        )
        .await;
    }
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&write_result_json(
                &updated,
                Some(&target_id),
                Some(completed),
                full,
            ))?
        );
    } else {
        let state = if completed { "checked [x]" } else { "unchecked [ ]" };
        println!("Toggled to-do {} -> {state}", sanitize_terminal_inline(&target_id));
    }
    Ok(())
}

pub(super) async fn cmd_delete(
    cfg: &AppConfig,
    page: &str,
    section: Option<&str>,
    yes: bool,
    json_out: bool,
) -> Result<()> {
    // Refuse before opening the runtime so this path never touches the network.
    if !yes && (json_out || !io::stdin().is_terminal()) {
        bail!("Refusing to delete page without `--yes` in JSON or non-interactive mode.");
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let resolved_page =
        resolve_page_query_in_section(&storage, &graph, page, section, false).await?;
    let cached_summary =
        resolver::resolve_page_cached(&storage, &resolved_page, None)?.into_option()?;
    if !yes {
        // Ask only after resolving, so the prompt names what will be deleted.
        eprint!("{}", delete_prompt_text(&resolved_page, cached_summary.as_ref()));
        io::stderr().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Cancelled.");
            return Ok(());
        }
    }
    let section_key = cached_summary.map(|s| s.scope.make_entity_key(&s.section_id));
    let deleted_key = delete_onenote_page_helper(&storage, &graph, &resolved_page).await?;
    let nudge_cmd = match section_key {
        Some(sec_key) => sync::SyncCommand::RefreshSectionPages {
            section_key: sec_key,
        },
        None => sync::SyncCommand::RefreshAll,
    };
    nudge_live_daemon(cfg, &graph, nudge_cmd).await;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "ok": true, "deleted": deleted_key }))?
        );
    } else {
        println!("Deleted OneNote page: {}", sanitize_terminal_inline(&deleted_key));
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_search(
    cfg: &AppConfig,
    query: &str,
    section: Option<&str>,
    refresh: bool,
    limit: usize,
    cursor: usize,
    json_out: bool,
) -> Result<()> {
    let mode = if refresh {
        IdentityMode::AllowNetwork
    } else {
        IdentityMode::LocalOnly
    };
    let (_auth, graph, storage) = open_runtime_inner(cfg, refresh, mode).await?;
    let (hits, has_more) =
        search_onenote_with_offset(&storage, &graph, query, section, refresh, limit, cursor)
            .await?;
    if json_out {
        let (pages, pages_with_body) = storage.count_onenote_pages()?;
        let next = next_cursor(cursor, limit, has_more);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({
                "query": query,
                "hits": hits,
                "returned": hits.len(),
                "has_more": has_more,
                "next_cursor": next,
                "cache": {
                    "pages": pages,
                    "pages_with_body": pages_with_body,
                },
            }))?
        );
    } else if hits.is_empty() {
        println!(
            "No matching OneNote pages found for '{}'.",
            sanitize_terminal_inline(query),
        );
    } else {
        for hit in &hits {
            let loc = match (hit.notebook_name.as_deref(), hit.section_name.as_deref()) {
                (Some(n), Some(s)) => format!(
                    "{} › {}",
                    sanitize_terminal_inline(n),
                    sanitize_terminal_inline(s),
                ),
                (None, Some(s)) => sanitize_terminal_inline(s).into_owned(),
                _ => sanitize_terminal_inline(&hit.section_id).into_owned(),
            };
            println!(
                "{}  ({loc})  [{}]",
                sanitize_terminal_inline(&hit.title),
                sanitize_terminal_inline(&hit.page_key),
            );
            if !hit.snippet.trim().is_empty() {
                println!("  {}", truncate_single_line(&hit.snippet, 120));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub(super) async fn cmd_resource(
    cfg: &AppConfig,
    page: &str,
    index: Option<usize>,
    out: Option<&Path>,
    preview: bool,
    protocol: Option<&str>,
    cols: Option<u16>,
    rows: Option<u16>,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let pref = protocol
        .and_then(ImageProtocolPreference::parse_str)
        .unwrap_or(cfg.image_protocol);

    if preview && out.is_none() && !json_out {
        let detail = load_onenote_page_detail(&storage, &graph, page, false).await?;
        let (_, res) = select_onenote_resource(&detail, index, true)?;
        let url = res.fullres_url.as_deref().unwrap_or(&res.resource_url);
        let (_, bytes) = graph.fetch_onenote_resource_bytes(url).await?;
        let (c, r) = default_preview_dimensions(cols, rows);
        let rendered = img_preview::render_ansi_stdout_with_pref(&bytes, c, r, pref)?;
        print!("{rendered}");
        if !rendered.ends_with('\n') {
            println!();
        }
        io::stdout().flush()?;
        return Ok(());
    }

    let (_detail, _idx, downloaded) =
        download_onenote_resource_helper(cfg, &storage, &graph, page, index, out).await?;

    if json_out {
        println!("{}", serde_json::to_string_pretty(&downloaded)?);
    } else {
        println!(
            "Downloaded OneNote resource: {} ({} bytes, {})",
            sanitize_terminal_inline(&downloaded.path),
            downloaded.size_bytes,
            sanitize_terminal_inline(&downloaded.mime_type),
        );
        if preview && downloaded.is_image {
            if let Ok(bytes) = std::fs::read(&downloaded.path) {
                let (c, r) = default_preview_dimensions(cols, rows);
                if let Ok(rendered) =
                    img_preview::render_ansi_stdout_with_pref(&bytes, c, r, pref)
                {
                    print!("{rendered}");
                    if !rendered.ends_with('\n') {
                        println!();
                    }
                }
            }
        }
    }
    Ok(())
}

pub(super) fn cmd_preview_file(
    cfg: &AppConfig,
    path: &Path,
    protocol: Option<&str>,
    cols: Option<u16>,
    rows: Option<u16>,
) -> Result<()> {
    let bytes = std::fs::read(path)
        .with_context(|| format!("Failed to read image file: {}", path.display()))?;
    let pref = protocol
        .and_then(ImageProtocolPreference::parse_str)
        .unwrap_or(cfg.image_protocol);
    let (c, r) = default_preview_dimensions(cols, rows);
    let rendered = img_preview::render_ansi_stdout_with_pref(&bytes, c, r, pref)?;
    print!("{rendered}");
    if !rendered.ends_with('\n') {
        println!();
    }
    io::stdout().flush()?;
    Ok(())
}
