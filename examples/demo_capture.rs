//! Captures real 120x34 Ratatui TUI frames for `@bitefoundry/onenote` using synthetic English demo data
//! and renders `assets/demo.gif` via the shared `render_tui_gif.py` script.
//!
//! The renderer is not part of this repository. Point `DEMO_RENDERER_SCRIPT` at it, or place it
//! at `scripts/render_tui_gif.py`. Preview PNGs go to `DEMO_PREVIEW_DIR`, default
//! `target/demo-previews/onenote`.

use std::{
    path::{Path, PathBuf},
    process::Command,
};

use anyhow::{bail, Context, Result};
use onenote_cli::{
    auth::{AuthManager, StoredTokens},
    config::{AppConfig, DensityMode, ThemePreset},
    graph::GraphClient,
    img_preview::{TerminalGraphicsCaps, TerminalImageProtocol},
    models::{
        OnenoteNotebook, OnenotePageSummary, OnenoteScope, OnenoteSection, UserProfile,
    },
    parser::parse_onenote_page_xhtml,
    storage::Storage,
    sync::{FocusTarget, SyncCommand, SyncController},
    ui::{
        self,
        app::{AppState, Focus, UiMsg, UiState},
        overlays::{
            command_palette::CommandPalette,
            file_picker::{FilePickerEntry, FilePickerMode, FilePickerOverlay},
            links::LinkExtractorOverlay,
            onenote_form::OnenoteFormOverlay,
            settings::SettingsState,
            Overlay,
        },
        theme::Theme,
    },
};
use ratatui::{
    backend::TestBackend,
    buffer::Buffer,
    style::{Color, Modifier},
    Terminal,
};
use serde_json::{json, Value};
use tokio::sync::{mpsc, watch};

const WIDTH: u16 = 120;
const HEIGHT: u16 = 34;

/// `DEMO_RENDERER_SCRIPT`, or `scripts/render_tui_gif.py` in the repository.
fn renderer_script() -> PathBuf {
    env_path("DEMO_RENDERER_SCRIPT").unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("scripts")
            .join("render_tui_gif.py")
    })
}

/// `DEMO_PREVIEW_DIR`, or `target/demo-previews/onenote` in the repository (git-ignored).
fn preview_dir() -> PathBuf {
    env_path("DEMO_PREVIEW_DIR").unwrap_or_else(|| {
        Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("target")
            .join("demo-previews")
            .join("onenote")
    })
}

fn env_path(name: &str) -> Option<PathBuf> {
    std::env::var_os(name)
        .filter(|value| !value.is_empty())
        .map(PathBuf::from)
}

fn color_to_json(c: Color) -> Value {
    match c {
        Color::Reset => Value::Null,
        Color::Rgb(r, g, b) => json!([r, g, b]),
        Color::Black => json!("Black"),
        Color::Red => json!("Red"),
        Color::Green => json!("Green"),
        Color::Yellow => json!("Yellow"),
        Color::Blue => json!("Blue"),
        Color::Magenta => json!("Magenta"),
        Color::Cyan => json!("Cyan"),
        Color::Gray => json!("Gray"),
        Color::DarkGray => json!("DarkGray"),
        Color::LightRed => json!("LightRed"),
        Color::LightGreen => json!("LightGreen"),
        Color::LightYellow => json!("LightYellow"),
        Color::LightBlue => json!("LightBlue"),
        Color::LightMagenta => json!("LightMagenta"),
        Color::LightCyan => json!("LightCyan"),
        Color::White => json!("White"),
        Color::Indexed(idx) => {
            let (r, g, b) = indexed_to_rgb(idx);
            json!([r, g, b])
        }
    }
}

