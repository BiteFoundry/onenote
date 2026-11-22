# Local HTTP JSON API (`onenote serve`)

`onenote serve` runs a local Axum HTTP server bound to `127.0.0.1:8793` by default.

```sh
# Start with default port (127.0.0.1:8793)
onenote serve

# Keep a client configured for the previous port
onenote serve --port 8788

# Set a bind address and require a Bearer token
onenote serve --bind 127.0.0.1 --token my-secret-token
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
| `GET` | `/v1/onenote/notebooks` | List notebooks (`?include_teams=false&refresh=false&offline=false`) |
| `POST` | `/v1/onenote/notebooks` | Create notebook (`{ name, group_id?, site_id? }`) |
| `GET` | `/v1/onenote/sections` | List sections (`?notebook=...&refresh=false&offline=false`) |
| `POST` | `/v1/onenote/sections` | Create section (`{ notebook, name }`) |
| `GET` | `/v1/onenote/pages` | List pages in a section (`?section=...&limit=50&refresh=false&offline=false`) |
| `POST` | `/v1/onenote/pages` | Create a top-level page (`{ section, title, markdown?, level?, image_paths? }`). `level` defaults to 0. |
| `GET` | `/v1/onenote/pages/{page}` | Get structured page detail (`?section=...&offline=false`) |
| `PATCH` | `/v1/onenote/pages/{page}` | Update block or page (`{ block_id?, title?, markdown?, expected_content?, force?, section? }`) |
| `DELETE` | `/v1/onenote/pages/{page}` | Delete page (`?section=...`) |
| `POST` | `/v1/onenote/pages/{page}/append` | Append Markdown/images (`{ markdown, images? \| image_paths?, section? }`) |
| `POST` | `/v1/onenote/pages/{page}/todo` | Set or toggle checkbox (`{ block_id?, index?, completed?, expected_content?, force?, section? }`; alias: `/pages/{page}/toggle-todo`) |
| `GET` | `/v1/onenote/search` | Search local FTS5 cache (`?q=...&section=...&limit=30&refresh=false`) |
| `POST` | `/v1/onenote/resources/download` | Download embedded resource (`{ page, index?, url?, out? \| out_dir?, section? }`; alias: `/pages/{page}/resources/download`) |

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

## Hierarchy operations

| Operation | Microsoft Graph OneNote v1.0 | Supported alternative |
| --- | --- | --- |
| Move an existing page to another section | Page parent relationships are read-only. Use CLI `copy-page` or MCP `copy_page` to create a separate page through Graph. [Page resource](https://learn.microsoft.com/en-us/graph/api/resources/page?view=graph-rest-1.0) · [Copy to section](#copy-a-page) | In OneNote for Windows, select **Move or Copy** on the page and then choose **Move**. Refresh the source and destination sections and verify the page ID, parent, and content after sync. [Move pages in OneNote](https://support.microsoft.com/en-us/onenote/organize-your-notes#move-a-page-or-subpage-to-another-section) |
| Change an existing page's level | `level` is read-only and `pagelevel=true` returns indentation and order. Page creation accepts XHTML and creates a top-level page. [Page resource](https://learn.microsoft.com/en-us/graph/api/resources/page?view=graph-rest-1.0) · [Read page hierarchy](https://learn.microsoft.com/en-us/graph/onenote-get-content#page-entity) · [Create page](https://learn.microsoft.com/en-us/graph/api/section-post-pages?view=graph-rest-1.0) | Use `desktop-set-page-level` or `desktop_set_page_level` on Windows. You can also drag the page tab to change indentation. [Create or promote a subpage](https://support.microsoft.com/en-us/onenote/onenote-help-and-learning/create-a-subpage-in-onenote) |
| Rename an existing notebook | The OneNote notebook service contract provides no rename method. `rename-notebook` and MCP `rename_notebook` use the Microsoft Graph Files API on a verified OneNote package. [OneNote notebook methods](https://learn.microsoft.com/en-us/graph/api/resources/notebook?view=graph-rest-1.0#methods) · [DriveItem update](https://learn.microsoft.com/en-us/graph/api/driveitem-update?view=graph-rest-1.0) | Rename through OneDrive or SharePoint with edit access. OneNote for Windows also supports a display name for your own view. A shared notebook's storage rename can disrupt existing access. [Rename a OneNote notebook](https://support.microsoft.com/en-us/onenote/onenote-help-and-learning/rename-a-onenote-notebook) |
| Rename an existing section | CLI `rename-section SECTION_ID --name NAME` and MCP `rename_section` use the SDK's `PATCH /me/onenote/sections/{id}` with `displayName`. The app verifies the same ID and parents after the write. [Graph PowerShell section update](https://learn.microsoft.com/en-us/powershell/module/microsoft.graph.notes/update-mguseronenotesection?view=graph-powershell-1.0) · [Graph .NET request builder](https://github.com/microsoftgraph/msgraph-sdk-dotnet/blob/main/src/Microsoft.Graph/Generated/Me/Onenote/Sections/Item/OnenoteSectionItemRequestBuilder.cs) | Choose **Rename** on the section tab. Desktop automation can use `UpdateHierarchy` with the existing section ID. [Rename a section in OneNote](https://support.microsoft.com/en-us/onenote/organize-your-notes#rename-a-section) · [Windows OneNote COM interface](https://learn.microsoft.com/en-us/office/client-developer/onenote/application-interface-onenote#updatehierarchy-method) |

Use delegated sign-in for supported Graph OneNote operations. `Notes.Create` is the least privileged delegated scope for page creation and `copyToSection` for personal Microsoft accounts and work or school accounts. Existing page title and HTML writes require `Notes.ReadWrite` and edit access. `copyToSection` documents user and group source paths. Follow the endpoint's documented scope and path for the account and resource. [OneNote authorization and scopes](https://learn.microsoft.com/en-us/graph/api/resources/onenote-api-overview?view=graph-rest-1.0) · [Create page permissions](https://learn.microsoft.com/en-us/graph/api/section-post-pages?view=graph-rest-1.0#permissions) · [Update page permissions](https://learn.microsoft.com/en-us/graph/api/page-update?view=graph-rest-1.0#permissions) · [Copy permissions](https://learn.microsoft.com/en-us/graph/api/page-copytosection?view=graph-rest-1.0#permissions)

Section rename uses the signed-in `/me` route, the app's delegated `Notes.ReadWrite` scope, and edit access on the section. The OneNote REST section reference omits a rename permissions table, so account-specific availability is unverified. Graph errors and mismatched read-backs surface as errors. Use the desktop Rename command for sections outside this route. [Graph permission reference](https://learn.microsoft.com/en-us/graph/permissions-reference#notesreadwrite) · [OneNote section methods](https://learn.microsoft.com/en-us/graph/api/resources/onenotesection?view=graph-rest-1.0#methods) · [Microsoft Q&A section rename example](https://learn.microsoft.com/en-us/answers/questions/1065090/the-onenote-graph-api-does-not-return-the-new-name)

### Copy a page

```sh
onenote copy-page 'me|PAGE_ID' --section 'me|SECTION_ID' --json
onenote copy-page 'me|PAGE_ID' --section 'me|SECTION_ID' --operation 'OPERATION_URL' --json
```

Use IDs or scoped keys from the page and section lists. A raw ID uses the signed-in user's `/me` route. A key such as `group:GROUP_ID|SECTION_ID` uses a Microsoft 365 group destination and sends its `groupId`. Group pages also accept a `group:GROUP_ID|PAGE_ID` source key. The client supports `/me` and `/groups/{id}` copy paths. [Microsoft Graph copyToSection paths and parameters](https://learn.microsoft.com/en-us/graph/api/page-copytosection?view=graph-rest-1.0)

Sign in with a delegated personal Microsoft account or work or school account. The copy and operation status endpoints list `Notes.Create` as the least privileged delegated scope. Page verification also needs read permission. A delegated `Notes.ReadWrite` token with access to both resources covers the workflow. Group notebooks require access to the relevant Microsoft 365 group resources. [Copy permissions](https://learn.microsoft.com/en-us/graph/api/page-copytosection?view=graph-rest-1.0#permissions) · [Operation permissions](https://learn.microsoft.com/en-us/graph/api/onenoteoperation-get?view=graph-rest-1.0#permissions) · [Page read permissions](https://learn.microsoft.com/en-us/graph/api/page-get?view=graph-rest-1.0#permissions) · [Delegated OneNote authentication](https://learn.microsoft.com/en-us/graph/integrate-with-onenote)

The client sends one copy request, takes the operation URL from the `202 Accepted` response, and polls it on the configured Graph origin. On completion, the client reads the new page and verifies its ID differs from the source ID and its parent matches the requested destination. The original page and its links stay in place. [Graph operation status](https://learn.microsoft.com/en-us/graph/api/onenoteoperation-get?view=graph-rest-1.0) · [Graph page metadata](https://learn.microsoft.com/en-us/graph/api/page-get?view=graph-rest-1.0)

The JSON result includes `source_page_id`, `destination_section_id`, `operation_id` and `operation_url`. A verified copy returns `status: "completed"`, `new_page_id` and `new_page_key`. If polling reaches its deadline, the status is `pending`. A terminal Graph error returns `failed`. Polling errors and destination read-back errors return `status_unavailable` and `verification_failed` respectively. The CLI exits nonzero for these four outcomes and prints a single JSON object with `ok: false`, the top-level operation fields, and `error: { code, message }` when `--json` is passed.

Polling defaults to 20 seconds. Use `--wait-seconds 0` to return the accepted operation handle immediately. To check the same operation, use `--operation` with the returned `operation_url` and the original page and section arguments. If submission fails before you receive a usable operation URL, check the destination before submitting a new copy. See also [MCP copy_page](MCP.md#copy_page).

### Notebook storage rename

Supply both the drive ID and item ID for an existing `driveItem` with `package.type = oneNote`. Set `ONENOTE_TOKEN_FILE` to a file containing a delegated Graph token with `Files.ReadWrite` and edit access. The Files API lists this permission for personal Microsoft accounts and work or school accounts. The command checks the package and parent, PATCHes only `name` with `If-Match` when an eTag is available, then verifies the same drive item ID and parent.

```sh
onenote rename-notebook --drive-id DRIVE_ID --item-id ITEM_ID --name 'Engineering Notes' --json
```

The result's `drive_item.item_id` identifies the notebook package in storage. Microsoft documents stable DriveItem IDs across rename. Its OneNote URL lookup can compare notebook IDs after syncing. Microsoft Support documents shared notebook access disruption and advises leaving Class and Staff Notebooks under their existing names. [OneNote notebook packages](https://learn.microsoft.com/en-us/graph/api/resources/package?view=graph-rest-1.0) · [DriveItem update and permissions](https://learn.microsoft.com/en-us/graph/api/driveitem-update?view=graph-rest-1.0) · [OneDrive item IDs](https://learn.microsoft.com/en-us/graph/onedrive-addressing-driveitems#id-based-addressing) · [OneNote URL lookup](https://learn.microsoft.com/en-us/graph/api/notebook-getnotebookfromweburl?view=graph-rest-1.0) · [Notebook rename in OneDrive and SharePoint](https://support.microsoft.com/en-us/onenote/onenote-help-and-learning/rename-a-onenote-notebook)

### Windows desktop hierarchy changes

Open the notebook in desktop OneNote with edit access. The desktop commands use Windows PowerShell and the OneNote COM session. Get the full `Page` or `Section` `ID` from the desktop hierarchy. In Windows PowerShell, pass a specific notebook ID to `GetHierarchy` or pass an empty string to read open notebooks.

```powershell
$note = New-Object -ComObject OneNote.Application
$hierarchy = ''
$note.GetHierarchy('', 4, [ref]$hierarchy, 2)
$hierarchy
onenote desktop-set-page-level 'FULL_DESKTOP_PAGE_ID' --level 1 --json
onenote desktop-rename-section 'FULL_DESKTOP_SECTION_ID' --name 'Engineering Notes' --json
```

Use levels 0, 1 and 2 for top-level pages and the two levels of subpages. A level update sends the page's ID and level and verifies the resulting level and original parent section. A section rename verifies the section ID, parent and name. The desktop Move workflow provides no documented Graph page-ID guarantee. [OneNote COM application interface](https://learn.microsoft.com/en-us/office/client-developer/onenote/application-interface-onenote) · [Hierarchy and XML schema enumerations](https://learn.microsoft.com/en-us/office/client-developer/onenote/enumerations-onenote-developer-reference)

After a native hierarchy edit, wait for OneNote to sync. Use `onenote read PAGE --json --full` or MCP `get_page` with `compact: false` to compare `summary.id`, `section_id`, `notebook_id`, `level`, and `raw_xhtml` with a previous read. Refresh hierarchy lists with `--refresh` and re-download referenced resources when binary preservation matters.
