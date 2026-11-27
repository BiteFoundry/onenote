//! Core TUI state structs (`AppState`, `UiState`), focus/task types, `UiMsg`, and resource sniffers.

use std::{
    collections::{HashMap, HashSet, VecDeque},
    path::PathBuf,
    sync::Arc,
    time::Instant,
};

use ratatui::layout::Rect;
use ratatui::style::Color;
use tokio::sync::mpsc;

use crate::{
    auth::AuthManager,
    config::{
        AppConfig, DensityMode, DraftStore, ImageProtocolPreference, NoteDraftEntry, ThemePreset,
    },
    graph::GraphClient,
    img_preview::{RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol},
    models::{
        OnenoteNotebook, OnenotePageDetail, OnenotePageSummary, OnenoteSearchHit, OnenoteSection,
        OnenoteSectionGroup, UpdateCheckResult, UserProfile,
    },
    storage::Storage,
    sync::SyncController,
    ui::{
        hitmap::HitMap,
        keymap::KeyMap,
        overlays::Overlay,
        theme::Theme,
        toast::ToastQueue,
    },
};

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Focus {
    Nav,
    Pages,
    Canvas,
    Overlay,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum NarrowPane {
    Notebooks,
    Pages,
    Canvas,
}

#[derive(Debug, Clone)]
pub struct PendingPixelGraphic {
    pub cache_key: String,
    pub rect: Rect,
    pub protocol: TerminalImageProtocol,
    /// Optional pre-encoded terminal escape sequence payload (used by inline HTML preview graphics
    /// whose rendered images live in `HtmlPreviewOverlay`'s cache rather than `AppState::image_cache`).
    pub preencoded_payload: Option<String>,
}

#[derive(Debug, Clone)]
pub struct RunningTask {
    pub id: u64,
    pub label: String,
    pub started_at: Instant,
}

#[derive(Debug, Clone, Default)]
pub struct TaskTracker {
    next_id: u64,
    active: Vec<RunningTask>,
}

impl TaskTracker {
    pub fn start(&mut self, label: impl Into<String>) -> u64 {
        self.next_id = self.next_id.wrapping_add(1);
        let id = self.next_id;
        self.active.push(RunningTask {
            id,
            label: label.into(),
            started_at: Instant::now(),
        });
        id
    }

    pub fn finish(&mut self, id: u64) {
        self.active.retain(|t| t.id != id);
    }

    pub fn first_running(&self) -> Option<&RunningTask> {
        self.active.first()
    }
}

/// Maximum number of raw fetched image byte entries kept in the LRU cache.
pub const MAX_RAW_IMAGE_CACHE_ENTRIES: usize = 8;
/// Maximum total raw image bytes (16 MiB) kept in the LRU cache.
pub const MAX_RAW_IMAGE_CACHE_BYTES: usize = 16 * 1024 * 1024;

/// Asynchronous results sent back from background tasks to the TUI event loop.
#[derive(Debug)]
pub enum UiMsg {
    DaemonAttached {
        pid: Option<u32>,
        sync_busy: bool,
    },
    DaemonDetached,
    StartLocalBootstrap,
    PagesLoaded {
        section_key: String,
        pages: Vec<OnenotePageSummary>,
    },
    PageDetailLoaded(Box<OnenotePageDetail>),
    NotebookCreated {
        task_id: u64,
        notebook: OnenoteNotebook,
    },
    SectionCreated {
        task_id: u64,
        section: OnenoteSection,
    },
    PageCreated {
        task_id: u64,
        section_key: String,
        page: OnenotePageSummary,
        preview_detail: Box<OnenotePageDetail>,
    },
    PageMutated {
        task_id: u64,
        detail: Box<OnenotePageDetail>,
        message: String,
        cleared_draft_key: Option<String>,
    },
    EditConflict {
        task_id: u64,
        detail: Box<OnenotePageDetail>,
        draft_to_preserve: Option<(String, NoteDraftEntry)>,
        message: String,
    },
    PageDeleted {
        task_id: u64,
        page_key: String,
    },
    ResourceDownloaded {
        task_id: u64,
        file_name: String,
        path: String,
        size_bytes: u64,
    },
    ImageBytesFetched {
        resource_url: String,
        bytes: Arc<[u8]>,
    },
    ImageLoaded {
        cache_key: String,
        rendered: RenderedTerminalImage,
    },
    ImageLoadFailed {
        cache_key: String,
        error: String,
    },
    ImageRoutedToHtml {
        cache_key: String,
        title: String,
        html: String,
    },
    HtmlResourceLoaded {
        task_id: u64,
        seq: u64,
        page_key: String,
        title: String,
        mime_type: Option<String>,
        html: String,
    },
    HtmlServed {
        url: String,
    },
    UpdateResult {
        task_id: u64,
        result: UpdateCheckResult,
    },
    TaskFailed {
        task_id: u64,
        error: String,
    },
    MutationFailed {
        task_id: u64,
        page_key: String,
        error: String,
        rollback_detail: Option<Box<OnenotePageDetail>>,
    },
    InlineSaved {
        task_id: u64,
        detail: Box<OnenotePageDetail>,
        cleared_draft_key: Option<String>,
    },
    InlineConflict {
        task_id: u64,
        detail: Box<OnenotePageDetail>,
        draft_to_preserve: Option<(String, NoteDraftEntry)>,
        message: String,
    },
    Info(String),
    Error(String),
}

pub struct UiState {
    pub focus: Focus,
    pub narrow_pane: NarrowPane,
    pub show_nav_rail: bool,
    pub notify: bool,
    pub inline_images: bool,
    pub show_inline_hrefs: bool,
    pub density: DensityMode,
    pub theme_preset: ThemePreset,
    pub theme: Theme,
    pub graphics_caps: TerminalGraphicsCaps,
    /// The user picked an image protocol in this session (settings / shortcut).
    pub image_protocol_user_chosen: bool,
    pub kitty_keyboard_supported: bool,
    pub term_width: u16,
    pub term_height: u16,
    pub hitmap: HitMap,
    pub keymap: KeyMap,
    pub overlays: Vec<Overlay>,
    pub toasts: ToastQueue,
    pub tasks: TaskTracker,
    pub spinner_tick: usize,
    pub poll_retrying: bool,
    pub editor_requested: bool,
    pub onenote_scroll: usize,
    pub onenote_selected_block: usize,
    pub onenote_follow_selection: bool,
    pub onenote_canvas_cache: Option<crate::ui::onenote_view::CanvasRenderCache>,
    pub inline_editor: Option<crate::ui::inline_editor::OnenoteInlineEditor>,
    pub pending_pixel_graphics: Vec<PendingPixelGraphic>,
    pub active_pixel_graphic: Option<(String, Rect, TerminalImageProtocol)>,
}

impl Default for UiState {
    fn default() -> Self {
        Self::new()
    }
}

impl UiState {
    pub fn new() -> Self {
        Self::from_config(&AppConfig::default())
    }

    pub fn from_config(cfg: &AppConfig) -> Self {
        let (w, h) = crossterm::terminal::size().unwrap_or((120, 36));
        Self {
            focus: Focus::Pages,
            narrow_pane: NarrowPane::Pages,
            show_nav_rail: cfg.show_nav_rail,
            notify: cfg.notify,
            inline_images: cfg.inline_images,
            show_inline_hrefs: cfg.show_inline_hrefs,
            density: cfg.density,
            theme_preset: cfg.theme,
            theme: Theme::detect(cfg.theme),
            graphics_caps: TerminalGraphicsCaps::default(),
            image_protocol_user_chosen: false,
            kitty_keyboard_supported: false,
            term_width: w,
            term_height: h,
            hitmap: HitMap::new(),
            keymap: KeyMap,
            overlays: Vec::new(),
            toasts: ToastQueue::default(),
            tasks: TaskTracker::default(),
            spinner_tick: 0,
            poll_retrying: false,
            editor_requested: false,
            onenote_scroll: 0,
            onenote_selected_block: 0,
            onenote_follow_selection: false,
            onenote_canvas_cache: None,
            inline_editor: None,
            pending_pixel_graphics: Vec::new(),
            active_pixel_graphic: None,
        }
    }

    pub fn focus(&self) -> Focus {
        if !self.overlays.is_empty() {
            Focus::Overlay
        } else {
            self.focus
        }
    }

    pub fn accepts_text_input(&self) -> bool {
        if !self.overlays.is_empty() {
            self.overlays
                .last()
                .is_some_and(Overlay::accepts_text)
        } else {
            self.inline_editor.is_some()
        }
    }

    pub fn info(&mut self, msg: impl Into<String>) {
        self.toasts.info(msg);
    }

    pub fn success(&mut self, msg: impl Into<String>) {
        self.toasts.success(msg);
    }

    pub fn warn(&mut self, msg: impl Into<String>) {
        self.toasts.warning(msg);
    }

    pub fn error(&mut self, msg: impl Into<String>) {
        self.toasts.error(msg);
    }

    /// Returns the concrete RGB background color for inline note-canvas image alpha blending.
    pub fn inline_image_bg_rgb(&self) -> (u8, u8, u8) {
        let is_light = self.theme.bg_composer == Color::Rgb(255, 255, 255);
        let fallback = if is_light {
            (245, 247, 250)
        } else {
            (20, 22, 26)
        };
        crate::img_preview::color_to_rgb(self.theme.bg_canvas, fallback)
    }

    /// Returns the concrete RGB background color for fullscreen modal image alpha blending.
    pub fn modal_image_bg_rgb(&self) -> (u8, u8, u8) {
        let is_light = self.theme.bg_composer == Color::Rgb(255, 255, 255);
        let fallback = if is_light {
            (224, 229, 240)
        } else {
            (37, 42, 61)
        };
        crate::img_preview::color_to_rgb(self.theme.bg_elevated, fallback)
    }
}

pub struct AppState {
    pub config: AppConfig,
    pub auth: AuthManager,
    pub graph: GraphClient,
    pub storage: Storage,
    pub sync_ctrl: SyncController,
    pub ui_tx: mpsc::UnboundedSender<UiMsg>,
    pub download_dir: PathBuf,
    pub drafts: DraftStore,

    pub profile: Option<UserProfile>,
    pub syncing: bool,
    pub sync_summary: String,
    pub daemon_pid: Option<u32>,

    pub notebooks: Vec<OnenoteNotebook>,
    pub section_groups: Vec<OnenoteSectionGroup>,
    pub sections: Vec<OnenoteSection>,
    pub pages: Vec<OnenotePageSummary>,
    pub page_details: HashMap<String, OnenotePageDetail>,
    pub canvas_content_gen: u64,
    pub(super) previous_page_key: Option<String>,

    pub active_notebook_key: Option<String>,
    pub active_section_key: Option<String>,
    pub active_page_key: Option<String>,
    pub loading_onenote_pages: bool,
    pub loading_onenote_detail: bool,

    pub search_query: String,
    pub search_results: Vec<OnenoteSearchHit>,

    pub image_cache: HashMap<String, RenderedTerminalImage>,
    pub inflight_images: HashSet<String>,
    pub image_raw_cache: HashMap<String, Arc<[u8]>>,
    pub(super) image_raw_order: VecDeque<String>,
    pub image_raw_bytes: usize,
    pub html_server: bitefoundry_html::NativeHtmlServer,

    pub(super) html_resource_seq: u64,
    pub(super) active_html_resource_req: Option<(u64, String)>,
    pub(super) locally_mutated_pages: HashMap<String, u64>,
    pub(super) locally_deleted_pages: HashMap<String, u64>,
    pub(super) locally_created_pages: HashMap<String, (String, u64)>,
    pub(super) locally_created_notebooks: HashMap<String, u64>,
    pub(super) locally_created_sections: HashMap<String, u64>,
}

/// Returns `true` when MIME type or file name identifies an HTML resource (`.html`/`.htm`/`.xhtml` or `text/html`).
pub fn is_html_resource_meta(mime_type: Option<&str>, file_name: Option<&str>) -> bool {
    bitefoundry_html::sniff_content_kind(mime_type, file_name, None)
        == bitefoundry_html::ContentKind::Html
}

/// Returns `true` when MIME type or file name identifies a Markdown resource (`.md`/`.markdown`/`.mdx` or `text/markdown`).
pub fn is_markdown_resource_meta(mime_type: Option<&str>, file_name: Option<&str>) -> bool {
    bitefoundry_html::sniff_content_kind(mime_type, file_name, None)
        == bitefoundry_html::ContentKind::Markdown
}

/// Returns `true` when MIME type or file name identifies a previewable HTML or Markdown resource.
pub fn is_markup_resource_meta(mime_type: Option<&str>, file_name: Option<&str>) -> bool {
    matches!(
        bitefoundry_html::sniff_content_kind(mime_type, file_name, None),
        bitefoundry_html::ContentKind::Html | bitefoundry_html::ContentKind::Markdown
    )
}

/// Sniffs downloaded preview bytes via `bitefoundry_html::sniff_content_kind`; returns decoded HTML string
/// when the payload is HTML (even if the URL/MIME claimed to be an image).
pub fn sniff_preview_bytes_as_html(
    mime_hint: Option<&str>,
    file_name: Option<&str>,
    bytes: &[u8],
) -> Option<String> {
    if bitefoundry_html::sniff_content_kind(mime_hint, file_name, Some(bytes))
        == bitefoundry_html::ContentKind::Html
    {
        Some(String::from_utf8_lossy(bytes).into_owned())
    } else {
        None
    }
}

impl AppState {
    pub fn new(
        config: AppConfig,
        auth: AuthManager,
        graph: GraphClient,
        storage: Storage,
        sync_ctrl: SyncController,
        ui_tx: mpsc::UnboundedSender<UiMsg>,
    ) -> Self {
        let download_dir = config.resolved_download_dir();
        let drafts = DraftStore::load();
        Self {
            config,
            auth,
            graph,
            storage,
            sync_ctrl,
            ui_tx,
            download_dir,
            drafts,
            profile: None,
            syncing: false,
            sync_summary: "Ready".to_string(),
            daemon_pid: None,
            notebooks: Vec::new(),
            section_groups: Vec::new(),
            sections: Vec::new(),
            pages: Vec::new(),
            page_details: HashMap::new(),
            canvas_content_gen: 0,
            previous_page_key: None,
            active_notebook_key: None,
            active_section_key: None,
            active_page_key: None,
            loading_onenote_pages: false,
            loading_onenote_detail: false,
            search_query: String::new(),
            search_results: Vec::new(),
            image_cache: HashMap::new(),
            inflight_images: HashSet::new(),
            image_raw_cache: HashMap::new(),
            image_raw_order: VecDeque::new(),
            image_raw_bytes: 0,
            html_server: bitefoundry_html::NativeHtmlServer::new(),
            html_resource_seq: 0,
            active_html_resource_req: None,
            locally_mutated_pages: HashMap::new(),
            locally_deleted_pages: HashMap::new(),
            locally_created_pages: HashMap::new(),
            locally_created_notebooks: HashMap::new(),
            locally_created_sections: HashMap::new(),
        }
    }

    pub fn bump_canvas_gen(&mut self) {
        self.canvas_content_gen = self.canvas_content_gen.wrapping_add(1);
    }
}

/// Copies the settings owned by the UI into `config` before a save. The detected
/// image protocol replaces the stored preference only when the preference is
/// already explicit or the user picked a protocol in this session, so `auto`
/// survives unrelated setting changes.
pub(super) fn apply_ui_settings(config: &mut AppConfig, ui: &UiState) {
    config.show_nav_rail = ui.show_nav_rail;
    config.notify = ui.notify;
    config.inline_images = ui.inline_images;
    config.show_inline_hrefs = ui.show_inline_hrefs;
    config.density = ui.density;
    config.theme = ui.theme_preset;
    if ui.image_protocol_user_chosen || config.image_protocol != ImageProtocolPreference::Auto {
        config.image_protocol = match ui.graphics_caps.protocol {
            TerminalImageProtocol::Kitty => ImageProtocolPreference::Kitty,
            TerminalImageProtocol::ITerm2 => ImageProtocolPreference::Iterm2,
            TerminalImageProtocol::Sixel => ImageProtocolPreference::Sixel,
            TerminalImageProtocol::Halfblock => ImageProtocolPreference::Halfblock,
        };
    }
}
