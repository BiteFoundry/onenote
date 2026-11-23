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
        /// Maximum number of notebooks to return (default: 50 with --json, unbounded in text mode)
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=1000))]
        limit: Option<u64>,
        /// Offset cursor from the previous response's `next_cursor`
        #[arg(long, default_value_t = 0)]
        cursor: usize,
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
        /// Maximum number of sections to return (default: 50 with --json, unbounded in text mode)
        #[arg(long, value_parser = clap::value_parser!(u64).range(1..=1000))]
        limit: Option<u64>,
        /// Offset cursor from the previous response's `next_cursor`
        #[arg(long, default_value_t = 0)]
        cursor: usize,
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
        /// Offset cursor from the previous response's `next_cursor`
        #[arg(long, default_value_t = 0)]
        cursor: usize,
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
        /// Optional section name, Notebook/Section path, ID, or scoped key to narrow title lookup
        #[arg(long)]
        section: Option<String>,
        /// Text output format: md, plain, blocks or xhtml [default: md].
        /// With --json, an explicit --format keeps the full JSON page unless --compact is given.
        #[arg(long, value_parser = ["md", "plain", "blocks", "xhtml"])]
        format: Option<String>,
        #[arg(long = "preview-images")]
        preview_images: bool,
        #[arg(long)]
        offline: bool,
        #[arg(long, visible_alias = "fast")]
        cached: bool,
        #[arg(long)]
        json: bool,
        /// With --json: return the full page (blocks, raw XHTML, resources)
        #[arg(long, requires = "json", conflicts_with = "compact")]
        full: bool,
        /// With --json: return the compact page (the default when --format is not given)
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
        /// Optional section name, Notebook/Section path, ID, or scoped key to narrow title lookup
        #[arg(long)]
        section: Option<String>,
        #[arg(long)]
        file: Option<PathBuf>,
        #[arg(long = "image", value_name = "img_path")]
        image: Vec<PathBuf>,
        #[arg(long)]
        json: bool,
        /// With --json: include the full page object in the response
        #[arg(long, requires = "json")]
        full: bool,
    },
    /// Update a OneNote page/block (`onenote update <page> ...`), or self-update when called without `<page>` (`onenote update --check`)
    #[command(
        after_help = "Examples:\n  onenote update PAGE --block-id 'p:{id}' --replace-text READY --with-text APPROVED\n  onenote update PAGE --block-id 'p:{id}' --body '**New block**'\n  onenote append PAGE 'New paragraph'"
    )]
    Update {
        /// Page key/ID/title to update. Omit to run the GitHub Releases self-updater.
        page: Option<String>,
        /// Optional section name, Notebook/Section path, ID, or scoped key to narrow title lookup
        #[arg(long)]
        section: Option<String>,
        #[arg(long)]
        title: Option<String>,
        #[arg(long)]
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
        #[arg(long = "expected-content", visible_alias = "base-content")]
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
        /// With --json: include the full page object in the response
        #[arg(long, requires = "json")]
        full: bool,
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
        /// Optional section name, Notebook/Section path, ID, or scoped key to narrow title lookup
        #[arg(long)]
        section: Option<String>,
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
        #[arg(long = "expected-content", visible_alias = "base-content")]
        expected_content: Option<String>,
        /// Override conflict check if the target to-do block changed remotely
        #[arg(long)]
        force: bool,
        #[arg(long)]
        json: bool,
        /// With --json: include the full page object in the response
        #[arg(long, requires = "json")]
        full: bool,
    },
    /// Delete a OneNote page (`--yes` required with `--json` or non-interactive mode)
    #[command(visible_alias = "rm")]
    Delete {
        page: String,
        /// Optional section name, Notebook/Section path, ID, or scoped key to narrow title lookup
        #[arg(long)]
        section: Option<String>,
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
        /// Offset cursor from the previous response's `next_cursor`
        #[arg(long, default_value_t = 0)]
        cursor: usize,
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
    /// Start the local HTTP JSON API server (default `127.0.0.1:8793`)
    #[command(visible_alias = "api")]
    Serve {
        #[arg(long, default_value = "127.0.0.1")]
        bind: String,
        #[arg(long, default_value_t = DEFAULT_SERVE_PORT)]
        port: u16,
        /// API key clients must send (`Authorization: Bearer <key>` or `x-api-key`).
        /// Falls back to `ONENOTE_API_KEY`; a loopback bind without either
        /// generates a random key and prints it once on stderr.
        #[arg(long = "api-key", visible_alias = "token")]
        api_key: Option<String>,
        /// Run a loopback server without an API key (not allowed on other binds)
        #[arg(long, conflicts_with = "api_key")]
        allow_keyless: bool,
        /// Extra `Host` header value to accept (repeatable). Loopback names and
        /// the `--bind` value are always accepted
        #[arg(long = "allowed-host", value_name = "HOST")]
        allowed_hosts: Vec<String>,
        /// Exact browser origin allowed to call the API, e.g. `https://app.example.com`
        /// (repeatable). Without it every cross-origin browser request is refused
        #[arg(long = "cors-origin", value_name = "ORIGIN")]
        cors_origins: Vec<String>,
    },
    /// Start the stdio MCP (JSON-RPC 2.0) server for AI agents
    #[command(visible_alias = "mcp-server")]
    Mcp,
}

impl Cli {
    pub fn parse() -> Self {
        Self::try_parse_from(std::env::args_os()).unwrap_or_else(|e| e.exit())
    }

    pub fn try_parse_from<I, T>(itr: I) -> std::result::Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let normalized = normalize_hyphen_text_args(itr);
        <Self as clap::Parser>::try_parse_from(normalized)
    }
}

fn find_subcommand<'a>(cmd: &'a clap::Command, name: &str) -> Option<&'a clap::Command> {
    cmd.get_subcommands()
        .find(|sub| sub.get_name() == name || sub.get_all_aliases().any(|alias| alias == name))
}

fn find_long_arg<'a>(subcmd: &'a clap::Command, long_name: &str) -> Option<&'a clap::Arg> {
    subcmd.get_arguments().find(|arg| {
        arg.get_long() == Some(long_name)
            || arg
                .get_all_aliases()
                .is_some_and(|aliases| aliases.contains(&long_name))
    })
}

fn is_known_short_token(subcmd: &clap::Command, short_rest: &str) -> bool {
    !short_rest.is_empty()
        && short_rest.chars().all(|ch| {
            matches!(ch, 'h' | 'V')
                || subcmd.get_arguments().any(|arg| {
                    arg.get_short() == Some(ch)
                        || arg
                            .get_all_short_aliases()
                            .is_some_and(|aliases| aliases.contains(&ch))
                })
        })
}

