//! Standalone Microsoft OneNote (`onenote` / `note`) CLI parser, command dispatcher,
//! and shared domain helpers used by CLI, HTTP API (`serve`), and MCP (`mcp`).

use std::{
    io::{self, IsTerminal, Write},
    path::{Path, PathBuf},
};

use anyhow::{bail, Context, Result};
use clap::{CommandFactory, Parser, Subcommand};
use serde_json::json;

use crate::{
    auth::{diagnose_access_token_scopes, AuthManager, LoginOptions},
    config::{self, AppConfig, ImageProtocolPreference, Persona, DEFAULT_SERVE_PORT},
    daemon,
    graph::{
        copy::{DEFAULT_COPY_WAIT_SECONDS, MAX_COPY_WAIT_SECONDS},
        media, GraphClient,
    },
    img_preview,
    models::{
        split_onenote_entity_key, DownloadedResource, InlineImageUpload, OnenoteBlock,
        OnenoteNotebook, OnenotePageDetail, OnenotePageSummary, OnenoteResource, OnenoteScope,
        OnenoteSearchHit, OnenoteSection, OnenoteTagKind, OutgoingOnenotePage,
    },
    parser, resolver, serve,
    storage::Storage,
    sync, tui, updater,
};

#[derive(Debug, Parser)]
#[command(
    name = "onenote",
    version,
    about = "Microsoft OneNote terminal client & AI agent toolkit (CLI · TUI · HTTP API · MCP)",
    long_about = "Microsoft Graph v1.0 standalone OneNote terminal client.\nRun without arguments to open the interactive OneNote TUI."
)]
pub struct Cli {
    #[command(subcommand)]
    pub command: Option<Command>,
}

#[derive(Debug, Subcommand)]
pub enum DaemonCommand {
    /// Start the background OneNote sync daemon
    Start {
        #[arg(long)]
        json: bool,
    },
    /// Show background sync daemon status
    Status {
        #[arg(long)]
        json: bool,
    },
    /// Stop the running background sync daemon
    Stop {
        #[arg(long)]
        json: bool,
    },
    /// Restart the background sync daemon
    Restart {
        #[arg(long)]
        json: bool,
    },
    /// Run the daemon event loop in the foreground (used internally by `daemon start`)
    #[command(hide = true)]
    Run,
}

