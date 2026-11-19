# Local HTTP JSON API (`onenote serve`)

`onenote serve` runs a local Axum HTTP server bound to `127.0.0.1:8788` by default.

```sh
# Start with default port (127.0.0.1:8788)
onenote serve

# Start on a custom address with Bearer token protection
onenote serve --bind 127.0.0.1:8788 --token my-secret-token
```

When `--token` is provided, every endpoint except `/health` requires:

```http
Authorization: Bearer my-secret-token
```

## Endpoints

All OneNote endpoints are served under both `/v1/onenote/...` and the shorter `/onenote/...` alias.

| Method | Path | Description |
| --- | --- | --- |
| `GET` | `/health` | Health check (`{ ok, service, version }`) |
| `GET` | `/v1/whoami` | Signed-in Microsoft 365 user profile |
| `GET` | `/v1/onenote/notebooks` | List notebooks (`?include_teams=true&refresh=false&offline=false`) |
| `POST` | `/v1/onenote/notebooks` | Create notebook (`{ name, group_id?, site_id? }`) |
| `GET` | `/v1/onenote/sections` | List sections (`?notebook=...&refresh=false&offline=false`) |
| `POST` | `/v1/onenote/sections` | Create section (`{ notebook, name }`) |
| `GET` | `/v1/onenote/pages` | List pages in a section (`?section=...&limit=50&refresh=false&offline=false`) |
| `POST` | `/v1/onenote/pages` | Create page (`{ section, title, markdown?, level?, images? }`) |
| `GET` | `/v1/onenote/pages/{page}` | Get structured page detail (`?section=...&refresh=false&offline=false`) |
| `PATCH` | `/v1/onenote/pages/{page}` | Update block or page (`{ block_id?, title?, markdown?, section? }`) |
| `DELETE` | `/v1/onenote/pages/{page}` | Delete page (`?section=...`) |
| `POST` | `/v1/onenote/pages/{page}/append` | Append Markdown/images (`{ markdown, images?, section? }`) |
| `POST` | `/v1/onenote/pages/{page}/todo` | Toggle checkbox (`{ block_id, completed?, section? }`) |
| `GET` | `/v1/onenote/search` | Search local FTS5 cache (`?q=...&section=...&limit=20&refresh=false`) |
| `POST` | `/v1/onenote/resources/download` | Download embedded resource (`{ page, index?, url?, out?, section? }`) |

### Resource Download Response

```json
{
  "ok": true,
  "path": "/Users/you/Downloads/onenote-cli/diagram.png",
  "file_name": "diagram.png",
  "mime_type": "image/png",
  "size_bytes": 48192,
  "is_image": true
}
```

See [openapi.yaml](openapi.yaml) for the complete OpenAPI 3.1 schema.
