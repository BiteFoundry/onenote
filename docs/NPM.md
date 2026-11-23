# npm distribution

`@bitefoundry/onenote` is distributed on npm as a small JavaScript launcher plus one
optional dependency per platform. The launcher resolves the prebuilt Rust binary
and execs it directly (with `scripts/postinstall.mjs` linking the native binary
into `bin/` and Windows shims for zero-Node-startup execution), so there is no
compiler, Node native addon, or network download during install.

The Rust crate is named `onenote-cli` and produces the standalone **Microsoft OneNote**
binary (`onenote` / `onenote.exe`). The npm package `@bitefoundry/onenote` installs the `onenote` command.

## Install and run

```sh
# one-off
npx @bitefoundry/onenote --version

# global install
npm install -g @bitefoundry/onenote
onenote --version

# project-local
npm install @bitefoundry/onenote
npx onenote notebooks
```

All workflows share the same Microsoft Entra ID login (or automatic M365 shared-session
bootstrap from `teams-cli` / `outlook-cli`), SQLite WAL + FTS5 cache (`cache.db`),
background sync daemon (`onenote daemon start | status | stop | restart`), local HTTP API
(`onenote serve`, default `127.0.0.1:8793`), MCP server (`onenote mcp`), and multi-protocol
terminal image preview (`Kitty` / `iTerm2` / `Sixel` / `Halfblock` via `onenote preview-file`
and the interactive TUI).

- `onenote login`, `onenote logout`, `onenote whoami`, `onenote doctor --online`
- `onenote notebooks [--limit N] [--cursor N]`, `onenote sections [notebook] [--limit N] [--cursor N]`, `onenote pages <section> [--limit N] [--cursor N]`, `onenote read <page> [--json [--full]]`
- `onenote create-notebook <name>`, `onenote create-section <notebook> <name>`
- `onenote rename-section <section-id> --name <new-name> [--json]`
- `onenote rename-notebook --drive-id <drive-id> --item-id <item-id> --name <name> [--json]`
- Windows `onenote desktop-set-page-level <desktop-page-id> --level <0|1|2> [--json]` and `onenote desktop-rename-section <desktop-section-id> --name <name> [--json]`
- `onenote create <section> --title <title> [markdown] [--file <file>] [--image <path>] [--level 0]`
- `onenote append <page> [markdown] [--file <file>] [--image <path>]`
- `onenote update <page> [--title <title>] [--body <markdown> | --file <file>] [--block-id <id>]`
- `onenote check <page> [--block-id <id> | --index <n>] [--done | --undone]`, `onenote delete <page> --yes`
- `onenote search <query> [--limit N] [--cursor N]`, `onenote resource <page> [--index <n>] [--out <directory>]`
- `onenote sync`, `onenote daemon <start|status|stop|restart|run>`, `onenote tui`, `onenote serve`, `onenote mcp`, `onenote preview-file <path>`, `onenote self-update`

```sh
onenote append 'me|PAGE_ID' 'Evidence' --json
onenote resource 'me|PAGE_ID' --index 0 --out ./downloads --json
onenote read 'me|PAGE_ID' --format blocks
onenote update 'me|PAGE_ID' --block-id 'p:{ELEMENT_ID}' --replace-text READY --with-text APPROVED
onenote update 'me|PAGE_ID' --block-id 'p:{ELEMENT_ID}' --body '**Approval complete**'
```

Use `--replace-text` and `--with-text` to change one exact substring in a block's text while keeping its XHTML markup. Use `--body` with `--block-id` to replace the entire block with Markdown. Page-wide `--body` updates on pages with multiple regions require text changes that can be matched to current elements. Use `append` to add content to the primary region.

Graph creates a top-level page. Windows desktop commands can promote or indent an existing page. Use the OneNote app's **Move** command to move a page. [Hierarchy operations and permissions](API.md#hierarchy-operations)

## Upgrade scripts and clients

Replace the previous `note` alias with `onenote` in scripts.

```sh
# Previous invocation
note notebooks --json

# Current invocation
onenote notebooks --json
```

