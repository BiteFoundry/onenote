<div align="center">

# `@shinick/onenote`

**Keyboard-first Microsoft OneNote terminal client, CLI, HTTP API, and MCP server — powered by Rust, Ratatui, and local SQLite FTS5.**

[![npm version](https://img.shields.io/npm/v/%40shinick%2Fonenote?style=flat-square&color=7719AA&label=npm)](https://www.npmjs.com/package/@shinick/onenote)
[![Platforms](https://img.shields.io/badge/platforms-Windows%20%7C%20macOS%20%7C%20Linux-2b3137?style=flat-square)](docs/NPM.md)
[![Built with Rust & Ratatui](https://img.shields.io/badge/built%20with-Rust%20%2B%20Ratatui-dea584?style=flat-square)](Cargo.toml)
[![License: MIT](https://img.shields.io/badge/license-MIT-3fb950?style=flat-square)](package.json)

<br />

![Microsoft OneNote TUI Demo](assets/demo.gif)

</div>

---

## Highlights

| Capability | Details |
| --- | --- |
| **Minimal-Chrome 3-Pane TUI** | Personal, Teams/Group & SharePoint notebooks, colored section ribbon, nested subpages (`level 0/1/2`), bidirectional XHTML ↔ Markdown canvas, surgical `data-id` `PATCH` edits, `0ms` optimistic to-do toggles, Link Extractor (`u`), File Picker with `zoxide` (`Ctrl+F`), and 6 themes. |
| **High-Res Terminal Images** | Inline thumbnails & fullscreen HD previews (`v` / `Alt+v`) with 80ms protocol probing for **Kitty**, **iTerm2**, **Sixel** (Floyd-Steinberg dithering), and **24-bit Unicode Half-Blocks** + `Ctrl+V` clipboard PNG upload. |
| **Local SQLite FTS5 + IPC Daemon** | WAL-mode SQLite cache with 4-reader pool, trigram full-text search (`/`), persistent `DraftStore`, and a shared background sync daemon (`onenote daemon start`). |
| **4-in-1 Agent Interface** | Interactive **TUI** (`onenote` / `note`), scriptable `--json` **CLI**, local **HTTP JSON API** (`onenote serve`, port `8788`), and stdio **MCP Server** (`onenote mcp`, 14 tools). |

---

## Quick Start

```sh
# 1. Install globally via npm (precompiled native binary for win32 / darwin / linux)
npm i -g @shinick/onenote

# 2. Authenticate with Microsoft 365 (PKCE loopback, --device-code, or --token-file)
onenote login

# 3. Launch the interactive TUI (or use the short alias `note`)
onenote
```

> Required Microsoft Graph delegated scopes: `Notes.Read`, `Notes.ReadWrite`, `Notes.Create`, `User.Read`, `offline_access`.  
> Prefer building from source? Run `cargo build --release` and see [docs/NPM.md](docs/NPM.md) for external token brokers & environment variables.

---

## Keybindings Cheat Sheet

| Key | Action |
| --- | --- |
| `Tab` / `Shift+Tab` | Cycle focus (Notebooks → Pages → Note Canvas) |
| `j` / `k` · `↑` / `↓` | Navigate notebooks, pages, subpages, or blocks |
| `[` / `]` | Switch to previous / next section tab |
| `n` / `N` | Create new page / subpage (Markdown + `Ctrl+V` PNG) |
| `a` / `e` | Append Markdown block / surgically edit selected block |
| `Space` / `x` | Toggle selected To-Do checkbox (`data-tag="to-do:completed"`) |
| `B` / `S` / `D` | Create notebook / create section / delete page |
| `u` / `h` | Open Link Extractor modal / toggle inline `href` URLs |
| `v` / `o` / `y` | HD image preview / open in browser / copy page Markdown |
| `Ctrl+F` | Interactive File Picker (`z` for `zoxide` directory jump) |
| `/` | Instant full-text search across cached pages (SQLite FTS5) |
| `Ctrl+P` / `Ctrl+K` | Unified Command Palette & Omni-Switcher |
| `.` / Right-click | Context Menu |
| `Ctrl+,` / `Ctrl+B` / `?` | Settings / Toggle sidebar / Keyboard shortcuts help |

---

## CLI, HTTP API & MCP

### CLI (`--json` on every command)

```sh
onenote notebooks --json
onenote pages "Acme Engineering/Architecture" --json
onenote read "Q4 Architecture RFC" --json
onenote create "Architecture" "Release Notes" --body "## Shipped\n- [x] WAL pool" --json
onenote check "Migration Cutover" "p:m3" --done --json
onenote search "canary" --json
```

### Local HTTP API (`onenote serve`)

```sh
# Start local Axum JSON server (default 127.0.0.1:8788)
onenote serve --bind 127.0.0.1:8788 --token dev-secret

# Query pages & FTS5 search from scripts or agents
curl -H "Authorization: Bearer dev-secret" \
  "http://127.0.0.1:8788/v1/onenote/search?q=canary&limit=10"
```

Full route reference & OpenAPI 3.1 spec: [docs/API.md](docs/API.md) · [docs/openapi.yaml](docs/openapi.yaml).

### Model Context Protocol (`onenote mcp`)

Add to Claude Desktop, Claude Code, Cursor, or Codex (`14` tools over stdio JSON-RPC 2.0):

```json
{
  "mcpServers": {
    "onenote": {
      "command": "onenote",
      "args": ["mcp"]
    }
  }
}
```

Full tool catalog & schemas: [docs/MCP.md](docs/MCP.md).

---

## `@shinick` Enterprise Terminal Suite

| Package | Commands | Workspace Coverage |
| --- | --- | --- |
| [`@shinick/teams`](https://www.npmjs.com/package/@shinick/teams) | `teams`, `tm` | Microsoft Teams chats, channels, threads & meetings |
| [`@shinick/outlook`](https://www.npmjs.com/package/@shinick/outlook) | `outlook`, `mail`, `cal` | Microsoft Outlook mail, calendar & contacts |
| **`@shinick/onenote`** | `onenote`, `note` | Microsoft OneNote notebooks, sections, pages & XHTML/Markdown |
| [`@shinick/sharepoint`](https://www.npmjs.com/package/@shinick/sharepoint) | `sharepoint`, `sp`, `onedrive`, `od` | Microsoft SharePoint sites, document libraries & OneDrive files |
| [`@shinick/atlassian`](https://www.npmjs.com/package/@shinick/atlassian) | `atlassian`, `jira`, `confluence` | Atlassian Jira issues, boards, sprints & Confluence pages |
| [`@shinick/notion`](https://www.npmjs.com/package/@shinick/notion) | `notion`, `nt` | Notion pages, databases, blocks & Markdown sync |

---

## License & Disclaimer

Licensed under the **MIT License**.

> Unofficial terminal client for Microsoft OneNote. Not affiliated with, endorsed by, or sponsored by Microsoft Corporation.
