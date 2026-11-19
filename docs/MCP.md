# Model Context Protocol (MCP) Server (`onenote mcp`)

`onenote mcp` exposes Microsoft OneNote over **newline-delimited JSON-RPC 2.0 on stdio** (`protocolVersion: "2024-11-05"`), designed for Claude Desktop, Claude Code, Codex, and custom AI agents.

## Client Configuration

### Claude Desktop / Claude Code / Cursor

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

If installed via `npx`:

```json
{
  "mcpServers": {
    "onenote": {
      "command": "npx",
      "args": ["-y", "@shinick/onenote", "mcp"]
    }
  }
}
```

## Available Tools (14)

| Tool | Description | Key Parameters |
| --- | --- | --- |
| `whoami` | Return signed-in Microsoft 365 profile & auth mode | _(none)_ |
| `list_notebooks` | List accessible OneNote notebooks (personal, Teams/Group, SharePoint) | `include_teams?`, `refresh?`, `offline?` |
| `list_sections` | List sections in a notebook or across all notebooks | `notebook?`, `refresh?`, `offline?` |
| `list_pages` | List pages in a section (supports `Notebook/Section`, name, or ID) | `section` (required), `limit?`, `refresh?`, `offline?` |
| `get_page` | Read a OneNote page as structured blocks, Markdown, and resources | `page` (required), `section?`, `refresh?`, `offline?` |
| `create_notebook` | Create a new OneNote notebook | `name` (required), `group_id?`, `site_id?` |
| `create_section` | Create a new section inside a notebook | `notebook` (required), `name` (required) |
| `create_page` | Create a new OneNote page from Markdown (with optional local images & subpage level) | `section` (required), `title` (required), `markdown?`, `level?`, `images?` |
| `append_page` | Append Markdown content (and optional local images) to an existing page | `page` (required), `markdown` (required), `section?`, `images?` |
| `update_block` | Surgically replace a single `data-id` block or update page title/body | `page` (required), `block_id?`, `title?`, `markdown?`, `section?` |
| `toggle_todo` | Toggle or set a OneNote checkbox (`data-tag="to-do"`) on a block | `page` (required), `block_id` (required), `completed?`, `section?` |
| `delete_page` | Permanently delete a OneNote page | `page` (required), `section?` |
| `search_pages` | Full-text search across locally cached OneNote pages (SQLite FTS5) | `query` (required), `section?`, `limit?`, `refresh?` |
| `download_resource` | Download an embedded OneNote image or file attachment to disk | `page` (required), `index?`, `url?`, `out?`, `section?` |

### `download_resource` Response Shape

```json
{
  "ok": true,
  "path": "/path/to/downloaded-file.png",
  "file_name": "downloaded-file.png",
  "mime_type": "image/png",
  "size_bytes": 12480,
  "is_image": true
}
```
