//! Shared OneNote mutation, conflict-guarded edit, resource, and search helpers (CLI / HTTP / MCP).

use std::path::{Path, PathBuf};

use anyhow::{bail, Context, Result};

use crate::{
    config::AppConfig,
    graph::{media, GraphClient},
    models::{
        split_onenote_entity_key, DownloadedResource, InlineImageUpload, OnenoteBlock,
        OnenotePageDetail, OnenotePageSummary, OnenoteResource, OnenoteSearchHit, OnenoteSection,
        OnenoteTagKind, OutgoingOnenotePage,
    },
    parser, resolver,
    storage::Storage,
    sync,
};

use super::helpers_read::{
    fetch_onenote_page_detail_remote, list_onenote_pages_with_cache, load_onenote_page_detail,
    resolve_onenote_section_query,
};

pub fn load_inline_images_from_paths(paths: &[PathBuf]) -> Result<Vec<InlineImageUpload>> {
    let mut out = Vec::with_capacity(paths.len());
    for (idx, path) in paths.iter().enumerate() {
        let bytes = std::fs::read(path)
            .with_context(|| format!("Failed to read image file: {}", path.display()))?;
        let file_name = path
            .file_name()
            .and_then(|s| s.to_str())
            .unwrap_or("image.png")
            .to_string();
        let content_type = media::guess_mime_type(path).to_string();
        let part_id = format!("img{}", idx + 1);
        out.push(InlineImageUpload {
            temp_id: part_id,
            mime_type: content_type,
            bytes,
            file_name: Some(file_name),
        });
    }
    Ok(out)
}

pub async fn create_onenote_page_helper(
    storage: &Storage,
    graph: &GraphClient,
    section_query: &str,
    title: &str,
    markdown_body: &str,
    inline_images: Vec<InlineImageUpload>,
    level: Option<i32>,
) -> Result<(OnenoteSection, OnenotePageSummary)> {
    let sec = resolve_onenote_section_query(storage, graph, section_query, false).await?;
    let outgoing = OutgoingOnenotePage {
        title: title.trim().to_string(),
        markdown_body: markdown_body.to_string(),
        inline_images,
        level,
    };
    let created = graph
        .create_onenote_page(&sec.scope, &sec.id, &outgoing)
        .await?;
    storage.upsert_onenote_pages(std::slice::from_ref(&created))?;
    Ok((sec, created))
}

pub async fn rename_onenote_section_helper(
    storage: &Storage,
    graph: &GraphClient,
    section_key: &str,
    name: &str,
) -> Result<OnenoteSection> {
    let trimmed = section_key.trim();
    let (scope, id) = split_onenote_entity_key(trimmed);
    let section = match graph.rename_onenote_section(&scope, &id, name).await {
        Ok(section) => section,
        Err(err) if crate::graph::onenote::is_not_found_error(&err) => {
            return Err(err.context(format!(
                "OneNote section not found: '{trimmed}'. rename-section takes a section ID or scoped key from 'onenote sections --json'."
            )));
        }
        Err(err) => return Err(err),
    };
    storage.upsert_onenote_sections(std::slice::from_ref(&section))?;
    Ok(section)
}

pub async fn append_onenote_page_helper(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    markdown: &str,
    inline_images: &[InlineImageUpload],
) -> Result<OnenotePageDetail> {
    let q = page_query.trim();
    if q.is_empty() {
        bail!("Page ID or title is empty.");
    }
    let cached_detail = if let Some(detail) = storage.get_onenote_page_detail(q)? {
        Some(detail)
    } else if let Some(summary) = resolver::resolve_page_cached(storage, q, None)?.into_option()? {
        storage.get_onenote_page_detail(&summary.page_key)?
    } else {
        None
    };

    let (mut detail, used_cache) = match cached_detail {
        Some(detail) => (detail, true),
        None => (
            load_onenote_page_detail(storage, graph, q, false).await?,
            false,
        ),
    };

    if let Err(err) = graph
        .append_onenote_page_markdown(
            &detail.summary.scope,
            &detail.summary.id,
            detail.primary_div_id.as_deref(),
            markdown,
            inline_images,
        )
        .await
    {
        let can_retry_stale_target = used_cache
            && matches!(
                crate::graph::client::graph_http_status(&err),
                Some(400 | 404 | 410)
            );
        if can_retry_stale_target {
            detail = load_onenote_page_detail(storage, graph, q, false).await?;
            graph
                .append_onenote_page_markdown(
                    &detail.summary.scope,
                    &detail.summary.id,
                    detail.primary_div_id.as_deref(),
                    markdown,
                    inline_images,
                )
                .await?;
        } else {
            return Err(err);
        }
    }

    let updated = graph
        .get_onenote_page_detail(
            &detail.summary.scope,
            &detail.summary.id,
            Some(detail.summary.clone()),
        )
        .await?;
    storage.upsert_onenote_page_detail(&updated)?;
    Ok(updated)
}