#[derive(Debug, Subcommand)]
pub enum Command {
    /// Sign in with Microsoft Entra ID (PKCE browser, Device Code, Client Secret, or token)
    #[command(visible_alias = "in")]
    Login {
        #[arg(long)]
        client_id: Option<String>,
        #[arg(long)]
        tenant_id: Option<String>,
        #[arg(long)]
        device_code: bool,
        #[arg(long)]
        client_secret: Option<String>,
        #[arg(long = "access-token", visible_alias = "token")]
        access_token: Option<String>,
        #[arg(long)]
        token_file: Option<PathBuf>,
        #[arg(long)]
        port: Option<u16>,
        #[arg(long)]
        json: bool,
    },
    /// Remove stored credentials from OS keyring and fallback file
    #[command(visible_alias = "out")]
    Logout {
        #[arg(long)]
        json: bool,
    },
    /// Show current signed-in user profile, token expiry, scopes, and auth source
    Whoami {
        #[arg(long)]
        json: bool,
    },
    /// Show config/DB/keyring paths, JWT scopes, image protocol, and optional online Graph probe
    Doctor {
        #[arg(long, visible_alias = "probe")]
        online: bool,
        #[arg(long)]
        json: bool,
    },
    /// List accessible OneNote notebooks (personal + optional team/group shared)
    #[command(visible_alias = "nb")]
    Notebooks {
        /// Include Microsoft Teams / M365 Group shared notebooks
        #[arg(long = "include-teams", default_value_t = false)]
        include_teams: bool,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create a new OneNote notebook
    #[command(name = "create-notebook")]
    CreateNotebook {
        name: String,
        #[arg(long)]
        group: Option<String>,
        #[arg(long)]
        site: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Rename a OneNote notebook's OneDrive or SharePoint package by storage ID
    RenameNotebook {
        #[arg(long)]
        drive_id: String,
        #[arg(long)]
        item_id: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// List OneNote sections (all or within a specific notebook)
    #[command(visible_alias = "sec")]
    Sections {
        notebook: Option<String>,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// Create a new OneNote section inside a notebook
    #[command(name = "create-section")]
    CreateSection {
        notebook: String,
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Rename an existing section by ID on the signed-in user's Graph OneNote route
    RenameSection {
        /// Section ID or me|ID from onenote sections
        section: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// Change an existing page's indentation in OneNote for Windows
    #[cfg(windows)]
    DesktopSetPageLevel {
        /// Full OneNote desktop Page ID from GetHierarchy
        page_id: String,
        #[arg(long, value_parser = clap::value_parser!(i64).range(0..=2))]
        level: i64,
        #[arg(long)]
        json: bool,
    },
    /// Rename an existing section in OneNote for Windows
    #[cfg(windows)]
    DesktopRenameSection {
        /// Full OneNote desktop Section ID from GetHierarchy
        section_id: String,
        #[arg(long)]
        name: String,
        #[arg(long)]
        json: bool,
    },
    /// List pages in a OneNote section (`"Notebook/Section"` or section name/ID)
    #[command(visible_alias = "ls")]
    Pages {
        section: String,
        #[arg(long, default_value_t = 50)]
        limit: usize,
        #[arg(long)]
        refresh: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
    },
    /// Read a OneNote page (Markdown, plain text, structured blocks, or raw XHTML)
    #[command(visible_alias = "cat")]
    Read {
        page: String,
        #[arg(long, default_value = "md", value_parser = ["md", "plain", "blocks", "xhtml"])]
        format: String,
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
        /// Return page ID, title, modification time and Markdown with --json
        #[arg(long, requires = "json")]
        compact: bool,
    },
    /// Create a new OneNote page from Markdown and optional inline images
    Create {
        section: String,
        #[arg(long, required = true)]
        title: String,
        markdown_body: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long = "image", value_name = "img_path")]
        image: Vec<PathBuf>,
        /// Page level (0 or omitted). Change indentation in OneNote for Windows.
        #[arg(long, value_parser = clap::value_parser!(i32).range(0..=2))]
        level: Option<i32>,
        #[arg(long)]
        json: bool,
    },
    /// Copy an existing page to a section through Microsoft Graph
    CopyPage {
        /// Source page ID or scoped page key
        page: String,
        /// Destination section ID or scoped section key
        #[arg(long)]
        section: String,
        /// Resume the existing operation using its returned operation_url
        #[arg(long)]
        operation: Option<String>,
        /// Seconds to poll (0 returns the operation handle immediately)
        #[arg(long, default_value_t = DEFAULT_COPY_WAIT_SECONDS, value_parser = clap::value_parser!(u32).range(0..=i64::from(MAX_COPY_WAIT_SECONDS)))]
        wait_seconds: u32,
        #[arg(long)]
        json: bool,
    },
    /// Append Markdown content and optional inline images to an existing OneNote page
    Append {
        page: String,
        markdown_body: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long = "image", value_name = "img_path")]
        image: Vec<PathBuf>,
        #[arg(long)]
        json: bool,
    },
    /// Update a OneNote page/block (`onenote update <page> ...`), or self-update when called without `<page>` (`onenote update --check`)
    #[command(
        after_help = "Examples:\n  onenote update PAGE --block-id 'p:{id}' --replace-text READY --with-text APPROVED\n  onenote update PAGE --block-id 'p:{id}' --body '**New block**'\n  onenote append PAGE 'New paragraph'"
    )]
    Update {
        /// Page key/ID/title to update. Omit to run the GitHub Releases self-updater.
        page: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long, allow_hyphen_values = true)]
        body: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long = "block-id")]
        block_id: Option<String>,
        /// Exact text to replace within a block while preserving its formatting
        #[arg(long, requires_all = ["block_id", "with_text"], conflicts_with_all = ["body", "file"])]
        replace_text: Option<String>,
        /// Replacement text for --replace-text
        #[arg(long, requires = "replace_text")]
        with_text: Option<String>,
        /// Expected baseline content of the target block/page from your prior read; aborts with conflict if remote changed
        #[arg(long = "expected-content", visible_alias = "base-content", allow_hyphen_values = true)]
        expected_content: Option<String>,
        /// Check for a newer `onenote` release on GitHub without installing (self-update mode)
        #[arg(long)]
        check: bool,
        /// Override conflict check on page/block update, or force reinstall in self-update mode
        #[arg(long)]
        force: bool,
        /// Override GitHub `owner/repo` for self-update
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Check GitHub Releases and self-update the `onenote` binary
    #[command(name = "self-update", visible_alias = "upgrade")]
    SelfUpdate {
        #[arg(long)]
        check: bool,
        #[arg(long)]
        force: bool,
        #[arg(long)]
        repo: Option<String>,
        #[arg(long)]
        json: bool,
    },
    /// Toggle a OneNote `to-do` checkbox block (`--block-id` or `--index`)
    Check {
        page: String,
        #[arg(long = "block-id")]
        block_id: Option<String>,
        #[arg(long)]
        index: Option<usize>,
        /// Set the checkbox to completed. Omit both state flags to toggle.
        #[arg(long, conflicts_with = "undone")]
        done: bool,
        /// Set the checkbox to incomplete
        #[arg(long)]
        undone: bool,
        /// Expected baseline content of the target to-do block from your prior read; aborts with conflict if remote changed
        #[arg(long = "expected-content", visible_alias = "base-content", allow_hyphen_values = true)]
        expected_content: Option<String>,
        /// Override conflict check if the target to-do block changed remotely
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
    },
    /// Delete a OneNote page (`--yes` required with `--json` or non-interactive mode)
    #[command(visible_alias = "rm")]
    Delete {
        page: String,
        #[arg(long)]
        yes: bool,
        #[arg(long)]
        json: bool,
    },
    /// Search OneNote pages using local SQLite FTS5 (`onenote_fts`)
    Search {
        query: String,
        #[arg(long)]
        section: Option<String>,
        #[arg(long)]
        refresh: bool,
        #[arg(long, default_value_t = 30)]
        limit: usize,
        #[arg(long)]
        json: bool,
    },
    /// Download or preview an image/file resource embedded in a OneNote page
    #[command(visible_alias = "dl")]
    Resource {
        page: String,
        #[arg(long)]
        index: Option<usize>,
        #[arg(long, visible_alias = "dir")]
        out: Option<PathBuf>,
        #[arg(long)]
        preview: bool,
        #[arg(long)]
        protocol: Option<String>,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
        #[arg(long)]
        json: bool,
    },
    /// Render a local image file directly in the terminal
    #[command(name = "preview-file")]
    PreviewFile {
        path: PathBuf,
        #[arg(long)]
        protocol: Option<String>,
        #[arg(long)]
        cols: Option<u16>,
        #[arg(long)]
        rows: Option<u16>,
    },
    /// Sync OneNote notebooks, sections, and pages to local SQLite cache
    Sync {
        #[arg(long)]
        json: bool,
    },
    /// Control the background OneNote sync daemon (`start`, `status`, `stop`, `restart`)
    Daemon {
        #[command(subcommand)]
        action: DaemonCommand,
    },
    /// Launch the interactive OneNote TUI
    Tui,
    /// Start the local HTTP JSON API server (default `127.0.0.1:8788`)
    #[command(visible_alias = "api")]
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = DEFAULT_SERVE_PORT)]
        port: u16,
        #[arg(long = "api-key", visible_alias = "token")]
        api_key: Option<String>,
    },
    /// Start the stdio MCP (JSON-RPC 2.0) server for AI agents
    #[command(visible_alias = "mcp-server")]
    Mcp,
}

/// Fast-path for `--version` / `-V` / `--help` / `-h` before Tokio runtime spin-up (<1ms).
pub fn try_fast_help_or_version() -> Option<Result<()>> {
    let mut args = std::env::args_os();
    let _bin = args.next();
    let first = args.next()?;
    if args.next().is_some() {
        return None;
    }
    match first.to_str()? {
        "-V" | "--version" => {
            let mut out = io::stdout().lock();
            let res = writeln!(out, "onenote {}", env!("CARGO_PKG_VERSION"));
            Some(res.map_err(Into::into))
        }
        "-h" | "--help" => Some(Cli::command().print_long_help().map_err(Into::into)),
        _ => None,
    }
}

