//! CLI argument and subcommand definitions (`clap` structs/enums).

use std::path::PathBuf;

use clap::{Parser, Subcommand};

use crate::{
    config::DEFAULT_SERVE_PORT,
    graph::copy::{DEFAULT_COPY_WAIT_SECONDS, MAX_COPY_WAIT_SECONDS},
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

impl Command {
    pub(super) fn wants_json(&self) -> bool {
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