/// Result of a conflict-guarded OneNote page or block write.
#[derive(Debug, Clone)]
pub enum GuardedUpdateResult {
    Updated(OnenotePageDetail),
    Conflict {
        latest: OnenotePageDetail,
        message: String,
    },
}

/// Result of a conflict-guarded OneNote to-do checkbox toggle.
#[derive(Debug, Clone)]
pub enum GuardedTodoResult {
    Updated {
        detail: OnenotePageDetail,
        block_id: String,
        completed: bool,
    },
    Conflict {
        latest: OnenotePageDetail,
        message: String,
    },
}

fn load_cached_detail_for_query(
    storage: &Storage,
    page_query: &str,
) -> Result<Option<OnenotePageDetail>> {
    let q = page_query.trim();
    if q.is_empty() {
        return Ok(None);
    }
    if let Some(d) = storage.get_onenote_page_detail(q)? {
        return Ok(Some(d));
    }
    match resolver::resolve_page_cached(storage, q, None)?.into_option()? {
        Some(s) => storage.get_onenote_page_detail(&s.page_key),
        None => Ok(None),
    }
}

fn normalize_text_for_compare(input: &str) -> String {
    input.replace("\r\n", "\n").trim().to_string()
}

pub async fn update_onenote_page_helper(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    title: Option<&str>,
    body_markdown: Option<&str>,
    block_id: Option<&str>,
) -> Result<OnenotePageDetail> {
    update_onenote_page_helper_with_guard(
        storage,
        graph,
        page_query,
        title,
        body_markdown,
        block_id,
        None,
        false,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn update_onenote_page_helper_with_guard(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    title: Option<&str>,
    body_markdown: Option<&str>,
    block_id: Option<&str>,
    expected_content: Option<&str>,
    force: bool,
) -> Result<OnenotePageDetail> {
    update_onenote_content_helper_with_guard(
        storage,
        graph,
        page_query,
        title,
        body_markdown,
        block_id,
        None,
        expected_content,
        force,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn update_onenote_content_helper_with_guard(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    title: Option<&str>,
    body_markdown: Option<&str>,
    block_id: Option<&str>,
    replace_text: Option<(&str, &str)>,
    expected_content: Option<&str>,
    force: bool,
) -> Result<OnenotePageDetail> {
    match update_onenote_content_guarded(
        storage,
        graph,
        page_query,
        title,
        body_markdown,
        block_id,
        replace_text,
        expected_content,
        force,
    )
    .await?
    {
        GuardedUpdateResult::Updated(detail) => Ok(detail),
        GuardedUpdateResult::Conflict { message, .. } => bail!("{message}"),
    }
}

/// Preflight read-compare conflict guard for OneNote page/block updates.
/// Note: Microsoft Graph v1.0 OneNote `PATCH /onenote/pages/{id}/content` does not provide
/// a verified atomic server precondition (`If-Match` / `412`), so this performs a live preflight
/// read and target-block content comparison before sending `PATCH`.
#[allow(clippy::too_many_arguments)]
pub async fn update_onenote_page_guarded(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    title: Option<&str>,
    body_markdown: Option<&str>,
    block_id: Option<&str>,
    expected_content: Option<&str>,
    force: bool,
) -> Result<GuardedUpdateResult> {
    update_onenote_content_guarded(
        storage,
        graph,
        page_query,
        title,
        body_markdown,
        block_id,
        None,
        expected_content,
        force,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn update_onenote_content_guarded(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    title: Option<&str>,
    body_markdown: Option<&str>,
    block_id: Option<&str>,
    replace_text: Option<(&str, &str)>,
    expected_content: Option<&str>,
    force: bool,
) -> Result<GuardedUpdateResult> {
    let trimmed_bid = block_id.map(str::trim).filter(|s| !s.is_empty());
    let trimmed_title = title.map(str::trim).filter(|s| !s.is_empty());
    if replace_text.is_some() && (trimmed_bid.is_none() || body_markdown.is_some()) {
        bail!("Provide a block ID and either Markdown or a text replacement.");
    }
    if trimmed_bid.is_some() && body_markdown.is_none() && replace_text.is_none() {
        bail!("A block update requires Markdown or a text replacement.");
    }
    if trimmed_title.is_none() && body_markdown.is_none() && replace_text.is_none() {
        bail!(
            "Specify --title, --body, --file, or --block-id with --replace-text and --with-text."
        );
    }
    let cached_before = load_cached_detail_for_query(storage, page_query)?;
    let detail = fetch_onenote_page_detail_remote(storage, graph, page_query).await?;
    let implicit_baseline = !force && expected_content.is_none() && cached_before.is_some();
    if !implicit_baseline {
        storage.upsert_onenote_page_detail(&detail)?;
    }

    if !force {
        if let Some(bid) = trimmed_bid {
            let live_block = parser::find_block_by_element_id(&detail, bid);
            if let Some(exp) = expected_content {
                match live_block {
                    None => {
                        return Ok(GuardedUpdateResult::Conflict {
                            message: format!(
                                "Conflict: target block '{bid}' no longer exists on remote page '{}'. Remote content preserved.",
                                detail.summary.title
                            ),
                            latest: detail,
                        });
                    }
                    Some(blk) if !parser::block_matches_expected(blk, exp) => {
                        return Ok(GuardedUpdateResult::Conflict {
                            message: format!(
                                "Conflict: target block '{bid}' changed remotely since your baseline read on '{}'. Remote content preserved; re-read or pass --force to overwrite.",
                                detail.summary.title
                            ),
                            latest: detail,
                        });
                    }
                    Some(_) => {}
                }
            } else if let Some(cached_blk) = cached_before
                .as_ref()
                .and_then(|c| parser::find_block_by_element_id(c, bid))
            {
                let unchanged = live_block
                    .is_some_and(|live_blk| parser::blocks_have_same_content(cached_blk, live_blk));
                if !unchanged {
                    return Ok(GuardedUpdateResult::Conflict {
                        message: format!(
                            "Conflict: target block '{bid}' changed remotely since your last read on '{}'. Remote content preserved; re-read or pass --force to overwrite.",
                            detail.summary.title
                        ),
                        latest: detail,
                    });
                }
            }
        } else if body_markdown.is_some() {
            if let Some(exp) = expected_content {
                let exp_norm = normalize_text_for_compare(exp);
                let matches = normalize_text_for_compare(&detail.body_markdown) == exp_norm
                    || normalize_text_for_compare(&detail.body_plain) == exp_norm
                    || normalize_text_for_compare(&detail.raw_xhtml) == exp_norm;
                if !matches {
                    return Ok(GuardedUpdateResult::Conflict {
                        message: format!(
                            "Conflict: page body changed remotely since your baseline read on '{}'. Remote content preserved; re-read or pass --force to overwrite.",
                            detail.summary.title
                        ),
                        latest: detail,
                    });
                }
            } else if let Some(ref cached) = cached_before {
                if normalize_text_for_compare(&cached.body_markdown)
                    != normalize_text_for_compare(&detail.body_markdown)
                {
                    return Ok(GuardedUpdateResult::Conflict {
                        message: format!(
                            "Conflict: page body changed remotely since your last read on '{}'. Remote content preserved; re-read or pass --force to overwrite.",
                            detail.summary.title
                        ),
                        latest: detail,
                    });
                }
            }
        } else if trimmed_title.is_some() {
            if let Some(exp) = expected_content {
                if detail.summary.title.trim() != exp.trim() {
                    return Ok(GuardedUpdateResult::Conflict {
                        message: format!(
                            "Conflict: page title changed remotely since your baseline read ('{}'). Remote content preserved; re-read or pass --force to overwrite.",
                            detail.summary.title
                        ),
                        latest: detail,
                    });
                }
            }
        }
    }
    if implicit_baseline {
        storage.upsert_onenote_page_detail(&detail)?;
    }

    let commands = if let Some(bid) = trimmed_bid {
        let mut commands: Vec<_> = trimmed_title
            .and_then(|title| parser::onenote::build_replace_title_patch(&detail, title))
            .into_iter()
            .collect();
        if let Some((from, to)) = replace_text {
            commands.extend(parser::onenote::build_replace_block_text_patch(
                &detail, bid, from, to,
            )?);
        } else if let Some(md) = body_markdown {
            commands.push(parser::onenote::build_checked_block_replacement(
                &detail, bid, md,
            )?);
        }
        commands
    } else if let Some(md) = body_markdown {
        parser::build_replace_page_body_commands(&detail, trimmed_title, md)?
    } else {
        trimmed_title
            .and_then(|title| parser::onenote::build_replace_title_patch(&detail, title))
            .into_iter()
            .collect()
    };

    if commands.is_empty() {
        return Ok(GuardedUpdateResult::Updated(detail));
    }

    graph
        .patch_onenote_page_commands(&detail.summary.scope, &detail.summary.id, &commands)
        .await?;
    let updated = graph
        .get_onenote_page_detail(
            &detail.summary.scope,
            &detail.summary.id,
            Some(detail.summary.clone()),
        )
        .await?;
    storage.upsert_onenote_page_detail(&updated)?;
    Ok(GuardedUpdateResult::Updated(updated))
}

pub async fn toggle_onenote_todo_helper(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    block_id: Option<&str>,
    todo_index: Option<usize>,
) -> Result<(OnenotePageDetail, String, bool)> {
    toggle_onenote_todo_helper_with_guard(
        storage,
        graph,
        page_query,
        block_id,
        todo_index,
        None,
        false,
    )
    .await
}

pub async fn toggle_onenote_todo_helper_with_guard(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    block_id: Option<&str>,
    todo_index: Option<usize>,
    expected_content: Option<&str>,
    force: bool,
) -> Result<(OnenotePageDetail, String, bool)> {
    change_onenote_todo_helper_with_guard(
        storage,
        graph,
        page_query,
        block_id,
        todo_index,
        None,
        expected_content,
        force,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn change_onenote_todo_helper_with_guard(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    block_id: Option<&str>,
    todo_index: Option<usize>,
    completed: Option<bool>,
    expected_content: Option<&str>,
    force: bool,
) -> Result<(OnenotePageDetail, String, bool)> {
    match change_onenote_todo_guarded(
        storage,
        graph,
        page_query,
        block_id,
        todo_index,
        completed,
        expected_content,
        force,
    )
    .await?
    {
        GuardedTodoResult::Updated {
            detail,
            block_id,
            completed,
        } => Ok((detail, block_id, completed)),
        GuardedTodoResult::Conflict { message, .. } => bail!("{message}"),
    }
}

pub async fn toggle_onenote_todo_guarded(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    block_id: Option<&str>,
    todo_index: Option<usize>,
    expected_content: Option<&str>,
    force: bool,
) -> Result<GuardedTodoResult> {
    change_onenote_todo_guarded(
        storage,
        graph,
        page_query,
        block_id,
        todo_index,
        None,
        expected_content,
        force,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
async fn change_onenote_todo_guarded(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    block_id: Option<&str>,
    todo_index: Option<usize>,
    requested_completed: Option<bool>,
    expected_content: Option<&str>,
    force: bool,
) -> Result<GuardedTodoResult> {
    let cached_before = load_cached_detail_for_query(storage, page_query)?;
    let detail = fetch_onenote_page_detail_remote(storage, graph, page_query).await?;
    let implicit_baseline = !force && expected_content.is_none() && cached_before.is_some();
    if !implicit_baseline {
        storage.upsert_onenote_page_detail(&detail)?;
    }
    let target_block: &OnenoteBlock =
        if let Some(bid) = block_id.map(str::trim).filter(|s| !s.is_empty()) {
            match parser::onenote::find_todo_block_by_element_id(&detail, bid) {
                Some(blk) => blk,
                None if expected_content.is_some() && !force => {
                    return Ok(GuardedTodoResult::Conflict {
                        message: format!(
                            "Conflict: to-do block '{bid}' no longer exists on remote page '{}'. Remote content preserved.",
                            detail.summary.title
                        ),
                        latest: detail,
                    });
                }
                None => bail!("Block '{bid}' not found on page."),
            }
        } else {
            let idx = todo_index.unwrap_or(0);
            let todos: Vec<&OnenoteBlock> = detail
                .blocks
                .iter()
                .filter(|b| b.tag.as_ref().is_some_and(OnenoteTagKind::is_todo))
                .collect();
            if todos.is_empty() {
                bail!("This page has no OneNote to-do checkbox blocks to toggle.");
            }
            todos.get(idx).copied().with_context(|| {
                format!(
                    "To-do checkbox index {idx} out of range (total {}).",
                    todos.len()
                )
            })?
        };

    if !force {
        if let Some(exp) = expected_content {
            if !parser::block_matches_expected(target_block, exp) {
                let bid = target_block.element_id.as_deref().unwrap_or("to-do");
                return Ok(GuardedTodoResult::Conflict {
                    message: format!(
                        "Conflict: to-do block '{bid}' changed remotely since your last read on '{}'. Remote content preserved; re-read or pass --force to overwrite.",
                        detail.summary.title
                    ),
                    latest: detail,
                });
            }
        } else if let Some(eid) = target_block.element_id.as_deref() {
            if let Some(cached_blk) = cached_before
                .as_ref()
                .and_then(|c| parser::find_block_by_element_id(c, eid))
            {
                if !parser::blocks_have_same_content(cached_blk, target_block) {
                    return Ok(GuardedTodoResult::Conflict {
                        message: format!(
                            "Conflict: to-do block '{eid}' changed remotely since your last read on '{}'. Remote content preserved; re-read or pass --force to overwrite.",
                            detail.summary.title
                        ),
                        latest: detail,
                    });
                }
            }
        }
    }
    if implicit_baseline {
        storage.upsert_onenote_page_detail(&detail)?;
    }

    let Some(OnenoteTagKind::Todo { completed: current }) = target_block.tag else {
        bail!("Selected block is not a OneNote to-do item.");
    };
    let completed = requested_completed.unwrap_or(!current);
    if completed == current {
        let block_id = target_block
            .element_id
            .clone()
            .context("Selected to-do item lacks an element_id.")?;
        return Ok(GuardedTodoResult::Updated {
            detail,
            block_id,
            completed,
        });
    }
    let cmd = parser::onenote::build_todo_state_patch(target_block, completed)
        .context("Selected to-do item lacks an element_id.")?;
    let target_id = cmd.target.clone();
    graph
        .patch_onenote_page_commands(&detail.summary.scope, &detail.summary.id, &[cmd])
        .await?;
    let updated = graph
        .get_onenote_page_detail(
            &detail.summary.scope,
            &detail.summary.id,
            Some(detail.summary.clone()),
        )
        .await?;
    storage.upsert_onenote_page_detail(&updated)?;
    Ok(GuardedTodoResult::Updated {
        detail: updated,
        block_id: target_id,
        completed,
    })
}

pub async fn delete_onenote_page_helper(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
) -> Result<String> {
    let q = page_query.trim();
    if q.is_empty() {
        bail!("Page ID or title is empty.");
    }
    let cached = resolver::resolve_page_cached(storage, q, None)?.into_option()?;
    let was_cached = cached.is_some();
    let (scope, raw_id, page_key) = match cached {
        Some(s) => (s.scope, s.id, s.page_key),
        None => {
            let (s, id) = split_onenote_entity_key(q);
            let key = s.make_entity_key(&id);
            (s, id, key)
        }
    };
    if let Err(err) = graph.delete_onenote_page(&scope, &raw_id).await {
        if !was_cached && crate::graph::onenote::is_not_found_error(&err) {
            return Err(err.context(format!(
                "OneNote page not found: '{q}' is neither a cached title nor a page ID. Pass a page key from 'onenote pages SECTION --json', or add --section to look the title up in one section."
            )));
        }
        return Err(err);
    }
    let _ = storage.delete_onenote_page(&page_key);
    Ok(page_key)
}

/// Pages read from Graph when a title is looked up inside one section
/// (well above the per-section cap used by sync).
const PAGE_TITLE_LOOKUP_LIMIT: usize = 5_000;

pub async fn resolve_page_query_in_section(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    section_query: Option<&str>,
    offline: bool,
) -> Result<String> {
    let q = page_query.trim();
    if q.is_empty() {
        bail!("Page ID or title is empty.");
    }
    let Some(sec_q) = section_query.map(str::trim).filter(|s| !s.is_empty()) else {
        return Ok(q.to_string());
    };
    let sec = resolve_onenote_section_query(storage, graph, sec_q, offline).await?;
    match resolver::resolve_page_cached(storage, q, Some(&sec.section_key))? {
        resolver::PageResolution::Found(p) => return Ok(p.page_key),
        resolver::PageResolution::Ambiguous { message, .. } => bail!("{message}"),
        resolver::PageResolution::NotFound => {}
    }
    if offline {
        bail!(
            "OneNote page '{q}' not found in section '{}' offline cache.",
            sec.display_name
        );
    }
    let (_sec, pages, has_more) = list_onenote_pages_with_cache(
        storage,
        graph,
        &sec.section_key,
        PAGE_TITLE_LOOKUP_LIMIT,
        0,
        false,
        false,
        false,
    )
    .await?;
    match resolver::resolve_page_in_slice(&pages, q)? {
        resolver::PageResolution::Found(p) => Ok(p.page_key),
        resolver::PageResolution::Ambiguous { message, .. } => bail!("{message}"),
        resolver::PageResolution::NotFound if has_more == Some(true) => bail!(
            "OneNote page '{q}' not found in the first {PAGE_TITLE_LOOKUP_LIMIT} pages of section '{}'. The section has more pages; pass a page key from 'onenote pages SECTION --json' instead of a title.",
            sec.display_name
        ),
        resolver::PageResolution::NotFound => bail!(
            "OneNote page '{q}' not found in section '{}'.",
            sec.display_name
        ),
    }
}

/// Scoped keys of cached sections matching a plain `--section` name filter:
/// raw section ID equality or a case-insensitive name substring. Lowercasing
/// stays in Rust because SQLite's `LOWER` only folds ASCII.
fn section_keys_matching_name(storage: &Storage, sec_q: &str) -> Result<Vec<String>> {
    let needle = sec_q.to_lowercase();
    let matches = |id: &str, name: Option<&str>| {
        id == sec_q || name.is_some_and(|n| n.to_lowercase().contains(&needle))
    };
    let mut known = std::collections::HashSet::new();
    let mut keys = std::collections::BTreeSet::new();
    for sec in storage.list_onenote_sections(None)? {
        if matches(&sec.id, Some(&sec.display_name)) {
            keys.insert(sec.section_key.clone());
        }
        known.insert(sec.section_key);
    }
    // Fallback for cached pages whose section row is missing from the sections table.
    for (section_key, section_id, section_name) in storage.list_onenote_page_section_refs()? {
        if !known.contains(&section_key) && matches(&section_id, section_name.as_deref()) {
            keys.insert(section_key);
        }
    }
    Ok(keys.into_iter().collect())
}

pub fn select_onenote_resource(
    detail: &OnenotePageDetail,
    index: Option<usize>,
    prefer_image: bool,
) -> Result<(usize, &OnenoteResource)> {
    if detail.resources.is_empty() {
        bail!("Page '{}' has no embedded images or file resources.", detail.summary.title);
    }
    if let Some(idx) = index {
        let res = detail.resources.get(idx).with_context(|| {
            format!(
                "Resource index {idx} out of range (page has {} resource(s)).",
                detail.resources.len()
            )
        })?;
        return Ok((idx, res));
    }
    if prefer_image {
        if let Some((idx, res)) = detail.resources.iter().enumerate().find(|(_, r)| r.is_image) {
            return Ok((idx, res));
        }
    }
    Ok((0, &detail.resources[0]))
}

pub fn select_onenote_resource_by_index_or_url<'a>(
    detail: &'a OnenotePageDetail,
    index: Option<usize>,
    url: Option<&str>,
    prefer_image: bool,
) -> Result<(usize, &'a OnenoteResource)> {
    let trimmed_url = url.map(str::trim).filter(|s| !s.is_empty());
    if let Some(u) = trimmed_url {
        if detail.resources.is_empty() {
            bail!(
                "Resource URL '{u}' not found on page '{}' (page has no embedded resources).",
                detail.summary.title
            );
        }
        let matches: Vec<(usize, &'a OnenoteResource)> = detail
            .resources
            .iter()
            .enumerate()
            .filter(|(_, r)| r.resource_url == u || r.fullres_url.as_deref() == Some(u))
            .collect();
        if matches.is_empty() {
            bail!(
                "Resource URL '{u}' not found on page '{}'.",
                detail.summary.title
            );
        }
        if let Some(idx) = index {
            if let Some(&(matched_idx, res)) = matches.iter().find(|(i, _)| *i == idx) {
                return Ok((matched_idx, res));
            }
            if idx >= detail.resources.len() {
                bail!(
                    "Resource index {idx} out of range (page has {} resource(s)).",
                    detail.resources.len()
                );
            }
            bail!(
                "Resource index {idx} does not match requested url '{u}' on page '{}'.",
                detail.summary.title
            );
        }
        return Ok(matches[0]);
    }
    select_onenote_resource(detail, index, prefer_image)
}

pub async fn download_onenote_resource_helper(
    cfg: &AppConfig,
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    index: Option<usize>,
    out_dir: Option<&Path>,
) -> Result<(OnenotePageDetail, usize, DownloadedResource)> {
    download_onenote_resource_helper_with_options(
        cfg, storage, graph, page_query, None, index, None, out_dir,
    )
    .await
}

#[allow(clippy::too_many_arguments)]
pub async fn download_onenote_resource_helper_with_options(
    cfg: &AppConfig,
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    section_query: Option<&str>,
    index: Option<usize>,
    url: Option<&str>,
    out_dir: Option<&Path>,
) -> Result<(OnenotePageDetail, usize, DownloadedResource)> {
    let resolved_page =
        resolve_page_query_in_section(storage, graph, page_query, section_query, false).await?;
    let detail = load_onenote_page_detail(storage, graph, &resolved_page, false).await?;
    let (resolved_idx, res) =
        select_onenote_resource_by_index_or_url(&detail, index, url, false)?;
    let dest_dir = out_dir
        .map(PathBuf::from)
        .unwrap_or_else(|| cfg.resolved_download_dir());
    let downloaded = graph.download_onenote_resource(res, &dest_dir).await?;
    Ok((detail, resolved_idx, downloaded))
}

pub async fn search_onenote_helper(
    storage: &Storage,
    graph: &GraphClient,
    query: &str,
    section_filter: Option<&str>,
    refresh: bool,
    limit: usize,
) -> Result<Vec<OnenoteSearchHit>> {
    let (hits, _has_more) =
        search_onenote_with_offset(storage, graph, query, section_filter, refresh, limit, 0)
            .await?;
    Ok(hits)
}

pub async fn search_onenote_with_offset(
    storage: &Storage,
    graph: &GraphClient,
    query: &str,
    section_filter: Option<&str>,
    refresh: bool,
    limit: usize,
    offset: usize,
) -> Result<(Vec<OnenoteSearchHit>, Option<bool>)> {
    if refresh {
        let _ = sync::bootstrap_onenote(graph, storage).await?;
    }
    if limit == 0 {
        return Ok((Vec::new(), None));
    }
    let sec_filter = section_filter.map(str::trim).filter(|s| !s.is_empty());
    let end = offset.saturating_add(limit);
    let want = end.saturating_add(1);
    let raw = match sec_filter {
        None => storage.search_onenote_pages(query, None, want)?,
        Some(sec_q) if sec_q.contains('|') => {
            storage.search_onenote_pages(query, Some(sec_q), want)?
        }
        Some(sec_q) if sec_q.contains('/') => {
            let sec = resolve_onenote_section_query(storage, graph, sec_q, true).await?;
            storage.search_onenote_pages(query, Some(&sec.section_key), want)?
        }
        // A plain name is resolved to section keys first so the SQL limit applies
        // inside those sections instead of to a window of global hits.
        Some(sec_q) => {
            let keys = section_keys_matching_name(storage, sec_q)?;
            if keys.is_empty() {
                return Ok((Vec::new(), Some(false)));
            }
            storage.search_onenote_pages_in_sections(query, &keys, want)?
        }
    };
    let has_more = Some(raw.len() > end);
    let hits = raw.into_iter().skip(offset).take(limit).collect();
    Ok((hits, has_more))
}