pub async fn run_from_env() -> Result<()> {
    let mut cfg = AppConfig::load();
    let cli = Cli::parse();
    dispatch_command(&mut cfg, cli.command).await
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
            refresh,
            offline,
            cached,
            json,
        }) => cmd_notebooks(cfg, include_teams, refresh, offline, cached, json).await,
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
                println!("Notebook storage item '{}' is named '{}'.", item.item_id, item.name);
            }
            Ok(())
        }
        Some(Command::Sections {
            notebook,
            refresh,
            offline,
            cached,
            json,
        }) => cmd_sections(cfg, notebook.as_deref(), refresh, offline, cached, json).await,
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
                println!("OneNote desktop page '{page_id}' is at level {level}.");
            }
            Ok(())
        }
        #[cfg(windows)]
        Some(Command::DesktopRenameSection { section_id, name, json }) => {
            let result = crate::desktop::rename_section(&section_id, &name).await?;
            if json {
                println!("{}", serde_json::to_string_pretty(&result)?);
            } else {
                println!("OneNote desktop section '{section_id}' is named '{}'.", name.trim());
            }
            Ok(())
        }
        Some(Command::Pages {
            section,
            limit,
            refresh,
            offline,
            cached,
            json,
        }) => cmd_pages(cfg, &section, limit, refresh, offline, cached, json).await,
        Some(Command::Read {
            page,
            format,
            preview_images,
            offline,
            cached,
            json,
            compact,
        }) => {
            cmd_read(
                cfg,
                &page,
                &format,
                preview_images,
                offline,
                cached,
                json,
                compact,
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
            if json {
                println!("{}", serde_json::to_string_pretty(&outcome)?);
            }
            if !outcome.ok {
                let detail = outcome.error.as_deref().unwrap_or("Graph has not confirmed completion");
                bail!(
                    "Copy {:?} ({}): {detail}. Check the existing operation with --operation '{}'.",
                    outcome.status, outcome.operation_id, outcome.operation_url
                );
            }
            if !json {
                println!(
                    "Copied {} to {} as {}",
                    outcome.source_page_key,
                    outcome.destination_section_key,
                    outcome.new_page_key.as_deref().unwrap_or_default(),
                );
            }
            Ok(())
        }
        Some(Command::Append {
            page,
            markdown_body,
            file,
            image,
            json,
        }) => {
            cmd_append(
                cfg,
                &page,
                markdown_body.as_deref(),
                file.as_deref(),
                &image,
                json,
            )
            .await
        }
        Some(Command::Update {
            page,
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
        }) => {
            if let Some(page_query) = page {
                cmd_update_page(
                    cfg,
                    &page_query,
                    title.as_deref(),
                    body.as_deref(),
                    file.as_deref(),
                    block_id.as_deref(),
                    replace_text.as_deref().zip(with_text.as_deref()),
                    expected_content.as_deref(),
                    force,
                    json,
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
            block_id,
            index,
            done,
            undone,
            expected_content,
            force,
            json,
        }) => {
            cmd_check(
                cfg,
                &page,
                block_id.as_deref(),
                index,
                (done || undone).then_some(done),
                expected_content.as_deref(),
                force,
                json,
            )
            .await
        }
        Some(Command::Delete { page, yes, json }) => cmd_delete(cfg, &page, yes, json).await,
        Some(Command::Search {
            query,
            section,
            refresh,
            limit,
            json,
        }) => cmd_search(cfg, &query, section.as_deref(), refresh, limit, json).await,
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
        }) => {
            let (_auth, graph, storage) = open_runtime(cfg, false).await?;
            serve::run(cfg.clone(), graph, storage, &bind, port, api_key).await
        }
        Some(Command::Mcp) => {
            let (_auth, graph, storage) = open_runtime(cfg, false).await?;
            crate::mcp::run(cfg.clone(), graph, storage).await
        }
    }
}

// ---------------------------------------------------------------------------
// Shared Runtime & Domain Helpers (CLI / HTTP / MCP)
// ---------------------------------------------------------------------------

const CURSOR_NOTEBOOKS_PERSONAL: &str = "onenote:notebooks:personal";
const CURSOR_NOTEBOOKS_ALL: &str = "onenote:notebooks:all";
const CURSOR_SECTIONS_ALL: &str = "onenote:sections:all";
const CURSOR_HIERARCHY: &str = "onenote:hierarchy";

pub async fn open_runtime(
    cfg: &AppConfig,
    require_auth: bool,
) -> Result<(AuthManager, GraphClient, Storage)> {
    open_runtime_inner(cfg, require_auth, false).await
}

async fn open_runtime_inner(
    cfg: &AppConfig,
    require_auth: bool,
    offline: bool,
) -> Result<(AuthManager, GraphClient, Storage)> {
    let auth = AuthManager::new(cfg.clone())?;
    let mut user_id = if require_auth {
        let _ = auth.valid_access_token().await?;
        auth.current_tokens().await.and_then(|t| t.user_id)
    } else {
        auth.current_tokens().await.and_then(|t| t.user_id)
    }
    .filter(|id| !id.trim().is_empty());

    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    if user_id.is_none() && !offline && (require_auth || auth.current_tokens().await.is_some()) {
        user_id = graph
            .resolve_my_user_id()
            .await
            .filter(|id| !id.trim().is_empty());
    }
    let graph = if let Some(ref uid) = user_id {
        graph.with_my_user_id(uid.clone())
    } else {
        graph
    };
    let db_file = config::db_path(user_id.as_deref());
    let storage = Storage::open(&db_file)?;
    Ok((auth, graph, storage))
}

fn is_cursor_fresh(storage: &Storage, scope_key: &str, ttl_secs: u64) -> bool {
    if ttl_secs == 0 {
        return false;
    }
    let Ok(Some(cursor)) = storage.get_sync_cursor(scope_key) else {
        return false;
    };
    let Ok(synced_at) = chrono::DateTime::parse_from_rfc3339(&cursor.last_synced_at) else {
        return false;
    };
    let age_secs = chrono::Utc::now()
        .signed_duration_since(synced_at.with_timezone(&chrono::Utc))
        .num_seconds();
    age_secs < (ttl_secs as i64)
}