fn is_known_flag_token(subcmd: &clap::Command, raw: &str) -> bool {
    if let Some(long_body) = raw.strip_prefix("--") {
        let name = long_body.split_once('=').map_or(long_body, |(name, _)| name);
        return matches!(name, "help" | "version") || find_long_arg(subcmd, name).is_some();
    }
    if let Some(short_rest) = raw.strip_prefix('-') {
        return is_known_short_token(subcmd, short_rest);
    }
    false
}

fn looks_like_long_flag(long_body: &str) -> bool {
    if long_body.contains(['\n', '\r']) {
        return false;
    }
    let name = long_body.split_once('=').map_or(long_body, |(name, _)| name);
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_hyphen_text_literal(subcmd: &clap::Command, raw: &str) -> bool {
    let Some(rest) = raw.strip_prefix('-') else {
        return false;
    };
    if rest.is_empty() || raw == "--" || is_known_flag_token(subcmd, raw) {
        return false;
    }
    if let Some(long_body) = raw.strip_prefix("--") {
        return !looks_like_long_flag(long_body);
    }
    true
}

fn subcommand_has_free_text(subcmd_name: &str) -> bool {
    matches!(
        subcmd_name,
        "create"
            | "append"
            | "update"
            | "check"
            | "rename-section"
            | "create-section"
            | "desktop-rename-section"
            | "create-notebook"
            | "rename-notebook"
            | "search"
    )
}

fn is_free_text_option(subcmd_name: &str, arg_id: &str) -> bool {
    match subcmd_name {
        "create" => arg_id == "title",
        "update" => matches!(
            arg_id,
            "title" | "body" | "replace_text" | "with_text" | "expected_content"
        ),
        "check" => arg_id == "expected_content",
        "rename-section" | "rename-notebook" | "desktop-rename-section" => arg_id == "name",
        _ => false,
    }
}

fn is_free_text_positional(subcmd_name: &str, pos_index: usize) -> bool {
    matches!(
        (subcmd_name, pos_index),
        ("create", 1)
            | ("append", 1)
            | ("create-section", 1)
            | ("create-notebook", 0)
            | ("search", 0)
    )
}

fn may_be_hyphen_text_literal(raw: &str) -> bool {
    let Some(rest) = raw.strip_prefix('-') else {
        return false;
    };
    if rest.is_empty() || raw == "--" || matches!(raw, "-h" | "-V") {
        return false;
    }
    if let Some(long_body) = raw.strip_prefix("--") {
        return !looks_like_long_flag(long_body);
    }
    true
}

fn argv_needs_hyphen_normalization(raw: &[std::ffi::OsString]) -> bool {
    if raw.len() <= 2 {
        return false;
    }
    let Some(sub_arg) = raw[1].to_str() else {
        return false;
    };
    if !subcommand_has_free_text(sub_arg) {
        return false;
    }
    for tok in &raw[2..] {
        let Some(tok_str) = tok.to_str() else {
            continue;
        };
        if tok_str == "--" {
            break;
        }
        if may_be_hyphen_text_literal(tok_str) {
            return true;
        }
    }
    false
}

fn normalize_hyphen_text_args<I, T>(itr: I) -> Vec<std::ffi::OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString>,
{
    let raw: Vec<std::ffi::OsString> = itr.into_iter().map(Into::into).collect();
    if !argv_needs_hyphen_normalization(&raw) {
        return raw;
    }
    let Some(sub_arg) = raw[1].to_str() else {
        return raw;
    };
    let mut root_cmd = <Cli as clap::CommandFactory>::command();
    root_cmd.build();
    let Some(subcmd) = find_subcommand(&root_cmd, sub_arg) else {
        return raw;
    };
    let sub_name = subcmd.get_name();

    let mut before_dash = Vec::with_capacity(raw.len() + 1);
    before_dash.push(raw[0].clone());
    before_dash.push(raw[1].clone());
    let mut deferred_pos: Vec<std::ffi::OsString> = Vec::new();
    let mut pos_index = 0usize;
    let mut i = 2usize;

    while i < raw.len() {
        let tok = &raw[i];
        let Some(tok_str) = tok.to_str() else {
            if deferred_pos.is_empty() {
                before_dash.push(tok.clone());
            } else {
                deferred_pos.push(tok.clone());
            }
            pos_index += 1;
            i += 1;
            continue;
        };

        if tok_str == "--" {
            if deferred_pos.is_empty() {
                before_dash.extend_from_slice(&raw[i..]);
            } else {
                deferred_pos.extend_from_slice(&raw[i + 1..]);
            }
            break;
        }

        if let Some(long_body) = tok_str.strip_prefix("--") {
            if let Some((long_name, _)) = long_body.split_once('=') {
                if find_long_arg(subcmd, long_name).is_some() || looks_like_long_flag(long_body) {
                    before_dash.push(tok.clone());
                    i += 1;
                    continue;
                }
            } else if let Some(arg) = find_long_arg(subcmd, long_body) {
                if arg.get_action().takes_values() {
                    if let Some(next) = raw.get(i + 1) {
                        match next.to_str() {
                            Some(next_str)
                                if is_free_text_option(sub_name, arg.get_id().as_str())
                                    && is_hyphen_text_literal(subcmd, next_str) =>
                            {
                                let mut combined = std::ffi::OsString::from(tok_str);
                                combined.push("=");
                                combined.push(next);
                                before_dash.push(combined);
                                i += 2;
                                continue;
                            }
                            Some(next_str) if next_str != "-" && next_str.starts_with('-') => {
                                before_dash.push(tok.clone());
                                i += 1;
                                continue;
                            }
                            _ => {
                                before_dash.push(tok.clone());
                                before_dash.push(next.clone());
                                i += 2;
                                continue;
                            }
                        }
                    }
                }
                before_dash.push(tok.clone());
                i += 1;
                continue;
            }
            if is_free_text_positional(sub_name, pos_index)
                && is_hyphen_text_literal(subcmd, tok_str)
            {
                deferred_pos.push(tok.clone());
                pos_index += 1;
            } else {
                before_dash.push(tok.clone());
            }
            i += 1;
            continue;
        }

        if tok_str.len() > 1 && tok_str.starts_with('-') {
            if is_known_flag_token(subcmd, tok_str) {
                before_dash.push(tok.clone());
                i += 1;
                continue;
            }
            if is_free_text_positional(sub_name, pos_index)
                && is_hyphen_text_literal(subcmd, tok_str)
            {
                deferred_pos.push(tok.clone());
                pos_index += 1;
            } else {
                before_dash.push(tok.clone());
            }
            i += 1;
            continue;
        }

        if deferred_pos.is_empty() {
            before_dash.push(tok.clone());
        } else {
            deferred_pos.push(tok.clone());
        }
        pos_index += 1;
        i += 1;
    }

    if !deferred_pos.is_empty() {
        before_dash.push(std::ffi::OsString::from("--"));
        before_dash.extend(deferred_pos);
    }
    before_dash
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

impl Command {
    fn wants_json(&self) -> bool {
        match self {
            Command::Login { json, .. }
            | Command::Logout { json }
            | Command::Whoami { json }
            | Command::Doctor { json, .. }
            | Command::Notebooks { json, .. }
            | Command::CreateNotebook { json, .. }
            | Command::RenameNotebook { json, .. }
            | Command::Sections { json, .. }
            | Command::CreateSection { json, .. }
            | Command::RenameSection { json, .. }
            | Command::Pages { json, .. }
            | Command::Read { json, .. }
            | Command::Create { json, .. }
            | Command::CopyPage { json, .. }
            | Command::Append { json, .. }
            | Command::Update { json, .. }
            | Command::SelfUpdate { json, .. }
            | Command::Check { json, .. }
            | Command::Delete { json, .. }
            | Command::Search { json, .. }
            | Command::Resource { json, .. }
            | Command::Sync { json } => *json,
            #[cfg(windows)]
            Command::DesktopSetPageLevel { json, .. }
            | Command::DesktopRenameSection { json, .. } => *json,
            Command::Daemon { action } => match action {
                DaemonCommand::Start { json }
                | DaemonCommand::Status { json }
                | DaemonCommand::Stop { json }
                | DaemonCommand::Restart { json } => *json,
                DaemonCommand::Run => false,
            },
            Command::PreviewFile { .. } | Command::Tui | Command::Serve { .. } | Command::Mcp => {
                false
            }
        }
    }
}

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
                println!("Notebook storage item '{}' is named '{}'.", item.item_id, item.name);
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

// ---------------------------------------------------------------------------
// Shared Runtime & Domain Helpers (CLI / HTTP / MCP)
// ---------------------------------------------------------------------------

const CURSOR_NOTEBOOKS_PERSONAL: &str = "onenote:notebooks:personal";
const CURSOR_NOTEBOOKS_ALL: &str = "onenote:notebooks:all";
const CURSOR_SECTIONS_ALL: &str = "onenote:sections:all";
const CURSOR_HIERARCHY: &str = "onenote:hierarchy";

const UNKNOWN_IDENTITY_MSG: &str = "Unknown identity: the current token carries no user ID and no identity is recorded for it, so the local cache cannot be selected. Run `onenote whoami` once while online (MCP: call the whoami tool), then retry.";

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentityMode {
    LocalOnly,
    AllowNetwork,
}

#[derive(Clone, Copy, PartialEq, Eq)]
pub(crate) enum IdentitySource {
    NoToken,
    Token,
    IdentityMap,
    GraphMe,
}

/// Selects the cache bucket for the current token: token claim, then the local
/// identity map, then `GET /me`. Shared by the CLI/TUI/MCP runtime and the daemon
/// so they all open the same database file.
pub(crate) async fn resolve_cache_user_id(
    auth: &AuthManager,
    graph: &GraphClient,
    mode: IdentityMode,
) -> Result<(Option<String>, IdentitySource)> {
    let Some(tok) = auth.current_tokens().await else {
        return Ok((None, IdentitySource::NoToken));
    };
    if let Some(id) = tok
        .user_id
        .as_deref()
        .map(str::trim)
        .filter(|id| !id.is_empty())
    {
        return Ok((Some(id.to_string()), IdentitySource::Token));
    }
    if let Some(id) = config::lookup_identity(&tok.access_token) {
        return Ok((Some(id), IdentitySource::IdentityMap));
    }
    if mode == IdentityMode::LocalOnly {
        bail!("{UNKNOWN_IDENTITY_MSG}");
    }
    let id = graph
        .require_my_user_id()
        .await
        .context(
            "Cannot select a cache for the current token: GET /me failed, so the signed-in account is unknown.",
        )?;
    let trimmed = id.trim();
    if trimmed.is_empty() {
        bail!(
            "Cannot select a cache for the current token: GET /me failed, so the signed-in account is unknown."
        );
    }
    let _ = config::record_identity(&tok.access_token, trimmed);
    Ok((Some(trimmed.to_string()), IdentitySource::GraphMe))
}

pub async fn open_runtime(
    cfg: &AppConfig,
    require_auth: bool,
) -> Result<(AuthManager, GraphClient, Storage)> {
    open_runtime_inner(cfg, require_auth, IdentityMode::AllowNetwork).await
}

pub(crate) async fn open_mcp_runtime(
    cfg: &AppConfig,
    mode: IdentityMode,
) -> Result<(GraphClient, Storage)> {
    let (_auth, graph, storage) = open_runtime_inner(cfg, false, mode).await?;
    Ok((graph, storage))
}

async fn open_runtime_inner(
    cfg: &AppConfig,
    require_auth: bool,
    mode: IdentityMode,
) -> Result<(AuthManager, GraphClient, Storage)> {
    let auth = AuthManager::new(cfg.clone())?;
    if require_auth {
        let _ = auth.valid_access_token().await?;
    }
    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let (user_id, _source) = resolve_cache_user_id(&auth, &graph, mode).await?;
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

pub(crate) fn latest_sync_at(storage: &Storage, scope_keys: &[&str]) -> Option<String> {
    let mut best: Option<(chrono::DateTime<chrono::FixedOffset>, String)> = None;
    for &key in scope_keys {
        let Ok(Some(cursor)) = storage.get_sync_cursor(key) else {
            continue;
        };
        let Ok(ts) = chrono::DateTime::parse_from_rfc3339(&cursor.last_synced_at) else {
            continue;
        };
        if best.as_ref().is_none_or(|(cur, _)| ts > *cur) {
            best = Some((ts, cursor.last_synced_at));
        }
    }
    best.map(|(_, raw)| raw)
}

pub(crate) fn notebooks_cache_synced_at(storage: &Storage) -> Option<String> {
    latest_sync_at(
        storage,
        &[
            CURSOR_NOTEBOOKS_PERSONAL,
            CURSOR_NOTEBOOKS_ALL,
            CURSOR_HIERARCHY,
        ],
    )
}

pub(crate) fn sections_cache_synced_at(
    storage: &Storage,
    notebook: Option<&OnenoteNotebook>,
) -> Option<String> {
    if let Some(nb) = notebook {
        let nb_cursor = format!("onenote:sections:{}", nb.notebook_key);
        latest_sync_at(
            storage,
            &[nb_cursor.as_str(), CURSOR_SECTIONS_ALL, CURSOR_HIERARCHY],
        )
    } else {
        latest_sync_at(storage, &[CURSOR_SECTIONS_ALL, CURSOR_HIERARCHY])
    }
}

pub(crate) fn pages_cache_synced_at(storage: &Storage, section_key: &str) -> Option<String> {
    let page_cursor = format!("onenote:pages:{section_key}");
    latest_sync_at(storage, &[page_cursor.as_str()])
}

fn record_sync_cursor(storage: &Storage, scope_key: impl Into<String>, delta_link: Option<String>) {
    let _ = storage.upsert_sync_cursor(&crate::models::SyncCursor {
        scope_key: scope_key.into(),
        delta_link,
        history_next_link: None,
        last_synced_at: chrono::Utc::now().to_rfc3339(),
    });
}

pub(crate) fn page_slice<T>(items: Vec<T>, offset: usize, limit: usize) -> (Vec<T>, bool) {
    let has_more = items.len() > offset.saturating_add(limit);
    (items.into_iter().skip(offset).take(limit).collect(), has_more)
}

pub(crate) fn next_cursor(offset: usize, limit: usize, has_more: Option<bool>) -> Option<String> {
    (has_more == Some(true)).then(|| offset.saturating_add(limit).to_string())
}

pub(crate) fn parse_mcp_cursor(args: &serde_json::Value) -> Result<usize> {
    match args.get("cursor") {
        None | Some(serde_json::Value::Null) => Ok(0),
        Some(serde_json::Value::String(s)) => s.trim().parse::<usize>().map_err(|_| {
            anyhow::anyhow!("Invalid cursor: pass the next_cursor value from the previous response.")
        }),
        Some(serde_json::Value::Number(n)) => n
            .as_u64()
            .and_then(|v| usize::try_from(v).ok())
            .ok_or_else(|| {
                anyhow::anyhow!(
                    "Invalid cursor: pass the next_cursor value from the previous response."
                )
            }),
        Some(_) => bail!("Invalid cursor: pass the next_cursor value from the previous response."),
    }
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
    Ok(
        load_onenote_notebooks_with_warnings(storage, graph, include_teams, refresh, offline, cached)
            .await?
            .0,
    )
}

/// Like [`load_onenote_notebooks_with_cache`], plus warnings about notebooks
/// that may be missing (Teams skipped for this sign-in, Teams cap reached,
/// Teams listing failed).
pub async fn load_onenote_notebooks_with_warnings(
    storage: &Storage,
    graph: &GraphClient,
    include_teams: bool,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<(Vec<OnenoteNotebook>, Vec<String>)> {
    let mut warnings = Vec::new();
    let notebooks = load_onenote_notebooks_inner(
        storage,
        graph,
        include_teams,
        refresh,
        offline,
        cached,
        &mut warnings,
    )
    .await?;
    // A cache hit makes no Graph call, so repeat the sign-in hint here: it is
    // derived from the token alone and explains why Teams notebooks are absent.
    if include_teams && !offline {
        let tokens = graph.auth().current_tokens().await;
        if let Some(hint) = crate::graph::onenote::teams_skipped_warning(tokens.as_ref()) {
            if !warnings.iter().any(|w| w == hint) {
                warnings.push(hint.to_string());
            }
        }
    }
    Ok((notebooks, warnings))
}

async fn load_onenote_notebooks_inner(
    storage: &Storage,
    graph: &GraphClient,
    include_teams: bool,
    refresh: bool,
    offline: bool,
    cached: bool,
    warnings: &mut Vec<String>,
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
    warnings.extend(nb_res.warnings);
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
    let (nb, sections, _warnings) =
        load_onenote_sections_with_warnings(storage, graph, notebook_query, refresh, offline, cached)
            .await?;
    Ok((nb, sections))
}

/// Like [`load_onenote_sections_with_cache`], plus warnings about sections
/// that could not be fetched. After a partial fetch nothing is pruned from the
/// cache, so the returned list still contains the previously cached sections.
pub async fn load_onenote_sections_with_warnings(
    storage: &Storage,
    graph: &GraphClient,
    notebook_query: Option<&str>,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<(Option<OnenoteNotebook>, Vec<OnenoteSection>, Vec<String>)> {
    let mut warnings = Vec::new();
    let (nb, sections) = load_onenote_sections_inner(
        storage,
        graph,
        notebook_query,
        refresh,
        offline,
        cached,
        &mut warnings,
    )
    .await?;
    Ok((nb, sections, warnings))
}

async fn load_onenote_sections_inner(
    storage: &Storage,
    graph: &GraphClient,
    notebook_query: Option<&str>,
    refresh: bool,
    offline: bool,
    cached: bool,
    warnings: &mut Vec<String>,
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
        let (fetched, complete) = graph
            .list_onenote_sections_scoped(&target_nb.scope, Some(&target_nb.id))
            .await?;
        // Prune cached sections only after a complete fetch: a section group
        // that failed to load must not delete its cached sections and pages.
        storage.reconcile_onenote_notebook_sections(&target_nb.notebook_key, &fetched, complete)?;
        if !complete {
            warnings.push(format!(
                "Some sections of '{}' could not be fetched; cached sections were kept. Run the command again with --refresh.",
                target_nb.display_name
            ));
        }
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
            warnings.extend(nb_res.warnings);
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
    let (sec, pages, _has_more) = list_onenote_pages_with_cache(
        storage,
        graph,
        section_query,
        limit,
        0,
        refresh,
        offline,
        false,
    )
    .await?;
    Ok((sec, pages))
}

#[allow(clippy::too_many_arguments)]
pub async fn list_onenote_pages_with_cache(
    storage: &Storage,
    graph: &GraphClient,
    section_query: &str,
    limit: usize,
    offset: usize,
    refresh: bool,
    offline: bool,
    cached: bool,
) -> Result<(OnenoteSection, Vec<OnenotePageSummary>, Option<bool>)> {
    let mut sec = resolve_onenote_section_query_with_refresh(
        storage,
        graph,
        section_query,
        offline,
        refresh,
    )
    .await?;
    if limit == 0 {
        return Ok((sec, Vec::new(), None));
    }
    if offline {
        let all = storage.list_onenote_pages(&sec.section_key)?;
        let (sliced, has_more) = page_slice(all, offset, limit);
        return Ok((sec, sliced, Some(has_more)));
    }

    let mut cursor_key = format!("onenote:pages:{}", sec.section_key);
    if !refresh {
        let cached_pages = storage.list_onenote_pages(&sec.section_key)?;
        let ttl = graph.config().cli_cache_ttl_secs;
        if !cached_pages.is_empty() && (cached || is_cursor_fresh(storage, &cursor_key, ttl)) {
            let (sliced, has_more) = page_slice(cached_pages, offset, limit);
            return Ok((sec, sliced, Some(has_more)));
        }
    }

    let end = offset.saturating_add(limit);
    let want = end.saturating_add(1);
    let fetched = match graph.list_onenote_pages(&sec.scope, &sec.id, want).await {
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
            graph.list_onenote_pages(&sec.scope, &sec.id, want).await?
        }
        Err(err) => return Err(err),
    };
    let has_more = fetched.len() > end;
    storage.reconcile_onenote_section_pages(&sec.section_key, &fetched, !has_more)?;
    record_sync_cursor(storage, cursor_key, sec.last_modified_at.clone());
    let sliced = fetched.into_iter().skip(offset).take(limit).collect();
    Ok((sec, sliced, Some(has_more)))
}

pub async fn load_onenote_page_detail(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
    offline: bool,
) -> Result<OnenotePageDetail> {
    load_onenote_page_detail_with_cache(storage, graph, page_query, offline, false).await
}

async fn fetch_onenote_page_detail_remote(
    storage: &Storage,
    graph: &GraphClient,
    page_query: &str,
) -> Result<OnenotePageDetail> {
    let q = page_query.trim();
    if q.is_empty() {
        bail!("Page ID or title is empty.");
    }
    let cached_summary = resolver::resolve_page_cached(storage, q, None)?.into_option()?;
    let was_cached = cached_summary.is_some();
    let (scope, raw_id) = match cached_summary.as_ref() {
        Some(s) => (s.scope.clone(), s.id.clone()),
        None => split_onenote_entity_key(q),
    };
    let resolved_page_key = cached_summary
        .as_ref()
        .map(|s| s.page_key.clone())
        .unwrap_or_else(|| scope.make_entity_key(&raw_id));

    match graph
        .get_onenote_page_detail(&scope, &raw_id, cached_summary)
        .await
    {
        Ok(d) => Ok(d),
        Err(err) => {
            if crate::graph::onenote::is_not_found_error(&err) {
                let _ = storage.delete_onenote_page(&resolved_page_key);
                if !was_cached {
                    return Err(err.context(format!(
                        "OneNote page not found: '{q}' is neither a cached title nor a page ID. Pass a page key from 'onenote pages SECTION --json', or add --section to look the title up in one section."
                    )));
                }
            }
            Err(err)
        }
    }
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
        let cached_summary = resolver::resolve_page_cached(storage, q, None)?.into_option()?;
        if let Some(ref summary) = cached_summary {
            if let Some(detail) = storage.get_onenote_page_detail(&summary.page_key)? {
                return Ok(detail);
            }
        }
        if offline {
            bail!("OneNote page not found in offline cache: {q}");
        }
    }

    let detail = fetch_onenote_page_detail_remote(storage, graph, q).await?;
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
    if let Some(t) = tok
        .as_ref()
        .filter(|t| t.user_id.as_deref().is_none_or(|u| u.trim().is_empty()))
    {
        config::record_identity(&t.access_token, &me.id)
            .context("Signed in, but the identity binding for this token could not be saved")?;
    }
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
    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let (db_file, cache_identity) =
        match resolve_cache_user_id(&auth, &graph, IdentityMode::LocalOnly).await {
            Ok((user_id, source)) => {
                let label = match source {
                    IdentitySource::NoToken => "no_token",
                    IdentitySource::Token => "token",
                    IdentitySource::IdentityMap => "identity_map",
                    IdentitySource::GraphMe => "graph_me",
                };
                (Some(config::db_path(user_id.as_deref())), label)
            }
            Err(_) => (None, "unresolved"),
        };
    let storage = db_file.as_ref().and_then(|p| Storage::open(p).ok());
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
    let mut diag = tok
        .as_ref()
        .map(|t| diagnose_access_token_scopes(&t.access_token))
        .unwrap_or_default();
    if tok.is_some() {
        if let Some(hint) = crate::graph::onenote::teams_skipped_warning(tok.as_ref()) {
            diag.warnings.push(hint.to_string());
        }
    }
    let img_caps = img_preview::probe_terminal_graphics_caps(cfg.image_protocol);
    let daemon_st = daemon::query_daemon_status(cfg).await.unwrap_or_default();

    let mut online_ok: Option<bool> = None;
    let mut online_error: Option<String> = None;
    let mut online_hint: Option<&'static str> = None;
    if online {
        match graph.list_onenote_notebooks(&OnenoteScope::Me).await {
            Ok(_) => online_ok = Some(true),
            Err(e) => {
                online_ok = Some(false);
                online_hint = crate::http::tls_hint_for(&e);
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
                "cache_identity": cache_identity,
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
                "online_probe_hint": online_hint,
            }))?
        );
    } else {
        println!("onenote-cli v{}", env!("CARGO_PKG_VERSION"));
        println!("Config:         {}", config::config_file_path().display());
        match db_file.as_ref() {
            Some(path) => println!("Database:       {}", path.display()),
            None => println!("Database:       (unresolved)"),
        }
        println!("Cache identity: {cache_identity}");
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
                if let Some(hint) = online_hint {
                    if !online_error.as_deref().is_some_and(|e| e.contains(hint)) {
                        eprintln!("hint: {hint}");
                    }
                }
            }
        }
        for w in &diag.warnings {
            eprintln!("Warning: {w}");
        }
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_notebooks(
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
            println!("{}  ({owner}){def}  [{}]", nb.display_name, nb.notebook_key);
        }
        if let Some(next) = next {
            eprintln!("More results: rerun with --cursor {next}");
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

#[allow(clippy::too_many_arguments)]
async fn cmd_sections(
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
            println!("{nb_label} › {}  [{}]", sec.display_name, sec.section_key);
        }
        if let Some(next) = next {
            eprintln!("More results: rerun with --cursor {next}");
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

#[allow(clippy::too_many_arguments)]
async fn cmd_pages(
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
        println!("No pages in section '{}'.", sec.display_name);
    } else {
        for p in &pages {
            let indent = "  ".repeat(p.level.clamp(0, 2) as usize);
            let todo_mark = if p.has_todo { " [todo]" } else { "" };
            println!("{indent}{}{}  [{}]", p.title, todo_mark, p.page_key);
        }
        if let Some(next) = next {
            eprintln!("More pages: rerun with --cursor {next}");
        }
    }
    Ok(())
}

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

fn read_json_is_compact(format_explicit: bool, compact: bool, full: bool) -> bool {
    if full {
        false
    } else if compact {
        true
    } else {
        !format_explicit
    }
}

#[allow(clippy::too_many_arguments)]
async fn cmd_read(
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

#[allow(clippy::too_many_arguments)]
async fn cmd_append(
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
        println!("Appended content to page '{}' ({}).", updated.summary.title, updated.summary.page_key);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_update_page(
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
        println!("Updated page '{}' ({}).", updated.summary.title, updated.summary.page_key);
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_check(
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
        println!("Toggled to-do {target_id} -> {state}");
    }
    Ok(())
}

/// Confirmation question for `onenote delete`, naming the page that was resolved.
fn delete_prompt_text(resolved_query: &str, cached: Option<&OnenotePageSummary>) -> String {
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
                "Delete this OneNote page?\n  Title:         {}\n  Location:      {notebook} / {section}\n  Last modified: {modified}\n  Page key:      {}\n[y/N]: ",
                page.title, page.page_key
            )
        }
        None => format!(
            "'{resolved_query}' is not in the local cache, so its title and location cannot be shown.\nIt will be sent to Microsoft Graph as a raw page ID.\nDelete OneNote page ID '{resolved_query}'? [y/N]: "
        ),
    }
}

async fn cmd_delete(
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
        println!("Deleted OneNote page: {deleted_key}");
    }
    Ok(())
}