fn indexed_to_rgb(idx: u8) -> (u8, u8, u8) {
    match idx {
        0 => (24, 26, 33),
        1 => (247, 118, 142),
        2 => (158, 206, 106),
        3 => (224, 175, 104),
        4 => (122, 162, 247),
        5 => (187, 154, 247),
        6 => (125, 207, 255),
        7 => (169, 177, 214),
        8 => (86, 95, 137),
        9 => (255, 140, 160),
        10 => (175, 224, 128),
        11 => (240, 200, 130),
        12 => (148, 184, 255),
        13 => (205, 176, 255),
        14 => (155, 222, 255),
        15 => (240, 244, 252),
        16..=231 => {
            let n = idx - 16;
            let r = n / 36;
            let g = (n % 36) / 6;
            let b = n % 6;
            let to_v = |v: u8| if v == 0 { 0 } else { 55 + v * 40 };
            (to_v(r), to_v(g), to_v(b))
        }
        232..=255 => {
            let v = 8 + (idx - 232) * 10;
            (v, v, v)
        }
    }
}

fn modifier_flags(m: Modifier) -> u8 {
    let mut flags = 0u8;
    if m.contains(Modifier::BOLD) {
        flags |= 1;
    }
    if m.contains(Modifier::ITALIC) {
        flags |= 2;
    }
    if m.contains(Modifier::DIM) {
        flags |= 4;
    }
    if m.contains(Modifier::UNDERLINED) {
        flags |= 8;
    }
    if m.contains(Modifier::REVERSED) {
        flags |= 16;
    }
    flags
}

/// Maps a single Ratatui cell symbol to exactly one crisp glyph so every cell maps 1:1 to a column
/// in `render_tui_gif.py` (including wide-char continuation cells which have empty symbols).
fn normalize_cell_char(sym: &str) -> char {
    let Some(ch) = sym.chars().next() else {
        return ' ';
    };
    match ch {
        '📓' => '■',
        '☐' => '□',
        '☑' => '■',
        '★' => '*',
        '📁' => '▸',
        '📄' => '▪',
        '⚡' => '▸',
        '🏷' => '▪',
        '🖍' => '▸',
        '❓' => '?',
        '❗' => '!',
        other => other,
    }
}

fn buffer_to_rows(buf: &Buffer) -> Value {
    let mut rows = Vec::with_capacity(buf.area.height as usize);
    for y in 0..buf.area.height {
        let mut runs: Vec<Value> = Vec::new();
        let mut cur_text = String::new();
        let mut cur_style: Option<(Color, Color, u8)> = None;
        let mut skip_next = false;

        for x in 0..buf.area.width {
            let cell = &buf[(x, y)];
            let (ch, fg, bg, flags) = if skip_next || cell.skip {
                skip_next = false;
                let prev_bg = cur_style.map(|(_, b, _)| b).unwrap_or(cell.bg);
                (' ', cell.fg, prev_bg, 0)
            } else {
                let sym = cell.symbol();
                if unicode_width::UnicodeWidthStr::width(sym) >= 2 {
                    skip_next = true;
                }
                (
                    normalize_cell_char(sym),
                    cell.fg,
                    cell.bg,
                    modifier_flags(cell.modifier),
                )
            };
            let key = (fg, bg, flags);

            if cur_style == Some(key) {
                cur_text.push(ch);
            } else {
                if let Some((fg, bg, flags)) = cur_style.take() {
                    runs.push(json!([cur_text, color_to_json(fg), color_to_json(bg), flags]));
                    cur_text.clear();
                }
                cur_text.push(ch);
                cur_style = Some(key);
            }
        }
        if let Some((fg, bg, flags)) = cur_style {
            runs.push(json!([cur_text, color_to_json(fg), color_to_json(bg), flags]));
        }
        rows.push(Value::Array(runs));
    }
    Value::Array(rows)
}

fn capture_frame(
    terminal: &mut Terminal<TestBackend>,
    app: &mut AppState,
    ui: &mut UiState,
    duration_ms: u64,
    caption: &str,
    key_badge: &str,
) -> Result<Value> {
    terminal.clear()?;
    terminal.draw(|f| {
        ui::draw(f, app, ui);
    })?;
    let rows = buffer_to_rows(terminal.backend().buffer());
    Ok(json!({
        "duration_ms": duration_ms,
        "caption": caption,
        "key_badge": key_badge,
        "rows": rows,
    }))
}

