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
        let (graph, storage) = match runtime.as_ref() {
            Some(rt) => rt,
            None => {
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
                    Ok(rt) => runtime.insert(rt),
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
        };
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
        "description": "Replace a block with Markdown, edit a block text substring, or update page title/body. Call get_page(fields: [\"blocks\"]) for current Graph element_ids without pulling full raw_xhtml.",
        "inputSchema": {
            "type": "object",
            "required": ["page"],
            "properties": {
                "page": { "type": "string", "description": "Page ID, page_key, or exact title from list_pages" },
                "section": { "type": "string", "description": "Optional section name, Notebook/Section path, ID, or scoped key to narrow title lookup" },
                "block_id": { "type": "string", "description": "Graph element_id from get_page(fields: [\"blocks\"]) (e.g. `p:{...}`)" },
                "title": { "type": "string", "description": "New page title when renaming the page" },
                "markdown": { "type": "string", "description": "Replacement Markdown for block_id or full page body" },
                "replace_text": { "type": "string", "description": "Exact text to replace in block_id; provide with_text and omit markdown" },
                "with_text": { "type": "string", "description": "New text for replace_text" },
                "expected_content": { "type": "string", "description": "Baseline block/page content from prior read; aborts with conflict if remote changed" },
                "force": { "type": "boolean", "description": "Override conflict check when true" },
                "compact": { "type": "boolean", "default": true, "description": "Default true. Set false to include the full page object in the response." }
            }
        }
    });
    let mut update_page = update_block.clone();
    update_page["name"] = json!("update_page");
    update_page["description"] = json!("Alias for update_block. Update a page title/body, replace a block, or edit block text; use get_page(fields: [\"blocks\"]) for current element_ids without pulling full raw_xhtml.");
    vec![
        json!({
            "name": "whoami",
            "description": "Return the currently signed-in Microsoft 365 user profile and the active auth mode.",
            "inputSchema": { "type": "object", "properties": {} }
        }),
        json!({
            "name": "list_notebooks",
            "description": "List accessible Microsoft OneNote notebooks (personal + optional Teams/Group shared notebooks). Calls Microsoft Graph unless offline:true (a recent cached answer is reused only when ONENOTE_CLI_CACHE_SECS is set). include_teams adds one Graph request per joined Team, up to ONENOTE_MAX_TEAMS (default 100), and can take several seconds.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "include_teams": { "type": "boolean", "description": "Include Microsoft Teams/Group shared notebooks (one extra Graph request per joined Team)" },
                    "limit": { "type": "integer", "default": 50, "minimum": 1, "maximum": 1000, "description": "Maximum notebooks to return per page (1-1000, default 50)" },
                    "cursor": { "type": "string", "description": "next_cursor from the previous response" },
                    "refresh": { "type": "boolean", "description": "Ignore any cached answer and refetch from Microsoft Graph. Costs extra Graph requests; use only when results look stale." },
                    "offline": { "type": "boolean", "description": "Read the local SQLite cache only: no network, may be stale." }
                }
            }
        }),
        json!({
            "name": "list_sections",
            "description": "List OneNote sections across all notebooks or within a specific notebook. Calls Microsoft Graph unless offline:true. Without notebook it first lists every notebook, including Teams notebooks, then each notebook's sections: many requests on large accounts, so pass notebook when you can.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "notebook": { "type": "string", "description": "Optional notebook key, ID, or display name" },
                    "limit": { "type": "integer", "default": 50, "minimum": 1, "maximum": 1000, "description": "Maximum sections to return per page (1-1000, default 50)" },
                    "cursor": { "type": "string", "description": "next_cursor from the previous response" },
                    "refresh": { "type": "boolean", "description": "Ignore any cached answer and refetch from Microsoft Graph. Costs extra Graph requests; use only when results look stale." },
                    "offline": { "type": "boolean", "description": "Read the local SQLite cache only: no network, may be stale." }
                }
            }
        }),
        json!({
            "name": "list_pages",
            "description": "List OneNote pages inside a section (supports `Notebook/Section`, section name, or ID). Calls Microsoft Graph unless offline:true; the request size grows with cursor + limit.",
            "inputSchema": {
                "type": "object",
                "required": ["section"],
                "properties": {
                    "section": { "type": "string", "description": "Notebook/Section path, section name, ID, or scoped key from list_sections" },
                    "limit": { "type": "integer", "default": 50, "minimum": 1, "maximum": 1000, "description": "Maximum pages to return per page (1-1000, default 50)" },
                    "cursor": { "type": "string", "description": "next_cursor from the previous response" },
                    "refresh": { "type": "boolean", "description": "Ignore any cached answer and refetch from Microsoft Graph. Costs extra Graph requests; use only when results look stale." },
                    "offline": { "type": "boolean", "description": "Read the local SQLite cache only: no network, may be stale." }
                }
            }
        }),
        json!({
            "name": "get_page",
            "description": "Read a OneNote page. Returns compact Markdown by default; pass fields (e.g. [\"blocks\",\"resources\"]) to fetch specific fields without full raw_xhtml, or compact:false for everything. Fetches from Microsoft Graph unless offline:true; compact:false can be large.",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string", "description": "Page key, ID, or title" },
                    "section": { "type": "string", "description": "Optional section name, Notebook/Section path, ID, or scoped key to narrow title lookup" },
                    "offline": { "type": "boolean", "description": "Return the cached copy of the page without contacting Microsoft Graph." },
                    "compact": { "type": "boolean", "default": true, "description": "Default true. Set false for blocks[].element_id, raw_xhtml and resources." },
                    "fields": {
                        "type": "array",
                        "items": { "type": "string" },
                        "description": "Optional list of specific page fields to return (e.g. blocks, resources, body_markdown, body_plain, raw_xhtml, primary_div_id, web_url, created_at, content_url, summary). Always includes id, page_key, and title; unknown names are returned in missing_fields."
                    }
                }
            }
        }),
        json!({
            "name": "create_notebook",
            "description": "Create a new OneNote notebook under /me by default, or in a Microsoft 365 Group or SharePoint site when group_id or site_id is set.",
            "inputSchema": {
                "type": "object",
                "required": ["name"],
                "properties": {
                    "name": { "type": "string", "description": "Display name for the new notebook" },
                    "group_id": { "type": "string", "description": "Optional Microsoft 365 Group/Team ID for a shared notebook (exclusive with site_id)" },
                    "site_id": { "type": "string", "description": "Optional SharePoint site ID for a site notebook (exclusive with group_id)" }
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
                    "drive_id": { "type": "string", "description": "OneDrive or SharePoint drive ID hosting the notebook" },
                    "item_id": { "type": "string", "description": "DriveItem ID with package.type oneNote" },
                    "name": { "type": "string", "description": "New display name for the notebook" }
                }
            }
        }),
        json!({
            "name": "create_section",
            "description": "Create a new OneNote section inside an existing notebook (pass a notebook ID, key, or display name from list_notebooks).",
            "inputSchema": {
                "type": "object",
                "required": ["notebook", "name"],
                "properties": {
                    "notebook": { "type": "string", "description": "Target notebook ID, key, or display name from list_notebooks" },
                    "name": { "type": "string", "description": "Display name for the new section" }
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
                    "level": { "type": "integer", "minimum": 0, "maximum": 2, "description": "Target subpage indentation level (0=top-level, 1=subpage, 2=sub-subpage)" }
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
                    "name": { "type": "string", "description": "New display name for the section" }
                }
            }
        }),
        json!({
            "name": "create_page",
            "description": "Create a new OneNote page from Markdown and optional local images inside a target section (from list_sections).",
            "inputSchema": {
                "type": "object",
                "required": ["section", "title"],
                "properties": {
                    "section": { "type": "string", "description": "Target Notebook/Section path, section name, ID, or key from list_sections" },
                    "title": { "type": "string", "description": "Display title for the new page" },
                    "markdown": { "type": "string", "description": "Optional Markdown body content for the new page" },
                    "image_paths": { "type": "array", "items": { "type": "string" }, "description": "Optional local file paths of images to embed inline" },
                    "level": { "type": "integer", "minimum": 0, "maximum": 0, "description": "Top-level page (0 or omitted)" }
                }
            }
        }),
        json!({
            "name": "copy_page",
            "description": "Copy a page via Microsoft Graph and verify its new ID and section. The original page and its links stay in place. The copy runs asynchronously on the server and wait_seconds polls it. To check a pending or interrupted copy, pass its operation_url with the original page and section.",
            "inputSchema": {
                "type": "object",
                "required": ["page", "section"],
                "properties": {
                    "page": { "type": "string", "description": "Source page ID or scoped page key" },
                    "section": { "type": "string", "description": "Destination section ID or scoped section key" },
                    "operation_url": { "type": "string", "description": "Existing Graph operation URL to poll instead of submitting a new copy" },
                    "wait_seconds": { "type": "integer", "minimum": 0, "maximum": MAX_COPY_WAIT_SECONDS, "default": DEFAULT_COPY_WAIT_SECONDS, "description": "Seconds to poll; the call blocks for up to this long. 0 returns the operation handle immediately" }
                }
            }
        }),
        json!({
            "name": "append_page",
            "description": "Append Markdown and optional local images to the bottom of an existing page (target by page ID, page_key, or title from list_pages) without overwriting existing content.",
            "inputSchema": {
                "type": "object",
                "required": ["page", "markdown"],
                "properties": {
                    "page": { "type": "string", "description": "Page ID, page_key, or exact title from list_pages" },
                    "section": { "type": "string", "description": "Optional section name, Notebook/Section path, ID, or scoped key to narrow title lookup" },
                    "markdown": { "type": "string", "description": "Markdown content appended to the bottom of the page" },
                    "image_paths": { "type": "array", "items": { "type": "string" }, "description": "Optional local file paths of images to append inline" },
                    "compact": { "type": "boolean", "default": true, "description": "Default true. Set false to include the full page object in the response." }
                }
            }
        }),
        update_block,
        update_page,
        json!({
            "name": "toggle_todo",
            "description": "Set or toggle a OneNote checkbox by block_id or zero-based todo_index from get_page(fields: [\"blocks\"]).",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string", "description": "Page ID, page_key, or exact title from list_pages" },
                    "section": { "type": "string", "description": "Optional section name, Notebook/Section path, ID, or scoped key to narrow title lookup" },
                    "block_id": { "type": "string", "description": "Graph element_id of the to-do block from get_page" },
                    "todo_index": { "type": "integer", "minimum": 0, "description": "Zero-based index of the to-do item from get_page" },
                    "completed": { "type": "boolean", "description": "Set this state; omit to toggle" },
                    "expected_content": { "type": "string", "description": "Baseline to-do block content from prior read; aborts with conflict if remote changed" },
                    "force": { "type": "boolean", "description": "Override conflict check when true" },
                    "compact": { "type": "boolean", "default": true, "description": "Default true. Set false to include the full page object in the response." }
                }
            }
        }),
        json!({
            "name": "delete_page",
            "description": "Permanently delete a page in OneNote. Irreversible; requires confirm: true to prevent accidental deletion.",
            "inputSchema": {
                "type": "object",
                "required": ["page", "confirm"],
                "properties": {
                    "page": { "type": "string", "description": "Page ID, page_key, or exact title from list_pages" },
                    "section": { "type": "string", "description": "Optional section name, Notebook/Section path, ID, or scoped key to narrow title lookup" },
                    "confirm": { "type": "boolean", "description": "Must be true to permanently delete the page" }
                }
            }
        }),
        json!({
            "name": "search_pages",
            "description": "Search cached OneNote pages via local SQLite FTS5 (`onenote_fts`); no network by default. refresh:true first runs a full account sync (many Graph requests, slow on large accounts), so avoid it in loops.",
            "inputSchema": {
                "type": "object",
                "required": ["query"],
                "properties": {
                    "query": { "type": "string", "description": "Full-text search query using local SQLite FTS5 syntax" },
                    "section": { "type": "string", "description": "Optional section name or Notebook/Section path to filter results" },
                    "refresh": { "type": "boolean", "description": "Run a full account sync before searching. Expensive: many Graph requests." },
                    "limit": { "type": "integer", "default": 30, "minimum": 1, "maximum": 1000, "description": "Maximum search hits to return per page (1-1000, default 30)" },
                    "cursor": { "type": "string", "description": "next_cursor from the previous response" }
                }
            }
        }),
        json!({
            "name": "download_resource",
            "description": "Download an embedded image or file attachment from a OneNote page and return `{ ok, path, file_name, mime_type, size_bytes, is_image }`. Downloads the full binary to disk.",
            "inputSchema": {
                "type": "object",
                "required": ["page"],
                "properties": {
                    "page": { "type": "string", "description": "Page ID, page_key, or exact title from list_pages" },
                    "index": { "type": "integer", "minimum": 0, "description": "Zero-based resource index from get_page(fields: [\"resources\"]) (default 0)" },
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
            let limit = parse_bounded_limit(args, 50)?;
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
            let section = args.get("section").and_then(Value::as_str);
            let offline = args.get("offline").and_then(Value::as_bool).unwrap_or(false);
            let fields = parse_optional_string_array(args, "fields")?;
            let resolved_page =
                cli::resolve_page_query_in_section(storage, graph, page, section, offline).await?;
            let detail = cli::load_onenote_page_detail(storage, graph, &resolved_page, offline).await?;
            let page_json = match fields {
                Some(ref selected) if !selected.is_empty() => project_page_fields(&detail, selected),
                _ => {
                    let compact = args
                        .get("compact")
                        .and_then(Value::as_bool)
                        .unwrap_or(true);
                    cli::page_content_json(&detail, compact, cli::PageSurface::Mcp)
                }
            };
            Ok(json!({
                "ok": true,
                "page": page_json,
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
            if outcome.ok {
                cli::nudge_live_daemon(
                    config,
                    graph,
                    crate::sync::SyncCommand::RefreshSectionPages {
                        section_key: outcome.destination_section_key.clone(),
                    },
                )
                .await;
                if let Some(ref new_page_key) = outcome.new_page_key {
                    cli::nudge_live_daemon(
                        config,
                        graph,
                        crate::sync::SyncCommand::HydratePage {
                            page_key: new_page_key.clone(),
                        },
                    )
                    .await;
                }
            }
            Ok(serde_json::to_value(outcome)?)
        }
        "append_page" | "onenote_append_page" => {
            let page = required_str(args, "page")?;
            let section = args.get("section").and_then(Value::as_str);
            let markdown = required_str(args, "markdown")?;
            let compact = args
                .get("compact")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let paths = string_array(args, "image_paths")
                .into_iter()
                .map(PathBuf::from)
                .collect::<Vec<_>>();
            let inline_images = cli::load_inline_images_from_paths(&paths)?;
            let resolved_page =
                cli::resolve_page_query_in_section(storage, graph, page, section, false).await?;
            let updated = cli::append_onenote_page_helper(
                storage,
                graph,
                &resolved_page,
                markdown,
                &inline_images,
            )
            .await?;
            Ok(cli::write_result_json(&updated, None, None, !compact))
        }
        "update_block" | "update_page" | "onenote_update_page" => {
            let page = required_str(args, "page")?;
            let section = args.get("section").and_then(Value::as_str);
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
            let compact = args
                .get("compact")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let resolved_page =
                cli::resolve_page_query_in_section(storage, graph, page, section, false).await?;
            let updated = cli::update_onenote_content_helper_with_guard(
                storage,
                graph,
                &resolved_page,
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
            Ok(cli::write_result_json(&updated, block_id, None, !compact))
        }
        "toggle_todo" | "onenote_toggle_todo" => {
            let page = required_str(args, "page")?;
            let section = args.get("section").and_then(Value::as_str);
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
            let compact = args
                .get("compact")
                .and_then(Value::as_bool)
                .unwrap_or(true);
            let requested_completed = args
                .get("completed")
                .map(|value| value.as_bool().context("completed must be a boolean"))
                .transpose()?;
            let resolved_page =
                cli::resolve_page_query_in_section(storage, graph, page, section, false).await?;
            let (updated, target_id, completed) = cli::change_onenote_todo_helper_with_guard(
                storage,
                graph,
                &resolved_page,
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
            Ok(cli::write_result_json(
                &updated,
                Some(&target_id),
                Some(completed),
                !compact,
            ))
        }
        "delete_page" | "onenote_delete_page" => {
            let page = required_str(args, "page")?;
            let section = args.get("section").and_then(Value::as_str);
            let confirm = args.get("confirm").and_then(Value::as_bool).unwrap_or(false);
            if !confirm {
                bail!("Refusing to delete OneNote page without `confirm: true`.");
            }
            let resolved_page =
                cli::resolve_page_query_in_section(storage, graph, page, section, false).await?;
            let section_key = resolver::resolve_page_cached(storage, &resolved_page, None)?
                .into_option()?
                .map(|s| s.scope.make_entity_key(&s.section_id));
            let deleted = cli::delete_onenote_page_helper(storage, graph, &resolved_page).await?;
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
            let limit = parse_bounded_limit(args, 30)?;
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

fn parse_optional_string_array(args: &Value, key: &str) -> Result<Option<Vec<String>>> {
    match args.get(key) {
        None | Some(Value::Null) => Ok(None),
        Some(Value::Array(arr)) => {
            let mut out = Vec::with_capacity(arr.len());
            for item in arr {
                let Some(s) = item.as_str() else {
                    bail!("{key} must be an array of strings");
                };
                let trimmed = s.trim();
                if !trimmed.is_empty() {
                    out.push(trimmed.to_string());
                }
            }
            Ok(Some(out))
        }
        Some(_) => bail!("{key} must be an array of strings"),
    }
}

fn project_page_fields(detail: &crate::models::OnenotePageDetail, fields: &[String]) -> Value {
    let mut map = serde_json::Map::new();
    map.insert("id".to_string(), json!(detail.summary.id));
    map.insert("page_key".to_string(), json!(detail.summary.page_key));
    map.insert("title".to_string(), json!(detail.summary.title));

    let mut missing: Vec<String> = Vec::new();
    for raw_field in fields {
        let field = raw_field.trim();
        if field.is_empty() {
            continue;
        }
        let value = match field {
            "id" | "summary.id" => Some(json!(detail.summary.id)),
            "page_key" | "summary.page_key" => Some(json!(detail.summary.page_key)),
            "title" | "summary.title" => Some(json!(detail.summary.title)),
            "summary" => Some(json!(detail.summary)),
            "scope" | "summary.scope" => Some(json!(detail.summary.scope)),
            "section_id" | "summary.section_id" => Some(json!(detail.summary.section_id)),
            "section_name" | "summary.section_name" => Some(json!(detail.summary.section_name)),
            "notebook_id" | "summary.notebook_id" => Some(json!(detail.summary.notebook_id)),
            "notebook_name" | "summary.notebook_name" => Some(json!(detail.summary.notebook_name)),
            "level" | "summary.level" => Some(json!(detail.summary.level)),
            "order" | "summary.order" => Some(json!(detail.summary.order)),
            "created_at" | "summary.created_at" => Some(json!(detail.summary.created_at)),
            "last_modified_at" | "summary.last_modified_at" => {
                Some(json!(detail.summary.last_modified_at))
            }
            "content_url" | "summary.content_url" => Some(json!(detail.summary.content_url)),
            "web_url" | "summary.web_url" => Some(json!(detail.summary.web_url)),
            "preview_text" | "summary.preview_text" => Some(json!(detail.summary.preview_text)),
            "has_todo" | "summary.has_todo" => Some(json!(detail.summary.has_todo)),
            "raw_xhtml" => Some(json!(detail.raw_xhtml)),
            "body_markdown" => Some(json!(detail.body_markdown)),
            "body_plain" => Some(json!(detail.body_plain)),
            "blocks" => Some(json!(detail.blocks)),
            "resources" => Some(json!(detail.resources)),
            "primary_div_id" => Some(json!(detail.primary_div_id)),
            "synced_at" => Some(json!(detail.synced_at)),
            _ => None,
        };
        if let Some(val) = value {
            let out_key = field.strip_prefix("summary.").unwrap_or(field);
            map.insert(out_key.to_string(), val);
        } else if !missing.iter().any(|m| m == field) {
            missing.push(field.to_string());
        }
    }

    if !missing.is_empty() {
        map.insert("missing_fields".to_string(), json!(missing));
    }
    Value::Object(map)
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

    /// Name, required arguments and property keys of every tool, sorted by name.
    fn schema_snapshot() -> Vec<(String, Vec<String>, Vec<String>)> {
        let mut out: Vec<_> = tool_definitions()
            .iter()
            .map(|tool| {
                let schema = &tool["inputSchema"];
                let required = schema["required"]
                    .as_array()
                    .map(|items| items.iter().map(|v| v.as_str().unwrap().to_string()).collect())
                    .unwrap_or_default();
                let mut props: Vec<String> = schema["properties"]
                    .as_object()
                    .map(|p| p.keys().cloned().collect())
                    .unwrap_or_default();
                props.sort();
                (tool["name"].as_str().unwrap().to_string(), required, props)
            })
            .collect();
        out.sort();
        out
    }

    #[test]
    fn tool_names_and_required_arguments_are_unchanged() {
        let got: Vec<(String, Vec<String>)> = schema_snapshot()
            .into_iter()
            .map(|(name, required, _)| (name, required))
            .collect();
        let s = |v: &[&str]| v.iter().map(|x| x.to_string()).collect::<Vec<_>>();
        #[allow(unused_mut)]
        let mut want: Vec<(String, Vec<String>)> = vec![
            ("append_page".into(), s(&["page", "markdown"])),
            ("copy_page".into(), s(&["page", "section"])),
            ("create_notebook".into(), s(&["name"])),
            ("create_page".into(), s(&["section", "title"])),
            ("create_section".into(), s(&["notebook", "name"])),
            ("delete_page".into(), s(&["page", "confirm"])),
            ("download_resource".into(), s(&["page"])),
            ("get_page".into(), s(&["page"])),
            ("list_notebooks".into(), s(&[])),
            ("list_pages".into(), s(&["section"])),
            ("list_sections".into(), s(&[])),
            ("rename_notebook".into(), s(&["drive_id", "item_id", "name"])),
            ("rename_section".into(), s(&["section", "name"])),
            ("search_pages".into(), s(&["query"])),
            ("toggle_todo".into(), s(&["page"])),
            ("update_block".into(), s(&["page"])),
            ("update_page".into(), s(&["page"])),
            ("whoami".into(), s(&[])),
        ];
        #[cfg(windows)]
        {
            want.push(("desktop_rename_section".into(), s(&["section_id", "name"])));
            want.push(("desktop_set_page_level".into(), s(&["page_id", "level"])));
            want.sort();
        }
        assert_eq!(got, want);
    }

    #[test]
    fn tool_descriptions_state_their_cost_and_stay_short() {
        let tools = tool_definitions();
        let find = |name: &str| {
            tools
                .iter()
                .find(|t| t["name"] == name)
                .unwrap_or_else(|| panic!("tool {name} missing"))
                .clone()
        };
        for tool in &tools {
            let name = tool["name"].as_str().unwrap();
            let description = tool["description"].as_str().unwrap();
            assert!(
                description.len() <= 400,
                "{name} description is {} chars",
                description.len()
            );
            for key in ["refresh", "offline"] {
                if let Some(prop) = tool["inputSchema"]["properties"].get(key) {
                    let text = prop["description"].as_str().unwrap_or_default();
                    assert!(!text.trim().is_empty(), "{name}.{key} has no description");
                }
            }
        }

        let search = find("search_pages");
        let text = search["description"].as_str().unwrap();
        assert!(text.contains("no network") && text.contains("full account sync"), "{text}");
        let refresh = search["inputSchema"]["properties"]["refresh"]["description"].as_str().unwrap();
        assert!(refresh.contains("Expensive"), "{refresh}");

        let notebooks = find("list_notebooks");
        let text = notebooks["description"].as_str().unwrap();
        assert!(text.contains("one Graph request per joined Team") && text.contains("ONENOTE_MAX_TEAMS"));

        let sections = find("list_sections")["description"].as_str().unwrap().to_string();
        assert!(sections.contains("pass notebook"), "{sections}");
        assert!(find("copy_page")["inputSchema"]["properties"]["wait_seconds"]["description"]
            .as_str()
            .unwrap()
            .contains("blocks"));
        assert!(find("download_resource")["description"]
            .as_str()
            .unwrap()
            .contains("full binary"));

        let get_page = find("get_page");
        let fields_prop = &get_page["inputSchema"]["properties"]["fields"];
        assert_eq!(fields_prop["type"], "array");
        assert_eq!(fields_prop["items"]["type"], "string");
        assert!(!fields_prop["description"]
            .as_str()
            .unwrap_or_default()
            .trim()
            .is_empty());
    }

    #[test]
    fn every_tool_property_has_non_empty_description() {
        let list = handle_protocol_value(&json!({
            "jsonrpc": "2.0",
            "id": 10,
            "method": "tools/list",
            "params": {}
        }))
        .expect("tools/list handled")
        .expect("tools/list returns response");
        let tools = list["result"]["tools"].as_array().expect("tools array");
        assert!(!tools.is_empty());

        for tool in tools {
            let tool_name = tool["name"].as_str().expect("tool name");
            let props = tool["inputSchema"]["properties"]
                .as_object()
                .unwrap_or_else(|| panic!("{tool_name} inputSchema.properties missing"));
            for (prop_name, prop_schema) in props {
                let desc = prop_schema
                    .get("description")
                    .and_then(Value::as_str)
                    .unwrap_or_default()
                    .trim();
                assert!(
                    !desc.is_empty(),
                    "{tool_name}.{prop_name} is missing a non-empty description"
                );
            }
        }
    }

    fn sample_page_detail() -> crate::models::OnenotePageDetail {
        use crate::models::{
            OnenoteBlock, OnenoteBlockKind, OnenotePageDetail, OnenotePageSummary,
            OnenoteResource, OnenoteScope, OnenoteTagKind, RichSpan,
        };
        OnenotePageDetail {
            summary: OnenotePageSummary {
                page_key: "me|page-42".to_string(),
                id: "page-42".to_string(),
                scope: OnenoteScope::Me,
                section_id: "sec-1".to_string(),
                section_name: Some("Notes".to_string()),
                notebook_id: Some("nb-1".to_string()),
                notebook_name: Some("Work".to_string()),
                title: "Architecture Spec".to_string(),
                level: 0,
                order: 3,
                created_at: Some("2026-01-01T00:00:00Z".to_string()),
                last_modified_at: Some("2026-01-02T12:00:00Z".to_string()),
                content_url: Some("https://graph.microsoft.com/v1.0/me/onenote/pages/page-42/content".to_string()),
                web_url: Some("https://onedrive.live.com/page-42".to_string()),
                preview_text: Some("Short preview".to_string()),
                has_todo: true,
            },
            raw_xhtml: "<html><body><p id=\"p:{1}\">Huge raw HTML</p></body></html>".to_string(),
            body_markdown: "# Architecture Spec\n- [ ] Ship fields projection".to_string(),
            body_plain: "Architecture Spec\nShip fields projection".to_string(),
            blocks: vec![OnenoteBlock {
                element_id: Some("p:{1}".to_string()),
                tag: Some(OnenoteTagKind::Todo { completed: false }),
                raw_html: Some("<p id=\"p:{1}\" data-tag=\"to-do\">Ship fields projection</p>".to_string()),
                kind: OnenoteBlockKind::Paragraph {
                    spans: vec![RichSpan::plain("Ship fields projection")],
                },
            }],
            resources: vec![OnenoteResource {
                element_id: Some("img:{2}".to_string()),
                resource_url: "https://graph.microsoft.com/v1.0/me/onenote/resources/res-1/$value".to_string(),
                fullres_url: None,
                mime_type: "image/png".to_string(),
                file_name: Some("diagram.png".to_string()),
                is_image: true,
                alt_text: Some("System diagram".to_string()),
            }],
            primary_div_id: Some("div:{root}".to_string()),
            synced_at: Some("2026-01-02T12:05:00Z".to_string()),
        }
    }

    #[test]
    fn get_page_fields_projection_returns_requested_fields_without_raw_xhtml() {
        let detail = sample_page_detail();
        let projected = project_page_fields(
            &detail,
            &[
                "blocks".to_string(),
                "resources".to_string(),
                "summary.created_at".to_string(),
                "web_url".to_string(),
                "primary_div_id".to_string(),
            ],
        );

        assert_eq!(projected["id"], "page-42");
        assert_eq!(projected["page_key"], "me|page-42");
        assert_eq!(projected["title"], "Architecture Spec");
        assert_eq!(projected["blocks"].as_array().unwrap().len(), 1);
        assert_eq!(projected["blocks"][0]["element_id"], "p:{1}");
        assert_eq!(projected["resources"].as_array().unwrap().len(), 1);
        assert_eq!(projected["resources"][0]["file_name"], "diagram.png");
        assert_eq!(projected["created_at"], "2026-01-01T00:00:00Z");
        assert_eq!(projected["web_url"], "https://onedrive.live.com/page-42");
        assert_eq!(projected["primary_div_id"], "div:{root}");

        assert!(projected.get("raw_xhtml").is_none());
        assert!(projected.get("body_markdown").is_none());
        assert!(projected.get("body_plain").is_none());
        assert!(projected.get("missing_fields").is_none());
    }

    #[test]
    fn get_page_fields_projection_reports_unknown_fields_in_missing_fields() {
        let detail = sample_page_detail();
        let projected = project_page_fields(
            &detail,
            &[
                "body_markdown".to_string(),
                "client_url".to_string(),
                "created_by".to_string(),
                "client_url".to_string(),
            ],
        );

        assert_eq!(projected["id"], "page-42");
        assert_eq!(projected["title"], "Architecture Spec");
        assert_eq!(
            projected["body_markdown"],
            "# Architecture Spec\n- [ ] Ship fields projection"
        );
        assert_eq!(projected["missing_fields"], json!(["client_url", "created_by"]));
        assert!(projected.get("raw_xhtml").is_none());
    }

    #[test]
    fn parse_optional_string_array_validates_types() {
        assert_eq!(parse_optional_string_array(&json!({}), "fields").unwrap(), None);
        assert_eq!(
            parse_optional_string_array(&json!({"fields": null}), "fields").unwrap(),
            None
        );
        assert_eq!(
            parse_optional_string_array(&json!({"fields": [" blocks ", "", "web_url"]}), "fields")
                .unwrap(),
            Some(vec!["blocks".to_string(), "web_url".to_string()])
        );
        assert!(parse_optional_string_array(&json!({"fields": "blocks"}), "fields").is_err());
        assert!(parse_optional_string_array(&json!({"fields": ["blocks", 42]}), "fields").is_err());
    }

    #[tokio::test]
    async fn mcp_get_page_handler_supports_offline_fields_projection() {
        let detail = sample_page_detail();
        let storage = Storage::open_in_memory().unwrap();
        storage.upsert_onenote_page_detail(&detail).unwrap();

        let cfg = AppConfig::default();
        let auth = crate::auth::AuthManager::from_tokens(
            cfg.clone(),
            crate::auth::StoredTokens {
                access_token: "test-token".to_string(),
                refresh_token: None,
                expires_at: chrono::Utc::now().timestamp() + 3600,
                client_id: "cid".to_string(),
                tenant_id: "tid".to_string(),
                user_id: Some("user-1".to_string()),
                user_display_name: None,
                user_email: None,
                auth_mode: "access_token".to_string(),
            },
        );
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();

        let resp = handle_request_value(
            &cfg,
            &graph,
            &storage,
            json!({
                "jsonrpc": "2.0",
                "id": 99,
                "method": "tools/call",
                "params": {
                    "name": "get_page",
                    "arguments": {
                        "page": "me|page-42",
                        "offline": true,
                        "fields": ["blocks", "web_url", "nonexistent_meta"]
                    }
                }
            }),
        )
        .await
        .expect("tools/call returns response");

        assert_eq!(resp["result"]["isError"], false);
        let page = &resp["result"]["structuredContent"]["page"];
        assert_eq!(page["id"], "page-42");
        assert_eq!(page["title"], "Architecture Spec");
        assert_eq!(page["blocks"].as_array().unwrap().len(), 1);
        assert_eq!(page["web_url"], "https://onedrive.live.com/page-42");
        assert_eq!(page["missing_fields"], json!(["nonexistent_meta"]));
        assert!(page.get("raw_xhtml").is_none());
        assert!(page.get("body_markdown").is_none());
    }
}
