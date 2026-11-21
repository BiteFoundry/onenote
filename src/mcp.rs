//! Standalone Microsoft OneNote stdio MCP (Model Context Protocol, JSON-RPC 2.0) server (`onenote mcp`).
//!
//! Exposes these tools:
//! `whoami`, `list_notebooks`, `list_sections`, `list_pages`, `get_page`,
//! `create_notebook`, `create_section`, `rename_section`, `create_page`, `copy_page`, `append_page`, `update_block`, `update_page`,
//! `toggle_todo`, `delete_page`, `search_pages`, `download_resource`.

use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};

use crate::{
    cli,
    config::AppConfig,
    graph::{
        copy::{DEFAULT_COPY_WAIT_SECONDS, MAX_COPY_WAIT_SECONDS},
        GraphClient,
    },
    resolver,
    storage::Storage,
};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

pub async fn run(config: AppConfig) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();
    let mut runtime: Option<(GraphClient, Storage)> = None;

    for line_res in stdin.lock().lines() {
        let line = match line_res {
            Ok(l) => l,
            Err(_) => break,
        };
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        let req: Value = match serde_json::from_str(trimmed) {
            Ok(v) => v,
            Err(err) => {
                let resp = json!({
                    "jsonrpc": "2.0",
                    "id": Value::Null,
                    "error": { "code": -32700, "message": format!("Parse error: {err}") }
                });
                writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
                stdout.flush()?;
                continue;
            }
        };
        if let Some(resp_opt) = handle_protocol_value(&req) {
            if let Some(resp) = resp_opt {
                writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
                stdout.flush()?;
            }
            continue;
        }
        if runtime.is_none() {
            let params = req.get("params");
            let tool_name = params
                .and_then(|p| p.get("name"))
                .and_then(Value::as_str)
                .unwrap_or("")
                .trim();
            let args = params.and_then(|p| p.get("arguments"));
            let is_search_local = matches!(tool_name, "search_pages" | "onenote_search_pages")
                && args
                    .and_then(|a| a.get("refresh"))
                    .and_then(Value::as_bool)
                    != Some(true);
            let is_offline = args
                .and_then(|a| a.get("offline"))
                .and_then(Value::as_bool)
                == Some(true);
            let mode = if is_search_local || is_offline {
                cli::IdentityMode::LocalOnly
            } else {
                cli::IdentityMode::AllowNetwork
            };
            match cli::open_mcp_runtime(&config, mode).await {
                Ok(rt) => runtime = Some(rt),
                Err(err) => {
                    let id = req.get("id").cloned().unwrap_or(Value::Null);
                    let resp = json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": format!("Error: {err:#}") }],
                            "isError": true,
                        }
                    });
                    writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
                    stdout.flush()?;
                    continue;
                }
            }
        }
        let (graph, storage) = runtime.as_ref().unwrap();
        if let Some(resp) = handle_request_value(&config, graph, storage, req).await {
            writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// Answers every method that needs no runtime. Returns None for tools/call.
pub fn handle_protocol_value(req: &Value) -> Option<Option<Value>> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");

    if method == "notifications/initialized" || method.starts_with("notifications/") {
        return Some(None);
    }

    let id = id.unwrap_or(Value::Null);
    match method {
        "initialize" => Some(Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "protocolVersion": MCP_PROTOCOL_VERSION,
                "capabilities": { "tools": {} },
                "serverInfo": {
                    "name": "onenote-cli",
                    "version": env!("CARGO_PKG_VERSION"),
                }
            }
        }))),
        "ping" => Some(Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {}
        }))),
        "tools/list" => Some(Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "tools": tool_definitions() }
        }))),
        "tools/call" => None,
        other => Some(Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Method not found: {other}")
            }
        }))),
    }
}