fn build_demo_state() -> Result<(AppState, UiState)> {
    // Ensure Settings overlay shows a clean fictional config path rather than a host user directory.
    std::env::set_var("ONENOTE_CONFIG_DIR", r"~\.config\onenote-cli");

    let cfg = AppConfig {
        theme: ThemePreset::Dark,
        density: DensityMode::Cozy,
        show_nav_rail: true,
        inline_images: true,
        show_inline_hrefs: false,
        notify: true,
        download_dir: r"~\Downloads\onenote".to_string(),
        ..AppConfig::default()
    };

    let dummy_tokens = StoredTokens {
        access_token: "demo-token".to_string(),
        refresh_token: None,
        expires_at: 2_000_000_000,
        client_id: "demo-client".to_string(),
        tenant_id: "acme-corp".to_string(),
        user_id: Some("usr-alex-chen".to_string()),
        user_display_name: Some("Alex Chen".to_string()),
        user_email: Some("alex.chen@acme.example".to_string()),
        auth_mode: "demo".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), dummy_tokens);
    let graph = GraphClient::new(cfg.clone(), auth.clone())?;
    let storage = Storage::open_in_memory()?;
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel::<UiMsg>();

    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    app.download_dir = PathBuf::from(r"~\Downloads\onenote");
    app.profile = Some(UserProfile {
        id: "usr-alex-chen".to_string(),
        display_name: "Alex Chen".to_string(),
        email: Some("alex.chen@acme.example".to_string()),
        job_title: Some("Staff Systems Engineer".to_string()),
        department: Some("Platform Engineering".to_string()),
    });
    app.sync_summary = "Synced · FTS5 ready".to_string();

    let team_scope = OnenoteScope::Group {
        group_id: "grp-eng".to_string(),
        group_name: Some("Acme Engineering".to_string()),
    };
    let ops_scope = OnenoteScope::Site {
        site_id: "site-sre".to_string(),
        site_name: Some("SRE Hub".to_string()),
    };

    let notebooks = vec![
        OnenoteNotebook {
            notebook_key: "me|nb-eng".to_string(),
            id: "nb-eng".to_string(),
            scope: OnenoteScope::Me,
            display_name: "Acme Engineering".to_string(),
            color: Some("#7719AA".to_string()),
            is_default: true,
            is_shared: false,
            user_role: Some("Owner".to_string()),
            created_at: Some("2026-01-12T09:00:00Z".to_string()),
            last_modified_at: Some("2026-11-19T16:40:00Z".to_string()),
            created_by: Some("Alex Chen".to_string()),
            web_url: Some("https://acme.example/onenote/acme-engineering".to_string()),
        },
        OnenoteNotebook {
            notebook_key: "group:grp-eng|nb-platform".to_string(),
            id: "nb-platform".to_string(),
            scope: team_scope.clone(),
            display_name: "Core Specs".to_string(),
            color: Some("#388BFD".to_string()),
            is_default: false,
            is_shared: true,
            user_role: Some("Contributor".to_string()),
            created_at: Some("2026-03-04T11:20:00Z".to_string()),
            last_modified_at: Some("2026-11-18T14:10:00Z".to_string()),
            created_by: Some("Maya Patel".to_string()),
            web_url: Some("https://acme.example/onenote/platform-specs".to_string()),
        },
        OnenoteNotebook {
            notebook_key: "site:site-sre|nb-runbooks".to_string(),
            id: "nb-runbooks".to_string(),
            scope: ops_scope.clone(),
            display_name: "SRE Ops".to_string(),
            color: Some("#3FB950".to_string()),
            is_default: false,
            is_shared: true,
            user_role: Some("Contributor".to_string()),
            created_at: Some("2026-05-20T08:00:00Z".to_string()),
            last_modified_at: Some("2026-11-17T21:15:00Z".to_string()),
            created_by: Some("Jordan Lee".to_string()),
            web_url: Some("https://acme.example/onenote/incident-runbooks".to_string()),
        },
        OnenoteNotebook {
            notebook_key: "me|nb-personal".to_string(),
            id: "nb-personal".to_string(),
            scope: OnenoteScope::Me,
            display_name: "1:1 & Notes".to_string(),
            color: Some("#D29922".to_string()),
            is_default: false,
            is_shared: false,
            user_role: Some("Owner".to_string()),
            created_at: Some("2026-02-01T10:00:00Z".to_string()),
            last_modified_at: Some("2026-11-19T11:05:00Z".to_string()),
            created_by: Some("Alex Chen".to_string()),
            web_url: Some("https://acme.example/onenote/scratchpad".to_string()),
        },
    ];

    let sections = vec![
        OnenoteSection {
            section_key: "me|sec-arch".to_string(),
            id: "sec-arch".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb-eng".to_string()),
            notebook_name: Some("Acme Engineering".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "Architecture".to_string(),
            color: Some("#A371F7".to_string()),
            is_default: true,
            pages_url: None,
            created_at: None,
            last_modified_at: Some("2026-11-19T16:40:00Z".to_string()),
            web_url: None,
        },
        OnenoteSection {
            section_key: "me|sec-release".to_string(),
            id: "sec-release".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb-eng".to_string()),
            notebook_name: Some("Acme Engineering".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "Release Ops".to_string(),
            color: Some("#388BFD".to_string()),
            is_default: false,
            pages_url: None,
            created_at: None,
            last_modified_at: Some("2026-11-18T17:20:00Z".to_string()),
            web_url: None,
        },
        OnenoteSection {
            section_key: "me|sec-sprint".to_string(),
            id: "sec-sprint".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb-eng".to_string()),
            notebook_name: Some("Acme Engineering".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "Sprint 42".to_string(),
            color: Some("#3FB950".to_string()),
            is_default: false,
            pages_url: None,
            created_at: None,
            last_modified_at: Some("2026-11-19T09:10:00Z".to_string()),
            web_url: None,
        },
        OnenoteSection {
            section_key: "me|sec-adrs".to_string(),
            id: "sec-adrs".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb-eng".to_string()),
            notebook_name: Some("Acme Engineering".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "ADRs".to_string(),
            color: Some("#D29922".to_string()),
            is_default: false,
            pages_url: None,
            created_at: None,
            last_modified_at: Some("2026-11-15T14:00:00Z".to_string()),
            web_url: None,
        },
        OnenoteSection {
            section_key: "group:grp-eng|sec-api".to_string(),
            id: "sec-api".to_string(),
            scope: team_scope.clone(),
            notebook_id: Some("nb-platform".to_string()),
            notebook_name: Some("Platform Specs".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "Graph API v2".to_string(),
            color: Some("#39C5CF".to_string()),
            is_default: true,
            pages_url: None,
            created_at: None,
            last_modified_at: None,
            web_url: None,
        },
        OnenoteSection {
            section_key: "site:site-sre|sec-oncall".to_string(),
            id: "sec-oncall".to_string(),
            scope: ops_scope.clone(),
            notebook_id: Some("nb-runbooks".to_string()),
            notebook_name: Some("Incident Runbooks".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "On-Call Playbooks".to_string(),
            color: Some("#F85149".to_string()),
            is_default: true,
            pages_url: None,
            created_at: None,
            last_modified_at: None,
            web_url: None,
        },
        OnenoteSection {
            section_key: "me|sec-weekly".to_string(),
            id: "sec-weekly".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb-personal".to_string()),
            notebook_name: Some("1:1 & Scratchpad".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "Weekly Notes".to_string(),
            color: Some("#DB6D28".to_string()),
            is_default: true,
            pages_url: None,
            created_at: None,
            last_modified_at: None,
            web_url: None,
        },
    ];

    let page1_summary = OnenotePageSummary {
        page_key: "me|page-rfc".to_string(),
        id: "page-rfc".to_string(),
        scope: OnenoteScope::Me,
        section_id: "sec-arch".to_string(),
        section_name: Some("Architecture".to_string()),
        notebook_id: Some("nb-eng".to_string()),
        notebook_name: Some("Acme Engineering".to_string()),
        title: "Q4 Architecture RFC".to_string(),
        level: 0,
        order: 0,
        created_at: Some("2026-11-14T10:00:00Z".to_string()),
        last_modified_at: Some("2026-11-19T16:40:00Z".to_string()),
        content_url: None,
        web_url: Some("https://acme.example/onenote/pages/q4-platform-rfc".to_string()),
        preview_text: Some("Edge cache topology, WAL replication, and rollout checklist".to_string()),
        has_todo: true,
    };

    let page2_summary = OnenotePageSummary {
        page_key: "me|page-migration".to_string(),
        id: "page-migration".to_string(),
        scope: OnenoteScope::Me,
        section_id: "sec-arch".to_string(),
        section_name: Some("Architecture".to_string()),
        notebook_id: Some("nb-eng".to_string()),
        notebook_name: Some("Acme Engineering".to_string()),
        title: "Migration Cutover".to_string(),
        level: 1,
        order: 1,
        created_at: Some("2026-11-16T09:15:00Z".to_string()),
        last_modified_at: Some("2026-11-19T15:12:00Z".to_string()),
        content_url: None,
        web_url: Some("https://acme.example/onenote/pages/migration-plan".to_string()),
        preview_text: Some("Dual-write shadow verification and canary cutover steps".to_string()),
        has_todo: true,
    };

    let page3_summary = OnenotePageSummary {
        page_key: "me|page-bench".to_string(),
        id: "page-bench".to_string(),
        scope: OnenoteScope::Me,
        section_id: "sec-arch".to_string(),
        section_name: Some("Architecture".to_string()),
        notebook_id: Some("nb-eng".to_string()),
        notebook_name: Some("Acme Engineering".to_string()),
        title: "FTS5 Benchmarks".to_string(),
        level: 1,
        order: 2,
        created_at: Some("2026-11-17T11:00:00Z".to_string()),
        last_modified_at: Some("2026-11-18T19:05:00Z".to_string()),
        content_url: None,
        web_url: Some("https://acme.example/onenote/pages/fts5-benchmarks".to_string()),
        preview_text: Some("Sub-2ms trigram search latency across 50k cached pages".to_string()),
        has_todo: false,
    };

    let page4_summary = OnenotePageSummary {
        page_key: "me|page-canary".to_string(),
        id: "page-canary".to_string(),
        scope: OnenoteScope::Me,
        section_id: "sec-arch".to_string(),
        section_name: Some("Architecture".to_string()),
        notebook_id: Some("nb-eng".to_string()),
        notebook_name: Some("Acme Engineering".to_string()),
        title: "Canary Guardrails".to_string(),
        level: 2,
        order: 3,
        created_at: Some("2026-11-17T14:00:00Z".to_string()),
        last_modified_at: Some("2026-11-18T16:30:00Z".to_string()),
        content_url: None,
        web_url: Some("https://acme.example/onenote/pages/canary-rollback".to_string()),
        preview_text: Some("Automated p99 latency abort thresholds".to_string()),
        has_todo: true,
    };

    let page5_summary = OnenotePageSummary {
        page_key: "me|page-sync".to_string(),
        id: "page-sync".to_string(),
        scope: OnenoteScope::Me,
        section_id: "sec-arch".to_string(),
        section_name: Some("Architecture".to_string()),
        notebook_id: Some("nb-eng".to_string()),
        notebook_name: Some("Acme Engineering".to_string()),
        title: "IPC Sync Protocol".to_string(),
        level: 0,
        order: 4,
        created_at: Some("2026-11-10T08:45:00Z".to_string()),
        last_modified_at: Some("2026-11-16T18:22:00Z".to_string()),
        content_url: None,
        web_url: Some("https://acme.example/onenote/pages/ipc-daemon".to_string()),
        preview_text: Some("Two-lane adaptive polling & local socket broadcast".to_string()),
        has_todo: false,
    };

    let page1_html = r#"<html><body><div id="div:{rfc-main}">
        <h2 id="h2:overview">1. Topology Overview</h2>
        <p id="p:intro" data-tag="important">Target: cut p99 read latency below <b>15ms</b> using local SQLite WAL + <a href="https://docs.acme.example/rfc-412">RFC-412 Spec</a> and <a href="https://metrics.acme.example/edge-cache">Live Dashboard</a>.</p>
        <p id="p:todo1" data-tag="to-do:completed">Benchmark WAL 4-reader pool under 64 agents</p>
        <p id="p:todo2" data-tag="to-do">Enable surgical data-id PATCH for checklist toggles</p>
        <p id="p:todo3" data-tag="to-do">Publish OpenAPI 3.1 to <a href="https://api.acme.example/openapi">Developer Portal</a></p>
        <table id="tbl:slo">
            <tr><th>Component</th><th>p50</th><th>p99</th><th>Status</th></tr>
            <tr><td>SQLite FTS5</td><td>1.1ms</td><td>3.8ms</td><td>Verified</td></tr>
            <tr><td>Graph Delta Sync</td><td>42ms</td><td>118ms</td><td>Canary</td></tr>
        </table>
        <pre id="pre:cfg"><code class="language-toml">[sync]
active_poll_ms = 10000
wal_readers = 4</code></pre>
    </div></body></html>"#;

    let page2_html = r#"<html><body><div id="div:{mig-main}">
        <h2 id="h2:cutover">Canary Cutover Sequence</h2>
        <p id="p:mig-summary">Coordinate rollout with SRE via <a href="https://runbooks.acme.example/cutover-v4">Cutover Runbook</a>, verify schema diff in <a href="https://git.acme.example/platform/pr/842">PR #842</a>, and track alerts on <a href="https://status.acme.example/incidents">Status Board</a>.</p>
        <p id="p:m1" data-tag="to-do:completed">Snapshot production metadata &amp; validate checksums</p>
        <p id="p:m2" data-tag="to-do:completed">Replay 24h shadow writes against staging cluster</p>
        <p id="p:m3" data-tag="to-do">Promote US-East canary to 25% traffic &amp; inspect SLOs</p>
        <p id="p:m4" data-tag="to-do">Flip global read alias after 60m zero-error window</p>
        <blockquote id="q:note">Rollback stays a single DNS/feature-flag revert for 72 hours post-cutover.</blockquote>
        <pre id="pre:cli"><code class="language-bash">onenote check "Migration Cutover" p:m3 --done
onenote search "canary" --json</code></pre>
    </div></body></html>"#;

    let page1_detail = parse_onenote_page_xhtml(page1_summary.clone(), page1_html);
    let page2_detail = parse_onenote_page_xhtml(page2_summary.clone(), page2_html);

    let pages = vec![
        page1_summary,
        page2_summary,
        page3_summary,
        page4_summary,
        page5_summary,
    ];

    app.storage.upsert_onenote_notebooks(&notebooks)?;
    app.storage.upsert_onenote_sections(&sections)?;
    app.storage.upsert_onenote_pages(&pages)?;
    app.storage.upsert_onenote_page_detail(&page1_detail)?;
    app.storage.upsert_onenote_page_detail(&page2_detail)?;

    app.notebooks = notebooks;
    app.sections = sections;
    app.pages = pages;
    app.page_details
        .insert(page1_detail.summary.page_key.clone(), page1_detail);
    app.page_details
        .insert(page2_detail.summary.page_key.clone(), page2_detail);
    app.active_notebook_key = Some("me|nb-eng".to_string());
    app.active_section_key = Some("me|sec-arch".to_string());
    app.active_page_key = Some("me|page-rfc".to_string());

    let mut ui = UiState::from_config(&cfg);
    ui.focus = Focus::Canvas;
    ui.theme_preset = ThemePreset::Dark;
    ui.theme = Theme::midnight_dark();
    ui.graphics_caps = TerminalGraphicsCaps {
        protocol: TerminalImageProtocol::Kitty,
        ..TerminalGraphicsCaps::default()
    };
    ui.onenote_selected_block = 3; // highlight active todo item
    ui.onenote_scroll = 0;

    Ok((app, ui))
}

fn build_demo_file_picker() -> FilePickerOverlay {
    let base = PathBuf::from("~/workspace/acme-platform");
    FilePickerOverlay {
        current_dir: base.clone(),
        entries: vec![
            FilePickerEntry {
                name: "diagrams".to_string(),
                path: base.join("diagrams"),
                is_dir: true,
                size_bytes: 0,
            },
            FilePickerEntry {
                name: "specs".to_string(),
                path: base.join("specs"),
                is_dir: true,
                size_bytes: 0,
            },
            FilePickerEntry {
                name: "architecture-topology.png".to_string(),
                path: base.join("architecture-topology.png"),
                is_dir: false,
                size_bytes: 248_320,
            },
            FilePickerEntry {
                name: "fts5-benchmarks.csv".to_string(),
                path: base.join("fts5-benchmarks.csv"),
                is_dir: false,
                size_bytes: 18_944,
            },
            FilePickerEntry {
                name: "migration-checklist.md".to_string(),
                path: base.join("migration-checklist.md"),
                is_dir: false,
                size_bytes: 6_412,
            },
            FilePickerEntry {
                name: "q4-capacity-plan.pdf".to_string(),
                path: base.join("q4-capacity-plan.pdf"),
                is_dir: false,
                size_bytes: 1_420_800,
            },
            FilePickerEntry {
                name: "wal-trace-flamegraph.svg".to_string(),
                path: base.join("wal-trace-flamegraph.svg"),
                is_dir: false,
                size_bytes: 94_210,
            },
        ],
        filter: String::new(),
        selected: 2,
        mode: FilePickerMode::Browse,
        zoxide_dirs: Vec::new(),
        zoxide_query: String::new(),
        zoxide_selected: 0,
        status: Some("Press z for zoxide jump · Enter to attach to OneNote page".to_string()),
        error: None,
    }
}

fn main() -> Result<()> {
    let (mut app, mut ui) = build_demo_state()?;
    let backend = TestBackend::new(WIDTH, HEIGHT);
    let mut terminal = Terminal::new(backend)?;

    let mut frames = Vec::new();

    // Frame 1: Notebook & Section tree + rich rendered OneNote Page
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        2200,
        "3-pane OneNote TUI · Notebooks, colored section ribbon, subpages & rich XHTML canvas",
        "onenote",
    )?);

    // Frame 2: Navigate to another page in the section (j / k)
    app.active_page_key = Some("me|page-migration".to_string());
    ui.focus = Focus::Pages;
    ui.onenote_scroll = 0;
    ui.onenote_selected_block = 4;
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        1900,
        "Navigate pages & nested subpages instantly from local SQLite WAL cache",
        "j / k",
    )?);

    // Frame 3: Toggle inline href URLs (h)
    ui.focus = Focus::Canvas;
    ui.show_inline_hrefs = true;
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        1900,
        "Toggle inline hyperlink URLs directly inside rendered OneNote blocks",
        "h",
    )?);

    // Frame 4: Open Link Extractor overlay (u)
    if let Some(detail) = app.active_page_detail().cloned() {
        let mut links_ov = LinkExtractorOverlay::from_onenote_page(&detail);
        links_ov.selected = 1;
        ui.overlays.push(Overlay::links(links_ov));
    }
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        1900,
        "Link Extractor · jump (1-9), copy URL (y), or open in browser (Enter)",
        "u",
    )?);
    ui.overlays.clear();

    // Frame 5: Open New Page / Quick Note form (n)
    ui.show_inline_hrefs = false;
    let mut form = OnenoteFormOverlay::create_page(
        "me|sec-arch".to_string(),
        "Architecture".to_string(),
        0,
    );
    form.title = "Incident 2026-11 Postmortem".to_string();
    form.body = "## Summary\nEdge cache failback completed in 14s with zero dropped requests.\n\n## Action Items\n- [x] Verify WAL checkpoint latency across replicas\n- [ ] Add automated synthetic probe for Graph delta sync\n- [ ] Attach latency flamegraph (Ctrl+V image / Ctrl+F file)".to_string();
    form.field = 1;
    ui.overlays.push(Overlay::onenote_form(form));
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        2100,
        "Markdown Page Composer · bidirectional GFM, - [ ] tasks & Ctrl+V clipboard PNG",
        "n",
    )?);

    // Frame 6: Open File Picker overlay (Ctrl+F) on top of composer
    ui.overlays.push(Overlay::FilePicker(build_demo_file_picker()));
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        1900,
        "Interactive File Picker · type to filter or press z for zoxide directory jump",
        "Ctrl+F",
    )?);
    ui.overlays.clear();

    // Frame 7: Open Command Palette (Ctrl+P)
    let mut palette = CommandPalette::new();
    palette.query = ">".to_string();
    palette.selected = 2;
    ui.overlays.push(Overlay::CommandPalette(palette));
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        1900,
        "Unified Command Palette · fuzzy jump across notebooks, sections, pages & actions",
        "Ctrl+P",
    )?);
    ui.overlays.clear();

    // Frame 8: Open Settings overlay (Ctrl+,)
    ui.overlays.push(Overlay::Settings(SettingsState { selected: 4 }));
    frames.push(capture_frame(
        &mut terminal,
        &mut app,
        &mut ui,
        2100,
        "Live Settings · themes, list density, Kitty / iTerm2 / Sixel / Halfblock images",
        "Ctrl+,",
    )?);
    ui.overlays.clear();

    let spec = json!({
        "title": "onenote — Acme Engineering · Microsoft OneNote TUI",
        "badge": "@bitefoundry/onenote v0.1.0",
        "width": WIDTH,
        "height": HEIGHT,
        "bg": [17, 19, 24],
        "fg": [226, 232, 240],
        "frames": frames,
    });

    let frames_path = std::env::temp_dir().join("onenote_demo_frames.json");
    std::fs::write(&frames_path, serde_json::to_vec(&spec)?)
        .with_context(|| format!("Failed to write {}", frames_path.display()))?;

    let repo_root = Path::new(env!("CARGO_MANIFEST_DIR"));
    let out_gif = repo_root.join("assets").join("demo.gif");

    let render_script = renderer_script();
    if !render_script.is_file() {
        bail!(
            "GIF renderer not found at {}. Set DEMO_RENDERER_SCRIPT to the path of render_tui_gif.py.",
            render_script.display()
        );
    }
    let preview_dir = preview_dir();
    std::fs::create_dir_all(&preview_dir)
        .with_context(|| format!("Failed to create {}", preview_dir.display()))?;

    let status = Command::new("python")
        .arg(&render_script)
        .arg(&frames_path)
        .arg(&out_gif)
        .arg("--preview-png")
        .arg(&preview_dir)
        .status()
        .context("Failed to execute render_tui_gif.py")?;

    if !status.success() {
        bail!("render_tui_gif.py exited with status {status}");
    }

    println!("Wrote frames spec: {}", frames_path.display());
    println!("Wrote demo GIF:    {}", out_gif.display());
    println!("Preview PNG dir:   {}", preview_dir.display());
    Ok(())
}