fn record_sync_cursor(storage: &Storage, scope_key: impl Into<String>, delta_link: Option<String>) {
    let _ = storage.upsert_sync_cursor(&crate::models::SyncCursor {
        scope_key: scope_key.into(),
        delta_link,
        history_next_link: None,
        last_synced_at: chrono::Utc::now().to_rfc3339(),
    });
}

pub(crate) async fn nudge_live_daemon(cfg: &AppConfig, graph: &GraphClient, cmd: sync::SyncCommand) {
    if daemon::is_daemon_alive_fast(cfg, graph.my_user_id()) {
        let _ = daemon::nudge_daemon(cfg, Some(cmd), None).await;
    }
}

pub fn build_onenote_scope_from_args(
    group_id: Option<&str>,
    site_id: Option<&str>,
) -> Result<OnenoteScope> {
    let g = group_id.map(str::trim).filter(|s| !s.is_empty());
    let s = site_id.map(str::trim).filter(|s| !s.is_empty());
    match (g, s) {
        (Some(_), Some(_)) => bail!("Cannot specify both --group and --site at the same time."),
        (Some(gid), None) => Ok(OnenoteScope::Group {
            group_id: gid.to_string(),
            group_name: None,
        }),
        (None, Some(sid)) => Ok(OnenoteScope::Site {
            site_id: sid.to_string(),
            site_name: None,
        }),
        (None, None) => Ok(OnenoteScope::Me),
    }
}

pub async fn load_onenote_notebooks(
    storage: &Storage,
    graph: &GraphClient,
    include_teams: bool,
    refresh: bool,
    offline: bool,
) -> Result<Vec<OnenoteNotebook>> {
    load_onenote_notebooks_with_cache(storage, graph, include_teams, refresh, offline, false).await
}