/// Handles a single JSON-RPC 2.0 MCP message and returns an optional JSON response (`None` for notifications).
pub async fn handle_request_value(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    req: Value,
) -> Option<Value> {
    if let Some(resp) = handle_protocol_value(&req) {
        return resp;
    }

    let id = req.get("id").cloned().unwrap_or(Value::Null);
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));
    let tool_name = params
        .get("name")
        .and_then(Value::as_str)
        .unwrap_or("")
        .trim();
    let args = params
        .get("arguments")
        .cloned()
        .unwrap_or_else(|| json!({}));
    match dispatch_tool(config, graph, storage, tool_name, &args).await {
        Ok(result_val) => {
            let is_error = tool_name == "copy_page" && result_val["ok"] == false;
            let text = serde_json::to_string_pretty(&result_val)
                .unwrap_or_else(|_| result_val.to_string());
            Some(json!({
                "jsonrpc": "2.0",
                "id": id,
                "result": {
                    "content": [{ "type": "text", "text": text }],
                    "structuredContent": result_val,
                    "isError": is_error,
                }
            }))
        }
        Err(err) => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {
                "content": [{ "type": "text", "text": format!("Error: {err}") }],
                "isError": true,
            }
        })),
    }
}

pub fn tool_definitions() -> Vec<Value> {
    let update_block = json!({
        "name": "update_block",
        "description": "Replace a block with Markdown, edit a block text substring, or update the page title/body. Get current Graph element IDs and baseline content from get_page.",
        "inputSchema": {
            "type": "object",
            "required": ["page"],
            "properties": {
                "page": { "type": "string" },
                "block_id": { "type": "string", "description": "Graph element ID from get_page (e.g. `p:{...}`)" },
                "title": { "type": "string" },
                "markdown": { "type": "string" },
                "replace_text": { "type": "string", "description": "Exact text to replace in block_id; provide with_text and omit markdown" },
                "with_text": { "type": "string", "description": "New text for replace_text" },
                "expected_content": { "type": "string", "description": "Baseline block/page content from prior read; aborts with conflict if remote changed" },
                "force": { "type": "boolean", "description": "Override conflict check when true" }
            }
        }
    });
    let mut update_page = update_block.clone();
    update_page["name"] = json!("update_page");
    update_page["description"] = json!("Alias for update_block. Update a page title or body, replace a block, or edit text within a block.");
    vec![
        json!({
            "name": "whoami",
            "description": "Return the currently signed-in Microsoft 365 user profile and the active auth mode.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "list_notebooks",
            "description": "List accessible Microsoft OneNote notebooks (personal + optional Teams/Group shared notebooks).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "include_teams": { "type": "boolean", "description": "Include Microsoft Teams/Group shared notebooks" },
                    "limit": { "type": "integer", "default": 50, "minimum": 1, "maximum": 1000 },
                    "cursor": { "type": "string", "description": "next_cursor from the previous response" },
                    "refresh": { "type": "boolean", "description": "Force refresh from Microsoft Graph" },
                    "offline": { "type": "boolean", "description": "Read local SQLite cache only" }
                }
            }
        }),
        json!({
            "name": "list_sections",
            "description": "List OneNote sections across all notebooks or within a specific notebook.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "notebook": { "type": "string", "description": "Optional notebook key, ID, or display name" },
                    "limit": { "type": "integer", "default": 50, "minimum": 1, "maximum": 1000 },
                    "cursor": { "type": "string", "description": "next_cursor from the previous response" },
                    "refresh": { "type": "boolean" },
                    "offline": { "type": "boolean" }
                }
            }
        }),
        json!({
            "name": "list_pages",
            "description": "List OneNote pages inside a section (supports `Notebook/Section`, section name, or ID).",
            "inputSchema": {
                "type": "object",
                "required": ["section"],
                "properties": {
                    "section": { "type": "string" },
                    "limit": { "type": "integer", "default": 50 },
                    "cursor": { "type": "string", "description": "next_cursor from the previous response" },
                    "refresh": { "type": "boolean" },
                    "offline": { "type": "boolean" }
                }
            }
        }),
        json!({
            "name": "get_page",
            "description": "Read a OneNote page. Returns compact Markdown and identifiers by default; set compact:false for parsed blocks (element_id, data-tag), plain text, raw XHTML and embedded resources.",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string", "description": "Page key, ID, or title" },
                    "offline": { "type": "boolean" },
                    "compact": { "type": "boolean", "default": true, "description": "Default true. Set false for blocks[].element_id, raw_xhtml and resources." }
                }
            }
        }),
        json!({
            "name": "create_notebook",
            "description": "Create a new Microsoft OneNote notebook.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string" },
                    "group_id": { "type": "string" },
                    "site_id": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "rename_notebook",
            "description": "Rename an existing OneDrive or SharePoint OneNote package by explicit storage IDs. Supply a token with file write permission. The returned ID is the drive item's ID.",
            "inputSchema": {
                "type": "object",
                "required": ["drive_id", "item_id", "name"],
                "properties": {
                    "drive_id": { "type": "string" },
                    "item_id": { "type": "string", "description": "DriveItem ID with package.type oneNote" },
                    "name": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "create_section",
            "description": "Create a new OneNote section inside a notebook.",
            "inputSchema": {
                "type": "object",
                "required": ["notebook", "name"],
                "properties": {
                    "notebook": { "type": "string" },
                    "name": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "rename_section",
            "description": "Rename an existing section under /me/onenote and verify its ID and parent after the write.",
            "inputSchema": {
                "type": "object",
                "required": ["section", "name"],
                "properties": {
                    "section": { "type": "string", "description": "Section ID or me|ID from list_sections" },
                    "name": { "type": "string", "description": "New section display name" }
                }
            }
        }),
        #[cfg(windows)]
        json!({
            "name": "desktop_set_page_level",
            "description": "Set the indentation of an existing page in OneNote for Windows using its full desktop ID. The notebook must be open in the desktop app.",
            "inputSchema": {
                "type": "object",
                "required": ["page_id", "level"],
                "properties": {
                    "page_id": { "type": "string", "description": "Full Page ID from desktop GetHierarchy" },
                    "level": { "type": "integer", "minimum": 0, "maximum": 2 }
                }
            }
        }),
        #[cfg(windows)]
        json!({
            "name": "desktop_rename_section",
            "description": "Rename an existing section in OneNote for Windows using its full desktop ID. The notebook must be open in the desktop app.",
            "inputSchema": {
                "type": "object",
                "required": ["section_id", "name"],
                "properties": {
                    "section_id": { "type": "string", "description": "Full Section ID from desktop GetHierarchy" },
                    "name": { "type": "string" }
                }
            }
        }),
        json!({
            "name": "create_page",
            "description": "Create a new OneNote page from Markdown with optional local inline image paths.",
            "inputSchema": {
                "type": "object",
                "required": ["section", "title"],
                "properties": {
                    "section": { "type": "string" },
                    "title": { "type": "string" },
                    "markdown": { "type": "string" },
                    "image_paths": { "type": "array", "items": { "type": "string" } },
                    "level": { "type": "integer", "minimum": 0, "maximum": 0, "description": "Top-level page (0 or omitted)" }
                }
            }
        }),
        json!({
            "name": "copy_page",
            "description": "Copy a page via Microsoft Graph and verify its new ID and section. The original page and its links stay in place. To check a pending or interrupted copy, pass its operation_url with the original page and section.",
            "inputSchema": {
                "type": "object",
                "required": ["page", "section"],
                "properties": {
                    "page": { "type": "string", "description": "Source page ID or scoped page key" },
                    "section": { "type": "string", "description": "Destination section ID or scoped section key" },
                    "operation_url": { "type": "string", "description": "Existing Graph operation URL to poll instead of submitting a new copy" },
                    "wait_seconds": { "type": "integer", "minimum": 0, "maximum": MAX_COPY_WAIT_SECONDS, "default": DEFAULT_COPY_WAIT_SECONDS, "description": "Seconds to poll; 0 returns the operation handle immediately" }
                }
            }
        }),
        json!({
            "name": "append_page",
            "description": "Append Markdown blocks and optional inline images to an existing OneNote page.",
            "inputSchema": {
                "type": "object",
                "required": ["page", "markdown"],
                "properties": {
                    "page": { "type": "string" },
                    "markdown": { "type": "string" },
                    "image_paths": { "type": "array", "items": { "type": "string" } }
                }
            }
        }),
        update_block,
        update_page,
        json!({
            "name": "toggle_todo",
            "description": "Set or toggle a OneNote checkbox by block_id or zero-based todo_index.",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string" },
                    "block_id": { "type": "string" },
                    "todo_index": { "type": "integer", "minimum": 0 },
                    "completed": { "type": "boolean", "description": "Set this state; omit to toggle" },
                    "expected_content": { "type": "string", "description": "Baseline to-do block content from prior read; aborts with conflict if remote changed" },
                    "force": { "type": "boolean", "description": "Override conflict check when true" }
                }
            }
        }),
        json!({
            "name": "delete_page",
            "description": "Delete a OneNote page. Requires `confirm: true` to prevent accidental deletion.",
            "inputSchema": {
                "type": "object",
                "required": ["page", "confirm"],
                "properties": {
                    "page": { "type": "string" },
                    "confirm": { "type": "boolean" }
                }
            }
        }),
        json!({
            "name": "search_pages",
            "description": "Search cached OneNote pages via local SQLite FTS5 (`onenote_fts`).",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string" },
                    "section": { "type": "string" },
                    "refresh": { "type": "boolean" },
                    "limit": { "type": "integer", "default": 30 },
                    "cursor": { "type": "string", "description": "next_cursor from the previous response" }
                }
            }
        }),
        json!({
            "name": "download_resource",
            "description": "Download an embedded image or file attachment from a OneNote page and return `{ ok, path, file_name, mime_type, size_bytes, is_image }`.",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string" },
                    "index": { "type": "integer", "minimum": 0 },
                    "out_dir": { "type": "string", "description": "Optional destination directory" }
                }
            }
        }),
    ]
}

