//! Central TUI `Action` enum and state dispatcher (`action::dispatch`).

use std::path::PathBuf;

use crate::{
    config::{DensityMode, ThemePreset},
    img_preview::TerminalImageProtocol,
    models::InlineImageUpload,
    parser,
    sync::SyncCommand,
    ui::{
        app::{AppState, Focus, NarrowPane, UiState},
        composer,
        hitmap::{ButtonId, HitTarget},
        overlays::{
            context_menu::{ContextMenu, ContextTarget},
            image_preview::ImagePreviewOverlay,
            links::LinkExtractorOverlay,
            onenote_form::OnenoteFormOverlay,
            Overlay, OverlayKind, OverlayOutcome,
        },
        theme::Theme,
    },
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Action {
    Noop,
    Quit,
    QuitConfirm,
    RefreshAll,
    SetFocus(Focus),
    NextFocus,
    PrevFocus,
    MoveUp,
    MoveDown,
    CursorUp,
    CursorDown,
    CursorLeft,
    CursorRight,
    CursorHome,
    CursorEnd,
    PageUp,
    PageDown,
    ScrollTop,
    ScrollBottom,
    Activate,
    Tab,
    Input(char),
    Newline,
    InsertNewline,
    Backspace,
    Delete,
    Paste(String),
    PasteClipboard,
    ClearPendingImages,
    OverlaySubmit,
    CloseOverlay,
    OpenOverlay(OverlayKind),
    OpenSearch,
    OpenFilePicker,
    OpenLinkExtractor,
    OpenImagePreview,
    OpenImagePreviewAt(usize),
    LoadPreviewImage,
    OpenContextMenu { col: u16, row: u16 },
    RequestExternalEditor,

    // OneNote Navigation & Actions
    SelectNotebook(String),
    SelectSection(String),
    SelectPage(String),
    OnenoteNextSection,
    OnenotePrevSection,
    OpenOnenoteCreateNotebook,
    OpenOnenoteCreateSection,
    OpenOnenoteCreatePage { level: i32 },
    OpenOnenoteAppend,
    OpenOnenoteEditBlock,
    OnenoteCreateNotebook { name: String },
    OnenoteCreateSection { notebook_key: String, name: String },
    OnenoteCreatePage {
        section_key: String,
        title: String,
        markdown_body: String,
        inline_images: Vec<InlineImageUpload>,
        level: i32,
    },
    OnenoteAppendMarkdown {
        page_key: String,
        markdown: String,
        inline_images: Vec<InlineImageUpload>,
    },
    OnenoteEditBlockSubmit {
        page_key: String,
        element_id: String,
        markdown: String,
    },
    OnenoteToggleTodo(Option<usize>),
    ConfirmDeleteOnenotePage,
    DeleteOnenotePage,
    OnenoteOpenWeb,
    DownloadResource(usize),
    CopyActivePageMarkdown,

    // Search
    SearchLocalPages(String),
    JumpToSearchHit(usize),

    // Links & Files
    OpenUrl(String),
    CopyUrlToClipboard(String),
    AttachFormFile(PathBuf),

    // Settings & Layout
    ToggleNavRail,
    ToggleNotify,
    ToggleInlineImages,
    ToggleShowLinkUrls,
    ToggleDensity,
    CycleTheme,
    CycleImageProtocol,
    ShowDownloadDir,
    ShowToast(String),
    CheckForUpdates,
    ApplySelfUpdate,

    // Mouse
    MouseClick { target: HitTarget, col: u16, row: u16 },
    MouseScroll { delta: i32, column: u16, row: u16 },
}

/// Dispatches an `Action` against `AppState` and `UiState`. Returns `true` when the TUI should quit.
pub fn dispatch(action: Action, app: &mut AppState, ui: &mut UiState) -> bool {
    // Allow top overlay to handle actions first unless it's a direct mouse click outside.
    if !ui.overlays.is_empty() && !matches!(action, Action::MouseClick { .. } | Action::MouseScroll { .. }) {
        // Special case: Ctrl+F inside OnenoteForm opens FilePicker on top of OnenoteForm!
        if matches!(action, Action::OpenFilePicker)
            && matches!(ui.overlays.last(), Some(Overlay::OnenoteForm(_)))
        {
            ui.overlays.push(Overlay::file_picker());
            return false;
        }
        // Special case: Ctrl+E inside OnenoteForm requests external editor
        if matches!(action, Action::RequestExternalEditor)
            && matches!(ui.overlays.last(), Some(Overlay::OnenoteForm(_)))
        {
            ui.editor_requested = true;
            return false;
        }

        let outcome = if let Some(mut top) = ui.overlays.pop() {
            let outcome = top.handle(&action, app, ui);
            match &outcome {
                OverlayOutcome::Close | OverlayOutcome::CloseAndDispatch(_) => {
                    // Auto-stash draft if closing a OneNote form via Esc
                    if matches!(action, Action::CloseOverlay) {
                        if let Overlay::OnenoteForm(ref form) = top {
                            if let Some((key, title, body)) = form.draft_snapshot() {
                                app.save_draft(key, title, body);
                            }
                        }
                    }
                }
                _ => {
                    ui.overlays.push(top);
                }
            }
            outcome
        } else {
            OverlayOutcome::NotHandled
        };

        match outcome {
            OverlayOutcome::Handled | OverlayOutcome::Close => return false,
            OverlayOutcome::Dispatch(next) | OverlayOutcome::CloseAndDispatch(next) => {
                return dispatch(next, app, ui);
            }
            OverlayOutcome::NotHandled => {}
        }
    }

    match action {
        Action::Noop => false,
        Action::Quit => true,
        Action::QuitConfirm => {
            ui.overlays.push(Overlay::confirm(
                "Quit Microsoft OneNote TUI?",
                Some(Action::Quit),
            ));
            false
        }
        Action::RefreshAll => {
            app.syncing = true;
            app.sync_summary = "Syncing OneNote...".to_string();
            app.push_focus_to_sync();
            app.sync_ctrl.request(SyncCommand::RefreshAll);
            ui.info("Refreshing OneNote notebooks, sections, and pages...");
            false
        }
        Action::SetFocus(f) => {
            ui.focus = f;
            match f {
                Focus::Nav => ui.narrow_pane = NarrowPane::Notebooks,
                Focus::Pages => ui.narrow_pane = NarrowPane::Pages,
                Focus::Canvas => ui.narrow_pane = NarrowPane::Canvas,
                Focus::Overlay => {}
            }
            false
        }
        Action::NextFocus => {
            ui.focus = match ui.focus {
                Focus::Nav => Focus::Pages,
                Focus::Pages => Focus::Canvas,
                Focus::Canvas | Focus::Overlay => {
                    if ui.show_nav_rail {
                        Focus::Nav
                    } else {
                        Focus::Pages
                    }
                }
            };
            sync_narrow_from_focus(ui);
            false
        }
        Action::PrevFocus => {
            ui.focus = match ui.focus {
                Focus::Nav | Focus::Overlay => Focus::Canvas,
                Focus::Pages => {
                    if ui.show_nav_rail {
                        Focus::Nav
                    } else {
                        Focus::Canvas
                    }
                }
                Focus::Canvas => Focus::Pages,
            };
            sync_narrow_from_focus(ui);
            false
        }
        Action::MoveUp | Action::CursorUp => {
            match ui.focus {
                Focus::Nav => move_notebook_selection(app, ui, -1),
                Focus::Pages => move_page_selection(app, ui, -1),
                Focus::Canvas => {
                    if ui.onenote_selected_block > 0 {
                        ui.onenote_selected_block -= 1;
                        ui.onenote_follow_selection = true;
                    } else {
                        ui.onenote_scroll = ui.onenote_scroll.saturating_sub(3);
                    }
                }
                Focus::Overlay => {}
            }
            false
        }
        Action::MoveDown | Action::CursorDown => {
            match ui.focus {
                Focus::Nav => move_notebook_selection(app, ui, 1),
                Focus::Pages => move_page_selection(app, ui, 1),
                Focus::Canvas => {
                    let block_count = app
                        .active_page_detail()
                        .map(|d| d.blocks.len())
                        .unwrap_or(0);
                    if block_count > 0 && ui.onenote_selected_block + 1 < block_count {
                        ui.onenote_selected_block += 1;
                        ui.onenote_follow_selection = true;
                    } else {
                        ui.onenote_scroll = ui.onenote_scroll.saturating_add(3);
                    }
                }
                Focus::Overlay => {}
            }
            false
        }
        Action::PageUp => {
            ui.onenote_scroll = ui.onenote_scroll.saturating_sub(12);
            ui.onenote_follow_selection = false;
            false
        }
        Action::PageDown => {
            ui.onenote_scroll = ui.onenote_scroll.saturating_add(12);
            ui.onenote_follow_selection = false;
            false
        }
        Action::ScrollTop => {
            ui.onenote_scroll = 0;
            ui.onenote_selected_block = 0;
            ui.onenote_follow_selection = false;
            false
        }
        Action::ScrollBottom => {
            let block_count = app
                .active_page_detail()
                .map(|d| d.blocks.len())
                .unwrap_or(0);
            ui.onenote_selected_block = block_count.saturating_sub(1);
            ui.onenote_follow_selection = true;
            false
        }
        Action::Activate => {
            match ui.focus {
                Focus::Nav => {
                    ui.focus = Focus::Pages;
                    ui.narrow_pane = NarrowPane::Pages;
                }
                Focus::Pages => {
                    ui.focus = Focus::Canvas;
                    ui.narrow_pane = NarrowPane::Canvas;
                    if let Some(pk) = app.active_page_key.clone() {
                        app.request_page_detail(pk);
                    }
                }
                Focus::Canvas => {
                    // Activate on a todo block toggles it; on an image block opens image preview; otherwise edits block
                    if let Some(detail) = app.active_page_detail() {
                        if let Some(blk) = detail.blocks.get(ui.onenote_selected_block) {
                            if blk.tag.as_ref().is_some_and(|t| t.is_todo()) {
                                let idx = ui.onenote_selected_block;
                                app.spawn_toggle_todo(idx, ui);
                                return false;
                            }
                            if let crate::models::OnenoteBlockKind::Image { resource_index, .. } =
                                blk.kind
                            {
                                return dispatch(Action::OpenImagePreviewAt(resource_index), app, ui);
                            }
                            if let crate::models::OnenoteBlockKind::Attachment {
                                resource_index,
                                ..
                            } = blk.kind
                            {
                                app.spawn_download_resource(resource_index, ui);
                                return false;
                            }
                        }
                    }
                    return dispatch(Action::OpenOnenoteEditBlock, app, ui);
                }
                Focus::Overlay => {}
            }
            false
        }
        Action::CloseOverlay => {
            ui.overlays.pop();
            false
        }
        Action::OpenOverlay(kind) => {
            match kind {
                OverlayKind::CommandPalette => ui.overlays.push(Overlay::command_palette()),
                OverlayKind::Settings => ui.overlays.push(Overlay::settings()),
                OverlayKind::Search => ui.overlays.push(Overlay::search()),
                OverlayKind::Attachments => ui.overlays.push(Overlay::attachments()),
                OverlayKind::PathInput => ui.overlays.push(Overlay::path_input()),
                OverlayKind::FilePicker => ui.overlays.push(Overlay::file_picker()),
                OverlayKind::Help => ui.overlays.push(Overlay::help()),
                OverlayKind::Links => return dispatch(Action::OpenLinkExtractor, app, ui),
                OverlayKind::ImagePreview => return dispatch(Action::OpenImagePreview, app, ui),
                _ => {}
            }
            false
        }
        Action::OpenSearch => {
            ui.overlays.push(Overlay::search());
            false
        }
        Action::OpenFilePicker => {
            ui.overlays.push(Overlay::file_picker());
            false
        }
        Action::OpenLinkExtractor => {
            if let Some(detail) = app.active_page_detail() {
                let overlay = LinkExtractorOverlay::from_onenote_page(detail);
                if overlay.links.is_empty() {
                    ui.info("No links found on this OneNote page");
                } else {
                    ui.overlays.push(Overlay::links(overlay));
                }
            } else {
                ui.warn("Open a OneNote page first to extract links");
            }
            false
        }
        Action::OpenImagePreview => {
            if let Some(detail) = app.active_page_detail() {
                if let Some(ov) = ImagePreviewOverlay::from_onenote_page(detail, None) {
                    ui.overlays.push(Overlay::ImagePreview(ov));
                    return dispatch(Action::LoadPreviewImage, app, ui);
                } else {
                    ui.info("No embedded images on this OneNote page");
                }
            } else {
                ui.warn("Open a OneNote page first");
            }
            false
        }
        Action::OpenImagePreviewAt(res_idx) => {
            if let Some(detail) = app.active_page_detail() {
                if let Some(ov) = ImagePreviewOverlay::from_onenote_page(detail, Some(res_idx)) {
                    ui.overlays.push(Overlay::ImagePreview(ov));
                    return dispatch(Action::LoadPreviewImage, app, ui);
                }
            }
            ui.info("Resource is not a previewable image");
            false
        }
        Action::LoadPreviewImage => {
            let req = if let Some(Overlay::ImagePreview(preview)) = ui.overlays.last() {
                preview.current().map(|target| {
                    let (cols, rows) = preview.target_dimensions(ui.term_width, ui.term_height);
                    let proto = ui.graphics_caps.protocol;
                    let cache_key = format!(
                        "hd:{}:{}:{}:{}:{:?}",
                        target.page_key, target.resource_index, cols, rows, proto
                    );
                    (cache_key, target.url.clone(), cols, rows)
                })
            } else {
                None
            };
            if let Some((cache_key, url, cols, rows)) = req {
                app.request_onenote_image(cache_key, url, cols, rows, ui.graphics_caps, false);
            }
            false
        }
        Action::OpenContextMenu { col, row } => {
            let target = match ui.focus {
                Focus::Pages => ContextTarget::PageItem,
                Focus::Canvas => ContextTarget::CanvasBlock,
                _ => ContextTarget::Global,
            };
            let menu = ContextMenu::build(target, (col, row), app, ui);
            ui.overlays.push(Overlay::context_menu(menu));
            false
        }
        Action::SelectNotebook(key) => {
            app.select_notebook(key, ui);
            false
        }
        Action::SelectSection(key) => {
            app.select_section(key, ui);
            false
        }
        Action::SelectPage(key) => {
            app.select_page(key, ui);
            false
        }
        Action::OnenoteNextSection => {
            let secs: Vec<String> = app
                .sections_for_active_notebook()
                .into_iter()
                .map(|s| s.section_key.clone())
                .collect();
            if secs.len() > 1 {
                let cur = app
                    .active_section_key
                    .as_deref()
                    .and_then(|k| secs.iter().position(|s| s == k))
                    .unwrap_or(0);
                let next = (cur + 1) % secs.len();
                app.select_section(secs[next].clone(), ui);
            }
            false
        }
        Action::OnenotePrevSection => {
            let secs: Vec<String> = app
                .sections_for_active_notebook()
                .into_iter()
                .map(|s| s.section_key.clone())
                .collect();
            if secs.len() > 1 {
                let cur = app
                    .active_section_key
                    .as_deref()
                    .and_then(|k| secs.iter().position(|s| s == k))
                    .unwrap_or(0);
                let prev = (cur + secs.len() - 1) % secs.len();
                app.select_section(secs[prev].clone(), ui);
            }
            false
        }
        Action::OpenOnenoteCreateNotebook => {
            ui.overlays
                .push(Overlay::onenote_form(OnenoteFormOverlay::create_notebook()));
            false
        }
        Action::OpenOnenoteCreateSection => {
            if let Some(nb) = app.active_onenote_notebook() {
                ui.overlays.push(Overlay::onenote_form(
                    OnenoteFormOverlay::create_section(
                        nb.notebook_key.clone(),
                        nb.display_name.clone(),
                    ),
                ));
            } else {
                ui.warn("Select or create a notebook first");
            }
            false
        }
        Action::OpenOnenoteCreatePage { level } => {
            if let Some(sec) = app.active_onenote_section() {
                let mut form = OnenoteFormOverlay::create_page(
                    sec.section_key.clone(),
                    sec.display_name.clone(),
                    level,
                );
                if let Some(draft) = app.drafts.get_note(&format!("create:{}", sec.section_key)) {
                    form.title = draft.title.clone();
                    form.body = draft.body.clone();
                }
                ui.overlays.push(Overlay::onenote_form(form));
            } else {
                ui.warn("Select a section first");
            }
            false
        }
        Action::OpenOnenoteAppend => {
            if let Some(summary) = app.active_page_summary() {
                let mut form = OnenoteFormOverlay::append_markdown(
                    summary.page_key.clone(),
                    summary.title.clone(),
                );
                if let Some(draft) = app.drafts.get_note(&format!("append:{}", summary.page_key)) {
                    form.body = draft.body.clone();
                }
                ui.overlays.push(Overlay::onenote_form(form));
            } else {
                ui.warn("Select a OneNote page first");
            }
            false
        }
        Action::OpenOnenoteEditBlock => {
            if let Some(detail) = app.active_page_detail() {
                let idx = ui
                    .onenote_selected_block
                    .min(detail.blocks.len().saturating_sub(1));
                if let Some(blk) = detail.blocks.get(idx) {
                    if let Some(eid) = blk.element_id.clone() {
                        let initial_md = parser::onenote_blocks_to_markdown(std::slice::from_ref(blk));
                        let form = OnenoteFormOverlay::edit_block(
                            detail.summary.page_key.clone(),
                            detail.summary.title.clone(),
                            eid,
                            idx,
                            initial_md,
                        );
                        ui.overlays.push(Overlay::onenote_form(form));
                        return false;
                    }
                }
                ui.warn("Selected block does not have an editable Graph element ID");
            } else {
                ui.warn("Open a OneNote page first");
            }
            false
        }
        Action::OnenoteCreateNotebook { name } => {
            app.spawn_create_notebook(name, ui);
            false
        }
        Action::OnenoteCreateSection { notebook_key, name } => {
            app.spawn_create_section(notebook_key, name, ui);
            false
        }
        Action::OnenoteCreatePage {
            section_key,
            title,
            markdown_body,
            inline_images,
            level,
        } => {
            app.spawn_create_page(section_key, title, markdown_body, inline_images, level, ui);
            false
        }
        Action::OnenoteAppendMarkdown {
            page_key,
            markdown,
            inline_images,
        } => {
            app.spawn_append_markdown(page_key, markdown, inline_images, ui);
            false
        }
        Action::OnenoteEditBlockSubmit {
            page_key,
            element_id,
            markdown,
        } => {
            app.spawn_edit_block(page_key, element_id, markdown, ui);
            false
        }
        Action::OnenoteToggleTodo(opt_idx) => {
            let idx = opt_idx.unwrap_or(ui.onenote_selected_block);
            app.spawn_toggle_todo(idx, ui);
            false
        }
        Action::ConfirmDeleteOnenotePage => {
            if let Some(page) = app.active_page_summary() {
                ui.overlays.push(Overlay::confirm(
                    format!("Delete OneNote page '{}'?", page.title),
                    Some(Action::DeleteOnenotePage),
                ));
            }
            false
        }
        Action::DeleteOnenotePage => {
            app.spawn_delete_active_page(ui);
            false
        }
        Action::OnenoteOpenWeb => {
            let url = app
                .active_page_summary()
                .and_then(|p| p.web_url.clone())
                .or_else(|| app.active_onenote_section().and_then(|s| s.web_url.clone()))
                .or_else(|| app.active_onenote_notebook().and_then(|n| n.web_url.clone()));
            if let Some(u) = url {
                return dispatch(Action::OpenUrl(u), app, ui);
            } else {
                ui.warn("No web URL available for this item");
            }
            false
        }
        Action::DownloadResource(idx) => {
            app.spawn_download_resource(idx, ui);
            false
        }
        Action::CopyActivePageMarkdown => {
            if let Some(detail) = app.active_page_detail() {
                match composer::copy_to_clipboard(&detail.body_markdown) {
                    Ok(()) => ui.success("Copied page Markdown to clipboard"),
                    Err(err) => ui.error(format!("Clipboard copy failed: {err}")),
                }
            }
            false
        }
        Action::SearchLocalPages(query) => {
            app.search_query = query.clone();
            app.search_results = app
                .storage
                .search_onenote_pages(&query, None, 40)
                .unwrap_or_default();
            false
        }
        Action::JumpToSearchHit(idx) => {
            if let Some(hit) = app.search_results.get(idx).cloned() {
                let sec_key = hit.scope.make_entity_key(&hit.section_id);
                if let Some(sec) = app.sections.iter().find(|s| s.section_key == sec_key) {
                    if let Some(nb_id) = sec.notebook_id.as_deref() {
                        app.active_notebook_key = Some(sec.scope.make_entity_key(nb_id));
                    }
                }
                app.select_section(sec_key, ui);
                app.select_page(hit.page_key, ui);
                ui.focus = Focus::Canvas;
                ui.narrow_pane = NarrowPane::Canvas;
            }
            false
        }
        Action::OpenUrl(url) => {
            match open::that(&url) {
                Ok(()) => ui.info(format!("Opened {url}")),
                Err(err) => ui.error(format!("Failed to open URL: {err}")),
            }
            false
        }
        Action::CopyUrlToClipboard(url) => {
            match composer::copy_to_clipboard(&url) {
                Ok(()) => ui.success(format!("Copied URL: {url}")),
                Err(err) => ui.error(format!("Clipboard copy failed: {err}")),
            }
            false
        }
        Action::AttachFormFile(path) => {
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .map(|s| s.to_ascii_lowercase())
                .unwrap_or_default();
            let is_image = matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "gif" | "webp");
            if let Some(Overlay::OnenoteForm(ref mut form)) = ui.overlays.last_mut() {
                if is_image {
                    match crate::cli::load_inline_images_from_paths(std::slice::from_ref(&path)) {
                        Ok(mut imgs) => {
                            if let Some(img) = imgs.pop() {
                                let name = img
                                    .file_name
                                    .clone()
                                    .unwrap_or_else(|| "image.png".to_string());
                                form.attach_inline_image(img);
                                ui.success(format!("Attached image '{name}'"));
                            }
                        }
                        Err(err) => ui.error(format!("Failed to attach image: {err}")),
                    }
                } else {
                    match std::fs::read_to_string(&path) {
                        Ok(content) => {
                            if !form.body.is_empty() && !form.body.ends_with('\n') {
                                form.body.push('\n');
                            }
                            form.body.push_str(&content);
                            ui.success(format!("Imported text from {}", path.display()));
                        }
                        Err(err) => ui.error(format!("Failed to read file: {err}")),
                    }
                }
            } else {
                ui.info(format!("Selected file: {} (open a note form with `n` or `a` first to attach)", path.display()));
            }
            false
        }
        Action::ToggleNavRail => {
            ui.show_nav_rail = !ui.show_nav_rail;
            app.persist_config(ui);
            false
        }
        Action::ToggleNotify => {
            ui.notify = !ui.notify;
            app.persist_config(ui);
            ui.info(format!(
                "Desktop notifications: {}",
                if ui.notify { "On" } else { "Off" }
            ));
            false
        }
        Action::ToggleInlineImages => {
            ui.inline_images = !ui.inline_images;
            app.persist_config(ui);
            ui.info(format!(
                "Inline image previews: {}",
                if ui.inline_images { "On" } else { "Off" }
            ));
            false
        }
        Action::ToggleShowLinkUrls => {
            ui.show_inline_hrefs = !ui.show_inline_hrefs;
            app.persist_config(ui);
            ui.info(format!(
                "Inline link URLs (href): {}",
                if ui.show_inline_hrefs { "Visible" } else { "Hidden" }
            ));
            false
        }
        Action::ToggleDensity => {
            ui.density = match ui.density {
                DensityMode::Cozy => DensityMode::Compact,
                DensityMode::Compact => DensityMode::Cozy,
            };
            app.persist_config(ui);
            ui.info(format!("List density: {}", ui.density.label()));
            false
        }
        Action::CycleTheme => {
            ui.theme_preset = match ui.theme_preset {
                ThemePreset::Auto => ThemePreset::Dark,
                ThemePreset::Dark => ThemePreset::Light,
                ThemePreset::Light => ThemePreset::Ansi256,
                ThemePreset::Ansi256 => ThemePreset::Ansi16,
                ThemePreset::Ansi16 => ThemePreset::NoColor,
                ThemePreset::NoColor => ThemePreset::Auto,
            };
            ui.theme = Theme::detect(ui.theme_preset);
            app.persist_config(ui);
            ui.info(format!("Theme: {}", ui.theme_preset.label()));
            false
        }
        Action::CycleImageProtocol => {
            ui.graphics_caps.protocol = match ui.graphics_caps.protocol {
                TerminalImageProtocol::Kitty => TerminalImageProtocol::ITerm2,
                TerminalImageProtocol::ITerm2 => TerminalImageProtocol::Sixel,
                TerminalImageProtocol::Sixel => TerminalImageProtocol::Halfblock,
                TerminalImageProtocol::Halfblock => TerminalImageProtocol::Kitty,
            };
            app.persist_config(ui);
            ui.info(format!(
                "Image protocol: {}",
                ui.graphics_caps.protocol.label()
            ));
            if matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))) {
                return dispatch(Action::LoadPreviewImage, app, ui);
            }
            false
        }
        Action::ShowDownloadDir => {
            ui.info(format!("Download folder: {}", app.download_dir.display()));
            false
        }
        Action::ShowToast(msg) => {
            ui.info(msg);
            false
        }
        Action::CheckForUpdates => {
            app.spawn_update_check(false, ui);
            false
        }
        Action::ApplySelfUpdate => {
            app.spawn_update_check(true, ui);
            false
        }
        Action::MouseClick { target, col, row } => {
            handle_mouse_click(target, col, row, app, ui)
        }
        Action::MouseScroll { delta, .. } => {
            if !ui.overlays.is_empty() {
                let act = if delta < 0 {
                    Action::MoveUp
                } else {
                    Action::MoveDown
                };
                return dispatch(act, app, ui);
            }
            if delta < 0 {
                ui.onenote_scroll = ui.onenote_scroll.saturating_sub(delta.unsigned_abs() as usize);
            } else {
                ui.onenote_scroll = ui.onenote_scroll.saturating_add(delta as usize);
            }
            ui.onenote_follow_selection = false;
            false
        }
        Action::RequestExternalEditor => {
            ui.info("Press Ctrl+E inside a OneNote editor modal to open $EDITOR");
            false
        }
        Action::Tab
        | Action::Input(_)
        | Action::Newline
        | Action::InsertNewline
        | Action::Backspace
        | Action::Delete
        | Action::Paste(_)
        | Action::PasteClipboard
        | Action::ClearPendingImages
        | Action::OverlaySubmit
        | Action::CursorLeft
        | Action::CursorRight
        | Action::CursorHome
        | Action::CursorEnd => false,
    }
}

