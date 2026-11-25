//! Core TUI application state (`AppState`), presentation state (`UiState`), and async `UiMsg` handlers.

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
    img_preview::{FitBox, RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol},
    models::{
        CoreEvent, InlineImageUpload, OnenoteNotebook, OnenotePageDetail, OnenotePageSummary,
        OnenoteScope, OnenoteSearchHit, OnenoteSection, OnenoteSectionGroup, OutgoingOnenotePage,
        UpdateCheckResult, UserProfile, POLL_RETRY_SUMMARY,
    },
    parser,
    storage::Storage,
    sync::{FocusTarget, SyncCommand, SyncController},
    ui::{
        hitmap::HitMap,
        keymap::KeyMap,
        notification,
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
    pub inline_editor: Option<crate::ui::inline_editor::OnenoteInlineEditor>,
    pub pending_pixel_graphics: Vec<PendingPixelGraphic>,
    pub active_pixel_graphic: Option<(String, Rect, TerminalImageProtocol)>,
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
    previous_page_key: Option<String>,

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
    image_raw_order: VecDeque<String>,
    pub image_raw_bytes: usize,
    pub html_server: bitefoundry_html::NativeHtmlServer,

    html_resource_seq: u64,
    active_html_resource_req: Option<(u64, String)>,
    locally_mutated_pages: HashMap<String, u64>,
    locally_deleted_pages: HashMap<String, u64>,
    locally_created_pages: HashMap<String, (String, u64)>,
    locally_created_notebooks: HashMap<String, u64>,
    locally_created_sections: HashMap<String, u64>,
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

    pub fn insert_raw_image_cache(&mut self, resource_url: String, bytes: Arc<[u8]>) {
        let entry_bytes = bytes.len();
        if entry_bytes > MAX_RAW_IMAGE_CACHE_BYTES {
            return;
        }
        if let Some(prev) = self.image_raw_cache.remove(&resource_url) {
            self.image_raw_bytes = self.image_raw_bytes.saturating_sub(prev.len());
            self.image_raw_order.retain(|k| k != &resource_url);
        }
        self.image_raw_cache.insert(resource_url.clone(), bytes);
        self.image_raw_order.push_back(resource_url);
        self.image_raw_bytes = self.image_raw_bytes.saturating_add(entry_bytes);

        while self.image_raw_order.len() > MAX_RAW_IMAGE_CACHE_ENTRIES {
            let Some(evicted) = self.image_raw_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.image_raw_cache.remove(&evicted) {
                self.image_raw_bytes = self.image_raw_bytes.saturating_sub(removed.len());
            }
        }

        while self.image_raw_bytes > MAX_RAW_IMAGE_CACHE_BYTES && self.image_raw_order.len() > 1 {
            let Some(evicted) = self.image_raw_order.pop_front() else {
                break;
            };
            if let Some(removed) = self.image_raw_cache.remove(&evicted) {
                self.image_raw_bytes = self.image_raw_bytes.saturating_sub(removed.len());
            }
        }
    }

    fn touch_raw_image_cache(&mut self, resource_url: &str) -> Option<Arc<[u8]>> {
        let bytes = self.image_raw_cache.get(resource_url).cloned()?;
        self.image_raw_order.retain(|k| k != resource_url);
        self.image_raw_order.push_back(resource_url.to_string());
        Some(bytes)
    }

    fn evict_raw_image_urls(&mut self, urls: &HashSet<String>) {
        if urls.is_empty() {
            return;
        }
        for url in urls {
            if let Some(removed) = self.image_raw_cache.remove(url) {
                self.image_raw_bytes = self.image_raw_bytes.saturating_sub(removed.len());
            }
        }
        self.image_raw_order.retain(|k| !urls.contains(k));
    }

    pub fn clear_raw_image_cache(&mut self) {
        self.image_raw_cache.clear();
        self.image_raw_order.clear();
        self.image_raw_bytes = 0;
    }

    pub fn evict_image_cache_for_page(&mut self, page_key: &str) {
        let thumb_prefix = format!("thumb:{page_key}:");
        let hd_prefix = format!("hd:{page_key}:");
        self.image_cache
            .retain(|k, _| !k.starts_with(&thumb_prefix) && !k.starts_with(&hd_prefix));
        self.inflight_images
            .retain(|k| !k.starts_with(&thumb_prefix) && !k.starts_with(&hd_prefix));
        if let Some(detail) = self.page_details.get(page_key) {
            let urls: HashSet<String> = detail
                .resources
                .iter()
                .flat_map(|r| {
                    std::iter::once(r.resource_url.clone())
                        .chain(r.fullres_url.clone())
                })
                .collect();
            self.evict_raw_image_urls(&urls);
        }
    }

    pub fn clear_drafts_for_page(&mut self, page_key: &str) {
        let append_key = format!("append:{page_key}");
        let edit_prefix = format!("edit:{page_key}:");
        let before = self.drafts.notes.len();
        self.drafts
            .notes
            .retain(|k, _| k != &append_key && !k.starts_with(&edit_prefix));
        if self.drafts.notes.len() != before {
            let _ = self.drafts.save();
        }
    }

    pub fn refresh_active_search(&mut self, ui: &UiState) {
        let active_query = ui.overlays.iter().rev().find_map(|ov| match ov {
            Overlay::Search(s) => Some(s.query.clone()),
            _ => None,
        });
        if let Some(q) = active_query {
            self.search_query = q.clone();
            self.search_results = if q.trim().is_empty() {
                Vec::new()
            } else {
                self.storage
                    .search_onenote_pages(&q, None, 40)
                    .unwrap_or_default()
            };
        }
    }

    pub fn cancel_pending_html_resource_load(&mut self) {
        self.active_html_resource_req = None;
    }

    pub fn load_cached_initial(&mut self) {
        self.notebooks = self.storage.list_onenote_notebooks().unwrap_or_default();
        self.section_groups = self.storage.list_onenote_section_groups().unwrap_or_default();
        self.sections = self.storage.list_onenote_sections(None).unwrap_or_default();

        if self.active_notebook_key.is_none() {
            self.active_notebook_key = self
                .notebooks
                .iter()
                .find(|n| n.is_default && n.scope.is_personal())
                .or_else(|| self.notebooks.first())
                .map(|n| n.notebook_key.clone());
        }
        self.ensure_active_section_and_pages_cached();
        if self.active_notebook_key.is_some()
            || self.active_section_key.is_some()
            || self.active_page_key.is_some()
        {
            self.push_focus_to_sync();
        }
        if let Some(pk) = self.active_page_key.clone() {
            if !self.page_details.contains_key(&pk) {
                self.request_page_detail(pk);
            }
        }
    }

    pub fn active_onenote_notebook(&self) -> Option<&OnenoteNotebook> {
        let key = self.active_notebook_key.as_deref()?;
        self.notebooks.iter().find(|n| n.notebook_key == key)
    }

    pub fn sections_for_active_notebook(&self) -> Vec<&OnenoteSection> {
        let Some(nb) = self.active_onenote_notebook() else {
            return self.sections.iter().collect();
        };
        let nb_scope = nb.scope.scope_key();
        let matched: Vec<&OnenoteSection> = self
            .sections
            .iter()
            .filter(|s| {
                s.scope.scope_key() == nb_scope && s.notebook_id.as_deref() == Some(nb.id.as_str())
            })
            .collect();
        if matched.is_empty() && self.notebooks.len() <= 1 {
            self.sections.iter().collect()
        } else {
            matched
        }
    }

    pub fn active_onenote_section(&self) -> Option<&OnenoteSection> {
        let key = self.active_section_key.as_deref()?;
        self.sections.iter().find(|s| s.section_key == key)
    }

    pub fn active_page_summary(&self) -> Option<&OnenotePageSummary> {
        let key = self.active_page_key.as_deref()?;
        self.pages.iter().find(|p| p.page_key == key)
    }

    pub fn active_page_detail(&self) -> Option<&OnenotePageDetail> {
        let key = self.active_page_key.as_deref()?;
        self.page_details.get(key)
    }

    pub fn ensure_active_section_and_pages_cached(&mut self) {
        let nb_sections: Vec<String> = self
            .sections_for_active_notebook()
            .into_iter()
            .map(|s| s.section_key.clone())
            .collect();
        let valid_sec = self
            .active_section_key
            .as_deref()
            .is_some_and(|k| nb_sections.iter().any(|sk| sk == k));
        if !valid_sec {
            self.active_section_key = nb_sections.into_iter().next();
        }
        if let Some(sec_key) = self.active_section_key.clone() {
            self.pages = self.storage.list_onenote_pages(&sec_key).unwrap_or_default();
            let valid_page = valid_sec
                && self
                    .active_page_key
                    .as_deref()
                    .is_some_and(|pk| self.pages.iter().any(|p| p.page_key == pk));
            let page_key = if valid_page {
                self.active_page_key.clone()
            } else {
                self.pages.first().map(|p| p.page_key.clone())
            };
            self.set_active_page(page_key);
        } else {
            self.pages.clear();
            self.set_active_page(None);
        }
    }

    fn set_active_page(&mut self, page_key: Option<String>) {
        if self.active_page_key != page_key {
            if self.active_page_key.is_some() {
                self.previous_page_key = self.active_page_key.take();
            }
            self.active_page_key = page_key;
            self.loading_onenote_detail = false;
            self.page_details.retain(|key, _| {
                Some(key) == self.active_page_key.as_ref()
                    || Some(key) == self.previous_page_key.as_ref()
            });
        }
        if let Some(key) = &self.active_page_key {
            if !self.page_details.contains_key(key) {
                if let Ok(Some(detail)) = self.storage.get_onenote_page_detail(key) {
                    self.page_details.insert(key.clone(), detail);
                }
            }
        }
    }

    fn cache_page_detail(&mut self, detail: OnenotePageDetail) {
        let key = &detail.summary.page_key;
        if Some(key) == self.active_page_key.as_ref() {
            self.loading_onenote_detail = false;
        } else if Some(key) != self.previous_page_key.as_ref() {
            // Sync and mutation handlers have already written the detail to SQLite.
            return;
        }
        self.page_details.insert(key.clone(), detail);
    }

    pub fn push_focus_to_sync(&self) {
        self.sync_ctrl.set_focus(FocusTarget {
            notebook_key: self.active_notebook_key.clone(),
            section_key: self.active_section_key.clone(),
            page_key: self.active_page_key.clone(),
        });
    }

    pub fn select_notebook(&mut self, notebook_key: String, ui: &mut UiState) {
        if ui.inline_editor.is_some() {
            self.flush_inline_editor(ui, true);
        }
        self.active_html_resource_req = None;
        self.active_notebook_key = Some(notebook_key);
        self.active_section_key = None;
        ui.onenote_scroll = 0;
        ui.onenote_selected_block = 0;
        self.ensure_active_section_and_pages_cached();
        if let Some(sec_key) = self.active_section_key.clone() {
            self.request_section_pages(sec_key);
        }
        if let Some(pk) = self.active_page_key.clone() {
            self.request_page_detail(pk);
        }
        self.push_focus_to_sync();
    }

    pub fn select_section(&mut self, section_key: String, ui: &mut UiState) {
        if ui.inline_editor.is_some() {
            self.flush_inline_editor(ui, true);
        }
        self.active_html_resource_req = None;
        if let Some(sec) = self
            .sections
            .iter()
            .find(|s| s.section_key == section_key)
        {
            if let Some(nb_id) = sec.notebook_id.as_deref() {
                self.active_notebook_key = Some(sec.scope.make_entity_key(nb_id));
            }
        }
        self.active_section_key = Some(section_key.clone());
        ui.onenote_scroll = 0;
        ui.onenote_selected_block = 0;
        self.pages = self
            .storage
            .list_onenote_pages(&section_key)
            .unwrap_or_default();
        self.set_active_page(self.pages.first().map(|p| p.page_key.clone()));
        if let Some(pk) = self.active_page_key.clone() {
            self.request_page_detail(pk);
        }
        self.request_section_pages(section_key);
        self.push_focus_to_sync();
    }

    pub fn select_page(&mut self, page_key: String, ui: &mut UiState) {
        if ui
            .inline_editor
            .as_ref()
            .is_some_and(|ed| ed.page_key != page_key)
        {
            self.flush_inline_editor(ui, true);
        }
        self.active_html_resource_req = None;
        self.set_active_page(Some(page_key.clone()));
        ui.onenote_scroll = 0;
        ui.onenote_selected_block = 0;
        self.request_page_detail(page_key);
        self.push_focus_to_sync();
    }

    /// Opens the direct in-place Note Canvas editor for the active page (no modal popup).
    pub fn open_inline_editor(&mut self, ui: &mut UiState, preferred_block: Option<usize>) {
        let Some(detail) = self.active_page_detail().cloned() else {
            ui.warn("Open a OneNote page first");
            return;
        };
        if ui
            .inline_editor
            .as_ref()
            .is_some_and(|ed| ed.page_key == detail.summary.page_key)
        {
            ui.focus = Focus::Canvas;
            ui.narrow_pane = NarrowPane::Canvas;
            return;
        }
        let mut editor =
            crate::ui::inline_editor::OnenoteInlineEditor::open_for_page(&detail, preferred_block);
        let draft_key = format!("inline:{}", detail.summary.page_key);
        if let Some(draft) = self.drafts.get_note(&draft_key) {
            if !draft.body.is_empty() && draft.body != editor.composer.text() {
                editor.composer.load_text(&draft.body);
                editor.mark_dirty();
            }
        }
        ui.focus = Focus::Canvas;
        ui.narrow_pane = NarrowPane::Canvas;
        ui.inline_editor = Some(editor);
    }

    /// Flushes any dirty inline canvas edits optimistically in 0ms and syncs to Graph in the background.
    pub fn flush_inline_editor(&mut self, ui: &mut UiState, close_after: bool) {
        let Some(ed) = ui.inline_editor.as_mut() else {
            return;
        };
        if !ed.dirty {
            if close_after {
                ui.inline_editor = None;
            }
            return;
        }

        let page_key = ed.page_key.clone();
        let page_title = ed.page_title.clone();
        let edited_md = ed.composer.text().to_string();
        let target = ed.target.clone();
        let draft_key = format!("inline:{page_key}");

        let unchanged = match &target {
            crate::ui::inline_editor::InlineEditTarget::PageBody { base_markdown } => {
                edited_md.replace("\r\n", "\n").trim()
                    == base_markdown.replace("\r\n", "\n").trim()
            }
            crate::ui::inline_editor::InlineEditTarget::Block { base_content, .. } => {
                edited_md.replace("\r\n", "\n").trim()
                    == base_content.replace("\r\n", "\n").trim()
            }
            crate::ui::inline_editor::InlineEditTarget::Append => edited_md.trim().is_empty(),
        };
        if unchanged {
            ed.dirty = false;
            ed.dirty_since = None;
            ed.save_status = crate::ui::inline_editor::InlineSaveStatus::Saved;
            self.clear_draft(&draft_key);
            if close_after {
                ui.inline_editor = None;
            }
            return;
        }

        let cursor_line_1based = ed.composer.cursor_line_col_1based().0;
        let total_editor_lines = ed.composer.text().lines().count().max(1);
        ed.mark_syncing();
        if close_after {
            ui.inline_editor = None;
        }

        // Apply 0ms optimistic local preview immediately so exiting or saving feels instant.
        let rollback_detail = self.page_details.get(&page_key).cloned();
        if let Some(current) = rollback_detail.as_ref() {
            match &target {
                crate::ui::inline_editor::InlineEditTarget::PageBody { .. } => {
                    let outgoing = OutgoingOnenotePage {
                        title: current.summary.title.clone(),
                        markdown_body: edited_md.clone(),
                        inline_images: Vec::new(),
                        level: None,
                    };
                    let xhtml = crate::parser::onenote::compile_new_page_xhtml(&outgoing);
                    let preview = crate::parser::onenote::parse_onenote_page_xhtml(
                        current.summary.clone(),
                        &xhtml,
                    );
                    self.page_details.insert(page_key.clone(), preview);
                }
                crate::ui::inline_editor::InlineEditTarget::Append => {
                    let md = if current.body_markdown.trim().is_empty() {
                        edited_md.clone()
                    } else {
                        format!("{}\n\n{}", current.body_markdown.trim_end(), edited_md)
                    };
                    let outgoing = OutgoingOnenotePage {
                        title: current.summary.title.clone(),
                        markdown_body: md,
                        inline_images: Vec::new(),
                        level: None,
                    };
                    let xhtml = crate::parser::onenote::compile_new_page_xhtml(&outgoing);
                    let preview = crate::parser::onenote::parse_onenote_page_xhtml(
                        current.summary.clone(),
                        &xhtml,
                    );
                    self.page_details.insert(page_key.clone(), preview);
                }
                crate::ui::inline_editor::InlineEditTarget::Block { block_index, .. } => {
                    let mut preview = current.clone();
                    if let Some(blk) = preview.blocks.get_mut(*block_index) {
                        let new_spans = vec![crate::models::RichSpan::plain(edited_md.clone())];
                        match &mut blk.kind {
                            crate::models::OnenoteBlockKind::Heading { spans, .. }
                            | crate::models::OnenoteBlockKind::Paragraph { spans }
                            | crate::models::OnenoteBlockKind::ListItem { spans, .. }
                            | crate::models::OnenoteBlockKind::Quote { spans } => {
                                *spans = new_spans;
                            }
                            crate::models::OnenoteBlockKind::CodeBlock { code, .. } => {
                                *code = edited_md.clone();
                            }
                            _ => {}
                        }
                    }
                    self.page_details.insert(page_key.clone(), preview);
                }
            }
        }
        if close_after {
            if let Some(detail) = self.page_details.get(&page_key) {
                if !detail.blocks.is_empty() {
                    let target_blk = match &target {
                        crate::ui::inline_editor::InlineEditTarget::Block { block_index, .. } => {
                            (*block_index).min(detail.blocks.len() - 1)
                        }
                        crate::ui::inline_editor::InlineEditTarget::Append => {
                            detail.blocks.len() - 1
                        }
                        crate::ui::inline_editor::InlineEditTarget::PageBody { .. } => {
                            let ratio_idx = cursor_line_1based
                                .saturating_sub(1)
                                .saturating_mul(detail.blocks.len())
                                / total_editor_lines;
                            ratio_idx.min(detail.blocks.len() - 1)
                        }
                    };
                    ui.onenote_selected_block = target_blk;
                    ui.onenote_follow_selection = true;
                }
            }
        }

        let task_id = ui.tasks.start("Syncing note");
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match target {
                crate::ui::inline_editor::InlineEditTarget::PageBody { base_markdown } => {
                    if base_markdown.trim().is_empty() {
                        match crate::cli::append_onenote_page_helper(
                            &storage,
                            &graph,
                            &page_key,
                            &edited_md,
                            &[],
                        )
                        .await
                        {
                            Ok(detail) => {
                                storage.record_page_mutated(&detail.summary.page_key);
                                let _ = tx.send(UiMsg::InlineSaved {
                                    task_id,
                                    detail: Box::new(detail),
                                    cleared_draft_key: Some(draft_key),
                                });
                            }
                            Err(err) => {
                                let _ = tx.send(UiMsg::MutationFailed {
                                    task_id,
                                    page_key,
                                    error: format!("Failed to save note: {err}"),
                                    rollback_detail: rollback_detail.map(Box::new),
                                });
                            }
                        }
                    } else {
                        match crate::cli::update_onenote_page_guarded(
                            &storage,
                            &graph,
                            &page_key,
                            None,
                            Some(&edited_md),
                            None,
                            Some(&base_markdown),
                            false,
                        )
                        .await
                        {
                            Ok(crate::cli::GuardedUpdateResult::Updated(detail)) => {
                                storage.record_page_mutated(&detail.summary.page_key);
                                let _ = tx.send(UiMsg::InlineSaved {
                                    task_id,
                                    detail: Box::new(detail),
                                    cleared_draft_key: Some(draft_key),
                                });
                            }
                            Ok(crate::cli::GuardedUpdateResult::Conflict { latest, message }) => {
                                let _ = tx.send(UiMsg::InlineConflict {
                                    task_id,
                                    detail: Box::new(latest),
                                    draft_to_preserve: Some((
                                        draft_key,
                                        NoteDraftEntry {
                                            title: page_title,
                                            body: edited_md,
                                            base_content: Some(base_markdown),
                                        },
                                    )),
                                    message: format!("{message} Local draft preserved."),
                                });
                            }
                            Err(err) => {
                                let _ = tx.send(UiMsg::MutationFailed {
                                    task_id,
                                    page_key,
                                    error: format!("Failed to save note: {err}"),
                                    rollback_detail: rollback_detail.map(Box::new),
                                });
                            }
                        }
                    }
                }
                crate::ui::inline_editor::InlineEditTarget::Block {
                    element_id,
                    base_content,
                    ..
                } => {
                    match crate::cli::update_onenote_page_guarded(
                        &storage,
                        &graph,
                        &page_key,
                        None,
                        Some(&edited_md),
                        Some(&element_id),
                        Some(&base_content),
                        false,
                    )
                    .await
                    {
                        Ok(crate::cli::GuardedUpdateResult::Updated(detail)) => {
                            storage.record_page_mutated(&detail.summary.page_key);
                            let _ = tx.send(UiMsg::InlineSaved {
                                task_id,
                                detail: Box::new(detail),
                                cleared_draft_key: Some(draft_key),
                            });
                        }
                        Ok(crate::cli::GuardedUpdateResult::Conflict { latest, message }) => {
                            let _ = tx.send(UiMsg::InlineConflict {
                                task_id,
                                detail: Box::new(latest),
                                draft_to_preserve: Some((
                                    draft_key,
                                    NoteDraftEntry {
                                        title: page_title,
                                        body: edited_md,
                                        base_content: Some(base_content),
                                    },
                                )),
                                message: format!("{message} Local draft preserved."),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::MutationFailed {
                                task_id,
                                page_key,
                                error: format!("Failed to save block: {err}"),
                                rollback_detail: rollback_detail.map(Box::new),
                            });
                        }
                    }
                }
                crate::ui::inline_editor::InlineEditTarget::Append => {
                    match crate::cli::append_onenote_page_helper(
                        &storage,
                        &graph,
                        &page_key,
                        &edited_md,
                        &[],
                    )
                    .await
                    {
                        Ok(detail) => {
                            storage.record_page_mutated(&detail.summary.page_key);
                            let _ = tx.send(UiMsg::InlineSaved {
                                task_id,
                                detail: Box::new(detail),
                                cleared_draft_key: Some(draft_key),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::MutationFailed {
                                task_id,
                                page_key,
                                error: format!("Failed to append content: {err}"),
                                rollback_detail: rollback_detail.map(Box::new),
                            });
                        }
                    }
                }
            }
        });
    }

    pub fn request_section_pages(&mut self, section_key: String) {
        self.loading_onenote_pages = true;
        self.sync_ctrl
            .request(SyncCommand::RefreshSectionPages { section_key });
    }

    pub fn request_page_detail(&mut self, page_key: String) {
        self.loading_onenote_detail = !self.page_details.contains_key(&page_key);
        self.sync_ctrl
            .request(SyncCommand::HydratePage { page_key });
    }

    /// Triggered when the terminal regains focus (`FocusGained`): immediately re-reads
    /// the open page via `HydratePage` (even if section-level timestamps look unchanged),
    /// preserving the last cached view while refreshing.
    pub fn on_terminal_focus_gained(&mut self) {
        if let Some(pk) = self.active_page_key.clone() {
            self.sync_ctrl.request(SyncCommand::HydratePage { page_key: pk });
        } else if let Some(sk) = self.active_section_key.clone() {
            self.sync_ctrl
                .request(SyncCommand::RefreshSectionPages { section_key: sk });
        }
    }

    /// Requests an inline thumbnail or fullscreen HD terminal image render for a OneNote resource URL.
    pub fn request_onenote_image(
        &mut self,
        cache_key: String,
        resource_url: String,
        fit: FitBox,
        bg_rgb: (u8, u8, u8),
        caps: TerminalGraphicsCaps,
        halfblock_only: bool,
    ) {
        self.request_onenote_image_with_meta(
            cache_key,
            resource_url,
            None,
            None,
            fit,
            bg_rgb,
            caps,
            halfblock_only,
        );
    }

    /// Requests a OneNote image resource with optional display name & MIME hint.
    /// Checks the LRU `image_raw_cache` first so zoom (`+`/`-`/`0`), protocol cycle (`p`),
    /// and resize re-render locally without refetching from Graph API.
    /// When `halfblock_only == false` (fullscreen `LoadPreviewImage`), sniffs bytes via
    /// `bitefoundry_html::sniff_content_kind` and routes `ContentKind::Html` to `Overlay::HtmlPreview`.
    #[allow(clippy::too_many_arguments)]
    pub fn request_onenote_image_with_meta(
        &mut self,
        cache_key: String,
        resource_url: String,
        display_name: Option<String>,
        mime_hint: Option<String>,
        fit: FitBox,
        bg_rgb: (u8, u8, u8),
        caps: TerminalGraphicsCaps,
        halfblock_only: bool,
    ) {
        if self.image_cache.contains_key(&cache_key) || self.inflight_images.contains(&cache_key) {
            return;
        }
        self.inflight_images.insert(cache_key.clone());
        let cached_raw = self.touch_raw_image_cache(&resource_url);
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            let fetched: anyhow::Result<(Option<String>, Arc<[u8]>)> = match cached_raw {
                Some(bytes) => Ok((None, bytes)),
                None => match graph.fetch_onenote_resource_bytes(&resource_url).await {
                    Ok((fetched_mime, bytes)) => {
                        let arc: Arc<[u8]> = Arc::from(bytes);
                        Ok((Some(fetched_mime), arc))
                    }
                    Err(err) => Err(err),
                },
            };

            match fetched {
                Ok((fetched_mime_opt, bytes)) => {
                    let effective_mime = match fetched_mime_opt.as_deref() {
                        Some("application/octet-stream") | None => mime_hint.as_deref(),
                        Some(m) => Some(m),
                    };
                    if let Some(html) =
                        sniff_preview_bytes_as_html(effective_mime, display_name.as_deref(), &bytes)
                    {
                        if !halfblock_only {
                            let title = display_name
                                .filter(|s| !s.trim().is_empty())
                                .unwrap_or_else(|| "HTML Resource".to_string());
                            let _ = tx.send(UiMsg::ImageRoutedToHtml {
                                cache_key,
                                title,
                                html,
                            });
                        } else {
                            let _ = tx.send(UiMsg::ImageLoadFailed {
                                cache_key,
                                error: "Resource is HTML, not a raster image".to_string(),
                            });
                        }
                        return;
                    }

                    // Populate LRU raw byte cache on a fresh network fetch.
                    if fetched_mime_opt.is_some() {
                        let _ = tx.send(UiMsg::ImageBytesFetched {
                            resource_url,
                            bytes: Arc::clone(&bytes),
                        });
                    }

                    let render_res = tokio::task::spawn_blocking(move || {
                        if halfblock_only {
                            crate::img_preview::decode_and_render_halfblocks_with_cell_px(
                                &bytes,
                                fit,
                                bg_rgb,
                                caps.cell_px,
                            )
                        } else {
                            crate::img_preview::decode_and_render_for_protocol(
                                &bytes, fit, bg_rgb, caps,
                            )
                        }
                    })
                    .await;
                    match render_res {
                        Ok(Ok(rendered)) => {
                            let _ = tx.send(UiMsg::ImageLoaded {
                                cache_key,
                                rendered,
                            });
                        }
                        Ok(Err(err)) => {
                            let _ = tx.send(UiMsg::ImageLoadFailed {
                                cache_key,
                                error: err.to_string(),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::ImageLoadFailed {
                                cache_key,
                                error: err.to_string(),
                            });
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::ImageLoadFailed {
                        cache_key,
                        error: err.to_string(),
                    });
                }
            }
        });
    }

    /// Fetches a `.html`/`.htm`/`.md` OneNote resource and opens it in `Overlay::HtmlPreview`.
    pub fn spawn_load_html_resource(&mut self, resource_idx: usize, ui: &mut UiState) {
        let Some(detail) = self.active_page_detail() else {
            ui.warn("No page loaded");
            return;
        };
        let page_key = detail.summary.page_key.clone();
        let Some(res) = detail.resources.get(resource_idx).cloned() else {
            ui.warn("Resource not found");
            return;
        };
        let url = res
            .fullres_url
            .as_deref()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or(&res.resource_url)
            .to_string();
        let is_md = is_markdown_resource_meta(Some(&res.mime_type), res.file_name.as_deref());
        let title = res
            .file_name
            .as_deref()
            .or(res.alt_text.as_deref())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .map(str::to_string)
            .unwrap_or_else(|| {
                if is_md {
                    format!("resource-{}.md", resource_idx + 1)
                } else {
                    format!("resource-{}.html", resource_idx + 1)
                }
            });
        let res_mime = res.mime_type.clone();
        self.html_resource_seq = self.html_resource_seq.wrapping_add(1);
        let seq = self.html_resource_seq;
        self.active_html_resource_req = Some((seq, page_key.clone()));
        let kind_label = if is_md { "Markdown" } else { "HTML" };
        let task_id = ui.tasks.start(format!("Loading {kind_label} '{title}'"));
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.fetch_onenote_resource_bytes(&url).await {
                Ok((fetched_mime, bytes)) => {
                    let effective_mime = if fetched_mime.trim().is_empty()
                        || fetched_mime.eq_ignore_ascii_case("application/octet-stream")
                    {
                        if res_mime.trim().is_empty() {
                            None
                        } else {
                            Some(res_mime)
                        }
                    } else {
                        Some(fetched_mime)
                    };
                    let html = String::from_utf8_lossy(&bytes).into_owned();
                    let _ = tx.send(UiMsg::HtmlResourceLoaded {
                        task_id,
                        seq,
                        page_key,
                        title,
                        mime_type: effective_mime,
                        html,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskFailed {
                        task_id,
                        error: format!("Failed to load {kind_label} resource: {err}"),
                    });
                }
            }
        });
    }

    /// Serves HTML natively via the lazy localhost `NativeHtmlServer` and sends `UiMsg::HtmlServed`.
    pub fn spawn_serve_html_in_browser(&mut self, title: String, html: String, ui: &mut UiState) {
        let server = self.html_server.clone();
        let tx = self.ui_tx.clone();
        ui.info("Starting local HTML preview server...");
        tokio::spawn(async move {
            match server.serve_html(&title, &html).await {
                Ok(url) => {
                    let _ = tx.send(UiMsg::HtmlServed { url });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::Error(format!(
                        "Failed to serve HTML locally: {err}"
                    )));
                }
            }
        });
    }

    pub fn save_draft(&mut self, key: impl Into<String>, title: String, body: String) {
        self.save_draft_entry(
            key,
            NoteDraftEntry {
                title,
                body,
                base_content: None,
            },
        );
    }

    pub fn save_draft_entry(&mut self, key: impl Into<String>, entry: NoteDraftEntry) {
        self.drafts.set_note(key, entry);
        let _ = self.drafts.save();
    }

    pub fn clear_draft(&mut self, key: &str) {
        if self.drafts.remove_note(key).is_some() {
            let _ = self.drafts.save();
        }
    }

    pub fn persist_config(&mut self, ui: &UiState) {
        apply_ui_settings(&mut self.config, ui);
        let _ = self.config.save();
    }

    pub fn spawn_create_notebook(&mut self, name: String, ui: &mut UiState) {
        let task_id = ui.tasks.start(format!("Creating notebook '{name}'"));
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.create_onenote_notebook(&OnenoteScope::Me, &name).await {
                Ok(nb) => {
                    let _ = storage.upsert_onenote_notebooks(std::slice::from_ref(&nb));
                    storage.record_notebook_created(&nb.notebook_key);
                    let _ = tx.send(UiMsg::NotebookCreated {
                        task_id,
                        notebook: nb,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskFailed {
                        task_id,
                        error: format!("Failed to create notebook: {err}"),
                    });
                }
            }
        });
    }

    pub fn spawn_create_section(&mut self, notebook_key: String, name: String, ui: &mut UiState) {
        let task_id = ui.tasks.start(format!("Creating section '{name}'"));
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let (scope, nb_id) = crate::models::split_onenote_entity_key(&notebook_key);
        tokio::spawn(async move {
            match graph.create_onenote_section(&scope, &nb_id, &name).await {
                Ok(sec) => {
                    let _ = storage.upsert_onenote_sections(std::slice::from_ref(&sec));
                    storage.record_section_created(&sec.section_key);
                    let _ = tx.send(UiMsg::SectionCreated {
                        task_id,
                        section: sec,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskFailed {
                        task_id,
                        error: format!("Failed to create section: {err}"),
                    });
                }
            }
        });
    }

    pub fn spawn_create_page(
        &mut self,
        section_key: String,
        title: String,
        markdown_body: String,
        inline_images: Vec<InlineImageUpload>,
        level: i32,
        ui: &mut UiState,
    ) {
        let task_id = ui.tasks.start(format!("Creating page '{title}'"));
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let (scope, sec_id) = crate::models::split_onenote_entity_key(&section_key);
        let outgoing = OutgoingOnenotePage {
            title,
            markdown_body,
            inline_images,
            level: (level > 0).then_some(level),
        };
        tokio::spawn(async move {
            match graph.create_onenote_page(&scope, &sec_id, &outgoing).await {
                Ok(page) => {
                    let _ = storage.upsert_onenote_pages(std::slice::from_ref(&page));
                    storage.record_page_created(&page.page_key);
                    let preview_xhtml = crate::parser::onenote::compile_new_page_xhtml(&outgoing);
                    let preview_detail = crate::parser::onenote::parse_onenote_page_xhtml(
                        page.clone(),
                        &preview_xhtml,
                    );
                    let _ = tx.send(UiMsg::PageCreated {
                        task_id,
                        section_key,
                        page,
                        preview_detail: Box::new(preview_detail),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskFailed {
                        task_id,
                        error: format!("Failed to create page: {err}"),
                    });
                }
            }
        });
    }

    pub fn spawn_append_markdown(
        &mut self,
        page_key: String,
        markdown: String,
        inline_images: Vec<InlineImageUpload>,
        ui: &mut UiState,
    ) {
        // Optimistic 0ms preview on canvas while Graph PATCH runs in the background.
        let rollback_detail = self.page_details.get(&page_key).cloned();
        if let Some(current) = rollback_detail.as_ref() {
            if inline_images.is_empty() && !markdown.trim().is_empty() {
                let merged_md = if current.body_markdown.trim().is_empty() {
                    markdown.clone()
                } else {
                    format!("{}\n\n{}", current.body_markdown.trim_end(), markdown)
                };
                let outgoing = OutgoingOnenotePage {
                    title: current.summary.title.clone(),
                    markdown_body: merged_md,
                    inline_images: Vec::new(),
                    level: None,
                };
                let xhtml = crate::parser::onenote::compile_new_page_xhtml(&outgoing);
                let preview = crate::parser::onenote::parse_onenote_page_xhtml(
                    current.summary.clone(),
                    &xhtml,
                );
                self.page_details.insert(page_key.clone(), preview);
            }
        }
        let task_id = ui.tasks.start("Appending to page");
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match crate::cli::append_onenote_page_helper(
                &storage,
                &graph,
                &page_key,
                &markdown,
                &inline_images,
            )
            .await
            {
                Ok(detail) => {
                    let pk = detail.summary.page_key.clone();
                    storage.record_page_mutated(&pk);
                    let _ = tx.send(UiMsg::PageMutated {
                        task_id,
                        detail: Box::new(detail),
                        message: "Appended content to OneNote page".to_string(),
                        cleared_draft_key: Some(format!("append:{pk}")),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::MutationFailed {
                        task_id,
                        page_key,
                        error: format!("Failed to append content: {err}"),
                        rollback_detail: rollback_detail.map(Box::new),
                    });
                }
            }
        });
    }

    pub fn spawn_edit_block(
        &mut self,
        page_key: String,
        element_id: String,
        markdown: String,
        base_content: String,
        ui: &mut UiState,
    ) {
        let task_id = ui.tasks.start("Updating block");
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        let draft_key = format!("edit:{page_key}:{element_id}");
        tokio::spawn(async move {
            match crate::cli::update_onenote_page_guarded(
                &storage,
                &graph,
                &page_key,
                None,
                Some(&markdown),
                Some(&element_id),
                Some(&base_content),
                false,
            )
            .await
            {
                Ok(crate::cli::GuardedUpdateResult::Updated(detail)) => {
                    storage.record_page_mutated(&detail.summary.page_key);
                    let _ = tx.send(UiMsg::PageMutated {
                        task_id,
                        detail: Box::new(detail),
                        message: "Updated OneNote block".to_string(),
                        cleared_draft_key: Some(draft_key),
                    });
                }
                Ok(crate::cli::GuardedUpdateResult::Conflict { latest, message }) => {
                    let _ = tx.send(UiMsg::EditConflict {
                        task_id,
                        detail: Box::new(latest),
                        draft_to_preserve: Some((
                            draft_key,
                            NoteDraftEntry {
                                title: String::new(),
                                body: markdown,
                                base_content: Some(base_content),
                            },
                        )),
                        message: format!("{message} Local draft preserved."),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskFailed {
                        task_id,
                        error: format!("Failed to update block: {err}"),
                    });
                }
            }
        });
    }

    pub fn spawn_toggle_todo(&mut self, block_index: usize, ui: &mut UiState) {
        let Some(detail) = self.active_page_detail().cloned() else {
            ui.warn("No OneNote page loaded");
            return;
        };
        let Some(block) = detail.blocks.get(block_index) else {
            ui.warn("Select a to-do block first");
            return;
        };
        if parser::build_toggle_todo_patch(block).is_none() {
            ui.warn("Selected block is not a OneNote to-do checkbox");
            return;
        }
        let Some(element_id) = block.element_id.clone() else {
            ui.warn("Selected block does not have an editable Graph element ID");
            return;
        };
        let base_content = parser::block_baseline_content(block);
        let page_key = detail.summary.page_key.clone();
        // Optimistic 0ms checkbox flip so toggling feels instantaneous; reverts on conflict/error.
        let rollback_detail = Some(detail.clone());
        if let Some(cached) = self.page_details.get_mut(&page_key) {
            if let Some(blk) = cached.blocks.get_mut(block_index) {
                if let Some(crate::models::OnenoteTagKind::Todo { completed }) = blk.tag.as_mut() {
                    *completed = !*completed;
                }
            }
        }
        let task_id = ui.tasks.start("Toggling to-do");
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match crate::cli::toggle_onenote_todo_guarded(
                &storage,
                &graph,
                &page_key,
                Some(&element_id),
                None,
                Some(&base_content),
                false,
            )
            .await
            {
                Ok(crate::cli::GuardedTodoResult::Updated {
                    detail: updated,
                    completed,
                    ..
                }) => {
                    storage.record_page_mutated(&updated.summary.page_key);
                    let state = if completed { "completed" } else { "reopened" };
                    let _ = tx.send(UiMsg::PageMutated {
                        task_id,
                        detail: Box::new(updated),
                        message: format!("Marked to-do {state}"),
                        cleared_draft_key: None,
                    });
                }
                Ok(crate::cli::GuardedTodoResult::Conflict { latest, message }) => {
                    let _ = tx.send(UiMsg::EditConflict {
                        task_id,
                        detail: Box::new(latest),
                        draft_to_preserve: None,
                        message,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::MutationFailed {
                        task_id,
                        page_key,
                        error: format!("Failed to toggle to-do: {err}"),
                        rollback_detail: rollback_detail.map(Box::new),
                    });
                }
            }
        });
    }

    pub fn spawn_delete_active_page(&mut self, ui: &mut UiState) {
        let Some(page_key) = self.active_page_key.clone() else {
            return;
        };
        let task_id = ui.tasks.start("Deleting page");
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match crate::cli::delete_onenote_page_helper(&storage, &graph, &page_key).await {
                Ok(deleted_key) => {
                    storage.record_page_deleted(&deleted_key);
                    let _ = tx.send(UiMsg::PageDeleted {
                        task_id,
                        page_key: deleted_key,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskFailed {
                        task_id,
                        error: format!("Failed to delete page: {err}"),
                    });
                }
            }
        });
    }

    pub fn spawn_download_resource(&mut self, resource_idx: usize, ui: &mut UiState) {
        let Some(detail) = self.active_page_detail() else {
            ui.warn("No page loaded");
            return;
        };
        let Some(res) = detail.resources.get(resource_idx).cloned() else {
            ui.warn("Resource not found");
            return;
        };
        let dest_dir = self.download_dir.clone();
        let task_id = ui.tasks.start("Downloading resource");
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.download_onenote_resource(&res, &dest_dir).await {
                Ok(dl) => {
                    let _ = tx.send(UiMsg::ResourceDownloaded {
                        task_id,
                        file_name: dl.file_name,
                        path: dl.path,
                        size_bytes: dl.size_bytes,
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskFailed {
                        task_id,
                        error: format!("Failed to download resource: {err}"),
                    });
                }
            }
        });
    }

    pub fn spawn_update_check(&mut self, apply: bool, ui: &mut UiState) {
        let label = if apply {
            "Installing self-update"
        } else {
            "Checking for updates"
        };
        let task_id = ui.tasks.start(label);
        let repo = self.config.github_repo.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match crate::updater::check_or_apply_update(&repo, !apply, false).await {
                Ok(result) => {
                    let _ = tx.send(UiMsg::UpdateResult { task_id, result });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskFailed {
                        task_id,
                        error: format!("Update check failed: {err}"),
                    });
                }
            }
        });
    }

    fn evict_stale_page_details(&mut self, incoming: &[OnenotePageSummary]) {
        for p in incoming {
            let is_stale = self.page_details.get(&p.page_key).is_some_and(|cached| {
                cached.summary.last_modified_at != p.last_modified_at
                    || cached.summary.title != p.title
                    || cached.summary.section_id != p.section_id
            });
            if is_stale {
                self.evict_image_cache_for_page(&p.page_key);
                let retained = (self.active_page_key.as_deref() == Some(p.page_key.as_str()))
                    .then(|| self.storage.get_onenote_page_detail(&p.page_key).ok().flatten())
                    .flatten()
                    .filter(|detail| {
                        detail.summary.last_modified_at == p.last_modified_at
                            && detail.summary.title == p.title
                            && detail.summary.section_id == p.section_id
                    });
                if let Some(detail) = retained {
                    self.page_details.insert(p.page_key.clone(), detail);
                } else {
                    self.page_details.remove(&p.page_key);
                }
            }
        }
    }

    fn evict_removed_section_page_details(
        &mut self,
        section_key: &str,
        incoming: &[OnenotePageSummary],
    ) {
        let keep_keys: HashSet<&str> = incoming.iter().map(|p| p.page_key.as_str()).collect();
        let candidate_keys: Vec<String> = self
            .page_details
            .iter()
            .filter_map(|(pk, cached)| {
                let cached_sec_key = cached
                    .summary
                    .scope
                    .make_entity_key(&cached.summary.section_id);
                (cached_sec_key == section_key && !keep_keys.contains(pk.as_str()))
                    .then(|| pk.clone())
            })
            .collect();
        for pk in candidate_keys {
            match self.storage.get_onenote_page_summary(&pk) {
                Ok(Some(row)) => {
                    // Page still exists in SQLite (moved to another section or preserved during a
                    // transient probe failure): update its section metadata in place instead of evicting.
                    if let Some(detail) = self.page_details.get_mut(&pk) {
                        if !row.section_id.trim().is_empty() {
                            detail.summary.section_id = row.section_id;
                        }
                        detail.summary.section_name = row.section_name;
                        detail.summary.notebook_id = row.notebook_id;
                        detail.summary.notebook_name = row.notebook_name;
                    }
                }
                Ok(None) => {
                    self.evict_image_cache_for_page(&pk);
                    self.page_details.remove(&pk);
                }
                Err(_) => {}
            }
        }
    }

    fn reconnect_active_page_section(&mut self, moved_section_key: String) {
        if let Some(sec) = self
            .sections
            .iter()
            .find(|s| s.section_key == moved_section_key)
        {
            if let Some(nb_id) = sec.notebook_id.as_deref() {
                self.active_notebook_key = Some(sec.scope.make_entity_key(nb_id));
            }
        }
        self.active_section_key = Some(moved_section_key.clone());
        self.pages = self
            .storage
            .list_onenote_pages(&moved_section_key)
            .unwrap_or_default();
        self.push_focus_to_sync();
    }

    fn reconcile_cached_page_details_from_storage(&mut self) {
        let cached_keys: Vec<String> = self.page_details.keys().cloned().collect();
        for pk in cached_keys {
            let Some(cached) = self.page_details.get(&pk) else {
                continue;
            };
            let should_evict = match self.storage.get_onenote_page_summary(&pk) {
                Ok(Some(row)) => {
                    row.last_modified_at != cached.summary.last_modified_at
                        || row.title != cached.summary.title
                        || row.section_id != cached.summary.section_id
                        || self
                            .storage
                            .get_onenote_page_detail(&pk)
                            .ok()
                            .flatten()
                            .is_none()
                }
                Ok(None) => true,
                Err(_) => false,
            };
            if should_evict {
                self.evict_image_cache_for_page(&pk);
                self.page_details.remove(&pk);
            }
        }
    }

    fn sync_detail_summary_into_pages(&mut self, summary: &OnenotePageSummary) {
        if let Some(existing) = self
            .pages
            .iter_mut()
            .find(|p| p.page_key == summary.page_key)
        {
            existing.title = summary.title.clone();
            existing.last_modified_at = summary.last_modified_at.clone();
            existing.preview_text = summary.preview_text.clone();
            existing.has_todo = summary.has_todo;
            if !summary.section_id.trim().is_empty() {
                existing.section_id = summary.section_id.clone();
            }
            if summary.section_name.is_some() {
                existing.section_name = summary.section_name.clone();
            }
        }
    }

    pub fn handle_core_event(&mut self, ui: &mut UiState, ev: CoreEvent) {
        match ev {
            CoreEvent::ProfileLoaded(profile) => {
                self.profile = Some(profile);
            }
            CoreEvent::OnenoteHierarchyUpdated {
                mut notebooks,
                section_groups,
                mut sections,
            } => {
                // Guard against stale hierarchy events dropping locally created notebooks/sections.
                if !self.locally_created_notebooks.is_empty() {
                    if let Ok(local_nbs) = self.storage.list_onenote_notebooks() {
                        for nb in local_nbs {
                            if self.locally_created_notebooks.contains_key(&nb.notebook_key)
                                && !notebooks
                                    .iter()
                                    .any(|existing| existing.notebook_key == nb.notebook_key)
                            {
                                notebooks.push(nb);
                            }
                        }
                    }
                }
                if !self.locally_created_sections.is_empty() {
                    if let Ok(local_secs) = self.storage.list_onenote_sections(None) {
                        for sec in local_secs {
                            if self.locally_created_sections.contains_key(&sec.section_key)
                                && !sections
                                    .iter()
                                    .any(|existing| existing.section_key == sec.section_key)
                            {
                                sections.push(sec);
                            }
                        }
                    }
                }
                self.notebooks = notebooks;
                self.section_groups = section_groups;
                self.sections = sections;
                let prev_sec = self.active_section_key.clone();
                let nb_valid = self
                    .active_notebook_key
                    .as_deref()
                    .is_some_and(|k| self.notebooks.iter().any(|n| n.notebook_key == k));
                if !nb_valid {
                    self.active_notebook_key = self
                        .notebooks
                        .iter()
                        .find(|n| n.is_default && n.scope.is_personal())
                        .or_else(|| self.notebooks.first())
                        .map(|n| n.notebook_key.clone());
                }
                self.ensure_active_section_and_pages_cached();
                let current_pages = self.pages.clone();
                self.evict_stale_page_details(&current_pages);
                self.reconcile_cached_page_details_from_storage();
                if prev_sec != self.active_section_key && self.pages.is_empty() {
                    if let Some(sec_key) = self.active_section_key.clone() {
                        self.request_section_pages(sec_key);
                    }
                }
                if self.active_notebook_key.is_some() || self.active_section_key.is_some() {
                    self.push_focus_to_sync();
                }
                if let Some(pk) = self.active_page_key.clone() {
                    if !self.page_details.contains_key(&pk) && !self.loading_onenote_detail {
                        self.request_page_detail(pk);
                    }
                }
                self.refresh_active_search(ui);
            }
            CoreEvent::OnenotePagesUpdated {
                section_key,
                mut pages,
                notify_candidate,
            } => {
                // Guard against stale queued page-list events resurrecting deleted pages or
                // pruning freshly created local pages.
                if !self.locally_deleted_pages.is_empty() {
                    pages.retain(|p| {
                        !self.locally_deleted_pages.contains_key(&p.page_key)
                            || self
                                .storage
                                .get_onenote_page_summary(&p.page_key)
                                .ok()
                                .flatten()
                                .is_some()
                    });
                }
                if !self.locally_created_pages.is_empty() {
                    let missing_local: Vec<String> = self
                        .locally_created_pages
                        .iter()
                        .filter(|(pk, (sk, _))| {
                            sk == &section_key && !pages.iter().any(|p| &p.page_key == *pk)
                        })
                        .map(|(pk, _)| pk.clone())
                        .collect();
                    for pk in missing_local {
                        if let Ok(Some(summary)) = self.storage.get_onenote_page_summary(&pk) {
                            pages.insert(0, summary);
                        }
                    }
                }
                self.evict_stale_page_details(&pages);
                self.evict_removed_section_page_details(&section_key, &pages);
                if self.active_section_key.as_deref() == Some(section_key.as_str()) {
                    self.loading_onenote_pages = false;
                    let prev_active_pk = self.active_page_key.clone();
                    self.pages = pages;
                    let valid = self
                        .active_page_key
                        .as_deref()
                        .is_some_and(|pk| self.pages.iter().any(|p| p.page_key == pk));
                    if !valid {
                        // If the open page moved to another section, reconnect to its new section
                        // instead of dropping it.
                        let moved_sec = prev_active_pk.as_deref().and_then(|pk| {
                            self.storage
                                .get_onenote_page_summary(pk)
                                .ok()
                                .flatten()
                                .filter(|s| !s.section_id.trim().is_empty())
                                .map(|s| s.scope.make_entity_key(&s.section_id))
                                .filter(|sk| sk != &section_key)
                        });
                        if let Some(new_sec_key) = moved_sec {
                            self.reconnect_active_page_section(new_sec_key);
                        } else {
                            self.set_active_page(self.pages.first().map(|p| p.page_key.clone()));
                        }
                    }
                    if let Some(pk) = self.active_page_key.clone() {
                        if !self.page_details.contains_key(&pk) && !self.loading_onenote_detail {
                            self.request_page_detail(pk);
                        }
                        self.push_focus_to_sync();
                    } else if self.active_notebook_key.is_some()
                        || self.active_section_key.is_some()
                    {
                        self.push_focus_to_sync();
                    }
                }
                self.refresh_active_search(ui);
                if let Some(cand) = notify_candidate {
                    if ui.notify {
                        notification::show(
                            &format!("OneNote · {}", cand.section_name),
                            &notification::chat_body(&cand.title, &cand.preview),
                        );
                    }
                }
            }
            CoreEvent::OnenotePageDetailUpdated(detail) => {
                let key = detail.summary.page_key.clone();
                // Guard against stale queued detail events resurrecting a deleted page or
                // reverting a newer local edit/todo-toggle already persisted in SQLite.
                if self.locally_deleted_pages.contains_key(&key)
                    && self
                        .storage
                        .get_onenote_page_summary(&key)
                        .ok()
                        .flatten()
                        .is_none()
                {
                    return;
                }
                if self.locally_mutated_pages.contains_key(&key) {
                    if let Ok(Some(stored)) = self.storage.get_onenote_page_detail(&key) {
                        if stored.raw_xhtml != detail.raw_xhtml {
                            return;
                        }
                    }
                }
                if self
                    .page_details
                    .get(&key)
                    .is_some_and(|cached| cached.raw_xhtml != detail.raw_xhtml)
                {
                    self.evict_image_cache_for_page(&key);
                }
                if self.active_page_key.as_ref() != Some(&key) {
                    self.sync_detail_summary_into_pages(&detail.summary);
                    self.cache_page_detail(*detail);
                    self.refresh_active_search(ui);
                    return;
                }
                if !detail.summary.section_id.trim().is_empty() {
                    let new_sec_key = detail
                        .summary
                        .scope
                        .make_entity_key(&detail.summary.section_id);
                    if self.active_section_key.as_deref() != Some(new_sec_key.as_str()) {
                        self.reconnect_active_page_section(new_sec_key);
                    }
                }
                // Preserve unsaved form edits untouched; surface a non-destructive drift notice
                // if the block currently open in EditBlock changed remotely.
                if let Some(Overlay::OnenoteForm(ref mut form)) = ui.overlays.last_mut() {
                    if let crate::ui::overlays::onenote_form::OnenoteFormMode::EditBlock {
                        ref page_key,
                        ref element_id,
                        ref base_content,
                        ..
                    } = form.mode
                    {
                        if page_key == &key {
                            let still_matches = parser::find_block_by_element_id(&detail, element_id)
                                .is_some_and(|blk| parser::block_matches_expected(blk, base_content));
                            if !still_matches {
                                form.error = Some(
                                    "Remote block changed; local draft preserved (saving will check conflicts)."
                                        .to_string(),
                                );
                            }
                        }
                    }
                }
                self.sync_detail_summary_into_pages(&detail.summary);
                self.cache_page_detail(*detail);
                self.refresh_active_search(ui);
            }
            CoreEvent::SyncStateChanged { busy, summary } => {
                self.syncing = busy;
                if summary == POLL_RETRY_SUMMARY {
                    ui.poll_retrying = true;
                    self.sync_summary = "Retrying...".to_string();
                } else {
                    ui.poll_retrying = false;
                    self.sync_summary = summary;
                }
            }
            CoreEvent::SyncError(msg) => {
                self.syncing = false;
                self.loading_onenote_pages = false;
                self.loading_onenote_detail = false;
                ui.error(msg);
            }
        }
    }

    pub fn handle_ui_msg(&mut self, ui: &mut UiState, msg: UiMsg) {
        match msg {
            UiMsg::DaemonAttached { pid, sync_busy } => {
                self.daemon_pid = pid;
                self.syncing = sync_busy;
                self.sync_summary = "Daemon connected".to_string();
                // The daemon may have populated the cache before this client subscribed.
                self.load_cached_initial();
            }
            UiMsg::DaemonDetached => {
                self.daemon_pid = None;
                self.sync_summary = "Local sync".to_string();
            }
            UiMsg::StartLocalBootstrap => {
                self.sync_ctrl.request(SyncCommand::Bootstrap);
            }
            UiMsg::PagesLoaded { section_key, pages } => {
                if self.active_section_key.as_deref() == Some(section_key.as_str()) {
                    self.loading_onenote_pages = false;
                    self.pages = pages;
                }
            }
            UiMsg::PageDetailLoaded(detail) => {
                self.cache_page_detail(*detail);
            }
            UiMsg::NotebookCreated { task_id, notebook } => {
                ui.tasks.finish(task_id);
                let name = notebook.display_name.clone();
                let key = notebook.notebook_key.clone();
                let epoch = self.storage.record_notebook_created(&key);
                self.locally_created_notebooks.insert(key.clone(), epoch);
                self.notebooks.retain(|n| n.notebook_key != key);
                self.notebooks.push(notebook);
                self.select_notebook(key, ui);
                ui.success(format!("Created notebook '{name}'"));
            }
            UiMsg::SectionCreated { task_id, section } => {
                ui.tasks.finish(task_id);
                let name = section.display_name.clone();
                let key = section.section_key.clone();
                let epoch = self.storage.record_section_created(&key);
                self.locally_created_sections.insert(key.clone(), epoch);
                self.sections.retain(|s| s.section_key != key);
                self.sections.push(section);
                self.select_section(key, ui);
                ui.success(format!("Created section '{name}'"));
            }
            UiMsg::PageCreated {
                task_id,
                section_key,
                page,
                preview_detail,
            } => {
                ui.tasks.finish(task_id);
                let title = page.title.clone();
                let pk = page.page_key.clone();
                let epoch = self.storage.record_page_created(&pk);
                self.locally_created_pages
                    .insert(pk.clone(), (section_key.clone(), epoch));
                self.locally_deleted_pages.remove(&pk);
                self.clear_draft(&format!("create:{section_key}"));
                self.clear_draft(&format!("new_page:{section_key}"));
                if self.active_section_key.as_deref() == Some(section_key.as_str()) {
                    self.pages.retain(|p| p.page_key != pk);
                    self.pages.insert(0, page);
                    // Display the locally rendered preview immediately while background
                    // `HydratePage` (triggered by `select_page`) upgrades with Graph `data-id`s.
                    self.page_details.insert(pk.clone(), *preview_detail);
                    self.select_page(pk, ui);
                }
                self.refresh_active_search(ui);
                ui.success(format!("Created page '{title}'"));
            }
            UiMsg::PageMutated {
                task_id,
                detail,
                message,
                cleared_draft_key,
            } => {
                ui.tasks.finish(task_id);
                let pk = detail.summary.page_key.clone();
                let epoch = self.storage.record_page_mutated(&pk);
                self.locally_mutated_pages.insert(pk.clone(), epoch);
                self.locally_deleted_pages.remove(&pk);
                if let Some(key) = cleared_draft_key {
                    self.clear_draft(&key);
                }
                self.evict_image_cache_for_page(&pk);
                self.sync_detail_summary_into_pages(&detail.summary);
                self.cache_page_detail(*detail);
                self.refresh_active_search(ui);
                ui.success(message);
            }
            UiMsg::EditConflict {
                task_id,
                detail,
                draft_to_preserve,
                message,
            } => {
                ui.tasks.finish(task_id);
                if let Some((draft_key, entry)) = draft_to_preserve {
                    self.save_draft_entry(draft_key, entry);
                }
                self.evict_image_cache_for_page(&detail.summary.page_key);
                self.sync_detail_summary_into_pages(&detail.summary);
                self.cache_page_detail(*detail);
                ui.warn(message);
            }
            UiMsg::PageDeleted { task_id, page_key } => {
                ui.tasks.finish(task_id);
                let epoch = self.storage.record_page_deleted(&page_key);
                self.locally_deleted_pages.insert(page_key.clone(), epoch);
                self.locally_created_pages.remove(&page_key);
                self.locally_mutated_pages.remove(&page_key);
                self.pages.retain(|p| p.page_key != page_key);
                self.evict_image_cache_for_page(&page_key);
                self.page_details.remove(&page_key);
                self.clear_drafts_for_page(&page_key);
                if self.previous_page_key.as_deref() == Some(page_key.as_str()) {
                    self.previous_page_key = None;
                }
                if self
                    .active_html_resource_req
                    .as_ref()
                    .is_some_and(|(_, pk)| pk == &page_key)
                {
                    self.active_html_resource_req = None;
                }
                if self.active_page_key.as_deref() == Some(page_key.as_str()) {
                    self.active_page_key = None;
                    ui.onenote_scroll = 0;
                    ui.onenote_selected_block = 0;
                    ui.onenote_follow_selection = false;
                    self.set_active_page(self.pages.first().map(|p| p.page_key.clone()));
                    if let Some(pk) = self.active_page_key.clone() {
                        self.request_page_detail(pk);
                    }
                }
                self.push_focus_to_sync();
                self.refresh_active_search(ui);
                ui.success("Deleted OneNote page");
            }
            UiMsg::ResourceDownloaded {
                task_id,
                file_name,
                path,
                size_bytes,
            } => {
                ui.tasks.finish(task_id);
                ui.success(format!("Saved {file_name} ({size_bytes} B) -> {path}"));
            }
            UiMsg::ImageBytesFetched {
                resource_url,
                bytes,
            } => {
                self.insert_raw_image_cache(resource_url, bytes);
            }
            UiMsg::ImageLoaded {
                cache_key,
                rendered,
            } => {
                self.inflight_images.remove(&cache_key);
                self.image_cache.insert(cache_key, rendered);
            }
            UiMsg::ImageLoadFailed { cache_key, error } => {
                self.inflight_images.remove(&cache_key);
                if let Some(Overlay::ImagePreview(ref mut preview)) = ui.overlays.last_mut() {
                    let expected = preview.current_cache_key(
                        ui.term_width,
                        ui.term_height,
                        ui.graphics_caps,
                    );
                    if expected.as_deref() == Some(cache_key.as_str()) {
                        preview.error = Some(error);
                    }
                }
            }
            UiMsg::ImageRoutedToHtml {
                cache_key,
                title,
                html,
            } => {
                self.inflight_images.remove(&cache_key);
                let matches_active_preview = matches!(
                    ui.overlays.last(),
                    Some(Overlay::ImagePreview(preview))
                        if preview
                            .current_cache_key(
                                ui.term_width,
                                ui.term_height,
                                ui.graphics_caps,
                            )
                            .as_deref()
                            == Some(cache_key.as_str())
                );
                if matches_active_preview {
                    ui.overlays.pop();
                    ui.overlays.push(Overlay::HtmlPreview(
                        crate::ui::overlays::html_preview::HtmlPreviewOverlay::new(
                            Some(title),
                            html,
                        ),
                    ));
                    ui.info("Resource returned HTML — opened in interactive HTML viewer");
                }
            }
            UiMsg::HtmlResourceLoaded {
                task_id,
                seq,
                page_key,
                title,
                mime_type,
                html,
            } => {
                ui.tasks.finish(task_id);
                let is_current = self
                    .active_html_resource_req
                    .as_ref()
                    .is_some_and(|(req_seq, req_pk)| *req_seq == seq && req_pk == &page_key)
                    && self.active_page_key.as_deref() == Some(page_key.as_str());
                if is_current {
                    self.active_html_resource_req = None;
                    let kind = bitefoundry_html::sniff_content_kind(
                        mime_type.as_deref(),
                        Some(title.as_str()),
                        Some(html.as_bytes()),
                    );
                    let overlay = if kind == bitefoundry_html::ContentKind::Markdown {
                        crate::ui::overlays::html_preview::HtmlPreviewOverlay::new_markdown(
                            Some(title),
                            html,
                        )
                    } else {
                        crate::ui::overlays::html_preview::HtmlPreviewOverlay::new(
                            Some(title),
                            html,
                        )
                    };
                    ui.overlays.push(Overlay::HtmlPreview(overlay));
                }
            }
            UiMsg::HtmlServed { url } => match open::that(&url) {
                Ok(()) => ui.success(format!("Opened local HTML preview: {url}")),
                Err(err) => ui.error(format!("Served at {url}, but browser open failed: {err}")),
            },
            UiMsg::UpdateResult { task_id, result } => {
                ui.tasks.finish(task_id);
                if result.updated {
                    ui.success(format!(
                        "Updated onenote {} -> {} (restart to use new binary)",
                        result.current_version, result.latest_version
                    ));
                } else if result.update_available {
                    ui.info(format!(
                        "Update available: {} -> {}",
                        result.current_version, result.latest_version
                    ));
                } else {
                    ui.info(format!(
                        "onenote {} is up to date",
                        result.current_version
                    ));
                }
            }
            UiMsg::TaskFailed { task_id, error } => {
                ui.tasks.finish(task_id);
                ui.error(error);
            }
            UiMsg::MutationFailed {
                task_id,
                page_key,
                error,
                rollback_detail,
            } => {
                ui.tasks.finish(task_id);
                if let Some(prev) = rollback_detail {
                    self.page_details.insert(page_key.clone(), *prev);
                }
                if let Some(ref mut ed) = ui.inline_editor {
                    if ed.page_key == page_key {
                        ed.mark_error(error.clone());
                    }
                }
                ui.error(error);
            }
            UiMsg::InlineSaved {
                task_id,
                detail,
                cleared_draft_key,
            } => {
                ui.tasks.finish(task_id);
                let pk = detail.summary.page_key.clone();
                let new_md = detail.body_markdown.clone();
                let epoch = self.storage.record_page_mutated(&pk);
                self.locally_mutated_pages.insert(pk.clone(), epoch);
                self.locally_deleted_pages.remove(&pk);
                if let Some(key) = cleared_draft_key {
                    self.clear_draft(&key);
                }
                if let Some(ref mut ed) = ui.inline_editor {
                    if ed.page_key == pk {
                        let new_base = match &ed.target {
                            crate::ui::inline_editor::InlineEditTarget::PageBody { .. } => {
                                Some(new_md)
                            }
                            crate::ui::inline_editor::InlineEditTarget::Block {
                                element_id, ..
                            } => parser::find_block_by_element_id(&detail, element_id)
                                .map(parser::block_baseline_content),
                            crate::ui::inline_editor::InlineEditTarget::Append => {
                                ed.composer.clear();
                                None
                            }
                        };
                        ed.mark_saved(new_base);
                    }
                }
                self.evict_image_cache_for_page(&pk);
                self.sync_detail_summary_into_pages(&detail.summary);
                self.cache_page_detail(*detail);
                self.refresh_active_search(ui);
            }
            UiMsg::InlineConflict {
                task_id,
                detail,
                draft_to_preserve,
                message,
            } => {
                ui.tasks.finish(task_id);
                if let Some((draft_key, entry)) = draft_to_preserve {
                    self.save_draft_entry(draft_key, entry);
                }
                let pk = detail.summary.page_key.clone();
                if let Some(ref mut ed) = ui.inline_editor {
                    if ed.page_key == pk {
                        ed.mark_conflict(message.clone());
                    }
                }
                self.evict_image_cache_for_page(&pk);
                self.sync_detail_summary_into_pages(&detail.summary);
                self.cache_page_detail(*detail);
                ui.warn(message);
            }
            UiMsg::Info(msg) => ui.info(msg),
            UiMsg::Error(msg) => ui.error(msg),
        }
    }
}