async fn dispatch_tool(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    name: &str,
    args: &Value,
) -> Result<Value> {
    match name {
        "whoami" => {
            let me = graph.get_me().await?;
            let auth_mode = graph.auth().current_tokens().await.map(|t| t.auth_mode);
            Ok(json!({ "ok": true, "user": me, "auth_mode": auth_mode }))
        }
        "list_notebooks" | "onenote_list_notebooks" => {
            let include_teams = args.get("include_teams").and_then(Value::as_bool).unwrap_or(false);
            let refresh = args.get("refresh").and_then(Value::as_bool).unwrap_or(false);
            let offline = args.get("offline").and_then(Value::as_bool).unwrap_or(false);
            let limit = parse_bounded_limit(args, 50)?;
            let offset = cli::parse_mcp_cursor(args)?;
            let all_notebooks =
                cli::load_onenote_notebooks(storage, graph, include_teams, refresh, offline).await?;
            let (notebooks, has_more) = cli::page_slice(all_notebooks, offset, limit);
            let next = cli::next_cursor(offset, limit, Some(has_more));
            let mut out = json!({
                "ok": true,
                "notebooks": notebooks,
                "returned": notebooks.len(),
                "has_more": has_more,
                "next_cursor": next,
            });
            if offline {
                out["cache_synced_at"] = json!(cli::notebooks_cache_synced_at(storage));
            }
            Ok(out)
        }
        "list_sections" | "onenote_list_sections" => {
            let notebook = args.get("notebook").and_then(Value::as_str);
            let refresh = args.get("refresh").and_then(Value::as_bool).unwrap_or(false);
            let offline = args.get("offline").and_then(Value::as_bool).unwrap_or(false);
            let limit = parse_bounded_limit(args, 50)?;
            let offset = cli::parse_mcp_cursor(args)?;
            let (nb, all_sections) =
                cli::load_onenote_sections(storage, graph, notebook, refresh, offline).await?;
            let (sections, has_more) = cli::page_slice(all_sections, offset, limit);
            let next = cli::next_cursor(offset, limit, Some(has_more));
            let mut out = json!({
                "ok": true,
                "notebook": nb,
                "sections": sections,
                "returned": sections.len(),
                "has_more": has_more,
                "next_cursor": next,
            });
            if offline {
                out["cache_synced_at"] = json!(cli::sections_cache_synced_at(storage, nb.as_ref()));
            }
            Ok(out)
        }
        "list_pages" | "onenote_list_pages" => {
            let section = required_str(args, "section")?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(50);
            let offset = cli::parse_mcp_cursor(args)?;
            let refresh = args.get("refresh").and_then(Value::as_bool).unwrap_or(false);
            let offline = args.get("offline").and_then(Value::as_bool).unwrap_or(false);
            let (sec, pages, has_more) = cli::list_onenote_pages_with_cache(
                storage, graph, section, limit, offset, refresh, offline, false,
            )
            .await?;
            let next = cli::next_cursor(offset, limit, has_more);
            let mut out = json!({
                "ok": true,
                "section": sec,
                "pages": pages,
                "returned": pages.len(),
                "has_more": has_more,
                "next_cursor": next,
            });
            if offline {
                out["cache_synced_at"] = json!(cli::pages_cache_synced_at(storage, &sec.section_key));
            }
            Ok(out)
        }
        "get_page" | "onenote_get_page" => {
            let page = required_str(args, "page")?;
            let offline = args.get("offline").and_then(Value::as_bool).unwrap_or(false);
            let detail = cli::load_onenote_page_detail(storage, graph, page, offline).await?;
            let compact = args
                .get("compact")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            Ok(json!({
                "ok": true,
                "page": cli::page_content_json(&detail, compact, cli::PageSurface::Mcp),
            }))
        }
        "create_notebook" | "onenote_create_notebook" => {
            let nb_name = required_str(args, "name")?;
            let group_id = args.get("group_id").and_then(Value::as_str);
            let site_id = args.get("site_id").and_then(Value::as_str);
            let scope = cli::build_onenote_scope_from_args(group_id, site_id)?;
            let created = graph.create_onenote_notebook(&scope, nb_name).await?;
            storage.upsert_onenote_notebooks(std::slice::from_ref(&created))?;
            Ok(json!({ "ok": true, "notebook": created }))
        }
        "rename_notebook" => {
            let drive = required_str(args, "drive_id")?;
            let item = required_str(args, "item_id")?;
            let name = required_str(args, "name")?;
            let updated = graph.rename_notebook_storage(drive, item, name).await?;
            Ok(json!({ "ok": true, "drive_item": updated }))
        }
        "create_section" | "onenote_create_section" => {
            let notebook = required_str(args, "notebook")?;
            let sec_name = required_str(args, "name")?;
            let nb = resolver::resolve_notebook(graph, storage, notebook, false).await?;
            let created = graph
                .create_onenote_section(&nb.scope, &nb.id, sec_name)
                .await?;
            storage.upsert_onenote_sections(std::slice::from_ref(&created))?;
            Ok(json!({ "ok": true, "notebook": nb, "section": created }))
        }
        "rename_section" => {
            let section_id = required_str(args, "section")?;
            let name = required_str(args, "name")?;
            let section = cli::rename_onenote_section_helper(storage, graph, section_id, name).await?;
            cli::nudge_live_daemon(config, graph, crate::sync::SyncCommand::RefreshHierarchy).await;
            Ok(json!({ "ok": true, "section": section }))
        }
        #[cfg(windows)]
        "desktop_set_page_level" => {
            let page_id = required_str(args, "page_id")?;
            let level = args.get("level").and_then(Value::as_i64).context("level must be an integer")?;
            crate::desktop::set_page_level(page_id, level).await
        }
        #[cfg(windows)]
        "desktop_rename_section" => {
            let section_id = required_str(args, "section_id")?;
            let name = required_str(args, "name")?;
            crate::desktop::rename_section(section_id, name).await
        }
        "create_page" | "onenote_create_page" => {
            let section = required_str(args, "section")?;
            let title = required_str(args, "title")?;
            let markdown = args.get("markdown").and_then(Value::as_str).unwrap_or("");
            let level = args
                .get("level")
                .map(|value| {
                    value
                        .as_i64()
                        .and_then(|n| i32::try_from(n).ok())
                        .context("level must be the integer 0 when supplied")
                })
                .transpose()?;
            crate::graph::onenote::validate_page_creation_level(level)?;
            let paths = string_array(args, "image_paths")
                .into_iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            let inline_images = cli::load_inline_images_from_paths(&paths)?;
            let (sec, page) = cli::create_onenote_page_helper(
                storage,
                graph,
                section,
                title,
                markdown,
                inline_images,
                level,
            )
            .await?;
            Ok(json!({ "ok": true, "section": sec, "page": page }))
        }
        "copy_page" => {
            let page = required_str(args, "page")?;
            let section = required_str(args, "section")?;
            let operation_url = args
                .get("operation_url")
                .map(|value| value.as_str().context("operation_url must be a string"))
                .transpose()?;
            let wait = args
                .get("wait_seconds")
                .map(|value| {
                    value.as_u64().and_then(|n| u32::try_from(n).ok())
                        .context("wait_seconds must be a non-negative integer")
                })
                .transpose()?
                .unwrap_or(DEFAULT_COPY_WAIT_SECONDS);
            let outcome = graph.copy_onenote_page(page, section, operation_url, wait).await?;
            Ok(serde_json::to_value(outcome)?)
        }
        "append_page" | "onenote_append_page" => {
            let page = required_str(args, "page")?;
            let markdown = required_str(args, "markdown")?;
            let paths = string_array(args, "image_paths")
                .into_iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            let inline_images = cli::load_inline_images_from_paths(&paths)?;
            let updated =
                cli::append_onenote_page_helper(storage, graph, page, markdown, &inline_images)
                    .await?;
            Ok(json!({ "ok": true, "page": updated }))
        }
        "update_block" | "update_page" | "onenote_update_page" => {
            let page = required_str(args, "page")?;
            let block_id = args.get("block_id").and_then(Value::as_str);
            let title = args.get("title").and_then(Value::as_str);
            let markdown = args
                .get("markdown")
                .or_else(|| args.get("body"))
                .and_then(Value::as_str);
            let text_edit = match (args.get("replace_text"), args.get("with_text")) {
                (None, None) => None,
                (Some(from), Some(to)) => Some((
                    from.as_str().context("replace_text must be a string")?,
                    to.as_str().context("with_text must be a string")?,
                )),
                _ => bail!("Provide both replace_text and with_text."),
            };
            let expected_content = args
                .get("expected_content")
                .or_else(|| args.get("base_content"))
                .and_then(Value::as_str);
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            let updated = cli::update_onenote_content_helper_with_guard(
                storage,
                graph,
                page,
                title,
                markdown,
                block_id,
                text_edit,
                expected_content,
                force,
            )
            .await?;
            cli::nudge_live_daemon(
                config,
                graph,
                crate::sync::SyncCommand::HydratePage {
                    page_key: updated.summary.page_key.clone(),
                },
            )
            .await;
            Ok(json!({ "ok": true, "page": updated }))
        }
        "toggle_todo" | "onenote_toggle_todo" => {
            let page = required_str(args, "page")?;
            let block_id = args.get("block_id").and_then(Value::as_str);
            let todo_index = args
                .get("todo_index")
                .or_else(|| args.get("index"))
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            let expected_content = args
                .get("expected_content")
                .or_else(|| args.get("base_content"))
                .and_then(Value::as_str);
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            let requested_completed = args
                .get("completed")
                .map(|value| value.as_bool().context("completed must be a boolean"))
                .transpose()?;
            let (updated, target_id, completed) = cli::change_onenote_todo_helper_with_guard(
                storage,
                graph,
                page,
                block_id,
                todo_index,
                requested_completed,
                expected_content,
                force,
            )
            .await?;
            cli::nudge_live_daemon(
                config,
                graph,
                crate::sync::SyncCommand::HydratePage {
                    page_key: updated.summary.page_key.clone(),
                },
            )
            .await;
            Ok(json!({
                "ok": true,
                "page_key": updated.summary.page_key,
                "block_id": target_id,
                "completed": completed,
                "page": updated,
            }))
        }
        "delete_page" | "onenote_delete_page" => {
            let page = required_str(args, "page")?;
            let confirm = args.get("confirm").and_then(Value::as_bool).unwrap_or(false);
            if !confirm {
                bail!("Refusing to delete OneNote page without `confirm: true`.");
            }
            let section_key = resolver::resolve_page_cached(storage, page, None)?
                .into_option()?
                .map(|s| s.scope.make_entity_key(&s.section_id));
            let deleted = cli::delete_onenote_page_helper(storage, graph, page).await?;
            let nudge_cmd = match section_key {
                Some(sec_key) => crate::sync::SyncCommand::RefreshSectionPages {
                    section_key: sec_key,
                },
                None => crate::sync::SyncCommand::RefreshAll,
            };
            cli::nudge_live_daemon(config, graph, nudge_cmd).await;
            Ok(json!({ "ok": true, "deleted": deleted }))
        }
        "search_pages" | "onenote_search_pages" => {
            let query = required_str(args, "query")?;
            let section = args.get("section").and_then(Value::as_str);
            let refresh = args.get("refresh").and_then(Value::as_bool).unwrap_or(false);
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|v| v as usize)
                .unwrap_or(30);
            let offset = cli::parse_mcp_cursor(args)?;
            let (hits, has_more) = cli::search_onenote_with_offset(
                storage, graph, query, section, refresh, limit, offset,
            )
            .await?;
            let (pages, pages_with_body) = storage.count_onenote_pages()?;
            let next = cli::next_cursor(offset, limit, has_more);
            Ok(json!({
                "ok": true,
                "query": query,
                "hits": hits,
                "returned": hits.len(),
                "has_more": has_more,
                "next_cursor": next,
                "cache": {
                    "pages": pages,
                    "pages_with_body": pages_with_body,
                },
            }))
        }
        "download_resource" | "onenote_download_resource" => {
            let page = required_str(args, "page")?;
            let index = args
                .get("index")
                .and_then(Value::as_u64)
                .map(|v| v as usize);
            let out_dir = args
                .get("out_dir")
                .and_then(Value::as_str)
                .map(PathBuf::from);
            let (_detail, _idx, downloaded) = cli::download_onenote_resource_helper(
                config,
                storage,
                graph,
                page,
                index,
                out_dir.as_deref(),
            )
            .await?;
            Ok(serde_json::to_value(downloaded)?)
        }
        unknown => bail!("Unknown MCP tool: {unknown}"),
    }
}

