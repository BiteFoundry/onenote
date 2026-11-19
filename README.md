# `@shinick/onenote` (`onenote` / `note`)

A standalone, fast, keyboard-first **Microsoft OneNote** terminal client (TUI), CLI, local HTTP API server, and MCP server written in Rust on top of Microsoft Graph v1.0 and local SQLite WAL + FTS5 caching.

## Highlights

- **Full Microsoft OneNote Coverage**: Personal (`/me/onenote`), Microsoft Teams / Group (`/groups/{id}/onenote`), and SharePoint Site (`/sites/{id}/onenote`) notebooks, section groups, colored section tabs, nested subpages (`level 0/1/2`), embedded images, and file attachments.
- **Bidirectional XHTML ↔ Markdown Engine**: Parses OneNote XHTML into structured blocks (`Heading`, `Paragraph`, `BulletList`, `NumberedList`, `TodoItem`, `Table`, `CodeBlock`, `Image`, `FileAttachment`, `Quote`, `Divider`), supports surgical `data-id` `PATCH` edits and `data-tag="to-do:completed"` toggling, and uploads images via `multipart/form-data` (`name:img-1`).
- **Minimal-Chrome Responsive TUI (`onenote` / `note`)**:
  - 3-column workspace: Notebook Sidebar + Colored Horizontal Section Ribbon + Action Toolbar + Page List + Rich Note Canvas.
  - Inline & fullscreen HD terminal image previews (**Kitty**, **iTerm2**, **Sixel** with Floyd-Steinberg dithering, and **24-bit Unicode Half-Blocks** with 80ms capability probing & Lanczos3 downscaling).
  - Link Extractor (`u`), inline `href` toggle (`h`), `Ctrl+V` clipboard PNG paste in OneNote forms, `Ctrl+F` interactive File Picker with `zoxide` jump (`z`), `Ctrl+,` Settings modal, `.` / right-click Context Menu, `Ctrl+P` Command Palette, persistent `DraftStore`, and desktop notifications.
- **Agent-Native Interfaces**:
  - **CLI** with `--json` output across all commands.
  - **Local HTTP API** (`onenote serve`, default `127.0.0.1:8788`, see [docs/API.md](docs/API.md) & [docs/openapi.yaml](docs/openapi.yaml)).
  - **MCP Server** (`onenote mcp`, stdio JSON-RPC 2.0 exposing `whoami` + 13 OneNote tools, see [docs/MCP.md](docs/MCP.md)).
- **Shared IPC Sync Daemon**: Background cache warm-up and event broadcast (`onenote daemon start | status | stop | restart`).

## Quick Start

### Install via npm

```sh
npm install -g @shinick/onenote
onenote --version
note --version
```

### Build from source (Cargo)

```sh
cargo build --release
./target/release/onenote --version
```

### Authenticate

```sh
# Interactive browser login (OAuth2 PKCE loopback)
onenote login

# Headless / SSH login (Device Code flow)
onenote login --device-code

# External token file (e.g., broker / Azure CLI)
onenote login --token-file ~/.cursor/tc-auth/access_token

# Inspect identity, scopes, local cache, terminal graphics, and daemon
onenote whoami
onenote doctor --online
```

Required Microsoft Graph delegated scopes: `Notes.Read`, `Notes.ReadWrite`, `Notes.Create`, `User.Read`, `offline_access`.

## CLI Commands

```sh
# Launch interactive TUI (default when no subcommand is given)
onenote
note

# Browse hierarchy
onenote notebooks [--personal-only] [--refresh] [--offline] [--json]
onenote sections [notebook] [--refresh] [--offline] [--json]
onenote pages <section> [--limit 50] [--refresh] [--offline] [--json]
onenote read <page> [--section <section>] [--raw-html] [--refresh] [--offline] [--json]

# Create & edit
onenote create-notebook <name> [--group <id> | --site <id>] [--json]
onenote create-section <notebook> <name> [--json]
onenote create <section> <title> [--body <md> | --body-file <path>] [--image <path>...] [--level 0|1|2] [--json]
onenote append <page> [--body <md> | --body-file <path>] [--image <path>...] [--section <sec>] [--json]
onenote update <page> [--block-id <id>] [--title <title>] [--body <md> | --body-file <path>] [--json]
onenote check <page> <block-id> [--done | --undone] [--section <sec>] [--json]
onenote delete <page> [--section <sec>] [--json]

# Search & resources
onenote search <query> [--section <sec>] [--limit 20] [--refresh] [--json]
onenote resource <page> [index] [--url <url>] [--out <path>] [--section <sec>] [--json]
onenote preview-file <path> [--cols 60] [--rows 24] [--protocol auto|kitty|iterm2|sixel|halfblock]

# Sync, daemon, API, MCP & self-update
onenote sync [--json]
onenote daemon <start|status|stop|restart|run> [--json]
onenote serve [--bind 127.0.0.1:8788] [--token <secret>]
onenote mcp
onenote self-update [--check] [--force] [--json]
```

## TUI Keybindings

| Key | Action |
| --- | --- |
| `Tab` / `Shift+Tab` | Cycle focus (Notebooks → Pages → Note Canvas) |
| `j` / `k` / `↑` / `↓` | Move selection / navigate blocks |
| `[` / `]` | Previous / next section tab |
| `B` / `S` | Create Notebook / Create Section |
| `n` / `N` | New Page / New Subpage |
| `a` | Append Markdown block (`Ctrl+V` paste image, `Ctrl+F` File Picker) |
| `e` | Surgically edit selected block |
| `Space` / `x` | Toggle selected To-Do checkbox (`0ms` optimistic UI) |
| `d` / `Delete` | Delete active page (with confirmation modal) |
| `v` | Fullscreen HD terminal image preview |
| `o` | Open page in browser or download attachments (`Attachments` modal) |
| `u` | Open Link Extractor modal |
| `h` | Toggle inline link URLs (`href`) |
| `y` | Copy page Markdown to clipboard |
| `/` | Local full-text search (SQLite FTS5) |
| `.` / Right-click | Context Menu |
| `Ctrl+P` / `Ctrl+K` | Command Palette |
| `Ctrl+,` | Settings modal |
| `Ctrl+B` | Toggle Notebook sidebar |
| `r` / `F5` | Refresh active view |
| `?` | Keyboard shortcuts help |
| `q` | Quit |

## Documentation

- [docs/NPM.md](docs/NPM.md) — npm packaging, environment variables, external token brokers, and Linux GLIBC 2.28 support
- [docs/API.md](docs/API.md) — Local HTTP JSON API (`onenote serve`, default port `8788`)
- [docs/MCP.md](docs/MCP.md) — MCP stdio JSON-RPC 2.0 server (`onenote mcp`)
- [docs/openapi.yaml](docs/openapi.yaml) — OpenAPI 3.1 contract
- [release-notes/v0.1.0.md](release-notes/v0.1.0.md) — Release notes for `v0.1.0`

## License

MIT