pub async fn load_onenote_notebooks_with_cache(
    storage: &Storage,
    graph: &GraphClient,
    include_teams: bool,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<Vec<OnenoteNotebook>> {
    let filter_nbs = |items: Vec<OnenoteNotebook>| -> Vec<OnenoteNotebook> {
        if include_teams {
            items
        } else {
            items.into_iter().filter(|n| n.scope.is_personal()).collect()
        }
    };

    if offline {
        return Ok(filter_nbs(storage.list_onenote_notebooks()?));
    }

    if !refresh {
        let cached_all = storage.list_onenote_notebooks()?;
        let filtered = filter_nbs(cached_all);
        if !filtered.is_empty() {
            if cached {
                let teams_synced = !include_teams
                    || storage.get_sync_cursor(CURSOR_NOTEBOOKS_ALL).ok().flatten().is_some()
                    || storage.get_sync_cursor(CURSOR_HIERARCHY).ok().flatten().is_some()
                    || filtered.iter().any(|n| !n.scope.is_personal());
                if teams_synced {
                    return Ok(filtered);
                }
            } else {
                let ttl = graph.config().cli_cache_ttl_secs;
                let fresh = if include_teams {
                    is_cursor_fresh(storage, CURSOR_NOTEBOOKS_ALL, ttl)
                        || is_cursor_fresh(storage, CURSOR_HIERARCHY, ttl)
                } else {
                    is_cursor_fresh(storage, CURSOR_NOTEBOOKS_PERSONAL, ttl)
                        || is_cursor_fresh(storage, CURSOR_NOTEBOOKS_ALL, ttl)
                        || is_cursor_fresh(storage, CURSOR_HIERARCHY, ttl)
                };
                if fresh {
                    return Ok(filtered);
                }
            }
        }
    }

    let nb_res = graph
        .list_all_accessible_notebooks_scoped(include_teams)
        .await?;
    storage.reconcile_onenote_notebooks_for_scopes(&nb_res.notebooks, &nb_res.complete_scopes)?;
    record_sync_cursor(storage, CURSOR_NOTEBOOKS_PERSONAL, None);
    if include_teams {
        record_sync_cursor(storage, CURSOR_NOTEBOOKS_ALL, None);
    }
    Ok(nb_res.notebooks)
}

pub async fn load_onenote_sections(
    storage: &Storage,
    graph: &GraphClient,
    notebook_query: Option<&str>,
    refresh: bool,
    offline: bool,
) -> Result<(Option<OnenoteNotebook>, Vec<OnenoteSection>)> {
    load_onenote_sections_with_cache(storage, graph, notebook_query, refresh, offline, false).await
}

pub async fn load_onenote_sections_with_cache(
    storage: &Storage,
    graph: &GraphClient,
    notebook_query: Option<&str>,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<(Option<OnenoteNotebook>, Vec<OnenoteSection>)> {
    let nb = if let Some(q) = notebook_query.map(str::trim).filter(|s| !s.is_empty()) {
        Some(resolver::resolve_notebook_with_refresh(graph, storage, q, offline, refresh).await?)
    } else {
        None
    };
    let nb_key = nb.as_ref().map(|n| n.notebook_key.clone());

    if offline {
        return Ok((nb, storage.list_onenote_sections(nb_key.as_deref())?));
    }

    let scope_key = nb_key
        .as_deref()
        .map(|k| format!("onenote:sections:{k}"))
        .unwrap_or_else(|| CURSOR_SECTIONS_ALL.to_string());
    let ttl = graph.config().cli_cache_ttl_secs;

    if !refresh {
        let cached_secs = storage.list_onenote_sections(nb_key.as_deref())?;
        if !cached_secs.is_empty()
            && (cached
                || is_cursor_fresh(storage, &scope_key, ttl)
                || is_cursor_fresh(storage, CURSOR_SECTIONS_ALL, ttl)
                || is_cursor_fresh(storage, CURSOR_HIERARCHY, ttl))
        {
            return Ok((nb, cached_secs));
        }
    }

    if let Some(ref target_nb) = nb {
        let fetched = graph
            .list_onenote_sections(&target_nb.scope, Some(&target_nb.id))
            .await?;
        storage.reconcile_onenote_notebook_sections(&target_nb.notebook_key, &fetched, true)?;
    } else {
        let mut notebooks = storage.list_onenote_notebooks()?;
        let mut nb_complete_scopes: Option<std::collections::HashSet<String>> = None;
        if notebooks.is_empty()
            || refresh
            || (!cached
                && !is_cursor_fresh(storage, CURSOR_NOTEBOOKS_ALL, ttl)
                && !is_cursor_fresh(storage, CURSOR_HIERARCHY, ttl))
        {
            let nb_res = graph.list_all_accessible_notebooks_scoped(true).await?;
            notebooks = nb_res.notebooks;
            nb_complete_scopes = Some(nb_res.complete_scopes);
            record_sync_cursor(storage, CURSOR_NOTEBOOKS_PERSONAL, None);
            record_sync_cursor(storage, CURSOR_NOTEBOOKS_ALL, None);
        }
        let sec_res = graph
            .list_all_accessible_sections_scoped(&notebooks, nb_complete_scopes.as_ref())
            .await?;
        if let Some(nb_scopes) = nb_complete_scopes {
            let reconciled_scopes: std::collections::HashSet<String> = nb_scopes
                .intersection(&sec_res.complete_scopes)
                .cloned()
                .collect();
            if !reconciled_scopes.is_empty() {
                storage.reconcile_onenote_hierarchy_for_scopes(
                    &notebooks,
                    &sec_res.section_groups,
                    &sec_res.sections,
                    &reconciled_scopes,
                )?;
            } else {
                storage.reconcile_onenote_notebooks_for_scopes(&notebooks, &nb_scopes)?;
                storage.reconcile_onenote_sections_for_scopes(
                    &sec_res.section_groups,
                    &sec_res.sections,
                    &sec_res.complete_scopes,
                )?;
            }
        } else {
            storage.reconcile_onenote_sections_for_scopes(
                &sec_res.section_groups,
                &sec_res.sections,
                &sec_res.complete_scopes,
            )?;
        }
    }
    record_sync_cursor(storage, scope_key, None);
    Ok((nb, storage.list_onenote_sections(nb_key.as_deref())?))
}

pub async fn resolve_onenote_section_query(
    storage: &Storage,
    graph: &GraphClient,
    section_query: &str,
    offline: bool,
) -> Result<OnenoteSection> {
    resolve_onenote_section_query_with_refresh(storage, graph, section_query, offline, false).await
}

pub async fn resolve_onenote_section_query_with_refresh(
    storage: &Storage,
    graph: &GraphClient,
    section_query: &str,
    offline: bool,
    force_refresh: bool,
) -> Result<OnenoteSection> {
    let trimmed = section_query.trim();
    if trimmed.is_empty() {
        bail!("OneNote section name or ID is empty.");
    }
    if let Some((nb_part, sec_part)) = trimmed.split_once('/') {
        if !nb_part.trim().is_empty() && !sec_part.trim().is_empty() && !trimmed.contains('|') {
            return resolver::resolve_section_with_refresh(
                graph,
                storage,
                sec_part.trim(),
                Some(nb_part.trim()),
                offline,
                force_refresh,
            )
            .await;
        }
    }
    resolver::resolve_section_with_refresh(graph, storage, trimmed, None, offline, force_refresh)
        .await
}

pub async fn list_onenote_pages_helper(
    storage: &Storage,
    graph: &GraphClient,
    section_query: &str,
    limit: usize,
    refresh: bool,
    offline: bool,
) -> Result<(OnenoteSection, Vec<OnenotePageSummary>)> {
    list_onenote_pages_with_cache(storage, graph, section_query, limit, refresh, offline, false)
        .await
}

pub async fn list_onenote_pages_with_cache(
    storage: &Storage,
    graph: &GraphClient,
    section_query: &str,
    limit: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<(OnenoteSection, Vec<OnenotePageSummary>)> {
    let mut sec = resolve_onenote_section_query_with_refresh(
        storage,
        graph,
        section_query,
        offline,
        refresh,
    )
    .await?;
    if offline {
        return Ok((
            sec.clone(),
            storage
                .list_onenote_pages(&sec.section_key)?
                .into_iter()
                .take(limit)
                .collect(),
        ));
    }

    let mut cursor_key = format!("onenote:pages:{}", sec.section_key);
    if !refresh {
        let cached_pages = storage.list_onenote_pages(&sec.section_key)?;
        let ttl = graph.config().cli_cache_ttl_secs;
        if !cached_pages.is_empty() && (cached || is_cursor_fresh(storage, &cursor_key, ttl)) {
            return Ok((sec, cached_pages.into_iter().take(limit).collect()));
        }
    }

    let fetched = match graph.list_onenote_pages(&sec.scope, &sec.id, limit).await {
        Ok(pages) => pages,
        Err(err) if !refresh && crate::graph::onenote::is_not_found_error(&err) => {
            let refreshed_sec = resolve_onenote_section_query_with_refresh(
                storage,
                graph,
                section_query,
                false,
                true,
            )
            .await?;
            if refreshed_sec.section_key == sec.section_key {
                return Err(err);
            }
            sec = refreshed_sec;
            cursor_key = format!("onenote:pages:{}", sec.section_key);
            graph.list_onenote_pages(&sec.scope, &sec.id, limit).await?
        }
        Err(err) => return Err(err),
    };
    let is_complete = fetched.len() < limit;
    storage.reconcile_onenote_section_pages(&sec.section_key, &fetched, is_complete)?;
    record_sync_cursor(storage, cursor_key, sec.last_modified_at.clone());
    Ok((sec, fetched.into_iter().take(limit).collect()))
}

pub async fn load_onenote_page_detail(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    offline: bool,
) -> Result<OnenotePageDetail> {
    load_onenote_page_detail_with_cache(storage, graph, page_query, offline, false).await
}

pub async fn load_onenote_page_detail_with_cache(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    offline: bool,
    cached: bool,
) -> Result<OnenotePageDetail> {
    let q = page_query.trim();
    if q.is_empty() {
        bail!("Page ID or title is empty.");
    }

    if offline || cached {
        if let Some(detail) = storage.get_onenote_page_detail(q)? {
            return Ok(detail);
        }
        if let Some(summary) = resolver::resolve_page_cached(storage, q, None) {
            if let Some(detail) = storage.get_onenote_page_detail(&summary.page_key)? {
                return Ok(detail);
            }
        }
        if offline {
            bail!("OneNote page not found in offline cache: {q}");
        }
    }

    let cached_summary = resolver::resolve_page_cached(storage, q, None);
    let (scope, raw_id) = match cached_summary.as_ref() {
        Some(s) => (s.scope.clone(), s.id.clone()),
        None => split_onenote_entity_key(q),
    };
    let resolved_page_key = cached_summary
        .as_ref()
        .map(|s| s.page_key.clone())
        .unwrap_or_else(|| scope.make_entity_key(&raw_id));

    let detail = match graph
        .get_onenote_page_detail(&scope, &raw_id, cached_summary)
        .await
    {
        Ok(d) => d,
        Err(err) => {
            if crate::graph::onenote::is_not_found_error(&err) {
                let _ = storage.delete_onenote_page(&resolved_page_key);
            }
            return Err(err);
        }
    };
    storage.upsert_onenote_page_detail(&detail)?;
    Ok(detail)
}

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
    let (scope, id) = split_onenote_entity_key(section_key.trim());
    let section = graph.rename_onenote_section(&scope, &id, name).await?;
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
    let detail = load_onenote_page_detail(storage, graph, page_query, false).await?;
    graph
        .append_onenote_page_markdown(
            &detail.summary.scope,
            &detail.summary.id,
            detail.primary_div_id.as_deref(),
            markdown,
            inline_images,
        )
        .await?;
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

fn load_cached_detail_for_query(storage: &Storage, page_query: &str) -> Option<OnenotePageDetail> {
    let q = page_query.trim();
    if q.is_empty() {
        return None;
    }
    if let Ok(Some(d)) = storage.get_onenote_page_detail(q) {
        return Some(d);
    }
    resolver::resolve_page_cached(storage, q, None)
        .and_then(|s| storage.get_onenote_page_detail(&s.page_key).ok().flatten())
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
    let cached_before = load_cached_detail_for_query(storage, page_query);
    let detail = load_onenote_page_detail(storage, graph, page_query, false).await?;

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
    let cached_before = load_cached_detail_for_query(storage, page_query);
    let detail = load_onenote_page_detail(storage, graph, page_query, false).await?;
    let target_block: &OnenoteBlock =
        if let Some(bid) = block_id.map(str::trim).filter(|s| !s.is_empty()) {
            match parser::find_block_by_element_id(&detail, bid) {
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
    let cached = resolver::resolve_page_cached(storage, page_query, None);
    let (scope, raw_id, page_key) = match cached {
        Some(s) => (s.scope, s.id, s.page_key),
        None => {
            let (s, id) = split_onenote_entity_key(page_query.trim());
            let key = s.make_entity_key(&id);
            (s, id, key)
        }
    };
    graph.delete_onenote_page(&scope, &raw_id).await?;
    let _ = storage.delete_onenote_page(&page_key);
    Ok(page_key)
}

pub fn select_onenote_resource<'a>(
    detail: &'a OnenotePageDetail,
    index: Option<usize>,
    prefer_image: bool,
) -> Result<(usize, &'a OnenoteResource)> {
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

pub async fn download_onenote_resource_helper(
    cfg: &AppConfig,
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    index: Option<usize>,
    out_dir: Option<&Path>,
) -> Result<(OnenotePageDetail, usize, DownloadedResource)> {
    let detail = load_onenote_page_detail(storage, graph, page_query, false).await?;
    let (resolved_idx, res) = select_onenote_resource(&detail, index, false)?;
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
    if refresh {
        let _ = sync::bootstrap_onenote(graph, storage).await?;
    }
    let mut hits = storage.search_onenote_pages(query, None, limit.saturating_mul(2).max(20))?;
    if let Some(sec_q) = section_filter.map(str::trim).filter(|s| !s.is_empty()) {
        let needle = sec_q.to_lowercase();
        hits.retain(|h| {
            h.section_id == sec_q
                || h.section_name
                    .as_deref()
                    .is_some_and(|n| n.to_lowercase().contains(&needle))
        });
    }
    hits.truncate(limit);
    Ok(hits)
}

// ---------------------------------------------------------------------------
// Command Implementations
// ---------------------------------------------------------------------------

async fn cmd_tui(cfg: &AppConfig) -> Result<()> {
    let (auth, graph, storage) = open_runtime(cfg, true).await?;
    tui::run_with_persona(cfg.clone(), Persona::detect(), auth, graph, storage).await
}

#[allow(clippy::too_many_arguments)]
async fn cmd_login(
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
        println!("Signed in to Microsoft OneNote as {who} ({}).", tokens.auth_mode);
        for w in &diag.warnings {
            eprintln!("Warning: {w}");
        }
    }
    Ok(())
}

async fn cmd_logout(cfg: &AppConfig, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    auth.logout().await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "ok": true }))?);
    } else {
        println!("Logged out and removed stored OneNote credentials.");
    }
    Ok(())
}

