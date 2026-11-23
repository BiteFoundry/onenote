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
      "args": ["-y", "@bitefoundry/onenote", "mcp"]
    }
  }
}
```

## Available Tools

| Tool | Description | Key Parameters |
| --- | --- | --- |
| `whoami` | Return signed-in Microsoft 365 profile & auth mode | _(none)_ |
| `list_notebooks` | List accessible OneNote notebooks (personal, plus Teams/Group with `include_teams`; SharePoint site notebooks are not auto-discovered) | `include_teams?`, `limit?`, `cursor?`, `refresh?`, `offline?` |
| `list_sections` | List sections in a notebook or across all notebooks | `notebook?`, `limit?`, `cursor?`, `refresh?`, `offline?` |
| `list_pages` | List pages in a section (supports `Notebook/Section`, name, or ID) | `section` (required), `limit?`, `cursor?`, `refresh?`, `offline?` |
| `get_page` | Read compact Markdown and identifiers; pass compact: false for XHTML, blocks and resources | `page` (required), `section?`, `compact?`, `offline?` |
| `create_notebook` | Create a new OneNote notebook | `name` (required), `group_id?`, `site_id?` |
| `rename_notebook` | Rename a OneNote notebook package by explicit storage IDs | `drive_id`, `item_id`, `name` (all required) |
| `create_section` | Create a new section inside a notebook | `notebook` (required), `name` (required) |
| `rename_section` | Rename a section under `/me/onenote` and verify its ID and parents | `section` (required section ID or `me\|ID`), `name` (required) |
| `desktop_rename_section` | Rename an existing section in OneNote for Windows | `section_id`, `name` (both required) |
| `desktop_set_page_level` | Set the indentation of an existing page in OneNote for Windows | `page_id`, `level` (both required, level 0–2) |
| `create_page` | Create a top-level OneNote page from Markdown and local images | `section` (required), `title` (required), `markdown?`, `level?` (0), `image_paths?` |
| `copy_page` | Copy an existing page through Graph and verify its destination and new ID | `page`, `section` (IDs or scoped keys, both required), `operation_url?`, `wait_seconds?` |
| `append_page` | Append Markdown and local images to the primary page region | `page` (required), `section?`, `markdown` (required), `image_paths?`, `compact?` |
| `update_block`, `update_page` | Replace a block, edit its text, or update the page title/body | `page` (required), `section?`, `block_id?`, `title?`, `markdown?`, `replace_text?`, `with_text?`, `expected_content?`, `force?`, `compact?` |
| `toggle_todo` | Set or toggle a checkbox by element ID or zero-based index | `page` (required), `section?`, `block_id?`, `todo_index?`, `completed?`, `expected_content?`, `force?`, `compact?` |
| `delete_page` | Permanently delete a OneNote page | `page` (required), `section?`, `confirm: true` (required) |
| `search_pages` | Full-text search across locally cached OneNote pages (SQLite FTS5) | `query` (required), `section?`, `limit?`, `cursor?`, `refresh?` |
| `download_resource` | Download an embedded OneNote image or file attachment to disk | `page` (required), `index?`, `out_dir?` |

The server resolves the signed-in account and local cache on the first `tools/call`; `initialize`, `ping`, and `tools/list` respond without contacting Graph or selecting a cache.

### Request cost

Each tool description in `tools/list` states what a call costs, so agents can avoid expensive patterns:

- `search_pages` is local (no network). `refresh: true` first runs a full account sync, which issues many Graph requests on large accounts; do not use it in loops.
- `list_notebooks`, `list_sections`, `list_pages` and `get_page` call Microsoft Graph unless `offline: true`. A recent cached list is reused only when `ONENOTE_CLI_CACHE_SECS` is set; `refresh: true` always refetches.
- `include_teams` adds one Graph request per joined Team (up to `ONENOTE_MAX_TEAMS`, default 100). `list_sections` without `notebook` lists every notebook, including Teams notebooks, and then each notebook's sections.
- `copy_page` blocks for up to `wait_seconds` while it polls the server-side copy. `download_resource` downloads the full file to disk.

### Page reads and writes

`get_page` returns the compact page by default, including `page.page_key`, `page.scope`, `page.omitted`, and `page.hint`. Pass `compact: false` to read `page.blocks[].element_id`, `page.raw_xhtml`, and `page.resources`. Write tools (`append_page`, `update_block`, `update_page`, `toggle_todo`) return a compact summary (`ok`, `page_key`, `title`, `last_modified_at`, plus `block_id` / `completed` where applicable) by default; pass `compact: false` to include the full `page` object. Page tools (`get_page`, `append_page`, `update_block`, `update_page`, `toggle_todo`, `delete_page`) accept an optional `section` argument to narrow title lookup. Use current element IDs for block writes. Both `update_block` and its `update_page` alias appear in `tools/list`.

### List paging

`list_notebooks`, `list_sections`, `list_pages`, and `search_pages` enforce `limit` in `1`–`1000` (`list_notebooks`, `list_sections`, and `list_pages` default to 50; `search_pages` defaults to 30). All four responses include `returned`, `has_more`, and `next_cursor` (pass `next_cursor` back as `cursor` to read the next page). When called with `offline: true`, `list_notebooks`, `list_sections`, and `list_pages` also include `cache_synced_at` (an RFC 3339 timestamp string, or `null` when the cache has never been synced). `search_pages` includes `cache: { pages, pages_with_body }` to show how many cached pages exist and how many have hydrated bodies; body search covers pages that have been read or synced.

`rename_section` takes the section's current ID from `list_sections`. `rename_notebook` takes the current DriveItem ID and requires a delegated file-write token. The desktop tools use the Windows app's object IDs from `GetHierarchy`. [Hierarchy operations and permissions](API.md#hierarchy-operations)

```json
{"name":"rename_section","arguments":{"section":"me|SECTION_ID","name":"Architecture Notes"}}
```

```json
{"name":"rename_notebook","arguments":{"drive_id":"DRIVE_ID","item_id":"ITEM_ID","name":"Engineering Notes"}}
```

```json
{"name":"desktop_set_page_level","arguments":{"page_id":"FULL_DESKTOP_PAGE_ID","level":1}}
```

The following JSON objects are `tools/call` parameters.

```json
{"name":"update_block","arguments":{"page":"me|PAGE_ID","block_id":"p:{ELEMENT_ID}","replace_text":"READY","with_text":"APPROVED"}}
```

To replace the full block, pass `markdown` with `block_id`. Send `expected_content` from a previous read to check the live content before writing. A page-wide Markdown update on a page with several regions requires text changes that can be matched to current elements. Use `append_page` to add new content.

```json
{"name":"toggle_todo","arguments":{"page":"me|PAGE_ID","block_id":"p:{TODO_ID}","completed":true}}
```

Set `completed` to `false` to uncheck the item. Omit `completed` to toggle. Repeating a set operation with the same state skips the write after the conflict check.

```json
{"name":"create_page","arguments":{"section":"SECTION_ID","title":"Evidence","markdown":"Release diagram","image_paths":["./diagram.png"]}}
```

### `copy_page`

```json
{"name":"copy_page","arguments":{"page":"me|PAGE_ID","section":"me|SECTION_ID"}}
```

Graph assigns a new page ID in the destination section. The original page and its links stay in place. A completed response includes `source_page_id`, `destination_section_id` and `new_page_id` in `structuredContent`. `wait_seconds` defaults to 20 and accepts 0 to return the accepted operation handle immediately.

For a pending copy, an unavailable status, or a verification error, check the same operation by passing its `operation_url` and the same page and section.

```json
{"name":"copy_page","arguments":{"page":"me|PAGE_ID","section":"me|SECTION_ID","operation_url":"OPERATION_URL"}}
```

`isError` is true until the result confirms the copied page. Use `structuredContent.status` and `structuredContent.operation_url` to distinguish a pending, failed, or unverified operation and identify it. [Copy workflow and permissions](API.md#copy-a-page)

### `download_resource` Response Shape

```json
{"name":"download_resource","arguments":{"page":"me|PAGE_ID","index":0,"out_dir":"./downloads"}}
```

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
