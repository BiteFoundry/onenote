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
| `list_notebooks` | List accessible OneNote notebooks (personal, Teams/Group, SharePoint) | `include_teams?`, `refresh?`, `offline?` |
| `list_sections` | List sections in a notebook or across all notebooks | `notebook?`, `refresh?`, `offline?` |
| `list_pages` | List pages in a section (supports `Notebook/Section`, name, or ID) | `section` (required), `limit?`, `refresh?`, `offline?` |
| `get_page` | Read Markdown and page metadata, with XHTML, blocks and resources in the default response | `page` (required), `compact?`, `offline?` |
| `create_notebook` | Create a new OneNote notebook | `name` (required), `group_id?`, `site_id?` |
| `rename_notebook` | Rename a OneNote notebook package by explicit storage IDs | `drive_id`, `item_id`, `name` (all required) |
| `create_section` | Create a new section inside a notebook | `notebook` (required), `name` (required) |
| `rename_section` | Rename a section under `/me/onenote` and verify its ID and parents | `section` (required section ID or `me\|ID`), `name` (required) |
| `desktop_rename_section` | Rename an existing section in OneNote for Windows | `section_id`, `name` (both required) |
| `desktop_set_page_level` | Set the indentation of an existing page in OneNote for Windows | `page_id`, `level` (both required, level 0–2) |
| `create_page` | Create a top-level OneNote page from Markdown and local images | `section` (required), `title` (required), `markdown?`, `level?` (0), `image_paths?` |
| `copy_page` | Copy an existing page through Graph and verify its destination and new ID | `page`, `section` (IDs or scoped keys, both required), `operation_url?`, `wait_seconds?` |
| `append_page` | Append Markdown and local images to the primary page region | `page` (required), `markdown` (required), `image_paths?` |
| `update_block`, `update_page` | Replace a block, edit its text, or update the page title/body | `page` (required), `block_id?`, `title?`, `markdown?`, `replace_text?`, `with_text?`, `expected_content?`, `force?` |
| `toggle_todo` | Set or toggle a checkbox by element ID or zero-based index | `page` (required), `block_id?`, `todo_index?`, `completed?`, `expected_content?`, `force?` |
| `delete_page` | Permanently delete a OneNote page | `page` (required), `confirm: true` (required) |
| `search_pages` | Full-text search across locally cached OneNote pages (SQLite FTS5) | `query` (required), `section?`, `limit?`, `refresh?` |
| `download_resource` | Download an embedded OneNote image or file attachment to disk | `page` (required), `index?`, `out_dir?` |

### Page reads and writes

Pass `compact: true` to `get_page` for `page.id`, `page.title`, `page.last_modified_at` and `page.body_markdown`. Read without `compact` to find `page.blocks[].element_id`, or inspect `page.raw_xhtml`. Use current element IDs for block writes. Both `update_block` and its `update_page` alias appear in `tools/list`.

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
