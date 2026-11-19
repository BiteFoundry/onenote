//! Core TUI application state (`AppState`), presentation state (`UiState`), and async `UiMsg` handlers.

use std::{
    collections::{HashMap, HashSet},
    path::PathBuf,
    time::Instant,
};

use ratatui::layout::Rect;
use tokio::sync::mpsc;

use crate::{
    auth::AuthManager,
    config::{
        AppConfig, DensityMode, DraftStore, ImageProtocolPreference, NoteDraftEntry, ThemePreset,
    },
    graph::GraphClient,
    img_preview::{RenderedTerminalImage, TerminalGraphicsCaps, TerminalImageProtocol},
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
    },
    PageMutated {
        task_id: u64,
        detail: Box<OnenotePageDetail>,
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
    ImageLoaded {
        cache_key: String,
        rendered: RenderedTerminalImage,
    },
    ImageLoadFailed {
        cache_key: String,
        error: String,
    },
    UpdateResult {
        task_id: u64,
        result: UpdateCheckResult,
    },
    TaskFailed {
        task_id: u64,
        error: String,
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
    pub pending_pixel_graphic: Option<PendingPixelGraphic>,
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
            pending_pixel_graphic: None,
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
        self.overlays
            .last()
            .is_some_and(Overlay::accepts_text)
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

    pub active_notebook_key: Option<String>,
    pub active_section_key: Option<String>,
    pub active_page_key: Option<String>,
    pub loading_onenote_pages: bool,
    pub loading_onenote_detail: bool,

    pub search_query: String,
    pub search_results: Vec<OnenoteSearchHit>,

    pub image_cache: HashMap<String, RenderedTerminalImage>,
    pub inflight_images: HashSet<String>,
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
            active_notebook_key: None,
            active_section_key: None,
            active_page_key: None,
            loading_onenote_pages: false,
            loading_onenote_detail: false,
            search_query: String::new(),
            search_results: Vec::new(),
            image_cache: HashMap::new(),
            inflight_images: HashSet::new(),
        }
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
            let valid_page = self
                .active_page_key
                .as_deref()
                .is_some_and(|pk| self.pages.iter().any(|p| p.page_key == pk));
            if !valid_page {
                self.active_page_key = self.pages.first().map(|p| p.page_key.clone());
            }
            if let Some(pk) = self.active_page_key.clone() {
                if !self.page_details.contains_key(&pk) {
                    if let Ok(Some(detail)) = self.storage.get_onenote_page_detail(&pk) {
                        self.page_details.insert(pk, detail);
                    }
                }
            }
        } else {
            self.pages.clear();
            self.active_page_key = None;
        }
    }

    pub fn push_focus_to_sync(&self) {
        self.sync_ctrl.set_focus(FocusTarget {
            notebook_key: self.active_notebook_key.clone(),
            section_key: self.active_section_key.clone(),
            page_key: self.active_page_key.clone(),
        });
    }

    pub fn select_notebook(&mut self, notebook_key: String, ui: &mut UiState) {
        self.active_notebook_key = Some(notebook_key);
        self.active_section_key = None;
        self.active_page_key = None;
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
        self.active_section_key = Some(section_key.clone());
        self.active_page_key = None;
        ui.onenote_scroll = 0;
        ui.onenote_selected_block = 0;
        self.pages = self
            .storage
            .list_onenote_pages(&section_key)
            .unwrap_or_default();
        self.active_page_key = self.pages.first().map(|p| p.page_key.clone());
        if let Some(pk) = self.active_page_key.clone() {
            if !self.page_details.contains_key(&pk) {
                if let Ok(Some(detail)) = self.storage.get_onenote_page_detail(&pk) {
                    self.page_details.insert(pk.clone(), detail);
                }
            }
            self.request_page_detail(pk);
        }
        self.request_section_pages(section_key);
        self.push_focus_to_sync();
    }

    pub fn select_page(&mut self, page_key: String, ui: &mut UiState) {
        self.active_page_key = Some(page_key.clone());
        ui.onenote_scroll = 0;
        ui.onenote_selected_block = 0;
        if !self.page_details.contains_key(&page_key) {
            if let Ok(Some(detail)) = self.storage.get_onenote_page_detail(&page_key) {
                self.page_details.insert(page_key.clone(), detail);
            }
        }
        self.request_page_detail(page_key);
        self.push_focus_to_sync();
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

    /// Requests an inline thumbnail or fullscreen HD terminal image render for a OneNote resource URL.
    pub fn request_onenote_image(
        &mut self,
        cache_key: String,
        resource_url: String,
        cols: u16,
        rows: u16,
        caps: TerminalGraphicsCaps,
        halfblock_only: bool,
    ) {
        if self.image_cache.contains_key(&cache_key) || self.inflight_images.contains(&cache_key) {
            return;
        }
        self.inflight_images.insert(cache_key.clone());
        let graph = self.graph.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.fetch_onenote_resource_bytes(&resource_url).await {
                Ok((_, bytes)) => {
                    let render_res = tokio::task::spawn_blocking(move || {
                        if halfblock_only {
                            crate::img_preview::decode_and_render_halfblocks(
                                &bytes,
                                cols,
                                rows,
                                (20, 22, 26),
                            )
                        } else {
                            crate::img_preview::decode_and_render_for_protocol(
                                &bytes,
                                cols,
                                rows,
                                (20, 22, 26),
                                caps,
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

    pub fn save_draft(&mut self, key: impl Into<String>, title: String, body: String) {
        self.drafts.set_note(key, NoteDraftEntry { title, body });
        let _ = self.drafts.save();
    }

    pub fn clear_draft(&mut self, key: &str) {
        if self.drafts.remove_note(key).is_some() {
            let _ = self.drafts.save();
        }
    }

    pub fn persist_config(&mut self, ui: &UiState) {
        self.config.show_nav_rail = ui.show_nav_rail;
        self.config.notify = ui.notify;
        self.config.inline_images = ui.inline_images;
        self.config.show_inline_hrefs = ui.show_inline_hrefs;
        self.config.density = ui.density;
        self.config.theme = ui.theme_preset;
        self.config.image_protocol = match ui.graphics_caps.protocol {
            TerminalImageProtocol::Kitty => ImageProtocolPreference::Kitty,
            TerminalImageProtocol::ITerm2 => ImageProtocolPreference::Iterm2,
            TerminalImageProtocol::Sixel => ImageProtocolPreference::Sixel,
            TerminalImageProtocol::Halfblock => ImageProtocolPreference::Halfblock,
        };
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
                    let _ = tx.send(UiMsg::PageCreated {
                        task_id,
                        section_key,
                        page,
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
                    let _ = tx.send(UiMsg::PageMutated {
                        task_id,
                        detail: Box::new(detail),
                        message: "Appended content to OneNote page".to_string(),
                    });
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskFailed {
                        task_id,
                        error: format!("Failed to append content: {err}"),
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
        ui: &mut UiState,
    ) {
        let task_id = ui.tasks.start("Updating block");
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match crate::cli::update_onenote_page_helper(
                &storage,
                &graph,
                &page_key,
                None,
                Some(&markdown),
                Some(&element_id),
            )
            .await
            {
                Ok(detail) => {
                    let _ = tx.send(UiMsg::PageMutated {
                        task_id,
                        detail: Box::new(detail),
                        message: "Updated OneNote block".to_string(),
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
        let Some((cmd, next_done)) = parser::build_toggle_todo_patch(block) else {
            ui.warn("Selected block is not a OneNote to-do checkbox");
            return;
        };
        let page_key = detail.summary.page_key.clone();
        let scope = detail.summary.scope.clone();
        let page_id = detail.summary.id.clone();
        let summary_hint = detail.summary.clone();
        let task_id = ui.tasks.start("Toggling to-do");
        let graph = self.graph.clone();
        let storage = self.storage.clone();
        let tx = self.ui_tx.clone();
        tokio::spawn(async move {
            match graph.patch_onenote_page_commands(&scope, &page_id, &[cmd]).await {
                Ok(()) => {
                    match graph
                        .get_onenote_page_detail(&scope, &page_id, Some(summary_hint))
                        .await
                    {
                        Ok(updated) => {
                            let _ = storage.upsert_onenote_page_detail(&updated);
                            let state = if next_done { "completed" } else { "reopened" };
                            let _ = tx.send(UiMsg::PageMutated {
                                task_id,
                                detail: Box::new(updated),
                                message: format!("Marked to-do {state}"),
                            });
                        }
                        Err(err) => {
                            let _ = tx.send(UiMsg::TaskFailed {
                                task_id,
                                error: format!("Failed to reload page ({page_key}): {err}"),
                            });
                        }
                    }
                }
                Err(err) => {
                    let _ = tx.send(UiMsg::TaskFailed {
                        task_id,
                        error: format!("Failed to toggle to-do: {err}"),
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

    pub fn handle_core_event(&mut self, ui: &mut UiState, ev: CoreEvent) {
        match ev {
            CoreEvent::ProfileLoaded(profile) => {
                self.profile = Some(profile);
            }
            CoreEvent::OnenoteHierarchyUpdated {
                notebooks,
                section_groups,
                sections,
            } => {
                self.notebooks = notebooks;
                self.section_groups = section_groups;
                self.sections = sections;
                let prev_sec = self.active_section_key.clone();
                if self.active_notebook_key.is_none() {
                    self.active_notebook_key = self
                        .notebooks
                        .iter()
                        .find(|n| n.is_default && n.scope.is_personal())
                        .or_else(|| self.notebooks.first())
                        .map(|n| n.notebook_key.clone());
                }
                self.ensure_active_section_and_pages_cached();
                if prev_sec != self.active_section_key {
                    if let Some(sec_key) = self.active_section_key.clone() {
                        self.request_section_pages(sec_key);
                    }
                    self.push_focus_to_sync();
                }
            }
            CoreEvent::OnenotePagesUpdated {
                section_key,
                pages,
                notify_candidate,
            } => {
                if self.active_section_key.as_deref() == Some(section_key.as_str()) {
                    self.loading_onenote_pages = false;
                    self.pages = pages;
                    let valid = self
                        .active_page_key
                        .as_deref()
                        .is_some_and(|pk| self.pages.iter().any(|p| p.page_key == pk));
                    if !valid {
                        self.active_page_key = self.pages.first().map(|p| p.page_key.clone());
                        if let Some(pk) = self.active_page_key.clone() {
                            self.request_page_detail(pk);
                        }
                    }
                }
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
                self.loading_onenote_detail = false;
                let key = detail.summary.page_key.clone();
                self.page_details.insert(key, *detail);
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
                self.loading_onenote_detail = false;
                let key = detail.summary.page_key.clone();
                self.page_details.insert(key, *detail);
            }
            UiMsg::NotebookCreated { task_id, notebook } => {
                ui.tasks.finish(task_id);
                let name = notebook.display_name.clone();
                let key = notebook.notebook_key.clone();
                self.notebooks.push(notebook);
                self.select_notebook(key, ui);
                ui.success(format!("Created notebook '{name}'"));
            }
            UiMsg::SectionCreated { task_id, section } => {
                ui.tasks.finish(task_id);
                let name = section.display_name.clone();
                let key = section.section_key.clone();
                self.sections.push(section);
                self.select_section(key, ui);
                ui.success(format!("Created section '{name}'"));
            }
            UiMsg::PageCreated {
                task_id,
                section_key,
                page,
            } => {
                ui.tasks.finish(task_id);
                let title = page.title.clone();
                let pk = page.page_key.clone();
                self.clear_draft(&format!("create:{section_key}"));
                if self.active_section_key.as_deref() == Some(section_key.as_str()) {
                    self.pages.insert(0, page);
                    self.select_page(pk, ui);
                }
                ui.success(format!("Created page '{title}'"));
            }
            UiMsg::PageMutated {
                task_id,
                detail,
                message,
            } => {
                ui.tasks.finish(task_id);
                let pk = detail.summary.page_key.clone();
                self.clear_draft(&format!("append:{pk}"));
                self.page_details.insert(pk, *detail);
                ui.success(message);
            }
            UiMsg::PageDeleted { task_id, page_key } => {
                ui.tasks.finish(task_id);
                self.pages.retain(|p| p.page_key != page_key);
                self.page_details.remove(&page_key);
                if self.active_page_key.as_deref() == Some(page_key.as_str()) {
                    self.active_page_key = self.pages.first().map(|p| p.page_key.clone());
                    if let Some(pk) = self.active_page_key.clone() {
                        self.request_page_detail(pk);
                    }
                }
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
                    preview.error = Some(error);
                }
            }
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
            UiMsg::Info(msg) => ui.info(msg),
            UiMsg::Error(msg) => ui.error(msg),
        }
    }
}