async fn cmd_whoami(cfg: &AppConfig, json_out: bool) -> Result<()> {
    let (auth, graph, _storage) = open_runtime(cfg, true).await?;
    let tok = auth.current_tokens().await;
    let me = graph.get_me().await?;
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
        println!("User:   {} ({})", me.display_name, me.id);
        if let Some(ref email) = me.email {
            println!("Email:  {email}");
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

async fn cmd_doctor(cfg: &AppConfig, online: bool, json_out: bool) -> Result<()> {
    let auth = AuthManager::new(cfg.clone())?;
    let tok = auth.current_tokens().await;
    let user_id = tok.as_ref().and_then(|t| t.user_id.clone());
    let db_file = config::db_path(user_id.as_deref());
    let storage = Storage::open(&db_file).ok();
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
    let diag = tok
        .as_ref()
        .map(|t| diagnose_access_token_scopes(&t.access_token))
        .unwrap_or_default();
    let img_caps = img_preview::probe_terminal_graphics_caps(cfg.image_protocol);
    let daemon_st = daemon::query_daemon_status(cfg).await.unwrap_or_default();

    let mut online_ok: Option<bool> = None;
    let mut online_error: Option<String> = None;
    if online {
        let graph = GraphClient::new(cfg.clone(), auth.clone())?;
        match graph.list_onenote_notebooks(&OnenoteScope::Me).await {
            Ok(_) => online_ok = Some(true),
            Err(e) => {
                online_ok = Some(false);
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
            }))?
        );
    } else {
        println!("onenote-cli v{}", env!("CARGO_PKG_VERSION"));
        println!("Config:         {}", config::config_file_path().display());
        println!("Database:       {}", db_file.display());
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
            }
        }
        for w in &diag.warnings {
            eprintln!("Warning: {w}");
        }
    }
    Ok(())
}