#[allow(clippy::too_many_arguments)]
async fn cmd_search(
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
            Command::Serve { port: 8793, .. }
        ));

        let cli = Cli::try_parse_from(["onenote", "serve", "--port", "8788"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Serve { port: 8788, .. }
        ));

        let cli = Cli::try_parse_from([
            "onenote",
            "serve",
            "--allow-keyless",
            "--allowed-host",
            "notes.corp.example",
            "--allowed-host",
            "10.0.0.5",
            "--cors-origin",
            "https://app.example.com",
        ])
        .unwrap();
        match cli.command.unwrap() {
            Command::Serve {
                api_key,
                allow_keyless,
                allowed_hosts,
                cors_origins,
                ..
            } => {
                assert_eq!(api_key, None);
                assert!(allow_keyless);
                assert_eq!(allowed_hosts, vec!["notes.corp.example", "10.0.0.5"]);
                assert_eq!(cors_origins, vec!["https://app.example.com"]);
            }
            other => panic!("unexpected command: {other:?}"),
        }
        assert!(
            Cli::try_parse_from(["onenote", "serve", "--token", "k", "--allow-keyless"]).is_err()
        );

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

    #[test]
    fn parses_hyphen_values_across_free_text_commands() {
        const ITEM: &str = "- 목록 항목";
        const TWO_LINE_LIST: &str = "- first\n- second";
        const KO_TWO_LINE: &str = "- 첫째 항목\n- 둘째 항목";

        // 1. create positional body + --title (Korean, two-line list, trailing --json)
        let cli = Cli::try_parse_from(["onenote", "create", "sec", "--title", "Test", ITEM])
            .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Create {
                ref section,
                ref title,
                markdown_body: Some(ref body),
                json: false,
                ..
            } if section == "sec" && title == "Test" && body == ITEM
        ));

        let cli = Cli::try_parse_from([
            "onenote",
            "create",
            "sec",
            "--title",
            ITEM,
            TWO_LINE_LIST,
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Create {
                ref section,
                ref title,
                markdown_body: Some(ref body),
                json: true,
                ..
            } if section == "sec" && title == ITEM && body == TWO_LINE_LIST
        ));

        // 2. append positional body (Korean, two-line list, trailing --json, positional --)
        let cli = Cli::try_parse_from(["onenote", "append", "page", ITEM, "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Append {
                ref page,
                markdown_body: Some(ref body),
                json: true,
                ..
            } if page == "page" && body == ITEM
        ));

        let cli = Cli::try_parse_from(["onenote", "append", "page", KO_TWO_LINE]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Append {
                markdown_body: Some(ref body),
                ..
            } if body == KO_TWO_LINE
        ));

        let cli = Cli::try_parse_from(["onenote", "append", "page", "---"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Append {
                markdown_body: Some(ref body),
                ..
            } if body == "---"
        ));

        let cli = Cli::try_parse_from(["onenote", "append", "page", "--", "--dry-run"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Append {
                markdown_body: Some(ref body),
                ..
            } if body == "--dry-run"
        ));

        let cli = Cli::try_parse_from(["onenote", "append", "page", "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Append {
                markdown_body: None,
                json: true,
                ..
            }
        ));

        // 3. update --body, --expected-content (--base-content), --replace-text, --with-text, --title
        let cli = Cli::try_parse_from([
            "onenote",
            "update",
            "page",
            "--title",
            ITEM,
            "--body",
            TWO_LINE_LIST,
            "--expected-content",
            ITEM,
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Update {
                page: Some(ref page),
                title: Some(ref title),
                body: Some(ref body),
                expected_content: Some(ref exp),
                json: true,
                ..
            } if page == "page" && title == ITEM && body == TWO_LINE_LIST && exp == ITEM
        ));

        let cli = Cli::try_parse_from([
            "onenote",
            "update",
            "page",
            "--block-id",
            "block",
            "--replace-text",
            "old",
            "--with-text",
            ITEM,
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Update {
                replace_text: Some(ref from),
                with_text: Some(ref to),
                json: true,
                ..
            } if from == "old" && to == ITEM
        ));

        let cli = Cli::try_parse_from([
            "onenote",
            "update",
            "page",
            "--block-id",
            "block",
            "--replace-text",
            ITEM,
            "--with-text",
            "new",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Update {
                replace_text: Some(ref from),
                with_text: Some(ref to),
                ..
            } if from == ITEM && to == "new"
        ));

        let cli = Cli::try_parse_from([
            "onenote",
            "update",
            "page",
            "--body",
            "Text",
            "--base-content",
            ITEM,
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Update {
                expected_content: Some(ref exp),
                ..
            } if exp == ITEM
        ));

        // 4. check --expected-content
        let cli = Cli::try_parse_from([
            "onenote",
            "check",
            "page",
            "--expected-content",
            "- [ ] task",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Check {
                expected_content: Some(ref exp),
                json: true,
                ..
            } if exp == "- [ ] task"
        ));

        // 5. section & notebook names + search
        let cli = Cli::try_parse_from([
            "onenote",
            "rename-section",
            "sec",
            "--name",
            ITEM,
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::RenameSection {
                ref section,
                ref name,
                json: true,
            } if section == "sec" && name == ITEM
        ));

        let cli =
            Cli::try_parse_from(["onenote", "create-section", "nb", ITEM, "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::CreateSection {
                ref notebook,
                ref name,
                json: true,
            } if notebook == "nb" && name == ITEM
        ));

        let cli = Cli::try_parse_from(["onenote", "create-notebook", ITEM, "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::CreateNotebook {
                ref name,
                json: true,
                ..
            } if name == ITEM
        ));

        let cli = Cli::try_parse_from([
            "onenote",
            "rename-notebook",
            "--drive-id",
            "d1",
            "--item-id",
            "i1",
            "--name",
            ITEM,
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::RenameNotebook {
                ref name,
                json: true,
                ..
            } if name == ITEM
        ));

        let cli = Cli::try_parse_from(["onenote", "search", ITEM, "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Search {
                ref query,
                json: true,
                ..
            } if query == ITEM
        ));

        // 6. Explicit `--flag=value` accepts flag-looking text literally
        let cli = Cli::try_parse_from([
            "onenote",
            "update",
            "page",
            "--body=--dry-run",
            "--title=--help",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Update {
                title: Some(ref title),
                body: Some(ref body),
                json: true,
                ..
            } if title == "--help" && body == "--dry-run"
        ));

        let cli = Cli::try_parse_from([
            "onenote",
            "rename-section",
            "sec",
            "--name=--json",
            "--json",
        ])
        .unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::RenameSection {
                ref name,
                json: true,
                ..
            } if name == "--json"
        ));
    }

    #[test]
    fn preserves_known_flags_help_and_missing_value_errors() {
        use clap::error::ErrorKind;

        const ITEM: &str = "- 목록 항목";

        // Actual help invocations (standalone and trailing after completed hyphen values) return DisplayHelp
        for argv in [
            vec!["onenote", "create", "--help"],
            vec!["onenote", "create", "sec", "--title", "Test", ITEM, "--help"],
            vec!["onenote", "create", "sec", "--title", ITEM, "--help"],
            vec!["onenote", "append", "--help"],
            vec!["onenote", "append", "page", ITEM, "--help"],
            vec!["onenote", "update", "--help"],
            vec!["onenote", "update", "page", "--body", ITEM, "--help"],
            vec![
                "onenote",
                "update",
                "page",
                "--block-id",
                "blk",
                "--replace-text",
                "old",
                "--with-text",
                ITEM,
                "--help",
            ],
            vec![
                "onenote",
                "update",
                "page",
                "--block-id",
                "blk",
                "--replace-text",
                ITEM,
                "--with-text",
                "new",
                "--help",
            ],
            vec![
                "onenote",
                "update",
                "page",
                "--body",
                "Text",
                "--expected-content",
                ITEM,
                "--help",
            ],
            vec!["onenote", "rename-section", "sec", "--name", ITEM, "--help"],
            vec!["onenote", "search", ITEM, "--help"],
            vec!["onenote", "search", "--help"],
            vec!["onenote", "search", "-h"],
        ] {
            let err = Cli::try_parse_from(argv.clone()).unwrap_err();
            assert_eq!(
                err.kind(),
                ErrorKind::DisplayHelp,
                "expected DisplayHelp for {argv:?}, got: {err}"
            );
        }

        // Missing option/positional values followed by known or flag-looking tokens must fail (never swallow flags)
        for argv in [
            vec!["onenote", "create", "sec", "--title", "--json"],
            vec!["onenote", "create", "sec", "--title", "--dry-run"],
            vec!["onenote", "create", "sec", "--title", "--help"],
            vec!["onenote", "create", "sec", "--title", "-h"],
            vec!["onenote", "append", "page", "--dry-run"],
            vec!["onenote", "update", "page", "--body", "--json"],
            vec!["onenote", "update", "page", "--body", "--dry-run"],
            vec!["onenote", "update", "page", "--body", "--help"],
            vec!["onenote", "update", "page", "--body", "-h"],
            vec!["onenote", "update", "page", "--body", "--"],
            vec![
                "onenote",
                "update",
                "page",
                "--block-id",
                "blk",
                "--replace-text",
                "--with-text",
                "new",
            ],
            vec![
                "onenote",
                "update",
                "page",
                "--block-id",
                "blk",
                "--replace-text",
                "old",
                "--with-text",
                "--json",
            ],
            vec![
                "onenote",
                "update",
                "page",
                "--body",
                "Text",
                "--expected-content",
                "--json",
            ],
            vec!["onenote", "check", "page", "--expected-content", "--json"],
            vec!["onenote", "rename-section", "sec", "--name", "--json"],
            vec!["onenote", "search", "--json"],
            vec!["onenote", "search", "--dry-run"],
            // Non-free-text fields keep their existing strict parsers
            vec!["onenote", "create", "sec", "--title", "Test", "--level", "-1"],
            vec!["onenote", "read", "page", "--format", "-bad"],
            vec!["onenote", "update", "page", "--block-id", "-bad", "--body", "Text"],
        ] {
            let err = Cli::try_parse_from(argv.clone()).unwrap_err();
            assert_ne!(
                err.kind(),
                ErrorKind::DisplayHelp,
                "expected parse error (not help) for {argv:?}"
            );
        }
    }

    #[test]
    fn skips_second_clap_command_tree_when_argv_has_no_hyphen_text_candidates() {
        let to_os = |args: &[&str]| -> Vec<std::ffi::OsString> {
            args.iter().map(std::ffi::OsString::from).collect()
        };

        for ordinary in [
            &["onenote"][..],
            &["onenote", "notebooks", "--include-teams", "--json"][..],
            &["onenote", "create", "sec", "--title", "RFC-01", "# Heading", "--json"][..],
            &["onenote", "create", "sec", "--title=- 목록 항목", "--json"][..],
            &["onenote", "append", "page", "Plain body", "--json"][..],
            &["onenote", "append", "page", "--", "- 목록 항목"][..],
            &["onenote", "update", "page", "--body", "Updated", "--json"][..],
            &["onenote", "update", "page", "--body", "--json"][..],
            &["onenote", "update", "page", "--body", "--dry-run"][..],
            &["onenote", "search", "architecture", "--help"][..],
        ] {
            assert!(
                !argv_needs_hyphen_normalization(&to_os(ordinary)),
                "should skip normalization for ordinary argv: {ordinary:?}"
            );
        }

        for hyphen_text in [
            &["onenote", "create", "sec", "--title", "- 목록 항목"][..],
            &["onenote", "append", "page", "- first\n- second", "--json"][..],
            &["onenote", "append", "page", "---"][..],
            &["onenote", "update", "page", "--body", "- 목록 항목"][..],
            &["onenote", "rename-section", "sec", "--name", "- 목록 항목"][..],
            &["onenote", "search", "- 목록 항목", "--json"][..],
        ] {
            assert!(
                argv_needs_hyphen_normalization(&to_os(hyphen_text)),
                "should normalize hyphen text argv: {hyphen_text:?}"
            );
        }
    }

    #[test]
    fn read_json_parse_forms() {
        let cli = Cli::try_parse_from(["onenote", "read", "p", "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Read {
                format: None,
                full: false,
                compact: false,
                json: true,
                ..
            }
        ));

        let cli =
            Cli::try_parse_from(["onenote", "read", "p", "--format", "md", "--json"]).unwrap();
        assert!(matches!(
            cli.command.unwrap(),
            Command::Read {
                format: Some(ref f),
                full: false,
                compact: false,
                json: true,
                ..
            } if f == "md"
        ));
    }

    #[test]
    fn read_json_shape_matrix() {
        assert!(read_json_is_compact(false, false, false));
        assert!(read_json_is_compact(false, true, false));
        assert!(!read_json_is_compact(false, false, true));
        assert!(!read_json_is_compact(true, false, false));
        assert!(read_json_is_compact(true, true, false));
        assert!(!read_json_is_compact(true, false, true));
    }

    #[test]
    fn read_full_conflicts_with_compact() {
        use clap::error::ErrorKind;
        let err = Cli::try_parse_from([
            "onenote", "read", "p", "--json", "--compact", "--full",
        ])
        .unwrap_err();
        assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
    }

    #[test]
    fn read_selectors_require_json() {
        use clap::error::ErrorKind;
        let err_full = Cli::try_parse_from(["onenote", "read", "p", "--full"]).unwrap_err();
        assert_eq!(err_full.kind(), ErrorKind::MissingRequiredArgument);

        let err_compact = Cli::try_parse_from(["onenote", "read", "p", "--compact"]).unwrap_err();
        assert_eq!(err_compact.kind(), ErrorKind::MissingRequiredArgument);
    }

    #[test]
    fn read_format_with_json_parses() {
        assert!(
            Cli::try_parse_from(["onenote", "read", "p", "--format", "xhtml", "--json"]).is_ok()
        );
        assert!(Cli::try_parse_from([
            "onenote", "read", "p", "--format", "xhtml", "--json", "--compact",
        ])
        .is_ok());
        assert!(Cli::try_parse_from(["onenote", "read", "p", "--format", "bogus"]).is_err());
    }

    #[test]
    fn list_limit_bounds() {
        assert!(Cli::try_parse_from(["onenote", "sections", "--limit", "0"]).is_err());
        assert!(Cli::try_parse_from(["onenote", "sections", "--limit", "1001"]).is_err());
        assert!(
            Cli::try_parse_from(["onenote", "sections", "--limit", "1000", "--cursor", "5"])
                .is_ok()
        );
        assert!(Cli::try_parse_from(["onenote", "sections", "--cursor", "x"]).is_err());
    }

    #[test]
    fn page_slice_boundaries() {
        let base = vec![1, 2, 3, 4, 5];
        let (s, m) = page_slice(base.clone(), 0, 5);
        assert_eq!((s.len(), m), (5, false));
        let (s, m) = page_slice(base.clone(), 0, 4);
        assert_eq!((s.len(), m), (4, true));
        let (s, m) = page_slice(base.clone(), 4, 1);
        assert_eq!((s.len(), m), (1, false));
        let (s, m) = page_slice(base.clone(), 5, 1);
        assert_eq!((s.len(), m), (0, false));
        let (s, m) = page_slice(base, 9, 2);
        assert_eq!((s.len(), m), (0, false));
    }

    #[test]
    fn compact_page_json_keys() {
        use crate::models::{OnenoteBlockKind, RichSpan};

        let make_block = |id: &str| OnenoteBlock {
            element_id: Some(id.to_string()),
            tag: None,
            raw_html: Some(format!("<p id=\"{id}\">text</p>")),
            kind: OnenoteBlockKind::Paragraph {
                spans: vec![RichSpan::plain("text")],
            },
        };
        let detail = OnenotePageDetail {
            summary: OnenotePageSummary {
                page_key: "group:G1|gpage-1".to_string(),
                id: "gpage-1".to_string(),
                scope: OnenoteScope::Group {
                    group_id: "G1".to_string(),
                    group_name: None,
                },
                section_id: "gsec-1".to_string(),
                section_name: Some("Specs".to_string()),
                notebook_id: Some("gnb-1".to_string()),
                notebook_name: Some("Engineering".to_string()),
                title: "Architecture Spec".to_string(),
                level: 0,
                order: 1,
                created_at: Some("2026-11-01T00:00:00Z".to_string()),
                last_modified_at: Some("2026-11-02T00:00:00Z".to_string()),
                content_url: Some("https://example.invalid/content".to_string()),
                web_url: Some("https://example.invalid/gpage-1".to_string()),
                preview_text: Some("preview".to_string()),
                has_todo: true,
            },
            raw_xhtml: "<html><body><p>text</p></body></html>".to_string(),
            body_markdown: "# Architecture Spec".to_string(),
            body_plain: "Architecture Spec".to_string(),
            blocks: vec![make_block("p:1"), make_block("p:2"), make_block("p:3")],
            resources: vec![OnenoteResource {
                element_id: Some("img:1".to_string()),
                resource_url: "https://example.invalid/res-1".to_string(),
                fullres_url: None,
                mime_type: "image/png".to_string(),
                file_name: Some("diagram.png".to_string()),
                is_image: true,
                alt_text: None,
            }],
            primary_div_id: Some("div:1".to_string()),
            synced_at: Some("2026-11-21T11:07:40Z".to_string()),
        };

        let cli_compact = page_content_json(&detail, true, PageSurface::Cli);
        assert_eq!(cli_compact["page_key"], "group:G1|gpage-1");
        assert_eq!(cli_compact["scope"]["scope_type"], "group");
        assert_eq!(cli_compact["scope"]["group_id"], "G1");
        assert_eq!(cli_compact["omitted"]["blocks"], 3);
        assert_eq!(cli_compact["omitted"]["resources"], 1);
        assert!(cli_compact.get("blocks").is_none());
        assert!(cli_compact.get("raw_xhtml").is_none());
        assert!(cli_compact["hint"].as_str().unwrap().contains("--full"));

        let mcp_compact = page_content_json(&detail, true, PageSurface::Mcp);
        assert_eq!(mcp_compact["page_key"], "group:G1|gpage-1");
        assert_eq!(mcp_compact["scope"]["group_id"], "G1");
        assert_eq!(mcp_compact["omitted"]["blocks"], 3);
        assert_eq!(mcp_compact["omitted"]["resources"], 1);
        assert!(mcp_compact.get("blocks").is_none());
        assert!(mcp_compact.get("raw_xhtml").is_none());
        assert!(mcp_compact["hint"]
            .as_str()
            .unwrap()
            .contains("\"compact\":false"));

        assert_eq!(page_content_json(&detail, false, PageSurface::Cli), json!(detail));
        assert_eq!(page_content_json(&detail, false, PageSurface::Mcp), json!(detail));
    }

    #[test]
    fn delete_prompt_names_the_resolved_page_or_flags_a_raw_id() {
        let page = OnenotePageSummary {
            page_key: "me|pg-1".to_string(),
            id: "pg-1".to_string(),
            scope: crate::models::OnenoteScope::Me,
            section_id: "sec-1".to_string(),
            section_name: Some("Specs".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Engineering".to_string()),
            title: "Release Plan".to_string(),
            level: 0,
            order: 0,
            created_at: None,
            last_modified_at: Some("2026-11-20T09:00:00Z".to_string()),
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: false,
        };
        let text = delete_prompt_text("me|pg-1", Some(&page));
        assert!(text.contains("Title:         Release Plan"));
        assert!(text.contains("Location:      Engineering / Specs"));
        assert!(text.contains("Last modified: 2026-11-20T09:00:00Z"));
        assert!(text.contains("Page key:      me|pg-1"));
        assert!(text.ends_with("[y/N]: "));

        // Missing names fall back to the section ID instead of an empty location.
        let bare = OnenotePageSummary {
            section_name: None,
            notebook_name: None,
            last_modified_at: None,
            ..page
        };
        let bare_text = delete_prompt_text("me|pg-1", Some(&bare));
        assert!(bare_text.contains("Location:      (unknown notebook) / sec-1"));
        assert!(bare_text.contains("Last modified: unknown"));

        let raw = delete_prompt_text("0-abc!1-def", None);
        assert!(raw.contains("not in the local cache"));
        assert!(raw.contains("raw page ID"));
        assert!(raw.contains("Delete OneNote page ID '0-abc!1-def'?"));
        assert!(raw.ends_with("[y/N]: "));
    }
}
