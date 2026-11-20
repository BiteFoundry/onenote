//! Standalone Microsoft OneNote stdio MCP (Model Context Protocol, JSON-RPC 2.0) server (`onenote mcp`).
//!
//! Exposes 14 tools (whoami + 13 OneNote tools):
//! `whoami`, `list_notebooks`, `list_sections`, `list_pages`, `get_page`,
//! `create_notebook`, `create_section`, `create_page`, `append_page`, `update_block`,
//! `toggle_todo`, `delete_page`, `search_pages`, `download_resource`.

use std::{
    io::{self, BufRead, Write},
    path::PathBuf,
};

use anyhow::{bail, Result};
use serde_json::{json, Value};

use crate::{cli, config::AppConfig, graph::GraphClient, resolver, storage::Storage};

const MCP_PROTOCOL_VERSION: &str = "2024-11-05";

pub async fn run(config: AppConfig, graph: GraphClient, storage: Storage) -> Result<()> {
    let stdin = io::stdin();
    let mut stdout = io::stdout().lock();

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
        if let Some(resp) = handle_request_value(&config, &graph, &storage, req).await {
            writeln!(stdout, "{}", serde_json::to_string(&resp)?)?;
            stdout.flush()?;
        }
    }
    Ok(())
}

/// Handles a single JSON-RPC 2.0 MCP message and returns an optional JSON response (`None` for notifications).
pub async fn handle_request_value(
    config: &AppConfig,
    graph: &GraphClient,
    storage: &Storage,
    req: Value,
) -> Option<Value> {
    let id = req.get("id").cloned();
    let method = req.get("method").and_then(Value::as_str).unwrap_or("");
    let params = req.get("params").cloned().unwrap_or_else(|| json!({}));

    if method == "notifications/initialized" || method.starts_with("notifications/") {
        return None;
    }

    let id = id.unwrap_or(Value::Null);
    match method {
        "initialize" => Some(json!({
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
        })),
        "ping" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": {}
        })),
        "tools/list" => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "result": { "tools": tool_definitions() }
        })),
        "tools/call" => {
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
                    let text = serde_json::to_string_pretty(&result_val)
                        .unwrap_or_else(|_| result_val.to_string());
                    Some(json!({
                        "jsonrpc": "2.0",
                        "id": id,
                        "result": {
                            "content": [{ "type": "text", "text": text }],
                            "structuredContent": result_val,
                            "isError": false,
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
        other => Some(json!({
            "jsonrpc": "2.0",
            "id": id,
            "error": {
                "code": -32601,
                "message": format!("Method not found: {other}")
            }
        })),
    }
}

pub fn tool_definitions() -> Vec<Value> {
    vec![
        json!({
            "name": "whoami",
            "description": "Return the currently signed-in Microsoft 365 user profile.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "list_notebooks",
            "description": "List accessible Microsoft OneNote notebooks (personal + optional Teams/Group shared notebooks).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "include_teams": { "type": "boolean", "description": "Include Microsoft Teams/Group shared notebooks" },
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
                    "refresh": { "type": "boolean" },
                    "offline": { "type": "boolean" }
                }
            }
        }),
        json!({
            "name": "get_page",
            "description": "Read a OneNote page including parsed blocks (with element_id & data-tag), GFM Markdown, plain text, and embedded resources.",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string", "description": "Page key, ID, or title" },
                    "offline": { "type": "boolean" }
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
                    "level": { "type": "integer", "minimum": 0, "maximum": 2 }
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
        json!({
            "name": "update_block",
            "description": "Surgically update a specific block (`block_id`) or replace page title/body on a OneNote page. Pass `expected_content` from a prior `get_page` read to guard against overwriting concurrent remote edits.",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string" },
                    "block_id": { "type": "string", "description": "Graph element ID from get_page (e.g. `p:{...}`)" },
                    "title": { "type": "string" },
                    "markdown": { "type": "string" },
                    "expected_content": { "type": "string", "description": "Baseline block/page content from prior read; aborts with conflict if remote changed" },
                    "force": { "type": "boolean", "description": "Override conflict check when true" }
                }
            }
        }),
        json!({
            "name": "toggle_todo",
            "description": "Toggle a OneNote `to-do` checkbox block by `block_id` or zero-based `todo_index`.",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string" },
                    "block_id": { "type": "string" },
                    "todo_index": { "type": "integer", "minimum": 0 },
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
                    "limit": { "type": "integer", "default": 30 }
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
            Ok(json!({ "ok": true, "user": me }))
        }
        "list_notebooks" | "onenote_list_notebooks" => {
            let include_teams = args.get("include_teams").and_then(Value::as_bool).unwrap_or(false);
            let refresh = args.get("refresh").and_then(Value::as_bool).unwrap_or(false);
            let offline = args.get("offline").and_then(Value::as_bool).unwrap_or(false);
            let notebooks =
                cli::load_onenote_notebooks(storage, graph, include_teams, refresh, offline).await?;
            Ok(json!({ "ok": true, "notebooks": notebooks }))
        }
        "list_sections" | "onenote_list_sections" => {
            let notebook = args.get("notebook").and_then(Value::as_str);
            let refresh = args.get("refresh").and_then(Value::as_bool).unwrap_or(false);
            let offline = args.get("offline").and_then(Value::as_bool).unwrap_or(false);
            let (nb, sections) =
                cli::load_onenote_sections(storage, graph, notebook, refresh, offline).await?;
            Ok(json!({ "ok": true, "notebook": nb, "sections": sections }))
        }
        "list_pages" | "onenote_list_pages" => {
            let section = required_str(args, "section")?;
            let limit = args
                .get("limit")
                .and_then(Value::as_u64)
                .map(|n| n as usize)
                .unwrap_or(50);
            let refresh = args.get("refresh").and_then(Value::as_bool).unwrap_or(false);
            let offline = args.get("offline").and_then(Value::as_bool).unwrap_or(false);
            let (sec, pages) =
                cli::list_onenote_pages_helper(storage, graph, section, limit, refresh, offline)
                    .await?;
            Ok(json!({ "ok": true, "section": sec, "pages": pages }))
        }
        "get_page" | "onenote_get_page" => {
            let page = required_str(args, "page")?;
            let offline = args.get("offline").and_then(Value::as_bool).unwrap_or(false);
            let detail = cli::load_onenote_page_detail(storage, graph, page, offline).await?;
            Ok(json!({ "ok": true, "page": detail }))
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
        "create_page" | "onenote_create_page" => {
            let section = required_str(args, "section")?;
            let title = required_str(args, "title")?;
            let markdown = args.get("markdown").and_then(Value::as_str).unwrap_or("");
            let level = args.get("level").and_then(Value::as_i64).map(|v| v as i32);
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
            let expected_content = args
                .get("expected_content")
                .or_else(|| args.get("base_content"))
                .and_then(Value::as_str);
            let force = args.get("force").and_then(Value::as_bool).unwrap_or(false);
            let updated = cli::update_onenote_page_helper_with_guard(
                storage,
                graph,
                page,
                title,
                markdown,
                block_id,
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
            let (updated, target_id, completed) = cli::toggle_onenote_todo_helper_with_guard(
                storage,
                graph,
                page,
                block_id,
                todo_index,
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
            let section_key = resolver::resolve_page_cached(storage, page, None)
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
            let hits =
                cli::search_onenote_helper(storage, graph, query, section, refresh, limit).await?;
            Ok(json!({ "ok": true, "query": query, "hits": hits }))
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