async fn cmd_notebooks(
    cfg: &AppConfig,
    include_teams: bool,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime_inner(cfg, !offline && !cached, offline).await?;
    let notebooks =
        load_onenote_notebooks_with_cache(&storage, &graph, include_teams, refresh, offline, cached)
            .await?;
    if json_out {
        println!("{}", serde_json::to_string_pretty(&json!({ "notebooks": notebooks }))?);
    } else if notebooks.is_empty() {
        println!("No OneNote notebooks found.");
    } else {
        for nb in &notebooks {
            let def = if nb.is_default { " [default]" } else { "" };
            let owner = nb.scope.owner_label();
            println!("{}  ({owner}){def}  [{}]", nb.display_name, nb.notebook_key);
        }
    }
    Ok(())
}

async fn cmd_create_notebook(
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
            created.display_name, created.notebook_key
        );
    }
    Ok(())
}

async fn cmd_sections(
    cfg: &AppConfig,
    notebook: Option<&str>,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime_inner(cfg, !offline && !cached, offline).await?;
    let (nb, sections) =
        load_onenote_sections_with_cache(&storage, &graph, notebook, refresh, offline, cached)
            .await?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "notebook": nb, "sections": sections }))?
        );
    } else if sections.is_empty() {
        println!("No OneNote sections found.");
    } else {
        for sec in &sections {
            let nb_label = sec.notebook_name.as_deref().unwrap_or("Notebook");
            println!("{nb_label} › {}  [{}]", sec.display_name, sec.section_key);
        }
    }
    Ok(())
}

async fn cmd_create_section(
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
            nb.display_name, created.display_name, created.section_key
        );
    }
    Ok(())
}

async fn cmd_rename_section(
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
            section.display_name, section.section_key
        );
    }
    Ok(())
}

async fn cmd_pages(
    cfg: &AppConfig,
    section_query: &str,
    limit: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime_inner(cfg, !offline && !cached, offline).await?;
    let (sec, pages) = list_onenote_pages_with_cache(
        &storage,
        &graph,
        section_query,
        limit,
        refresh,
        offline,
        cached,
    )
    .await?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "section": sec, "pages": pages }))?
        );
    } else if pages.is_empty() {
        println!("No pages in section '{}'.", sec.display_name);
    } else {
        for p in &pages {
            let indent = "  ".repeat(p.level.clamp(0, 2) as usize);
            let todo_mark = if p.has_todo { " [todo]" } else { "" };
            println!("{indent}{}{}  [{}]", p.title, todo_mark, p.page_key);
        }
    }
    Ok(())
}

pub(crate) fn page_content_json(detail: &OnenotePageDetail, compact: bool) -> serde_json::Value {
    if compact {
        json!({
            "id": detail.summary.id,
            "title": detail.summary.title,
            "last_modified_at": detail.summary.last_modified_at,
            "body_markdown": detail.body_markdown,
        })
    } else {
        json!(detail)
    }
}

#[allow(clippy::too_many_arguments)]
async fn cmd_read(
    cfg: &AppConfig,
    page: &str,
    format: &str,
    preview_images: bool,
    offline: bool,
    cached: bool,
    json_out: bool,
    compact: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime_inner(cfg, !offline && !cached, offline).await?;
    let detail = load_onenote_page_detail_with_cache(&storage, &graph, page, offline, cached).await?;
    if json_out {
        let page = page_content_json(&detail, compact);
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "page": page }))?
        );
        return Ok(());
    }

    match format {
        "xhtml" => println!("{}", detail.raw_xhtml),
        "plain" => {
            println!("# {}\n", detail.summary.title);
            println!("{}", detail.body_plain);
        }
        "blocks" => {
            println!("# {} [{}]\n", detail.summary.title, detail.summary.page_key);
            for (idx, blk) in detail.blocks.iter().enumerate() {
                let id = blk.element_id.as_deref().unwrap_or("-");
                let tag = blk
                    .tag
                    .as_ref()
                    .map(|t| format!(" [{}]", t.data_tag_value()))
                    .unwrap_or_default();
                let preview = truncate_single_line(&parser::onenote_blocks_to_plain(std::slice::from_ref(blk)), 90);
                println!("[{idx}] id={id}{tag}  {preview}");
            }
        }
        _ => {
            println!("# {}\n", detail.summary.title);
            println!("{}", detail.body_markdown);
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
async fn cmd_create(
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
            created.title, sec.display_name, created.page_key
        );
    }
    Ok(())
}

async fn cmd_append(
    cfg: &AppConfig,
    page: &str,
    markdown_body: Option<&str>,
    file: Option<&Path>,
    images: &[PathBuf],
    json_out: bool,
) -> Result<()> {
    let body = resolve_markdown_input(markdown_body, file)?;
    let inline_images = load_inline_images_from_paths(images)?;
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let updated =
        append_onenote_page_helper(&storage, &graph, page, &body, &inline_images).await?;
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
            serde_json::to_string_pretty(&json!({ "ok": true, "page": updated }))?
        );
    } else {
        println!("Appended content to page '{}' ({}).", updated.summary.title, updated.summary.page_key);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_update_page(
    cfg: &AppConfig,
    page: &str,
    title: Option<&str>,
    body: Option<&str>,
    file: Option<&Path>,
    block_id: Option<&str>,
    replace_text: Option<(&str, &str)>,
    expected_content: Option<&str>,
    force: bool,
    json_out: bool,
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
    let updated = update_onenote_content_helper_with_guard(
        &storage,
        &graph,
        page,
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
            serde_json::to_string_pretty(&json!({ "ok": true, "page": updated }))?
        );
    } else {
        println!("Updated page '{}' ({}).", updated.summary.title, updated.summary.page_key);
    }
    Ok(())
}