/// Copies the settings owned by the UI into `config` before a save. The detected
/// image protocol replaces the stored preference only when the preference is
/// already explicit or the user picked a protocol in this session, so `auto`
/// survives unrelated setting changes.
fn apply_ui_settings(config: &mut AppConfig, ui: &UiState) {
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::auth::StoredTokens;
    use tokio::sync::watch;

    fn make_test_app_with_cached_summary() -> (
        AppState,
        UiState,
        watch::Receiver<FocusTarget>,
        mpsc::UnboundedReceiver<SyncCommand>,
    ) {
        let cfg = AppConfig::default();
        let tokens = StoredTokens {
            access_token: "test-token".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("user-1".to_string()),
            user_display_name: Some("Tester".to_string()),
            user_email: Some("tester@example.com".to_string()),
            auth_mode: "access_token".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        storage
            .upsert_onenote_notebooks(&[OnenoteNotebook {
                notebook_key: "me|nb-1".to_string(),
                id: "nb-1".to_string(),
                scope: OnenoteScope::Me,
                display_name: "Engineering".to_string(),
                color: None,
                is_default: true,
                is_shared: false,
                user_role: Some("Owner".to_string()),
                created_at: None,
                last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
                created_by: None,
                web_url: None,
            }])
            .unwrap();
        storage
            .upsert_onenote_sections(&[OnenoteSection {
                section_key: "me|sec-1".to_string(),
                id: "sec-1".to_string(),
                scope: OnenoteScope::Me,
                notebook_id: Some("nb-1".to_string()),
                notebook_name: Some("Engineering".to_string()),
                parent_section_group_id: None,
                parent_section_group_name: None,
                display_name: "Specs".to_string(),
                color: None,
                is_default: true,
                pages_url: None,
                created_at: None,
                last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
                web_url: None,
            }])
            .unwrap();
        storage
            .upsert_onenote_pages(&[OnenotePageSummary {
                page_key: "me|page-1".to_string(),
                id: "page-1".to_string(),
                scope: OnenoteScope::Me,
                section_id: "sec-1".to_string(),
                section_name: Some("Specs".to_string()),
                notebook_id: Some("nb-1".to_string()),
                notebook_name: Some("Engineering".to_string()),
                title: "Initial Spec".to_string(),
                level: 0,
                order: 0,
                created_at: Some("2026-11-18T09:00:00Z".to_string()),
                last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
                content_url: None,
                web_url: None,
                preview_text: None,
                has_todo: false,
            }])
            .unwrap();

        let (focus_tx, focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel::<UiMsg>();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);

        let app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let ui = UiState::from_config(&cfg);
        (app, ui, focus_rx, nudge_rx)
    }

    #[test]
    fn test_recent_page_cache_reopens_evicted_details_and_isolates_late_updates() {
        let (mut app, mut ui, _, _) = make_test_app_with_cached_summary();
        app.load_cached_initial();

        let make_detail = |mut summary: OnenotePageSummary, id: &str, body: &str| {
            summary.id = id.to_string();
            summary.page_key = format!("me|{id}");
            parser::parse_onenote_page_xhtml(
                summary,
                &format!("<html><body><p>{body}</p></body></html>"),
            )
        };
        let summary = app.active_page_summary().unwrap().clone();
        let a = make_detail(summary.clone(), "page-1", "First cached body");
        let b = make_detail(summary.clone(), "page-2", "Second cached body");
        for detail in [&a, &b] {
            app.storage.upsert_onenote_page_detail(detail).unwrap();
        }

        app.select_page(a.summary.page_key.clone(), &mut ui);
        app.select_page(b.summary.page_key.clone(), &mut ui);
        assert_eq!(app.page_details.len(), 2);

        let mut form = crate::ui::overlays::onenote_form::OnenoteFormOverlay::edit_block(
            b.summary.page_key.clone(),
            b.summary.title.clone(),
            "p:1".into(),
            0,
            b.body_plain.clone(),
        );
        form.body = "Unsaved draft".into();
        ui.overlays.push(Overlay::OnenoteForm(form));

        app.select_page("me|page-3".into(), &mut ui);
        assert!(!app.page_details.contains_key(&a.summary.page_key));
        for (id, body) in [
            ("page-1", "Late evicted update"),
            ("page-2", "Late previous update"),
        ] {
            let detail = make_detail(summary.clone(), id, body);
            app.storage.upsert_onenote_page_detail(&detail).unwrap();
            app.handle_core_event(&mut ui, CoreEvent::OnenotePageDetailUpdated(Box::new(detail)));
        }
        assert_eq!(app.active_page_key.as_deref(), Some("me|page-3"));
        assert!(app.loading_onenote_detail);
        assert_eq!(app.page_details.len(), 1);
        assert_eq!(app.page_details["me|page-2"].body_plain, "Late previous update");
        let Some(Overlay::OnenoteForm(form)) = ui.overlays.last() else {
            panic!("expected edit form");
        };
        assert_eq!(form.body, "Unsaved draft");
        assert!(form.error.is_none());

        app.select_page(a.summary.page_key.clone(), &mut ui);
        assert_eq!(app.active_page_detail().unwrap().body_plain, "Late evicted update");
        assert!(!app.loading_onenote_detail);
        assert_eq!(app.page_details.len(), 1);

        for i in 3..=21 {
            let detail = make_detail(summary.clone(), &format!("page-{i}"), "Visited body");
            app.select_page(detail.summary.page_key.clone(), &mut ui);
            app.storage.upsert_onenote_page_detail(&detail).unwrap();
            app.handle_core_event(&mut ui, CoreEvent::OnenotePageDetailUpdated(Box::new(detail)));
            assert_eq!(app.page_details.len(), 2);
        }
        app.select_page(a.summary.page_key, &mut ui);
        assert_eq!(app.active_page_detail().unwrap().body_plain, "Late evicted update");
        assert_eq!(app.page_details.len(), 2);
        let stored_keys = app.storage.list_onenote_page_keys("me|sec-1").unwrap();
        assert_eq!(stored_keys.len(), 21);
        assert!(stored_keys.iter().all(|key| app.storage.get_onenote_page_detail(key).unwrap().is_some()));
    }

    #[test]
    fn test_startup_pushes_focus_hydrates_initial_page_and_rehydrates_on_remote_edit() {
        let (mut app, mut ui, mut focus_rx, mut nudge_rx) = make_test_app_with_cached_summary();

        // 1. load_cached_initial must push initial focus and hydrate the selected summary-only page
        app.load_cached_initial();
        assert!(focus_rx.has_changed().unwrap());
        {
            let focus = focus_rx.borrow_and_update();
            assert_eq!(focus.section_key.as_deref(), Some("me|sec-1"));
            assert_eq!(focus.page_key.as_deref(), Some("me|page-1"));
        }
        assert!(app.loading_onenote_detail);
        assert_eq!(
            nudge_rx.try_recv().ok(),
            Some(SyncCommand::HydratePage {
                page_key: "me|page-1".to_string()
            })
        );

        // 2. Deliver hydrated page detail at T1
        let summary_t1 = OnenotePageSummary {
            page_key: "me|page-1".to_string(),
            id: "page-1".to_string(),
            scope: OnenoteScope::Me,
            section_id: "sec-1".to_string(),
            section_name: Some("Specs".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Engineering".to_string()),
            title: "Initial Spec".to_string(),
            level: 0,
            order: 0,
            created_at: Some("2026-11-18T09:00:00Z".to_string()),
            last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
            content_url: None,
            web_url: None,
            preview_text: Some("Old body".to_string()),
            has_todo: false,
        };
        let detail_t1 = OnenotePageDetail {
            summary: summary_t1.clone(),
            raw_xhtml: "<html><body><p>Old body</p></body></html>".to_string(),
            body_markdown: "Old body".to_string(),
            body_plain: "Old body".to_string(),
            blocks: Vec::new(),
            resources: Vec::new(),
            primary_div_id: None,
            synced_at: Some("2026-11-18T10:00:00Z".to_string()),
        };
        app.handle_core_event(&mut ui, CoreEvent::OnenotePageDetailUpdated(Box::new(detail_t1)));
        assert!(!app.loading_onenote_detail);
        assert!(app.page_details.contains_key("me|page-1"));

        // 3. Unchanged OnenotePagesUpdated at T1 keeps cached detail, avoids redundant HydratePage,
        //    and does not re-notify the focus watch channel.
        app.handle_core_event(
            &mut ui,
            CoreEvent::OnenotePagesUpdated {
                section_key: "me|sec-1".to_string(),
                pages: vec![summary_t1.clone()],
                notify_candidate: None,
            },
        );
        assert!(app.page_details.contains_key("me|page-1"));
        assert!(nudge_rx.try_recv().is_err());
        assert!(!focus_rx.has_changed().unwrap());

        // 4. Remote edit arrives with newer last_modified_at = T2 -> evicts stale detail and queues HydratePage
        let mut summary_t2 = summary_t1;
        summary_t2.last_modified_at = Some("2026-11-18T11:00:00Z".to_string());
        app.handle_core_event(
            &mut ui,
            CoreEvent::OnenotePagesUpdated {
                section_key: "me|sec-1".to_string(),
                pages: vec![summary_t2],
                notify_candidate: None,
            },
        );
        assert!(!app.page_details.contains_key("me|page-1"));
        assert!(app.loading_onenote_detail);
        assert_eq!(
            nudge_rx.try_recv().ok(),
            Some(SyncCommand::HydratePage {
                page_key: "me|page-1".to_string()
            })
        );

        // 5. Action::RefreshAll sets syncing state, pushes focus, and requests RefreshAll
        crate::ui::action::dispatch(crate::ui::action::Action::RefreshAll, &mut app, &mut ui);
        assert!(app.syncing);
        assert_eq!(app.sync_summary, "Syncing OneNote...");
        assert_eq!(nudge_rx.try_recv().ok(), Some(SyncCommand::RefreshAll));
    }

    #[test]
    fn test_draft_reopen_canonical_keys_conflict_preservation_and_inactive_section_eviction() {
        let (mut app, mut ui, mut focus_rx, _nudge_rx) = make_test_app_with_cached_summary();

        // Add an inactive section + page so we can verify inactive section eviction and move handling
        app.storage
            .upsert_onenote_sections(&[OnenoteSection {
                section_key: "me|sec-2".to_string(),
                id: "sec-2".to_string(),
                scope: OnenoteScope::Me,
                notebook_id: Some("nb-1".to_string()),
                notebook_name: Some("Engineering".to_string()),
                parent_section_group_id: None,
                parent_section_group_name: None,
                display_name: "InactiveSec".to_string(),
                color: None,
                is_default: false,
                pages_url: None,
                created_at: None,
                last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
                web_url: None,
            }])
            .unwrap();
        let p2_summary = OnenotePageSummary {
            page_key: "me|page-2".to_string(),
            id: "page-2".to_string(),
            scope: OnenoteScope::Me,
            section_id: "sec-2".to_string(),
            section_name: Some("InactiveSec".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Engineering".to_string()),
            title: "Inactive Page v1".to_string(),
            level: 0,
            order: 0,
            created_at: None,
            last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
            content_url: None,
            web_url: None,
            preview_text: Some("Old inactive body".to_string()),
            has_todo: false,
        };
        let p2_detail = crate::parser::parse_onenote_page_xhtml(
            p2_summary.clone(),
            r#"<html><head><title>Inactive Page v1</title></head><body><div id="div:2"><p id="p:2">Old inactive body</p></div></body></html>"#,
        );
        app.storage.upsert_onenote_page_detail(&p2_detail).unwrap();

        let p1_summary = app.storage.get_onenote_page_summary("me|page-1").unwrap().unwrap();
        let p1_detail = crate::parser::parse_onenote_page_xhtml(
            p1_summary,
            r#"<html><head><title>Initial Spec</title></head><body><div id="div:1"><p id="p:1">Alpha paragraph</p></div></body></html>"#,
        );
        app.storage.upsert_onenote_page_detail(&p1_detail).unwrap();

        app.load_cached_initial();
        let _ = focus_rx.borrow_and_update();
        app.page_details.insert("me|page-2".to_string(), p2_detail);
        assert!(app.page_details.contains_key("me|page-2"));

        // 1. CreatePage draft stashed via canonical key ("create:me|sec-1") restores on reopen
        let mut create_form = crate::ui::overlays::onenote_form::OnenoteFormOverlay::create_page(
            "me|sec-1".to_string(),
            "Specs".to_string(),
            0,
        );
        create_form.title = "Draft Page Title".to_string();
        create_form.body = "Unsent draft body".to_string();
        let (ckey, centry) = create_form.draft_entry_snapshot().unwrap();
        assert_eq!(ckey, "create:me|sec-1");
        app.drafts.set_note(ckey, centry);

        crate::ui::action::dispatch(
            crate::ui::action::Action::OpenOnenoteCreatePage { level: 0 },
            &mut app,
            &mut ui,
        );
        match ui.overlays.pop() {
            Some(Overlay::OnenoteForm(form)) => {
                assert_eq!(form.title, "Draft Page Title");
                assert_eq!(form.body, "Unsent draft body");
            }
            other => panic!("expected create form overlay, got {other:?}"),
        }

        // 2. EditBlock draft saved via Esc or conflict restores on reopen & flags remote drift
        crate::ui::action::dispatch(
            crate::ui::action::Action::OpenOnenoteEditBlock,
            &mut app,
            &mut ui,
        );
        if let Some(Overlay::OnenoteForm(ref mut form)) = ui.overlays.last_mut() {
            form.body = "Local unsent edit".to_string();
        }
        // Simulate Esc close -> stashes edit draft with base_content = "Alpha paragraph"
        crate::ui::action::dispatch(crate::ui::action::Action::CloseOverlay, &mut app, &mut ui);
        let stashed = app.drafts.get_note("edit:me|page-1:p:1").unwrap();
        assert_eq!(stashed.body, "Local unsent edit");
        assert_eq!(stashed.base_content.as_deref(), Some("Alpha paragraph"));

        // Simulate remote edit arriving via EditConflict -> preserves local draft & updates detail
        let mut remote_detail = p1_detail.clone();
        remote_detail.summary.title = "Remote Renamed Spec".to_string();
        remote_detail.summary.last_modified_at = Some("2026-11-18T12:00:00Z".to_string());
        remote_detail = crate::parser::parse_onenote_page_xhtml(
            remote_detail.summary.clone(),
            r#"<html><head><title>Remote Renamed Spec</title></head><body><div id="div:1"><p id="p:1">Remote edit by Alice</p></div></body></html>"#,
        );
        app.handle_ui_msg(
            &mut ui,
            UiMsg::EditConflict {
                task_id: 1,
                detail: Box::new(remote_detail),
                draft_to_preserve: Some((
                    "edit:me|page-1:p:1".to_string(),
                    NoteDraftEntry {
                        title: String::new(),
                        body: "Local unsent edit".to_string(),
                        base_content: Some("Alpha paragraph".to_string()),
                    },
                )),
                message: "Conflict: block changed remotely.".to_string(),
            },
        );
        assert_eq!(app.pages[0].title, "Remote Renamed Spec");
        assert!(app.drafts.get_note("edit:me|page-1:p:1").is_some());

        // Reopen EditBlock -> restores local draft AND shows remote drift warning
        crate::ui::action::dispatch(
            crate::ui::action::Action::OpenOnenoteEditBlock,
            &mut app,
            &mut ui,
        );
        match ui.overlays.pop() {
            Some(Overlay::OnenoteForm(form)) => {
                assert_eq!(form.body, "Local unsent edit");
                assert!(form
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains("Remote block changed")));
            }
            other => panic!("expected edit block overlay, got {other:?}"),
        }

        // 3. Inactive section page modified in SQLite -> OnenoteHierarchyUpdated evicts RAM page_details
        let mut p2_newer = p2_summary;
        p2_newer.last_modified_at = Some("2026-11-18T13:00:00Z".to_string());
        p2_newer.title = "Inactive Page v2".to_string();
        app.storage.upsert_onenote_pages(&[p2_newer]).unwrap();
        let nbs = app.storage.list_onenote_notebooks().unwrap();
        let secs = app.storage.list_onenote_sections(None).unwrap();
        app.handle_core_event(
            &mut ui,
            CoreEvent::OnenoteHierarchyUpdated {
                notebooks: nbs,
                section_groups: Vec::new(),
                sections: secs,
            },
        );
        assert!(
            !app.page_details.contains_key("me|page-2"),
            "inactive section stale detail must be evicted on hierarchy update"
        );

        // 4. PageDeleted pushes updated focus to sync engine
        let _ = focus_rx.borrow_and_update();
        app.handle_ui_msg(
            &mut ui,
            UiMsg::PageDeleted {
                task_id: 2,
                page_key: "me|page-1".to_string(),
            },
        );
        assert!(focus_rx.has_changed().unwrap());
        assert_eq!(focus_rx.borrow_and_update().page_key, None);
    }

    #[tokio::test]
    async fn test_tui_spawn_edit_block_and_toggle_todo_preflight_conflict_and_success() {
        use axum::{
            body::Bytes,
            extract::State,
            http::StatusCode,
            routing::get,
            Json, Router,
        };
        use serde_json::json;
        use std::sync::{
            atomic::{AtomicUsize, Ordering},
            Arc, Mutex,
        };

        #[derive(Clone)]
        struct TuiMockState {
            xhtml: Arc<Mutex<String>>,
            patch_hits: Arc<AtomicUsize>,
        }

        let state = TuiMockState {
            xhtml: Arc::new(Mutex::new(
                r#"<!DOCTYPE html><html><head><title>Initial Spec</title></head><body><div id="div:{root}"><p id="p:{b1}">REMOTE EDIT BY ALICE</p><p id="p:{todo1}" data-tag="to-do">Ship sync audit</p></div></body></html>"#
                    .to_string(),
            )),
            patch_hits: Arc::new(AtomicUsize::new(0)),
        };

        let app_router = Router::new()
            .route(
                "/v1.0/me/onenote/pages/page-1",
                get(|| async {
                    Json(json!({
                        "id": "page-1",
                        "title": "Initial Spec",
                        "level": 0,
                        "order": 0,
                        "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                        "parentSection": { "id": "sec-1", "displayName": "Specs" }
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/pages/page-1/content",
                get(|State(st): State<TuiMockState>| async move {
                    let body = st.xhtml.lock().unwrap().clone();
                    (StatusCode::OK, [("content-type", "text/html")], body)
                })
                .patch(|State(st): State<TuiMockState>, body: Bytes| async move {
                    st.patch_hits.fetch_add(1, Ordering::SeqCst);
                    let cmds: Vec<serde_json::Value> =
                        serde_json::from_slice(&body).unwrap_or_default();
                    let mut html = st.xhtml.lock().unwrap();
                    for cmd in cmds {
                        let target = cmd["target"].as_str().unwrap_or("");
                        let content = cmd["content"].as_str().unwrap_or("");
                        if target == "p:{b1}" {
                            if let Some(start) = html.find(r#"<p id="p:{b1}""#) {
                                if let Some(rel_end) = html[start..].find("</p>") {
                                    let repl = content.replacen("<p>", r#"<p id="p:{b1}">"#, 1);
                                    html.replace_range(start..start + rel_end + 4, &repl);
                                }
                            }
                        } else if target == "p:{todo1}" {
                            if let Some(start) = html.find(r#"<p id="p:{todo1}""#) {
                                if let Some(rel_end) = html[start..].find("</p>") {
                                    html.replace_range(start..start + rel_end + 4, content);
                                }
                            }
                        }
                    }
                    StatusCode::NO_CONTENT
                }),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app_router).await;
        });

        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tokens = crate::auth::StoredTokens {
            access_token: "tui-test-token".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("user-1".to_string()),
            user_display_name: Some("Tester".to_string()),
            user_email: None,
            auth_mode: "access_token".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMsg>();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);

        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);

        // Seed TUI with stale local snapshot (before Alice edited p:{b1})
        let stale_summary = OnenotePageSummary {
            page_key: "me|page-1".to_string(),
            id: "page-1".to_string(),
            scope: OnenoteScope::Me,
            section_id: "sec-1".to_string(),
            section_name: Some("Specs".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Engineering".to_string()),
            title: "Initial Spec".to_string(),
            level: 0,
            order: 0,
            created_at: None,
            last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: true,
        };
        let stale_detail = parser::parse_onenote_page_xhtml(
            stale_summary.clone(),
            r#"<!DOCTYPE html><html><head><title>Initial Spec</title></head><body><div id="div:{root}"><p id="p:{b1}">Old local text</p><p id="p:{todo1}" data-tag="to-do">Ship sync audit</p></div></body></html>"#,
        );
        app.storage.upsert_onenote_page_detail(&stale_detail).unwrap();
        app.pages = vec![stale_summary];
        app.active_page_key = Some("me|page-1".to_string());
        app.page_details.insert("me|page-1".to_string(), stale_detail);

        // 1. Stale edit on p:{b1} -> EditConflict, 0 PATCH, remote preserved, draft preserved
        app.spawn_edit_block(
            "me|page-1".to_string(),
            "p:{b1}".to_string(),
            "Local stale edit".to_string(),
            "Old local text".to_string(),
            &mut ui,
        );
        let msg1 = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(msg1, UiMsg::EditConflict { .. }));
        app.handle_ui_msg(&mut ui, msg1);
        assert_eq!(state.patch_hits.load(Ordering::SeqCst), 0);
        assert!(state.xhtml.lock().unwrap().contains("REMOTE EDIT BY ALICE"));
        assert_eq!(
            app.drafts
                .get_note("edit:me|page-1:p:{b1}")
                .map(|d| d.body.as_str()),
            Some("Local stale edit")
        );

        // 2. Different-paragraph to-do toggle (block index 1 = p:{todo1}) -> succeeds, BOTH edits kept
        app.spawn_toggle_todo(1, &mut ui);
        let msg2 = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(msg2, UiMsg::PageMutated { .. }));
        app.handle_ui_msg(&mut ui, msg2);
        assert_eq!(state.patch_hits.load(Ordering::SeqCst), 1);
        let after_todo = state.xhtml.lock().unwrap().clone();
        assert!(after_todo.contains("REMOTE EDIT BY ALICE"));
        assert!(after_todo.contains("to-do:completed"));

        // 3. Matching/new baseline edit on p:{b1} -> succeeds and clears edit draft
        app.spawn_edit_block(
            "me|page-1".to_string(),
            "p:{b1}".to_string(),
            "Merged edit on top of Alice".to_string(),
            "REMOTE EDIT BY ALICE".to_string(),
            &mut ui,
        );
        let msg3 = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(msg3, UiMsg::PageMutated { .. }));
        app.handle_ui_msg(&mut ui, msg3);
        assert_eq!(state.patch_hits.load(Ordering::SeqCst), 2);
        assert!(state
            .xhtml
            .lock()
            .unwrap()
            .contains("Merged edit on top of Alice"));
        assert!(app.drafts.get_note("edit:me|page-1:p:{b1}").is_none());

        srv.abort();
    }

    #[test]
    fn test_focus_gained_moved_page_reconnect_and_live_draft_preservation() {
        let (mut app, mut ui, mut focus_rx, mut nudge_rx) = make_test_app_with_cached_summary();

        app.storage
            .upsert_onenote_sections(&[OnenoteSection {
                section_key: "me|sec-2".to_string(),
                id: "sec-2".to_string(),
                scope: OnenoteScope::Me,
                notebook_id: Some("nb-1".to_string()),
                notebook_name: Some("Engineering".to_string()),
                parent_section_group_id: None,
                parent_section_group_name: None,
                display_name: "MovedTarget".to_string(),
                color: None,
                is_default: false,
                pages_url: None,
                created_at: None,
                last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
                web_url: None,
            }])
            .unwrap();

        let p1_summary = app.storage.get_onenote_page_summary("me|page-1").unwrap().unwrap();
        let p1_detail = parser::parse_onenote_page_xhtml(
            p1_summary.clone(),
            r#"<html><head><title>Initial Spec</title></head><body><div id="div:1"><p id="p:1">Alpha paragraph</p></div></body></html>"#,
        );
        app.storage.upsert_onenote_page_detail(&p1_detail).unwrap();
        app.load_cached_initial();
        let _ = focus_rx.borrow_and_update();
        while nudge_rx.try_recv().is_ok() {}

        // 1. Terminal focus gained unconditionally requests HydratePage even when detail is cached.
        assert!(app.page_details.contains_key("me|page-1"));
        app.on_terminal_focus_gained();
        assert_eq!(
            nudge_rx.try_recv().ok(),
            Some(SyncCommand::HydratePage {
                page_key: "me|page-1".to_string()
            })
        );

        // 2. Open EditBlock form, type unsaved text, then receive remote OnenotePageDetailUpdated:
        //    unsaved form.body & base_content must stay untouched while drift error notice appears.
        crate::ui::action::dispatch(
            crate::ui::action::Action::OpenOnenoteEditBlock,
            &mut app,
            &mut ui,
        );
        if let Some(Overlay::OnenoteForm(ref mut form)) = ui.overlays.last_mut() {
            form.body = "Alpha paragraph LOCAL_UNSAVED".to_string();
        }
        let remote_detail = parser::parse_onenote_page_xhtml(
            p1_summary.clone(),
            r#"<html><head><title>Initial Spec</title></head><body><div id="div:1"><p id="p:1">Remote edit while editing</p></div></body></html>"#,
        );
        app.handle_core_event(
            &mut ui,
            CoreEvent::OnenotePageDetailUpdated(Box::new(remote_detail)),
        );
        match ui.overlays.last() {
            Some(Overlay::OnenoteForm(form)) => {
                assert_eq!(form.body, "Alpha paragraph LOCAL_UNSAVED");
                assert!(form
                    .error
                    .as_deref()
                    .is_some_and(|e| e.contains("Remote block changed")));
            }
            other => panic!("expected OnenoteForm overlay, got {other:?}"),
        }
        ui.overlays.pop();

        // 3. Page moved from sec-1 -> sec-2 in SQLite, then sec-1 OnenotePagesUpdated arrives without me|page-1:
        //    AppState reconnects active_section_key to me|sec-2 and preserves me|page-1 open!
        let mut moved_summary = p1_summary;
        moved_summary.section_id = "sec-2".to_string();
        moved_summary.section_name = Some("MovedTarget".to_string());
        app.storage.upsert_onenote_pages(&[moved_summary]).unwrap();
        let _ = focus_rx.borrow_and_update();

        app.handle_core_event(
            &mut ui,
            CoreEvent::OnenotePagesUpdated {
                section_key: "me|sec-1".to_string(),
                pages: Vec::new(),
                notify_candidate: None,
            },
        );
        assert_eq!(app.active_section_key.as_deref(), Some("me|sec-2"));
        assert_eq!(app.active_page_key.as_deref(), Some("me|page-1"));
        assert!(app.page_details.contains_key("me|page-1"));
        assert_eq!(focus_rx.borrow_and_update().section_key.as_deref(), Some("me|sec-2"));
    }

    #[test]
    fn apply_ui_settings_keeps_auto_image_protocol_until_the_user_picks_one() {
        let mut config = AppConfig {
            image_protocol: ImageProtocolPreference::Auto,
            notify: true,
            ..AppConfig::default()
        };
        let mut ui = UiState::from_config(&config);
        ui.graphics_caps.protocol = TerminalImageProtocol::Sixel;
        ui.notify = false;

        // An unrelated setting change saves the UI flags but leaves `auto` alone.
        apply_ui_settings(&mut config, &ui);
        assert!(!config.notify);
        assert_eq!(config.image_protocol, ImageProtocolPreference::Auto);

        // Cycling the protocol in the UI is an explicit choice and is stored.
        ui.image_protocol_user_chosen = true;
        apply_ui_settings(&mut config, &ui);
        assert_eq!(config.image_protocol, ImageProtocolPreference::Sixel);

        // An explicit stored preference keeps tracking the protocol in use.
        ui.image_protocol_user_chosen = false;
        ui.graphics_caps.protocol = TerminalImageProtocol::Kitty;
        apply_ui_settings(&mut config, &ui);
        assert_eq!(config.image_protocol, ImageProtocolPreference::Kitty);
    }

    #[tokio::test]
    async fn raw_image_cache_hit_skips_refetch_on_zoom_and_protocol_cycle() {
        use axum::{extract::State, http::StatusCode, routing::get, Router};
        use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
        use std::io::Cursor;
        use std::sync::atomic::{AtomicUsize, Ordering};

        // 1. Verify LRU count cap, byte budget cap, oversized skip, and page eviction
        let (mut lru_app, _, _, _) = make_test_app_with_cached_summary();
        for i in 0..(MAX_RAW_IMAGE_CACHE_ENTRIES + 3) {
            lru_app.insert_raw_image_cache(
                format!("https://example.com/img-{i}.png"),
                Arc::from(vec![i as u8; 1024]),
            );
        }
        assert_eq!(lru_app.image_raw_cache.len(), MAX_RAW_IMAGE_CACHE_ENTRIES);
        assert_eq!(lru_app.image_raw_bytes, MAX_RAW_IMAGE_CACHE_ENTRIES * 1024);
        assert!(!lru_app.image_raw_cache.contains_key("https://example.com/img-0.png"));

        let six_mib = 6 * 1024 * 1024;
        for i in 0..4 {
            lru_app.insert_raw_image_cache(
                format!("https://example.com/big-{i}.png"),
                Arc::from(vec![1u8; six_mib]),
            );
        }
        assert!(lru_app.image_raw_bytes <= MAX_RAW_IMAGE_CACHE_BYTES);

        // Oversized (> 16 MiB) entry is ignored without corrupting cache budget
        lru_app.insert_raw_image_cache(
            "https://example.com/huge.png".to_string(),
            Arc::from(vec![0u8; MAX_RAW_IMAGE_CACHE_BYTES + 1]),
        );
        assert!(!lru_app.image_raw_cache.contains_key("https://example.com/huge.png"));
        assert!(lru_app.image_raw_bytes <= MAX_RAW_IMAGE_CACHE_BYTES);
        lru_app.clear_raw_image_cache();
        assert_eq!(lru_app.image_raw_cache.len(), 0);
        assert_eq!(lru_app.image_raw_bytes, 0);

        // 2. Live mock server verifying zoom (+/- /0) and protocol cycle (p) hit raw_image_cache with 0 refetches
        let img: RgbaImage = ImageBuffer::from_pixel(32, 32, Rgba([40, 120, 220, 255]));
        let mut png_buf = Cursor::new(Vec::new());
        DynamicImage::ImageRgba8(img)
            .write_to(&mut png_buf, ImageFormat::Png)
            .unwrap();
        let png_bytes = Arc::new(png_buf.into_inner());
        let fetch_hits = Arc::new(AtomicUsize::new(0));

        let app_router = Router::new()
            .route(
                "/res/pic.png",
                get(
                    |State((hits, body)): State<(Arc<AtomicUsize>, Arc<Vec<u8>>)>| async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        (StatusCode::OK, [("content-type", "image/png")], (*body).clone())
                    },
                ),
            )
            .with_state((Arc::clone(&fetch_hits), Arc::clone(&png_bytes)));

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app_router).await;
        });

        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tokens = StoredTokens {
            access_token: "raw-cache-token".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("user-1".to_string()),
            user_display_name: Some("Tester".to_string()),
            user_email: None,
            auth_mode: "access_token".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMsg>();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);

        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        let mut ui = UiState::from_config(&cfg);
        ui.term_width = 120;
        ui.term_height = 34;
        ui.graphics_caps = TerminalGraphicsCaps {
            protocol: TerminalImageProtocol::Halfblock,
            cell_px: (10, 20),
            cell_px_exact: true,
        };

        let resource_url = format!("http://{addr}/res/pic.png");
        let summary = OnenotePageSummary {
            page_key: "me|page-raw".to_string(),
            id: "page-raw".to_string(),
            scope: OnenoteScope::Me,
            section_id: "sec-1".to_string(),
            section_name: Some("Specs".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Eng".to_string()),
            title: "Raw Cache Page".to_string(),
            level: 0,
            order: 0,
            created_at: None,
            last_modified_at: None,
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: false,
        };
        let detail = OnenotePageDetail {
            summary,
            raw_xhtml: String::new(),
            body_markdown: String::new(),
            body_plain: String::new(),
            blocks: Vec::new(),
            resources: vec![crate::models::OnenoteResource {
                element_id: Some("img:1".to_string()),
                resource_url: resource_url.clone(),
                fullres_url: None,
                mime_type: "image/png".to_string(),
                file_name: Some("pic.png".to_string()),
                is_image: true,
                alt_text: None,
            }],
            primary_div_id: None,
            synced_at: None,
        };
        app.active_page_key = Some("me|page-raw".to_string());
        app.page_details.insert("me|page-raw".to_string(), detail);

        // Open preview modal -> initial fetch (hits == 1)
        crate::ui::action::dispatch(crate::ui::action::Action::OpenImagePreview, &mut app, &mut ui);
        let mut saw_bytes = false;
        let mut saw_loaded = false;
        while !saw_bytes || !saw_loaded {
            let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
                .await
                .unwrap()
                .unwrap();
            match &msg {
                UiMsg::ImageBytesFetched { .. } => saw_bytes = true,
                UiMsg::ImageLoaded { .. } => saw_loaded = true,
                other => panic!("unexpected msg during initial load: {other:?}"),
            }
            app.handle_ui_msg(&mut ui, msg);
        }
        assert_eq!(fetch_hits.load(Ordering::SeqCst), 1);
        assert!(app.image_raw_cache.contains_key(&resource_url));

        // Zoom in (`+`) -> renders from raw_image_cache with 0 additional HTTP fetches
        crate::ui::action::dispatch(crate::ui::action::Action::Input('+'), &mut app, &mut ui);
        let msg_zoom = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(msg_zoom, UiMsg::ImageLoaded { .. }));
        app.handle_ui_msg(&mut ui, msg_zoom);
        assert_eq!(fetch_hits.load(Ordering::SeqCst), 1);

        // Cycle protocol (`p`: Halfblock -> Kitty) -> renders from raw_image_cache with 0 additional HTTP fetches
        crate::ui::action::dispatch(crate::ui::action::Action::Input('p'), &mut app, &mut ui);
        let msg_proto = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(msg_proto, UiMsg::ImageLoaded { .. }));
        app.handle_ui_msg(&mut ui, msg_proto);
        assert_eq!(fetch_hits.load(Ordering::SeqCst), 1);

        // Resize / cell_px change -> renders from raw_image_cache with 0 additional HTTP fetches
        ui.term_width = 160;
        ui.term_height = 44;
        ui.graphics_caps.cell_px = (12, 24);
        crate::ui::action::dispatch(crate::ui::action::Action::LoadPreviewImage, &mut app, &mut ui);
        let msg_resize = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .unwrap()
            .unwrap();
        assert!(matches!(msg_resize, UiMsg::ImageLoaded { .. }));
        app.handle_ui_msg(&mut ui, msg_resize);
        assert_eq!(fetch_hits.load(Ordering::SeqCst), 1);

        // Evicting page removes both rendered and raw byte cache entries for that page
        app.evict_image_cache_for_page("me|page-raw");
        assert!(!app.image_raw_cache.contains_key(&resource_url));
        assert_eq!(app.image_raw_bytes, 0);

        srv.abort();
    }

    #[test]
    fn page_created_renders_preview_immediately_without_loading_blank() {
        let storage = Storage::open_in_memory().unwrap();
        let cfg = AppConfig::default();
        let auth = crate::auth::AuthManager::from_tokens(
            cfg.clone(),
            crate::auth::StoredTokens {
                access_token: "tok".to_string(),
                refresh_token: None,
                expires_at: chrono::Utc::now().timestamp() + 3600,
                client_id: "cid".to_string(),
                tenant_id: "tid".to_string(),
                user_id: Some("u1".to_string()),
                user_display_name: None,
                user_email: None,
                auth_mode: "access_token".to_string(),
            },
        );
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
        let (nudge_tx, mut nudge_rx) = tokio::sync::mpsc::unbounded_channel();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
        let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
        let mut app = AppState::new(cfg, auth, graph, storage, sync_ctrl, ui_tx);
        app.active_section_key = Some("me|sec-1".to_string());
        let mut ui = UiState::new();

        let page = OnenotePageSummary {
            page_key: "me|p-new".to_string(),
            id: "p-new".to_string(),
            scope: crate::models::OnenoteScope::Me,
            section_id: "sec-1".to_string(),
            section_name: Some("Notes".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Personal".to_string()),
            title: "Instant Page".to_string(),
            level: 0,
            order: 0,
            created_at: None,
            last_modified_at: None,
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: false,
        };
        let outgoing = OutgoingOnenotePage {
            title: "Instant Page".to_string(),
            markdown_body: "Hello instant preview".to_string(),
            inline_images: Vec::new(),
            level: None,
        };
        let preview_xhtml = crate::parser::onenote::compile_new_page_xhtml(&outgoing);
        let preview_detail =
            crate::parser::onenote::parse_onenote_page_xhtml(page.clone(), &preview_xhtml);

        let task_id = ui.tasks.start("Creating page");
        app.handle_ui_msg(
            &mut ui,
            UiMsg::PageCreated {
                task_id,
                section_key: "me|sec-1".to_string(),
                page,
                preview_detail: Box::new(preview_detail),
            },
        );

        assert_eq!(app.active_page_key.as_deref(), Some("me|p-new"));
        assert!(
            !app.loading_onenote_detail,
            "newly created page must display immediately instead of showing loading blank"
        );
        let detail = app
            .active_page_detail()
            .expect("preview detail must be present immediately");
        assert!(detail.body_plain.contains("Hello instant preview"));

        // Background HydratePage must still be queued to fetch authoritative Graph element IDs.
        let mut saw_hydrate = false;
        while let Ok(cmd) = nudge_rx.try_recv() {
            if matches!(cmd, SyncCommand::HydratePage { ref page_key } if page_key == "me|p-new") {
                saw_hydrate = true;
            }
        }
        assert!(saw_hydrate, "background HydratePage must still be requested");
    }
}