fn parse_bounded_limit(args: &Value, default: usize) -> Result<usize> {
    match args.get("limit") {
        None | Some(Value::Null) => Ok(default),
        Some(v) => {
            let Some(n) = v.as_i64() else {
                bail!("limit must be between 1 and 1000");
            };
            if !(1..=1000).contains(&n) {
                bail!("limit must be between 1 and 1000");
            }
            Ok(n as usize)
        }
    }
}

fn required_str<'a>(args: &'a Value, key: &str) -> Result<&'a str> {
    let val = args
        .get(key)
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow::anyhow!("Missing required string argument: `{key}`"))?;
    Ok(val)
}

fn string_array(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(Value::as_array)
        .map(|arr| {
            arr.iter()
                .filter_map(Value::as_str)
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(str::to_string)
                .collect()
        })
        .unwrap_or_default()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn protocol_methods_need_no_runtime() {
        let init = handle_protocol_value(&json!({
            "jsonrpc": "2.0",
            "id": 1,
            "method": "initialize",
            "params": {}
        }));
        let init_val = init.expect("initialize handled").expect("initialize returns response");
        assert!(init_val["result"]["serverInfo"].is_object());

        let ping = handle_protocol_value(&json!({
            "jsonrpc": "2.0",
            "id": 2,
            "method": "ping",
            "params": {}
        }));
        assert!(ping.expect("ping handled").is_some());

        let list = handle_protocol_value(&json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/list",
            "params": {}
        }));
        let list_val = list.expect("tools/list handled").expect("tools/list returns response");
        let tools = list_val["result"]["tools"].as_array().expect("tools array");
        assert!(!tools.is_empty());

        let notif = handle_protocol_value(&json!({
            "jsonrpc": "2.0",
            "method": "notifications/initialized"
        }));
        assert_eq!(notif, Some(None));

        let call = handle_protocol_value(&json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": { "name": "whoami", "arguments": {} }
        }));
        assert_eq!(call, None);

        let bogus = handle_protocol_value(&json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "bogus/method",
            "params": {}
        }));
        let bogus_val = bogus.expect("bogus handled").expect("bogus returns error");
        assert!(bogus_val["error"].is_object());
    }
}
