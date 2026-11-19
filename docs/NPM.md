# npm distribution

`@shinick/onenote` is distributed on npm as a small JavaScript launcher plus one
optional dependency per platform. The launcher resolves the prebuilt Rust binary
and execs it directly (with `scripts/postinstall.mjs` linking the native binary
into `bin/` and Windows shims for zero-Node-startup execution), so there is no
compiler, Node native addon, or network download during install.

The Rust crate is named `onenote-cli` and produces the standalone **Microsoft OneNote**
binary (`onenote` / `onenote.exe`). The npm package is scoped as `@shinick/onenote`
and installs both **`onenote`** and the short alias **`note`**.

## Install and run

```sh
# one-off
npx @shinick/onenote --version

# global install (installs `onenote` and `note`)
npm install -g @shinick/onenote
onenote --version
note --version

# project-local
npm install @shinick/onenote
npx onenote notebooks
```

All workflows share the same Microsoft Entra ID login (or automatic M365 shared-session
bootstrap from `teams-cli` / `outlook-cli`), SQLite WAL + FTS5 cache (`cache.db`),
background sync daemon (`onenote daemon start | status | stop | restart`), local HTTP API
(`onenote serve`, default `127.0.0.1:8788`), MCP server (`onenote mcp`), and multi-protocol
terminal image preview (`Kitty` / `iTerm2` / `Sixel` / `Halfblock` via `onenote preview-file`
and the interactive TUI).

- `onenote login`, `onenote logout`, `onenote whoami`, `onenote doctor --online`
- `onenote notebooks`, `onenote sections [notebook]`, `onenote pages <section>`, `onenote read <page>`
- `onenote create-notebook <name>`, `onenote create-section <notebook> <name>`
- `onenote create <section> <title> [--body ...] [--image ...] [--level 0|1|2]`
- `onenote append <page> [--body ...] [--image ...]`
- `onenote update <page> [--block-id <id> --body ...] [--title ...] [--body ...]`
- `onenote check <page> <block-id> [--done | --undone]`, `onenote delete <page>`
- `onenote search <query>`, `onenote resource <page> [index] [--out ...]`
- `onenote sync`, `onenote daemon <start|status|stop|restart|run>`, `onenote tui`, `onenote serve`, `onenote mcp`, `onenote preview-file <path>`, `onenote self-update`

## Supported platforms

| npm package                     | OS      | CPU   | Rust target                         |
| ------------------------------- | ------- | ----- | ----------------------------------- |
| `@shinick/onenote-win32-x64`    | Windows | x64   | `x86_64-pc-windows-msvc`            |
| `@shinick/onenote-win32-arm64`  | Windows | arm64 | `aarch64-pc-windows-msvc`           |
| `@shinick/onenote-darwin-x64`   | macOS   | x64   | `x86_64-apple-darwin`               |
| `@shinick/onenote-darwin-arm64` | macOS   | arm64 | `aarch64-apple-darwin`              |
| `@shinick/onenote-linux-x64`    | Linux   | x64   | `x86_64-unknown-linux-gnu` (2.28)   |
| `@shinick/onenote-linux-arm64`  | Linux   | arm64 | `aarch64-unknown-linux-gnu` (2.28)  |

Linux packages are cross-compiled in CI with `cargo-zigbuild` against **GLIBC 2.28** (`*.2.28`)
and verified with `scripts/verify-glibc.mjs` plus an `almalinux:8` container smoke test, so
prebuilt binaries run out of the box on **AlmaLinux 8 / RHEL 8+ / Rocky Linux 8+ / Ubuntu 20.04+ / Debian 10+**.
Older glibc distributions (e.g., CentOS 7 with `GLIBC < 2.28`) and Alpine/musl Linux should
build from source with Cargo (`cargo build --release`).

## How resolution works

`bin/onenote.js` and `bin/note.js` look up `@shinick/onenote-<platform>-<arch>` through
`require.resolve`, then execute `node_modules/@shinick/onenote-<platform>-<arch>/bin/onenote`
(or `onenote.exe` on Windows) with `stdio: 'inherit'`. On `npm install`, `scripts/postinstall.mjs`
additionally links the native binary directly for zero-Node-startup execution.
Signals (`SIGINT`, `SIGTERM`, `SIGHUP`) are forwarded and the launcher exits with the child's
exit code. If the platform package is missing, the launcher prints the supported platforms
and the `--include=optional` hint instead of a stack trace.

## Layout

```
package.json                  # @shinick/onenote launcher package (bin: onenote, note)
bin/onenote.js                # onenote launcher
bin/note.js                   # note shorthand launcher
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

### Required Microsoft Graph delegated scopes

`offline_access`, `User.Read`, `Notes.Read`, `Notes.ReadWrite`, `Notes.Create`

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
| `ONENOTE_IMAGE_PROTOCOL` | Terminal image protocol override (`auto`, `kitty`, `iterm2`, `sixel`, `halfblock`) |
| `ONENOTE_GITHUB_REPO` | Override GitHub `owner/repo` used by `onenote self-update` (default `shinick-dev/onenote`) |
| `ONENOTE_GITHUB_TOKEN` | GitHub token for private release checks in `onenote self-update` |

## Troubleshooting

- **`no prebuilt binary found for <platform>-<arch>`** — reinstall with
  `npm install -g @shinick/onenote --include=optional`.
- **Older Linux (`GLIBC < 2.28`) or Alpine/musl Linux** — build from source with Cargo (`cargo build --release`).
- **Verify the binary** — `node -p "require.resolve('@shinick/onenote-win32-x64/package.json')"` (adjust for your platform).