In your MCP configuration, change `{"command":"note","args":["mcp"]}` to `{"command":"onenote","args":["mcp"]}`. If you installed the PowerShell profile launcher, rerun `scripts/install-profile.ps1` and restart the shell to load the updated profile.

npm removes the previous package's global `note` launcher during an upgrade. If you manually replaced that file with another program's launcher, move it to a separate `PATH` directory before upgrading.

Change HTTP clients from `http://127.0.0.1:8788` to `http://127.0.0.1:8793` to use the new default. To keep an existing client on port `8788`, run `onenote serve --port 8788` when that port is available. Teams uses `8788` by default.

From 0.4.1 the HTTP API requires an API key by default: pass `--token <SECRET>` or set `ONENOTE_API_KEY` and send it as `Authorization: Bearer <SECRET>`, or copy the key that a bare `onenote serve` prints on stderr. `--allow-keyless` restores keyless access on loopback only. See [docs/API.md](API.md#authentication-and-access-control).

## Supported platforms

| npm package                     | OS      | CPU   | Rust target                         |
| ------------------------------- | ------- | ----- | ----------------------------------- |
| `@bitefoundry/onenote-win32-x64`    | Windows | x64   | `x86_64-pc-windows-msvc`            |
| `@bitefoundry/onenote-win32-arm64`  | Windows | arm64 | `aarch64-pc-windows-msvc`           |
| `@bitefoundry/onenote-darwin-x64`   | macOS   | x64   | `x86_64-apple-darwin`               |
| `@bitefoundry/onenote-darwin-arm64` | macOS   | arm64 | `aarch64-apple-darwin`              |
| `@bitefoundry/onenote-linux-x64`    | Linux   | x64   | `x86_64-unknown-linux-gnu` (2.28)   |
| `@bitefoundry/onenote-linux-arm64`  | Linux   | arm64 | `aarch64-unknown-linux-gnu` (2.28)  |

Linux packages are cross-compiled in CI with `cargo-zigbuild` against **GLIBC 2.28** (`*.2.28`)
and verified with `scripts/verify-glibc.mjs` plus an `almalinux:8` container smoke test, so
prebuilt binaries run out of the box on **AlmaLinux 8 / RHEL 8+ / Rocky Linux 8+ / Ubuntu 20.04+ / Debian 10+**.
Older glibc distributions (e.g., CentOS 7 with `GLIBC < 2.28`) and Alpine/musl Linux should
build from source with Cargo (`cargo build --release`).

## How resolution works

`bin/onenote.js` looks up `@bitefoundry/onenote-<platform>-<arch>` through
`require.resolve`, then execute `node_modules/@bitefoundry/onenote-<platform>-<arch>/bin/onenote`
(or `onenote.exe` on Windows) with `stdio: 'inherit'`. On `npm install`, `scripts/postinstall.mjs`
additionally links the native binary directly for zero-Node-startup execution.
Signals (`SIGINT`, `SIGTERM`, `SIGHUP`) are forwarded and the launcher exits with the child's
exit code. If the platform package is missing, the launcher prints the supported platforms
and the `--include=optional` hint instead of a stack trace.

## Layout

```
package.json                  # @bitefoundry/onenote launcher package (bin: onenote)
bin/onenote.js                # onenote launcher
scripts/postinstall.mjs       # zero-Node native binary linker on npm install
npm/onenote-<platform>/       # one package per platform
  package.json                # os/cpu/files, same version as root
  bin/onenote[.exe]           # added by scripts/build-npm.mjs, never committed
scripts/build-npm.mjs         # copy target/<triple>/release/onenote[.exe]
scripts/sync-version.mjs      # Cargo.toml version -> all package.json files
scripts/verify-glibc.mjs      # verify Linux ELF GLIBC <= 2.28 symbol ceiling
.github/workflows/release.yml # tag-triggered build, publish, GitHub Release
```

## Versioning

`Cargo.toml` (`[package].version`) is the single source of truth. To bump:

```sh
# edit Cargo.toml [package].version
node scripts/sync-version.mjs
node scripts/sync-version.mjs --check
```

## External token brokers, M365 bootstrap & token scopes

### Microsoft Graph delegated scopes

Basic sign-in requests `offline_access`, `User.Read`, `Notes.Read`, `Notes.ReadWrite`, and `Notes.Create`. A company tenant sign-in also requests `Notes.Read.All`, `Notes.ReadWrite.All`, and `Team.ReadBasic.All`. [Company-managed sign-in and permissions](../README.md#company-managed-microsoft-entra-sign-in)

Run `onenote whoami` or `onenote doctor` to inspect JWT `scp`/`roles` claims automatically.

### `config.toml` (recommended persistent setup)

```toml
# Windows: %APPDATA%\onenote-cli\config.toml
# Linux/macOS: ~/.config/onenote-cli/config.toml
token_file = "~/.cursor/tc-auth/access_token"
token_refresh_cmd = "python ~/.cursor/tc-auth/graph-auth.py refresh"
token_refresh_mins = 5
```

### Key environment variables

| Variable | Summary |
| --- | --- |
| `ONENOTE_ACCESS_TOKEN` | Static Bearer token override (automatically skips OS keyring) |
| `ONENOTE_TOKEN_FILE` | External token file path (raw JWT or JSON; skips OS keyring) |
| `ONENOTE_TOKEN_REFRESH_CMD` | Shell command run to refresh expiring/`401` tokens |
| `ONENOTE_TOKEN_REFRESH_MINS` | Refresh lead window in minutes before expiry (default `5`) |
| `ONENOTE_SHARE_M365_AUTH` | Share/bootstrap M365 token session from `TEAMS_TOKEN_FILE` or `teams-cli` (default `true`) |
| `ONENOTE_DISABLE_KEYRING=1` | Disable OS keyring and use `credentials.json` only |
| `ONENOTE_CLIENT_ID` | Override Entra ID application client ID (falls back to `TEAMS_CLIENT_ID`, `AZURE_CLIENT_ID`) |
| `ONENOTE_TENANT_ID` | Override Entra ID tenant ID (default `common`; falls back to `TEAMS_TENANT_ID`, `AZURE_TENANT_ID`) |
| `ONENOTE_CONFIG_DIR` | Override config/credentials directory |
| `ONENOTE_DATA_DIR` | Override SQLite cache (`cache.db`) directory |
| `ONENOTE_NO_BROWSER` | Disable automatic browser launch during login (`1` / `true`) |
| `ONENOTE_AUTO_DAEMON` | Auto-start/attach to the background sync daemon (`1` / `true`, default `false`) |
| `ONENOTE_MAX_TEAMS` | Joined Teams queried for notebooks by `--include-teams` (default `100`, range `1`–`500`; one Graph request per Team). Background sync always uses `25` |
| `ONENOTE_IMAGE_PROTOCOL` | Terminal image protocol override (`auto`, `kitty`, `iterm2`, `sixel`, `halfblock`) |
| `ONENOTE_GITHUB_REPO` | Override GitHub `owner/repo` used by `onenote self-update` (default `BiteFoundry/onenote`) |
| `ONENOTE_GITHUB_TOKEN` | GitHub token for private release checks in `onenote self-update` |
| `ONENOTE_EXTRA_CA_PEM` | PEM root certificate(s) to trust in addition to the bundled and OS roots, for TLS-inspecting corporate proxies. An unusable file is an error |
| `SSL_CERT_FILE`, `NODE_EXTRA_CA_CERTS` | Also loaded as extra PEM roots; an unusable file is a stderr warning and is skipped |

## Troubleshooting

- **`no prebuilt binary found for <platform>-<arch>`** — reinstall with
  `npm install -g @bitefoundry/onenote --include=optional`.
- **`TLS certificate was not trusted (unknown issuer)`** — your network inspects HTTPS with a private root CA.
  Export it as PEM and set `ONENOTE_EXTRA_CA_PEM=/path/to/root.pem` (`SSL_CERT_FILE` and `NODE_EXTRA_CA_CERTS`
  are also honoured), then check with `onenote doctor --online`.
- **Older Linux (`GLIBC < 2.28`) or Alpine/musl Linux** — build from source with Cargo (`cargo build --release`).
- **Verify the binary** — `node -p "require.resolve('@bitefoundry/onenote-win32-x64/package.json')"` (adjust for your platform).
