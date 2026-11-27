//! JSON envelope builders, prompt formatting, markdown input loading, and text helpers.

use std::path::Path;

use anyhow::{Context, Result};
use serde_json::json;

use crate::{
    models::{OnenotePageDetail, OnenotePageSummary},
    safe_text::sanitize_terminal_inline,
};

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum PageSurface {
    Cli,
    Mcp,
}

const COMPACT_PAGE_OMITTED_FIELDS: &[&str] = &[
    "raw_xhtml",
    "body_plain",
    "primary_div_id",
    "summary.created_at",
    "summary.content_url",
    "summary.order",
    "summary.preview_text",
];

pub(crate) fn page_content_json(
    detail: &OnenotePageDetail,
    compact: bool,
    surface: PageSurface,
) -> serde_json::Value {
    if !compact {
        return json!(detail);
    }
    let page_key = &detail.summary.page_key;
    let hint = match surface {
        PageSurface::Cli => format!(
            "Run: onenote read \"{page_key}\" --json --full  (adds blocks[].element_id, raw_xhtml, resources)"
        ),
        PageSurface::Mcp => {
            let encoded_key =
                serde_json::to_string(page_key).unwrap_or_else(|_| format!("\"{page_key}\""));
            format!(
                "Call get_page with {{\"page\":{encoded_key},\"compact\":false}} for blocks[].element_id, raw_xhtml and resources"
            )
        }
    };
    json!({
        "page_key": detail.summary.page_key,
        "id": detail.summary.id,
        "scope": detail.summary.scope,
        "title": detail.summary.title,
        "section_id": detail.summary.section_id,
        "section_name": detail.summary.section_name,
        "notebook_id": detail.summary.notebook_id,
        "notebook_name": detail.summary.notebook_name,
        "level": detail.summary.level,
        "last_modified_at": detail.summary.last_modified_at,
        "web_url": detail.summary.web_url,
        "has_todo": detail.summary.has_todo,
        "synced_at": detail.synced_at,
        "body_markdown": detail.body_markdown,
        "omitted": {
            "fields": COMPACT_PAGE_OMITTED_FIELDS,
            "blocks": detail.blocks.len(),
            "resources": detail.resources.len(),
        },
        "hint": hint,
    })
}

pub(crate) fn write_result_json(
    detail: &OnenotePageDetail,
    block_id: Option<&str>,
    completed: Option<bool>,
    full: bool,
) -> serde_json::Value {
    let mut out = json!({
        "ok": true,
        "page_key": detail.summary.page_key,
        "title": detail.summary.title,
        "last_modified_at": detail.summary.last_modified_at,
    });
    if let Some(bid) = block_id.map(str::trim).filter(|s| !s.is_empty()) {
        out["block_id"] = json!(bid);
    }
    if let Some(done) = completed {
        out["completed"] = json!(done);
    }
    if full {
        out["page"] = json!(detail);
    }
    out
}

pub(super) fn read_json_is_compact(format_explicit: bool, compact: bool, full: bool) -> bool {
    if full {
        false
    } else if compact {
        true
    } else {
        !format_explicit
    }
}

/// Confirmation question for `onenote delete`, naming the page that was resolved.
pub(super) fn delete_prompt_text(resolved_query: &str, cached: Option<&OnenotePageSummary>) -> String {
    match cached {
        Some(page) => {
            let notebook = page
                .notebook_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or("(unknown notebook)");
            let section = page
                .section_name
                .as_deref()
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .unwrap_or(page.section_id.as_str());
            let modified = page.last_modified_at.as_deref().unwrap_or("unknown");
            format!(
                "Delete this OneNote page?\n  Title:         {}\n  Location:      {} / {}\n  Last modified: {}\n  Page key:      {}\n[y/N]: ",
                sanitize_terminal_inline(&page.title),
                sanitize_terminal_inline(notebook),
                sanitize_terminal_inline(section),
                sanitize_terminal_inline(modified),
                sanitize_terminal_inline(&page.page_key),
            )
        }
        None => {
            let safe_query = sanitize_terminal_inline(resolved_query);
            format!(
                "'{safe_query}' is not in the local cache, so its title and location cannot be shown.\nIt will be sent to Microsoft Graph as a raw page ID.\nDelete OneNote page ID '{safe_query}'? [y/N]: "
            )
        }
    }
}

pub(super) fn resolve_markdown_input(inline: Option<&str>, file: Option<&Path>) -> Result<String> {
    if let Some(path) = file {
        return std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read markdown file: {}", path.display()));
    }
    Ok(inline.unwrap_or_default().to_string())
}

pub(super) fn default_preview_dimensions(cols: Option<u16>, rows: Option<u16>) -> (u16, u16) {
    let (tw, th) = crossterm::terminal::size().unwrap_or((100, 32));
    let c = cols.unwrap_or_else(|| tw.saturating_sub(4).clamp(24, 96));
    let r = rows.unwrap_or_else(|| th.saturating_sub(6).clamp(10, 32));
    (c, r)
}

pub(super) fn format_unix_ts(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| ts.to_string())
}

pub(super) fn truncate_single_line(input: &str, max_chars: usize) -> String {
    let single = sanitize_terminal_inline(input);
    let trimmed = single.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let prefix: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{prefix}…")
    }
}