fn sync_narrow_from_focus(ui: &mut UiState) {
    match ui.focus {
        Focus::Nav => ui.narrow_pane = NarrowPane::Notebooks,
        Focus::Pages => ui.narrow_pane = NarrowPane::Pages,
        Focus::Canvas => ui.narrow_pane = NarrowPane::Canvas,
        Focus::Overlay => {}
    }
}

fn move_notebook_selection(app: &mut AppState, ui: &mut UiState, delta: i32) {
    if app.notebooks.is_empty() {
        return;
    }
    let cur = app
        .active_notebook_key
        .as_deref()
        .and_then(|k| app.notebooks.iter().position(|n| n.notebook_key == k))
        .unwrap_or(0);
    let next = if delta < 0 {
        cur.saturating_sub(1)
    } else {
        (cur + 1).min(app.notebooks.len() - 1)
    };
    if let Some(nb) = app.notebooks.get(next) {
        let key = nb.notebook_key.clone();
        if app.active_notebook_key.as_deref() != Some(key.as_str()) {
            app.select_notebook(key, ui);
        }
    }
}

fn move_page_selection(app: &mut AppState, ui: &mut UiState, delta: i32) {
    if app.pages.is_empty() {
        return;
    }
    let cur = app
        .active_page_key
        .as_deref()
        .and_then(|k| app.pages.iter().position(|p| p.page_key == k))
        .unwrap_or(0);
    let next = if delta < 0 {
        cur.saturating_sub(1)
    } else {
        (cur + 1).min(app.pages.len() - 1)
    };
    if let Some(p) = app.pages.get(next) {
        let key = p.page_key.clone();
        if app.active_page_key.as_deref() != Some(key.as_str()) {
            app.select_page(key, ui);
        }
    }
}