async fn cmd_check(
    cfg: &AppConfig,
    page: &str,
    block_id: Option<&str>,
    index: Option<usize>,
    completed: Option<bool>,
    expected_content: Option<&str>,
    force: bool,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let (updated, target_id, completed) = change_onenote_todo_helper_with_guard(
        &storage,
        &graph,
        page,
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
            serde_json::to_string_pretty(&json!({
                "ok": true,
                "page_key": updated.summary.page_key,
                "block_id": target_id,
                "completed": completed,
                "page": updated,
            }))?
        );
    } else {
        let state = if completed { "checked [x]" } else { "unchecked [ ]" };
        println!("Toggled to-do {target_id} -> {state}");
    }
    Ok(())
}

async fn cmd_delete(cfg: &AppConfig, page: &str, yes: bool, json_out: bool) -> Result<()> {
    if !yes {
        if json_out || !io::stdin().is_terminal() {
            bail!("Refusing to delete page without `--yes` in JSON or non-interactive mode.");
        }
        eprint!("Delete OneNote page '{page}'? [y/N]: ");
        io::stderr().flush()?;
        let mut line = String::new();
        io::stdin().read_line(&mut line)?;
        if !matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes") {
            println!("Cancelled.");
            return Ok(());
        }
    }
    let (_auth, graph, storage) = open_runtime(cfg, true).await?;
    let section_key = resolver::resolve_page_cached(&storage, page, None)
        .map(|s| s.scope.make_entity_key(&s.section_id));
    let deleted_key = delete_onenote_page_helper(&storage, &graph, page).await?;
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
        println!("Deleted OneNote page: {deleted_key}");
    }
    Ok(())
}

async fn cmd_search(
    cfg: &AppConfig,
    query: &str,
    section: Option<&str>,
    refresh: bool,
    limit: usize,
    json_out: bool,
) -> Result<()> {
    let (_auth, graph, storage) = open_runtime(cfg, refresh).await?;
    let hits = search_onenote_helper(&storage, &graph, query, section, refresh, limit).await?;
    if json_out {
        println!(
            "{}",
            serde_json::to_string_pretty(&json!({ "query": query, "hits": hits }))?
        );
    } else if hits.is_empty() {
        println!("No matching OneNote pages found for '{query}'.");
    } else {
        for hit in &hits {
            let loc = match (hit.notebook_name.as_deref(), hit.section_name.as_deref()) {
                (Some(n), Some(s)) => format!("{n} › {s}"),
                (None, Some(s)) => s.to_string(),
                _ => hit.section_id.clone(),
            };
            println!("{}  ({loc})  [{}]", hit.title, hit.page_key);
            if !hit.snippet.trim().is_empty() {
                println!("  {}", truncate_single_line(&hit.snippet, 120));
            }
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_resource(
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
            downloaded.path, downloaded.size_bytes, downloaded.mime_type
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

fn cmd_preview_file(
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

async fn cmd_sync(cfg: &AppConfig, json_out: bool) -> Result<()> {
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

async fn cmd_daemon(cfg: &AppConfig, action: DaemonCommand) -> Result<()> {
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

async fn cmd_self_update(
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

fn resolve_markdown_input(inline: Option<&str>, file: Option<&Path>) -> Result<String> {
    if let Some(path) = file {
        return std::fs::read_to_string(path)
            .with_context(|| format!("Failed to read markdown file: {}", path.display()));
    }
    Ok(inline.unwrap_or_default().to_string())
}

fn default_preview_dimensions(cols: Option<u16>, rows: Option<u16>) -> (u16, u16) {
    let (tw, th) = crossterm::terminal::size().unwrap_or((100, 32));
    let c = cols.unwrap_or_else(|| tw.saturating_sub(4).clamp(24, 96));
    let r = rows.unwrap_or_else(|| th.saturating_sub(6).clamp(10, 32));
    (c, r)
}

fn format_unix_ts(ts: i64) -> String {
    chrono::DateTime::<chrono::Utc>::from_timestamp(ts, 0)
        .map(|dt| {
            dt.with_timezone(&chrono::Local)
                .format("%Y-%m-%d %H:%M:%S")
                .to_string()
        })
        .unwrap_or_else(|| ts.to_string())
}

fn truncate_single_line(input: &str, max_chars: usize) -> String {
    let single = input.replace(['\r', '\n'], " ");
    let trimmed = single.trim();
    if trimmed.chars().count() <= max_chars {
        trimmed.to_string()
    } else {
        let prefix: String = trimmed.chars().take(max_chars.saturating_sub(1)).collect();
        format!("{prefix}…")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_default_tui_and_core_onenote_commands() {
        let cli = Cli::try_parse_from(["onenote"]).unwrap();
        assert!(cli.command.is_none());

        let cli = Cli::try_parse_from(["onenote", "notebooks", "--include-teams", "--json"])
            .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Notebooks {
                include_teams: true,
                json: true,
                ..
            }
        ));

        let cli = Cli::try_parse_from([
            "onenote",
            "create",
            "Work/Specs",
            "--title",
            "RFC-01",
            "# Heading",
            "--level",
            "1",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Create {
                level: Some(1),
                json: true,
                ..
            }
        ));

        let cli = Cli::try_parse_from(["onenote", "resource", "me|p1", "--index", "0", "--json"])
            .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Resource {
                index: Some(0),
                json: true,
                ..
            }
        ));

        let cli = Cli::try_parse_from(["onenote", "serve"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Serve { port: 8788, .. }
        ));

        let cli = Cli::try_parse_from(["onenote", "update", "--check", "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Update {
                page: None,
                check: true,
                json: true,
                ..
            }
        ));

        let cli = Cli::try_parse_from([
            "onenote",
            "update",
            "me|p1",
            "--block-id",
            "p:123",
            "--body",
            "Updated",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Update {
                page: Some(_),
                block_id: Some(_),
                ..
            }
        ));
    }
}