fn handle_mouse_click(
    target: HitTarget,
    _col: u16,
    _row: u16,
    app: &mut AppState,
    ui: &mut UiState,
) -> bool {
    match target {
        HitTarget::OverlayBody => false,
        HitTarget::OverlayItem { index } => {
            if let Some(top) = ui.overlays.last_mut() {
                top.select(index);
            }
            dispatch(Action::Activate, app, ui)
        }
        HitTarget::NavRow(idx) => {
            ui.overlays.clear();
            ui.focus = Focus::Nav;
            if let Some(nb) = app.notebooks.get(idx) {
                let key = nb.notebook_key.clone();
                app.select_notebook(key, ui);
            }
            false
        }
        HitTarget::OnenoteSectionTab(idx) => {
            ui.overlays.clear();
            let secs: Vec<String> = app
                .sections_for_active_notebook()
                .into_iter()
                .map(|s| s.section_key.clone())
                .collect();
            if let Some(sk) = secs.get(idx) {
                app.select_section(sk.clone(), ui);
            }
            false
        }
        HitTarget::OnenotePageItem(idx) => {
            ui.overlays.clear();
            ui.focus = Focus::Pages;
            if let Some(p) = app.pages.get(idx) {
                let key = p.page_key.clone();
                app.select_page(key, ui);
            }
            false
        }
        HitTarget::OnenoteBlock(idx) => {
            ui.overlays.clear();
            ui.focus = Focus::Canvas;
            ui.onenote_selected_block = idx;
            false
        }
        HitTarget::OnenoteTodoToggle(idx) => {
            ui.overlays.clear();
            ui.focus = Focus::Canvas;
            ui.onenote_selected_block = idx;
            app.spawn_toggle_todo(idx, ui);
            false
        }
        HitTarget::OnenoteResource(res_idx) => {
            ui.overlays.clear();
            if app
                .active_page_detail()
                .and_then(|d| d.resources.get(res_idx))
                .is_some_and(|r| r.is_image)
            {
                dispatch(Action::OpenImagePreviewAt(res_idx), app, ui)
            } else {
                app.spawn_download_resource(res_idx, ui);
                false
            }
        }
        HitTarget::Button(btn) => match btn {
            ButtonId::ConfirmOk => dispatch(Action::Activate, app, ui),
            ButtonId::ConfirmCancel => dispatch(Action::CloseOverlay, app, ui),
            ButtonId::Search => dispatch(Action::OpenSearch, app, ui),
            ButtonId::CommandPalette => {
                dispatch(Action::OpenOverlay(OverlayKind::CommandPalette), app, ui)
            }
            ButtonId::DensityToggle => dispatch(Action::ToggleDensity, app, ui),
            ButtonId::Help => dispatch(Action::OpenOverlay(OverlayKind::Help), app, ui),
            ButtonId::Settings => dispatch(Action::OpenOverlay(OverlayKind::Settings), app, ui),
            ButtonId::Refresh => dispatch(Action::RefreshAll, app, ui),
            ButtonId::OnenoteNewNotebook => dispatch(Action::OpenOnenoteCreateNotebook, app, ui),
            ButtonId::OnenoteNewSection => dispatch(Action::OpenOnenoteCreateSection, app, ui),
            ButtonId::OnenoteNewPage => {
                dispatch(Action::OpenOnenoteCreatePage { level: 0 }, app, ui)
            }
            ButtonId::OnenoteNewSubpage => {
                dispatch(Action::OpenOnenoteCreatePage { level: 1 }, app, ui)
            }
            ButtonId::OnenoteAppend => dispatch(Action::OpenOnenoteAppend, app, ui),
            ButtonId::OnenoteEditBlock => dispatch(Action::OpenOnenoteEditBlock, app, ui),
            ButtonId::OnenoteToggleTodo => dispatch(Action::OnenoteToggleTodo(None), app, ui),
            ButtonId::OnenoteDeletePage => dispatch(Action::ConfirmDeleteOnenotePage, app, ui),
            ButtonId::OnenoteOpenWeb => dispatch(Action::OnenoteOpenWeb, app, ui),
            ButtonId::OnenoteLinks => dispatch(Action::OpenLinkExtractor, app, ui),
            ButtonId::OnenoteToggleHrefs => dispatch(Action::ToggleShowLinkUrls, app, ui),
            ButtonId::Attachments => {
                dispatch(Action::OpenOverlay(OverlayKind::Attachments), app, ui)
            }
            ButtonId::ImagePreview => dispatch(Action::OpenImagePreview, app, ui),
            ButtonId::NarrowNotebooks => dispatch(Action::SetFocus(Focus::Nav), app, ui),
            ButtonId::NarrowPages => dispatch(Action::SetFocus(Focus::Pages), app, ui),
            ButtonId::NarrowCanvas => dispatch(Action::SetFocus(Focus::Canvas), app, ui),
        },
    }
}
