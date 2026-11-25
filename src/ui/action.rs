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
            html_preview::HtmlPreviewOverlay,
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
    OpenSearchWithQuery(String),
    OpenFilePicker,
    OpenLinkExtractor,
    OpenImagePreview,
    OpenImagePreviewAt(usize),
    LoadPreviewImage,
    OpenHtmlPreview,
    OpenHtmlContent { title: String, html: String },
    OpenHtmlResource(usize),
    ServeHtmlPreviewInBrowser,
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
        base_content: String,
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
    MouseRightClick { col: u16, row: u16 },
    MouseScroll { delta: i32, column: u16, row: u16 },
}

/// Dispatches an `Action` against `AppState` and `UiState`. Returns `true` when the TUI should quit.
pub fn dispatch(action: Action, app: &mut AppState, ui: &mut UiState) -> bool {
    // Block right-clicks while any modal overlay is open, except when replacing an open ContextMenu
    // by right-clicking outside the menu itself.
    if let Action::MouseRightClick { col, row } = action {
        if !ui.overlays.is_empty() {
            if ui.overlays.len() == 1
                && matches!(ui.overlays.last(), Some(Overlay::ContextMenu(_)))
            {
                if matches!(
                    ui.hitmap.hit(row, col),
                    Some(HitTarget::OverlayBody | HitTarget::OverlayItem { .. })
                ) {
                    return false;
                }
                ui.overlays.pop();
            } else {
                return false;
            }
        }
        return dispatch_inner(Action::MouseRightClick { col, row }, app, ui);
    }

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
                OverlayOutcome::Close => {
                    // Auto-stash draft whenever a OneNote form is closed without submitting
                    // (Esc, backdrop click, Cancel button click, or Enter/Space on Cancel).
                    if let Overlay::OnenoteForm(ref form) = top {
                        if let Some((key, entry)) = form.draft_entry_snapshot() {
                            app.save_draft_entry(key, entry);
                        }
                    }
                }
                OverlayOutcome::CloseAndDispatch(_) => {}
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
                return dispatch_inner(next, app, ui);
            }
            OverlayOutcome::NotHandled => {}
        }
    }

    dispatch_inner(action, app, ui)
}

fn dispatch_inner(action: Action, app: &mut AppState, ui: &mut UiState) -> bool {
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
            match ui.focus {
                Focus::Nav => move_notebook_selection(app, ui, -8),
                Focus::Pages => move_page_selection(app, ui, -8),
                Focus::Canvas | Focus::Overlay => {
                    ui.onenote_scroll = ui.onenote_scroll.saturating_sub(12);
                    ui.onenote_follow_selection = false;
                }
            }
            false
        }
        Action::PageDown => {
            match ui.focus {
                Focus::Nav => move_notebook_selection(app, ui, 8),
                Focus::Pages => move_page_selection(app, ui, 8),
                Focus::Canvas | Focus::Overlay => {
                    ui.onenote_scroll = ui.onenote_scroll.saturating_add(12);
                    ui.onenote_follow_selection = false;
                }
            }
            false
        }
        Action::ScrollTop => {
            match ui.focus {
                Focus::Nav => {
                    if let Some(first) = app.notebooks.first() {
                        let key = first.notebook_key.clone();
                        if app.active_notebook_key.as_deref() != Some(key.as_str()) {
                            app.select_notebook(key, ui);
                        }
                    }
                }
                Focus::Pages => {
                    if let Some(first) = app.pages.first() {
                        let key = first.page_key.clone();
                        if app.active_page_key.as_deref() != Some(key.as_str()) {
                            app.select_page(key, ui);
                        }
                    }
                }
                Focus::Canvas | Focus::Overlay => {
                    ui.onenote_scroll = 0;
                    ui.onenote_selected_block = 0;
                    ui.onenote_follow_selection = false;
                }
            }
            false
        }
        Action::ScrollBottom => {
            match ui.focus {
                Focus::Nav => {
                    if let Some(last) = app.notebooks.last() {
                        let key = last.notebook_key.clone();
                        if app.active_notebook_key.as_deref() != Some(key.as_str()) {
                            app.select_notebook(key, ui);
                        }
                    }
                }
                Focus::Pages => {
                    if let Some(last) = app.pages.last() {
                        let key = last.page_key.clone();
                        if app.active_page_key.as_deref() != Some(key.as_str()) {
                            app.select_page(key, ui);
                        }
                    }
                }
                Focus::Canvas | Focus::Overlay => {
                    let block_count = app
                        .active_page_detail()
                        .map(|d| d.blocks.len())
                        .unwrap_or(0);
                    ui.onenote_selected_block = block_count.saturating_sub(1);
                    ui.onenote_follow_selection = true;
                }
            }
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
                                ref name,
                                ref mime_type,
                                ..
                            } = blk.kind
                            {
                                if crate::ui::app::is_markup_resource_meta(
                                    Some(mime_type),
                                    Some(name),
                                ) {
                                    return dispatch(Action::OpenHtmlResource(resource_index), app, ui);
                                }
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
            app.cancel_pending_html_resource_load();
            ui.overlays.pop();
            false
        }
        Action::OpenOverlay(kind) => {
            match kind {
                OverlayKind::CommandPalette => ui.overlays.push(Overlay::command_palette()),
                OverlayKind::Settings => ui.overlays.push(Overlay::settings()),
                OverlayKind::Search => {
                    app.search_query.clear();
                    app.search_results.clear();
                    ui.overlays.push(Overlay::search());
                }
                OverlayKind::Attachments => ui.overlays.push(Overlay::attachments()),
                OverlayKind::PathInput => ui.overlays.push(Overlay::path_input()),
                OverlayKind::FilePicker => ui.overlays.push(Overlay::file_picker()),
                OverlayKind::Help => ui.overlays.push(Overlay::help()),
                OverlayKind::Links => return dispatch(Action::OpenLinkExtractor, app, ui),
                OverlayKind::ImagePreview => return dispatch(Action::OpenImagePreview, app, ui),
                OverlayKind::HtmlPreview => return dispatch(Action::OpenHtmlPreview, app, ui),
                _ => {}
            }
            false
        }
        Action::OpenSearch => {
            app.search_query.clear();
            app.search_results.clear();
            ui.overlays.push(Overlay::search());
            false
        }
        Action::OpenSearchWithQuery(query) => {
            app.search_query = query.clone();
            app.search_results = if query.trim().is_empty() {
                Vec::new()
            } else {
                app.storage
                    .search_onenote_pages(&query, None, 40)
                    .unwrap_or_default()
            };
            ui.overlays.push(Overlay::search_with_query(query));
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
                if let Some(res) = detail.resources.get(res_idx) {
                    if crate::ui::app::is_markup_resource_meta(
                        Some(&res.mime_type),
                        res.file_name.as_deref(),
                    ) {
                        return dispatch(Action::OpenHtmlResource(res_idx), app, ui);
                    }
                }
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
                preview.current().and_then(|target| {
                    let fit = preview.fit_box(ui.term_width, ui.term_height);
                    let cache_key = preview.current_cache_key(
                        ui.term_width,
                        ui.term_height,
                        ui.graphics_caps,
                    )?;
                    Some((
                        cache_key,
                        target.url.clone(),
                        target.display_name.clone(),
                        target.mime_type.clone(),
                        fit,
                    ))
                })
            } else {
                None
            };
            if let Some((cache_key, url, display_name, mime_type, fit)) = req {
                let bg_rgb = ui.modal_image_bg_rgb();
                app.request_onenote_image_with_meta(
                    cache_key,
                    url,
                    Some(display_name),
                    Some(mime_type),
                    fit,
                    bg_rgb,
                    ui.graphics_caps,
                    false,
                );
            }
            false
        }
        Action::OpenHtmlPreview => {
            if let Some(detail) = app.active_page_detail() {
                let ov = HtmlPreviewOverlay::from_onenote_page(detail);
                ui.overlays.push(Overlay::HtmlPreview(ov));
            } else {
                ui.warn("Open a OneNote page first to inspect HTML");
            }
            false
        }
        Action::OpenHtmlContent { title, html } => {
            ui.overlays
                .push(Overlay::HtmlPreview(HtmlPreviewOverlay::new(Some(title), html)));
            false
        }
        Action::OpenHtmlResource(res_idx) => {
            app.spawn_load_html_resource(res_idx, ui);
            false
        }
        Action::ServeHtmlPreviewInBrowser => {
            let payload = if let Some(Overlay::HtmlPreview(preview)) = ui.overlays.last() {
                Some((preview.title(), preview.serve_html()))
            } else {
                None
            };
            if let Some((title, html)) = payload {
                app.spawn_serve_html_in_browser(title, html, ui);
            }
            false
        }
        Action::OpenContextMenu { col, row } => {
            if !ui.overlays.is_empty() {
                return false;
            }
            let target = match ui.focus {
                Focus::Pages => ContextTarget::PageItem,
                Focus::Canvas => ContextTarget::CanvasBlock,
                _ => ContextTarget::Global,
            };
            let menu = ContextMenu::build(target, (col, row), app, ui);
            ui.overlays.push(Overlay::context_menu(menu));
            false
        }
        Action::MouseRightClick { col, row } => {
            if !ui.overlays.is_empty() {
                return false;
            }
            let target = match ui.hitmap.hit_under_overlay(row, col) {
                Some(HitTarget::OnenotePageItem(idx)) => {
                    ui.focus = Focus::Pages;
                    sync_narrow_from_focus(ui);
                    if let Some(p) = app.pages.get(idx) {
                        let key = p.page_key.clone();
                        if app.active_page_key.as_deref() != Some(key.as_str()) {
                            app.select_page(key, ui);
                        }
                    }
                    ContextTarget::PageItem
                }
                Some(HitTarget::PanePages) => {
                    ui.focus = Focus::Pages;
                    sync_narrow_from_focus(ui);
                    ContextTarget::PageItem
                }
                Some(HitTarget::OnenoteBlock(idx) | HitTarget::OnenoteTodoToggle(idx)) => {
                    ui.focus = Focus::Canvas;
                    sync_narrow_from_focus(ui);
                    ui.onenote_selected_block = idx;
                    ContextTarget::CanvasBlock
                }
                Some(HitTarget::OnenoteResource(_) | HitTarget::PaneCanvas) => {
                    ui.focus = Focus::Canvas;
                    sync_narrow_from_focus(ui);
                    ContextTarget::CanvasBlock
                }
                Some(HitTarget::NavRow(idx)) => {
                    ui.focus = Focus::Nav;
                    sync_narrow_from_focus(ui);
                    if let Some(nb) = app.notebooks.get(idx) {
                        let key = nb.notebook_key.clone();
                        if app.active_notebook_key.as_deref() != Some(key.as_str()) {
                            app.select_notebook(key, ui);
                        }
                    }
                    ContextTarget::Global
                }
                Some(HitTarget::PaneNav) => {
                    ui.focus = Focus::Nav;
                    sync_narrow_from_focus(ui);
                    ContextTarget::Global
                }
                Some(HitTarget::OnenoteSectionTab(idx)) => {
                    let secs: Vec<String> = app
                        .sections_for_active_notebook()
                        .into_iter()
                        .map(|s| s.section_key.clone())
                        .collect();
                    if let Some(sk) = secs.get(idx) {
                        if app.active_section_key.as_deref() != Some(sk.as_str()) {
                            app.select_section(sk.clone(), ui);
                        }
                    }
                    ContextTarget::Global
                }
                _ => match ui.focus {
                    Focus::Pages => ContextTarget::PageItem,
                    Focus::Canvas => ContextTarget::CanvasBlock,
                    _ => ContextTarget::Global,
                },
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
                let draft = app
                    .drafts
                    .get_note(&format!("create:{}", sec.section_key))
                    .or_else(|| app.drafts.get_note(&format!("new_page:{}", sec.section_key)));
                if let Some(draft) = draft {
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
                        let initial_md = parser::block_baseline_content(blk);
                        let mut form = OnenoteFormOverlay::edit_block(
                            detail.summary.page_key.clone(),
                            detail.summary.title.clone(),
                            eid.clone(),
                            idx,
                            initial_md.clone(),
                        );
                        let draft_key = format!("edit:{}:{eid}", detail.summary.page_key);
                        if let Some(draft) = app.drafts.get_note(&draft_key) {
                            form.body = draft.body.clone();
                            if let Some(saved_base) = draft.base_content.as_deref() {
                                if let crate::ui::overlays::onenote_form::OnenoteFormMode::EditBlock {
                                    ref mut base_content,
                                    ..
                                } = form.mode
                                {
                                    *base_content = saved_base.to_string();
                                }
                                if !parser::block_matches_expected(blk, saved_base) {
                                    form.error = Some(
                                        "Remote block changed since this draft was saved; review before saving."
                                            .to_string(),
                                    );
                                }
                            }
                        }
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
            base_content,
        } => {
            app.spawn_edit_block(page_key, element_id, markdown, base_content, ui);
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
            app.search_results = if query.trim().is_empty() {
                Vec::new()
            } else {
                app.storage
                    .search_onenote_pages(&query, None, 40)
                    .unwrap_or_default()
            };
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
            app.image_cache.clear();
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
            ui.image_protocol_user_chosen = true;
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
        Action::MouseScroll { delta, column, row } => {
            if let Some(Overlay::ImagePreview(preview)) = ui.overlays.last_mut() {
                let _ = preview.handle(&Action::MouseScroll { delta, column, row });
                return false;
            }
            if matches!(
                ui.overlays.last(),
                Some(Overlay::HtmlPreview(_) | Overlay::OnenoteForm(_) | Overlay::Help(_))
            ) {
                if let Some(mut top) = ui.overlays.pop() {
                    let _ = top.handle(&Action::MouseScroll { delta, column, row }, app, ui);
                    ui.overlays.push(top);
                }
                return false;
            }
            if !ui.overlays.is_empty() {
                let act = if delta < 0 {
                    Action::MoveUp
                } else {
                    Action::MoveDown
                };
                return dispatch(act, app, ui);
            }
            match ui.hitmap.hit(row, column) {
                Some(HitTarget::NavRow(_) | HitTarget::PaneNav) => {
                    move_notebook_selection(app, ui, if delta < 0 { -1 } else { 1 });
                }
                Some(HitTarget::OnenotePageItem(_) | HitTarget::PanePages) => {
                    move_page_selection(app, ui, if delta < 0 { -1 } else { 1 });
                }
                _ => {
                    if delta < 0 {
                        ui.onenote_scroll =
                            ui.onenote_scroll.saturating_sub(delta.unsigned_abs() as usize);
                    } else {
                        ui.onenote_scroll = ui.onenote_scroll.saturating_add(delta as usize);
                    }
                    ui.onenote_follow_selection = false;
                }
            }
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
    let step = delta.unsigned_abs() as usize;
    let next = if delta < 0 {
        cur.saturating_sub(step)
    } else {
        cur.saturating_add(step).min(app.notebooks.len() - 1)
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
    let step = delta.unsigned_abs() as usize;
    let next = if delta < 0 {
        cur.saturating_sub(step)
    } else {
        cur.saturating_add(step).min(app.pages.len() - 1)
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
    col: u16,
    row: u16,
    app: &mut AppState,
    ui: &mut UiState,
) -> bool {
    match target {
        HitTarget::OverlayBackdrop => dispatch(Action::CloseOverlay, app, ui),
        HitTarget::OverlayBody => {
            if matches!(ui.overlays.last(), Some(Overlay::HtmlPreview(_))) {
                let outcome = if let Some(Overlay::HtmlPreview(mut preview)) = ui.overlays.pop() {
                    let res = preview.handle_click(col, row, ui);
                    match &res {
                        OverlayOutcome::Close | OverlayOutcome::CloseAndDispatch(_) => {}
                        _ => ui.overlays.push(Overlay::HtmlPreview(preview)),
                    }
                    res
                } else {
                    OverlayOutcome::Handled
                };
                match outcome {
                    OverlayOutcome::Dispatch(next) | OverlayOutcome::CloseAndDispatch(next) => {
                        return dispatch_inner(next, app, ui);
                    }
                    _ => {}
                }
            }
            false
        }
        HitTarget::OverlayItem { index } => {
            let is_form = matches!(ui.overlays.last(), Some(Overlay::OnenoteForm(_)));
            if let Some(top) = ui.overlays.last_mut() {
                top.select(index);
            }
            if is_form {
                // Clicking a text input field in OnenoteForm only focuses that field;
                // never submit, jump focus away, or insert '\n'.
                false
            } else {
                dispatch(Action::Activate, app, ui)
            }
        }
        HitTarget::PaneNav => {
            ui.overlays.clear();
            ui.focus = Focus::Nav;
            sync_narrow_from_focus(ui);
            false
        }
        HitTarget::PanePages => {
            ui.overlays.clear();
            ui.focus = Focus::Pages;
            sync_narrow_from_focus(ui);
            false
        }
        HitTarget::PaneCanvas => {
            ui.overlays.clear();
            ui.focus = Focus::Canvas;
            sync_narrow_from_focus(ui);
            false
        }
        HitTarget::NavRow(idx) => {
            ui.overlays.clear();
            ui.focus = Focus::Nav;
            sync_narrow_from_focus(ui);
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
            sync_narrow_from_focus(ui);
            if let Some(p) = app.pages.get(idx) {
                let key = p.page_key.clone();
                app.select_page(key, ui);
            }
            false
        }
        HitTarget::OnenoteBlock(idx) => {
            ui.overlays.clear();
            ui.focus = Focus::Canvas;
            sync_narrow_from_focus(ui);
            ui.onenote_selected_block = idx;
            false
        }
        HitTarget::OnenoteTodoToggle(idx) => {
            ui.overlays.clear();
            ui.focus = Focus::Canvas;
            sync_narrow_from_focus(ui);
            ui.onenote_selected_block = idx;
            app.spawn_toggle_todo(idx, ui);
            false
        }
        HitTarget::OnenoteResource(res_idx) => {
            ui.overlays.clear();
            let res_meta = app
                .active_page_detail()
                .and_then(|d| d.resources.get(res_idx))
                .map(|r| (r.is_image, r.mime_type.clone(), r.file_name.clone()));
            match res_meta {
                Some((_, mime, name))
                    if crate::ui::app::is_markup_resource_meta(Some(&mime), name.as_deref()) =>
                {
                    dispatch(Action::OpenHtmlResource(res_idx), app, ui)
                }
                Some((true, _, _)) => dispatch(Action::OpenImagePreviewAt(res_idx), app, ui),
                _ => {
                    app.spawn_download_resource(res_idx, ui);
                    false
                }
            }
        }
        HitTarget::Button(btn) => {
            if !matches!(
                btn,
                ButtonId::ConfirmOk
                    | ButtonId::ConfirmCancel
                    | ButtonId::FormSubmit
                    | ButtonId::FormCancel
                    | ButtonId::HtmlToggleRaw
                    | ButtonId::HtmlServeBrowser
            ) {
                ui.overlays.clear();
            }
            match btn {
                ButtonId::ConfirmOk => dispatch(Action::Activate, app, ui),
                ButtonId::ConfirmCancel => {
                    if matches!(ui.overlays.last(), Some(Overlay::HtmlPreview(_))) {
                        // `[✕ Esc]` closes HtmlPreview immediately even while editing an `<input>`.
                        ui.overlays.pop();
                        false
                    } else {
                        dispatch(Action::CloseOverlay, app, ui)
                    }
                }
                ButtonId::FormSubmit => {
                    if let Some(Overlay::OnenoteForm(ref mut form)) = ui.overlays.last_mut() {
                        form.focus_submit_button();
                    }
                    dispatch(Action::OverlaySubmit, app, ui)
                }
                ButtonId::FormCancel => {
                    if let Some(Overlay::OnenoteForm(ref mut form)) = ui.overlays.last_mut() {
                        form.focus_cancel_button();
                    }
                    dispatch(Action::CloseOverlay, app, ui)
                }
                ButtonId::HtmlPreview => dispatch(Action::OpenHtmlPreview, app, ui),
                ButtonId::HtmlToggleRaw => {
                    if let Some(Overlay::HtmlPreview(ref mut preview)) = ui.overlays.last_mut() {
                        preview.toggle_raw_mode();
                    }
                    false
                }
                ButtonId::HtmlServeBrowser => {
                    dispatch_inner(Action::ServeHtmlPreviewInBrowser, app, ui)
                }
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
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use ratatui::{backend::TestBackend, Terminal};
    use tokio::sync::{mpsc, watch};

    use super::*;
    use crate::{
        auth::{AuthManager, StoredTokens},
        config::AppConfig,
        graph::GraphClient,
        models::{OnenoteNotebook, OnenotePageSummary, OnenoteScope, OnenoteSection},
        storage::Storage,
        sync::{FocusTarget, SyncController},
        ui::{self, app::UiMsg},
    };

    fn make_ui_test_harness() -> (AppState, UiState, mpsc::UnboundedReceiver<SyncCommand>) {
        let cfg = AppConfig::default();
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("u1".to_string()),
            user_display_name: Some("Tester".to_string()),
            user_email: None,
            auth_mode: "access_token".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
        let (ui_tx, _ui_rx) = mpsc::unbounded_channel::<UiMsg>();
        let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);

        let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
        app.notebooks.push(OnenoteNotebook {
            notebook_key: "me|nb1".to_string(),
            id: "nb1".to_string(),
            scope: OnenoteScope::Me,
            display_name: "Eng".to_string(),
            color: None,
            is_default: true,
            is_shared: false,
            user_role: None,
            created_at: None,
            last_modified_at: None,
            created_by: None,
            web_url: None,
        });
        app.sections.push(OnenoteSection {
            section_key: "me|sec1".to_string(),
            id: "sec1".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb1".to_string()),
            notebook_name: Some("Eng".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "Notes".to_string(),
            color: None,
            is_default: true,
            pages_url: None,
            created_at: None,
            last_modified_at: None,
            web_url: None,
        });
        app.pages.push(OnenotePageSummary {
            page_key: "me|p1".to_string(),
            id: "p1".to_string(),
            scope: OnenoteScope::Me,
            section_id: "sec1".to_string(),
            section_name: Some("Notes".to_string()),
            notebook_id: Some("nb1".to_string()),
            notebook_name: Some("Eng".to_string()),
            title: "Page 1".to_string(),
            level: 0,
            order: 0,
            created_at: None,
            last_modified_at: None,
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: false,
        });
        app.active_notebook_key = Some("me|nb1".to_string());
        app.active_section_key = Some("me|sec1".to_string());
        app.active_page_key = Some("me|p1".to_string());

        let ui = UiState::from_config(&cfg);
        (app, ui, nudge_rx)
    }

    #[test]
    fn test_all_header_toolbar_and_status_buttons_register_and_dispatch_clicks() {
        let (mut app, mut ui, mut nudge_rx) = make_ui_test_harness();
        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();

        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        // Every wide-mode button must be registered in HitMap
        let expected_wide_buttons = [
            ButtonId::Refresh,
            ButtonId::DensityToggle,
            ButtonId::Search,
            ButtonId::CommandPalette,
            ButtonId::Settings,
            ButtonId::Help,
            ButtonId::OnenoteNewNotebook,
            ButtonId::OnenoteNewSection,
            ButtonId::OnenoteNewPage,
            ButtonId::OnenoteAppend,
            ButtonId::OnenoteEditBlock,
            ButtonId::OnenoteToggleTodo,
            ButtonId::OnenoteLinks,
            ButtonId::OnenoteToggleHrefs,
            ButtonId::ImagePreview,
            ButtonId::HtmlPreview,
            ButtonId::Attachments,
            ButtonId::OnenoteOpenWeb,
            ButtonId::OnenoteDeletePage,
        ];
        for btn in expected_wide_buttons {
            assert!(
                ui.hitmap.region_of(HitTarget::Button(btn)).is_some(),
                "expected HitTarget::Button({btn:?}) to be registered in HitMap"
            );
        }

        // Clicking DensityToggle flips Cozy <-> Compact
        let initial_density = ui.density;
        let density_rect = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::DensityToggle))
            .unwrap();
        let target = ui.hitmap.hit(density_rect.y, density_rect.x).unwrap();
        dispatch(
            Action::MouseClick {
                target,
                col: density_rect.x,
                row: density_rect.y,
            },
            &mut app,
            &mut ui,
        );
        assert_ne!(ui.density, initial_density);

        // Clicking Refresh triggers SyncCommand::RefreshAll
        let refresh_rect = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::Refresh))
            .unwrap();
        let target = ui.hitmap.hit(refresh_rect.y, refresh_rect.x).unwrap();
        dispatch(
            Action::MouseClick {
                target,
                col: refresh_rect.x,
                row: refresh_rect.y,
            },
            &mut app,
            &mut ui,
        );
        assert!(app.syncing);
        assert_eq!(nudge_rx.try_recv().ok(), Some(SyncCommand::RefreshAll));

        // Clicking Attachments opens Attachments overlay
        let att_rect = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::Attachments))
            .unwrap();
        let target = ui.hitmap.hit(att_rect.y, att_rect.x).unwrap();
        dispatch(
            Action::MouseClick {
                target,
                col: att_rect.x,
                row: att_rect.y,
            },
            &mut app,
            &mut ui,
        );
        assert!(matches!(ui.overlays.last(), Some(Overlay::Attachments(_))));

        // Clicking Search while Attachments is open dismisses Attachments and opens Search
        let search_rect = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::Search))
            .unwrap();
        dispatch(
            Action::MouseClick {
                target: HitTarget::Button(ButtonId::Search),
                col: search_rect.x,
                row: search_rect.y,
            },
            &mut app,
            &mut ui,
        );
        assert!(matches!(ui.overlays.last(), Some(Overlay::Search(_))));
        assert_eq!(ui.overlays.len(), 1);
    }

    #[test]
    fn test_narrow_bar_buttons_register_and_switch_focus() {
        let (mut app, mut ui, _nudge_rx) = make_ui_test_harness();
        let mut terminal = Terminal::new(TestBackend::new(70, 24)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        for (btn, expected_focus) in [
            (ButtonId::NarrowNotebooks, Focus::Nav),
            (ButtonId::NarrowPages, Focus::Pages),
            (ButtonId::NarrowCanvas, Focus::Canvas),
        ] {
            let rect = ui
                .hitmap
                .region_of(HitTarget::Button(btn))
                .expect("narrow bar button must be registered");
            let hit = ui.hitmap.hit(rect.y, rect.x).unwrap();
            assert_eq!(hit, HitTarget::Button(btn));
            dispatch(
                Action::MouseClick {
                    target: hit,
                    col: rect.x,
                    row: rect.y,
                },
                &mut app,
                &mut ui,
            );
            assert_eq!(ui.focus, expected_focus);
        }
    }

    #[tokio::test]
    async fn test_html_preview_pill_shortcut_native_serving_and_click_outside() {
        let (mut app, mut ui, _nudge_rx) = make_ui_test_harness();
        // Attach a separate ui_rx so we can inspect UiMsg::HtmlServed without launching a real browser
        let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMsg>();
        app.ui_tx = ui_tx;

        let summary = app.pages[0].clone();
        let raw_xhtml = r#"<!DOCTYPE html><html><head><title>Spec HTML</title></head><body><p id="out">Init</p><button onclick="document.getElementById('out').textContent='Clicked!'">Run</button></body></html>"#;
        app.page_details.insert(
            summary.page_key.clone(),
            crate::models::OnenotePageDetail {
                summary,
                raw_xhtml: raw_xhtml.to_string(),
                body_markdown: "Init".to_string(),
                body_plain: "Init".to_string(),
                blocks: Vec::new(),
                resources: Vec::new(),
                primary_div_id: None,
                synced_at: None,
            },
        );

        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        // 1. Clicking [H: HTML] pill opens Overlay::HtmlPreview
        let html_btn = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::HtmlPreview))
            .expect("expected [H: HTML] pill in HitMap");
        dispatch(
            Action::MouseClick {
                target: HitTarget::Button(ButtonId::HtmlPreview),
                col: html_btn.x,
                row: html_btn.y,
            },
            &mut app,
            &mut ui,
        );
        assert!(matches!(ui.overlays.last(), Some(Overlay::HtmlPreview(_))));

        // Redraw with HtmlPreview open
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        // 2. Clicking the HTML <button onclick> inside OverlayBody mutates DOM via QuickJS-NG
        let (btn_col, btn_row) = {
            let Some(Overlay::HtmlPreview(ref mut ov)) = ui.overlays.last_mut() else {
                panic!("expected HtmlPreview");
            };
            let theme = ui.theme.to_html_theme();
            let ctrl = ov
                .session
                .render(ov.content_rect.width, &theme, false)
                .controls[0]
                .clone();
            (
                ov.content_rect.x + ctrl.col_start + 1,
                ov.content_rect.y + ctrl.line as u16,
            )
        };
        let hit = ui.hitmap.hit(btn_row, btn_col).unwrap();
        assert_eq!(hit, HitTarget::OverlayBody);
        dispatch(
            Action::MouseClick {
                target: hit,
                col: btn_col,
                row: btn_row,
            },
            &mut app,
            &mut ui,
        );
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();
        let buf = terminal.backend().buffer();
        let screen: String = (0..34)
            .flat_map(|y| (0..120).map(move |x| buf[(x, y)].symbol().to_string()))
            .collect();
        assert!(screen.contains("Clicked!"), "expected QuickJS onclick result on screen");

        // 3. Pressing 'o' triggers native Axum serving via NativeHtmlServer::serve_html
        dispatch(Action::Input('o'), &mut app, &mut ui);
        let served_url = match tokio::time::timeout(std::time::Duration::from_secs(3), ui_rx.recv())
            .await
            .expect("timed out waiting for UiMsg::HtmlServed")
        {
            Some(UiMsg::HtmlServed { url }) => url,
            other => panic!("expected UiMsg::HtmlServed, got {other:?}"),
        };
        assert!(served_url.starts_with("http://127.0.0.1:"));

        // Verify live HTTP 200 text/html response from NativeHtmlServer
        let without_scheme = served_url.strip_prefix("http://").unwrap();
        let (host_port, path) = without_scheme.split_once('/').unwrap();
        let mut stream = tokio::net::TcpStream::connect(host_port).await.unwrap();
        use tokio::io::{AsyncReadExt, AsyncWriteExt};
        let req = format!("GET /{path} HTTP/1.1\r\nHost: {host_port}\r\nConnection: close\r\n\r\n");
        stream.write_all(req.as_bytes()).await.unwrap();
        let mut resp = String::new();
        stream.read_to_string(&mut resp).await.unwrap();
        assert!(resp.starts_with("HTTP/1.1 200 OK"));
        assert!(resp.to_ascii_lowercase().contains("content-type: text/html; charset=utf-8"));
        assert!(resp.contains("Spec HTML"));

        // 4. Clicking outside modal (OverlayBackdrop at (0,0)) closes HtmlPreview
        let backdrop_hit = ui.hitmap.hit(0, 0).unwrap();
        assert_eq!(backdrop_hit, HitTarget::OverlayBackdrop);
        dispatch(
            Action::MouseClick {
                target: backdrop_hit,
                col: 0,
                row: 0,
            },
            &mut app,
            &mut ui,
        );
        assert!(ui.overlays.is_empty());

        // 5. Key shortcut 'H' reopens HtmlPreview
        let key_h = crossterm::event::KeyEvent::new(
            crossterm::event::KeyCode::Char('H'),
            crossterm::event::KeyModifiers::SHIFT,
        );
        let act = crate::ui::event::key_to_action(key_h, Focus::Canvas, &ui.keymap, false);
        assert_eq!(act, Some(Action::OpenHtmlPreview));
        dispatch(act.unwrap(), &mut app, &mut ui);
        assert!(matches!(ui.overlays.last(), Some(Overlay::HtmlPreview(_))));

        app.html_server.shutdown();
    }

    #[test]
    fn test_html_resource_routing_in_attachments_and_image_preview_sniff() {
        let (mut app, mut ui, _nudge_rx) = make_ui_test_harness();
        let summary = app.pages[0].clone();
        app.page_details.insert(
            summary.page_key.clone(),
            crate::models::OnenotePageDetail {
                summary,
                raw_xhtml: "<html><body><p>Page</p></body></html>".to_string(),
                body_markdown: "Page".to_string(),
                body_plain: "Page".to_string(),
                blocks: Vec::new(),
                resources: vec![
                    crate::models::OnenoteResource {
                        element_id: Some("obj:1".to_string()),
                        resource_url: "https://example.com/report.html".to_string(),
                        fullres_url: None,
                        mime_type: "text/html".to_string(),
                        file_name: Some("report.html".to_string()),
                        is_image: false,
                        alt_text: None,
                    },
                    crate::models::OnenoteResource {
                        element_id: Some("obj:2".to_string()),
                        resource_url: "https://example.com/widget.htm".to_string(),
                        fullres_url: None,
                        mime_type: "application/octet-stream".to_string(),
                        file_name: Some("widget.htm".to_string()),
                        is_image: false,
                        alt_text: None,
                    },
                    crate::models::OnenoteResource {
                        element_id: Some("img:3".to_string()),
                        resource_url: "https://example.com/photo.png".to_string(),
                        fullres_url: None,
                        mime_type: "image/png".to_string(),
                        file_name: Some("photo.png".to_string()),
                        is_image: true,
                        alt_text: None,
                    },
                ],
                primary_div_id: None,
                synced_at: None,
            },
        );

        // 1. Attachments overlay detects .html and .htm resources and routes Enter/v to OpenHtmlResource (NOT ImagePreview)
        let mut tray = crate::ui::overlays::attachments::AttachmentsState::default();
        assert_eq!(
            tray.handle(&Action::Input('v'), &app),
            OverlayOutcome::CloseAndDispatch(Action::OpenHtmlResource(0))
        );
        assert_eq!(
            tray.handle(&Action::Activate, &app),
            OverlayOutcome::CloseAndDispatch(Action::OpenHtmlResource(0))
        );
        tray.select(1);
        assert_eq!(
            tray.handle(&Action::Input('v'), &app),
            OverlayOutcome::CloseAndDispatch(Action::OpenHtmlResource(1))
        );
        tray.select(2);
        assert_eq!(
            tray.handle(&Action::Input('v'), &app),
            OverlayOutcome::CloseAndDispatch(Action::OpenImagePreviewAt(2))
        );

        // 2. LoadPreviewImage byte sniffer detects HTML masquerading as image/png and routes to Overlay::HtmlPreview
        let fake_png_html = b"<!DOCTYPE html><html><body><h1>Login Required</h1></body></html>";
        let sniffed = crate::ui::app::sniff_preview_bytes_as_html(
            Some("image/png"),
            Some("photo.png"),
            fake_png_html,
        );
        assert!(sniffed.is_some());

        // Real PNG is NOT routed to HTML
        let real_png = b"\x89PNG\r\n\x1a\n\x00\x00\x00\rIHDR";
        assert!(
            crate::ui::app::sniff_preview_bytes_as_html(Some("image/png"), Some("photo.png"), real_png)
                .is_none()
        );

        // Simulate ImagePreview open -> stale key ignored, matching key swaps ImagePreview for HtmlPreview
        let detail = app.active_page_detail().unwrap();
        let img_ov = ImagePreviewOverlay::from_onenote_page(detail, Some(2)).unwrap();
        let expected_key = img_ov
            .current_cache_key(ui.term_width, ui.term_height, ui.graphics_caps)
            .unwrap();
        ui.overlays.push(Overlay::ImagePreview(img_ov));
        app.inflight_images.insert("hd:stale".to_string());
        app.handle_ui_msg(
            &mut ui,
            UiMsg::ImageRoutedToHtml {
                cache_key: "hd:stale".to_string(),
                title: "stale.html".to_string(),
                html: "<p>stale</p>".to_string(),
            },
        );
        assert!(!app.inflight_images.contains("hd:stale"));
        assert!(matches!(ui.overlays.last(), Some(Overlay::ImagePreview(_))));

        app.inflight_images.insert(expected_key.clone());
        app.handle_ui_msg(
            &mut ui,
            UiMsg::ImageRoutedToHtml {
                cache_key: expected_key.clone(),
                title: "photo.png".to_string(),
                html: sniffed.unwrap(),
            },
        );
        assert!(!app.inflight_images.contains(&expected_key));
        assert_eq!(ui.overlays.len(), 1);
        assert!(matches!(ui.overlays.last(), Some(Overlay::HtmlPreview(_))));
    }

    #[tokio::test]
    async fn test_markdown_resource_preview_routing_and_raw_toggle() {
        let (mut app, mut ui, _nudge_rx) = make_ui_test_harness();
        let summary = app.pages[0].clone();
        let page_key = summary.page_key.clone();
        app.page_details.insert(
            page_key.clone(),
            crate::models::OnenotePageDetail {
                summary,
                raw_xhtml: "<html><body><p>Page</p></body></html>".to_string(),
                body_markdown: "Page".to_string(),
                body_plain: "Page".to_string(),
                blocks: vec![crate::models::OnenoteBlock {
                    element_id: Some("obj:md0".to_string()),
                    tag: None,
                    raw_html: None,
                    kind: crate::models::OnenoteBlockKind::Attachment {
                        resource_index: 0,
                        name: "README.md".to_string(),
                        mime_type: "text/markdown".to_string(),
                        src: "https://example.com/README.md".to_string(),
                    },
                }],
                resources: vec![
                    crate::models::OnenoteResource {
                        element_id: Some("obj:md0".to_string()),
                        resource_url: "https://example.com/README.md".to_string(),
                        fullres_url: None,
                        mime_type: "text/markdown".to_string(),
                        file_name: Some("README.md".to_string()),
                        is_image: false,
                        alt_text: None,
                    },
                    crate::models::OnenoteResource {
                        element_id: Some("obj:md1".to_string()),
                        resource_url: "https://example.com/NOTES.md".to_string(),
                        fullres_url: None,
                        mime_type: "application/octet-stream".to_string(),
                        file_name: Some("NOTES.md".to_string()),
                        is_image: false,
                        alt_text: None,
                    },
                ],
                primary_div_id: None,
                synced_at: None,
            },
        );

        // 1. Both text/markdown and application/octet-stream (.md) show [MD] badge
        let rows = crate::ui::overlays::attachments::rows_from_onenote_page(
            app.active_page_detail().unwrap(),
        );
        assert_eq!(rows.len(), 2);
        assert!(rows[0].is_markdown);
        assert!(rows[0].label().starts_with("[MD] README.md"));
        assert!(rows[1].is_markdown);
        assert!(rows[1].label().starts_with("[MD] NOTES.md"));

        // 2. Enter / v / H in attachments tray dispatch OpenHtmlResource (NOT DownloadResource)
        let mut tray = crate::ui::overlays::attachments::AttachmentsState::default();
        assert_eq!(
            tray.handle(&Action::Activate, &app),
            OverlayOutcome::CloseAndDispatch(Action::OpenHtmlResource(0))
        );
        assert_eq!(
            tray.handle(&Action::Input('v'), &app),
            OverlayOutcome::CloseAndDispatch(Action::OpenHtmlResource(0))
        );
        assert_eq!(
            tray.handle(&Action::Input('H'), &app),
            OverlayOutcome::CloseAndDispatch(Action::OpenHtmlResource(0))
        );
        tray.select(1);
        assert_eq!(
            tray.handle(&Action::Activate, &app),
            OverlayOutcome::CloseAndDispatch(Action::OpenHtmlResource(1))
        );

        // 3. Clicking resource chip dispatches OpenHtmlResource (starts HTML/MD load task, not download)
        dispatch(
            Action::MouseClick {
                target: HitTarget::OnenoteResource(0),
                col: 4,
                row: 4,
            },
            &mut app,
            &mut ui,
        );
        assert!(
            ui.tasks
                .first_running()
                .is_some_and(|t| t.label.contains("Loading Markdown 'README.md'")),
            "expected resource chip click to trigger Markdown preview load"
        );

        // 4. Delivering UiMsg::HtmlResourceLoaded opens Overlay::HtmlPreview with SourceKind::Markdown
        let md_payload = "# Title\n\n**bold** | [link](https://example.com)".to_string();
        app.handle_ui_msg(
            &mut ui,
            UiMsg::HtmlResourceLoaded {
                task_id: 1,
                seq: 1,
                page_key,
                title: "README.md".to_string(),
                mime_type: Some("application/octet-stream".to_string()),
                html: md_payload,
            },
        );
        let html_theme = ui.theme.to_html_theme();
        {
            let Some(Overlay::HtmlPreview(ref mut preview)) = ui.overlays.last_mut() else {
                panic!("expected Overlay::HtmlPreview to open for .md resource");
            };
            assert_eq!(
                preview.session.source_kind(),
                bitefoundry_html::SourceKind::Markdown
            );
            assert!(!preview.session.js_active);

            // Rendered mode: 0 raw `**` and 0 raw HTML tags, contains rendered text
            let rendered_text: String = preview
                .session
                .render(80, &html_theme, false)
                .lines
                .iter()
                .map(|l| {
                    l.spans
                        .iter()
                        .map(|s| s.content.as_ref())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            assert!(rendered_text.contains("Title"), "{rendered_text}");
            assert!(rendered_text.contains("bold"), "{rendered_text}");
            assert!(!rendered_text.contains("**"), "raw ** leaked:\n{rendered_text}");
            assert!(!rendered_text.contains("<h1>"), "raw <h1> leaked:\n{rendered_text}");

            // serve_html() compiles Markdown into standalone HTML document for `o`
            let served = preview.serve_html();
            assert!(served.contains("<!DOCTYPE html>"), "{served}");
            assert!(served.contains("<strong>bold</strong>"), "{served}");
        }

        // Pressing `r` toggles Raw Markdown mode showing raw `# Title`
        dispatch(Action::Input('r'), &mut app, &mut ui);
        let Some(Overlay::HtmlPreview(ref mut preview)) = ui.overlays.last_mut() else {
            panic!("expected Overlay::HtmlPreview to stay open after `r`");
        };
        assert!(preview.session.raw_mode);
        let raw_text: String = preview
            .session
            .render(80, &html_theme, false)
            .lines
            .iter()
            .map(|l| {
                l.spans
                    .iter()
                    .map(|s| s.content.as_ref())
                    .collect::<String>()
            })
            .collect::<Vec<_>>()
            .join("\n");
        assert!(raw_text.contains("# Title"), "expected raw '# Title':\n{raw_text}");
        assert!(raw_text.contains("**bold**"), "expected raw '**bold**':\n{raw_text}");
    }

    #[test]
    fn test_stacked_confirm_backdrop_click_does_not_quit_or_lose_form_draft() {
        let (mut app, mut ui, _nudge_rx) = make_ui_test_harness();
        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();

        // Open CreatePage form and enter unsaved draft text
        dispatch(Action::OpenOnenoteCreatePage { level: 0 }, &mut app, &mut ui);
        if let Some(Overlay::OnenoteForm(ref mut form)) = ui.overlays.last_mut() {
            form.title = "Important Draft".to_string();
            form.body = "Do not lose this body".to_string();
        }
        // Stack Quit ConfirmDialog on top of OnenoteForm
        dispatch(Action::QuitConfirm, &mut app, &mut ui);
        assert_eq!(ui.overlays.len(), 2);

        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        // Right-click while modal is open must be blocked (no ContextMenu pushed)
        let rclick = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right),
            column: 5,
            row: 5,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        if let Some(act) = crate::ui::event::mouse_to_action(rclick, &ui.hitmap) {
            assert!(!dispatch(act, &mut app, &mut ui));
        }
        assert_eq!(ui.overlays.len(), 2);
        assert!(matches!(ui.overlays.last(), Some(Overlay::Confirm(_))));

        // Clicking outside ConfirmDialog hits OverlayBackdrop, closes ONLY ConfirmDialog, does NOT quit
        let hit = ui.hitmap.hit(0, 0).expect("backdrop must cover (0,0)");
        assert_eq!(hit, HitTarget::OverlayBackdrop);
        let quit = dispatch(
            Action::MouseClick {
                target: hit,
                col: 0,
                row: 0,
            },
            &mut app,
            &mut ui,
        );
        assert!(!quit, "backdrop click must never trigger Quit");
        assert_eq!(ui.overlays.len(), 1);
        match ui.overlays.last() {
            Some(Overlay::OnenoteForm(form)) => {
                assert_eq!(form.title, "Important Draft");
                assert_eq!(form.body, "Do not lose this body");
            }
            other => panic!("expected OnenoteForm preserved underneath, got {other:?}"),
        }

        // Redraw with only OnenoteForm open -> clicking outside closes form and stashes draft cleanly
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();
        let form_backdrop = ui.hitmap.hit(0, 0).unwrap();
        assert_eq!(form_backdrop, HitTarget::OverlayBackdrop);
        dispatch(
            Action::MouseClick {
                target: form_backdrop,
                col: 0,
                row: 0,
            },
            &mut app,
            &mut ui,
        );
        assert!(ui.overlays.is_empty());
        let stashed = app
            .drafts
            .get_note("create:me|sec1")
            .expect("backdrop close of OnenoteForm must stash draft");
        assert_eq!(stashed.title, "Important Draft");
        assert_eq!(stashed.body, "Do not lose this body");
    }

    #[test]
    fn test_onenote_form_field_clicks_focus_without_submit_or_newline_and_cancel_and_scroll() {
        let (mut app, mut ui, _nudge_rx) = make_ui_test_harness();
        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();

        dispatch(Action::OpenOnenoteCreatePage { level: 0 }, &mut app, &mut ui);
        if let Some(Overlay::OnenoteForm(ref mut form)) = ui.overlays.last_mut() {
            form.title = "My Note".to_string();
            form.body = (1..=30)
                .map(|i| format!("Line {i}"))
                .collect::<Vec<_>>()
                .join("\n");
            form.field = 2; // buttons
        }

        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        // Modal registers OverlayBody, FormSubmit, FormCancel
        assert!(ui.hitmap.region_of(HitTarget::OverlayBody).is_some());
        let submit_rect = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::FormSubmit))
            .expect("FormSubmit button must be registered");
        let cancel_rect = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::FormCancel))
            .expect("FormCancel button must be registered");
        assert!(cancel_rect.x > submit_rect.x);

        // Click title field (index 0): focuses field 0, does NOT jump focus or submit
        let title_rect = ui
            .hitmap
            .region_of(HitTarget::OverlayItem { index: 0 })
            .unwrap();
        dispatch(
            Action::MouseClick {
                target: HitTarget::OverlayItem { index: 0 },
                col: title_rect.x,
                row: title_rect.y,
            },
            &mut app,
            &mut ui,
        );
        let Some(Overlay::OnenoteForm(form)) = ui.overlays.last() else {
            panic!("form must stay open on title click");
        };
        assert_eq!(form.field, 0);

        // Click body field (index 1): focuses field 1, does NOT append '\n'
        let body_before = form.body.clone();
        let body_rect = ui
            .hitmap
            .region_of(HitTarget::OverlayItem { index: 1 })
            .unwrap();
        dispatch(
            Action::MouseClick {
                target: HitTarget::OverlayItem { index: 1 },
                col: body_rect.x,
                row: body_rect.y,
            },
            &mut app,
            &mut ui,
        );
        let Some(Overlay::OnenoteForm(form)) = ui.overlays.last() else {
            panic!("form must stay open on body click");
        };
        assert_eq!(form.field, 1);
        assert_eq!(form.body, body_before, "clicking body field must not insert newline");

        // Wheel scroll up moves body_scroll up
        let scroll_at_bottom = form.body_scroll;
        assert!(scroll_at_bottom > 0, "30-line body must have positive bottom scroll");
        dispatch(
            Action::MouseScroll {
                delta: -3,
                column: body_rect.x,
                row: body_rect.y,
            },
            &mut app,
            &mut ui,
        );
        let Some(Overlay::OnenoteForm(form)) = ui.overlays.last() else {
            panic!("form must stay open on wheel scroll");
        };
        assert!(form.body_scroll < scroll_at_bottom);
        assert!(!form.follow_body_bottom);

        // Click Cancel button -> closes form and stashes draft cleanly
        dispatch(
            Action::MouseClick {
                target: HitTarget::Button(ButtonId::FormCancel),
                col: cancel_rect.x,
                row: cancel_rect.y,
            },
            &mut app,
            &mut ui,
        );
        assert!(ui.overlays.is_empty());
        assert_eq!(
            app.drafts.get_note("create:me|sec1").map(|d| d.title.as_str()),
            Some("My Note")
        );
    }

    #[test]
    fn test_right_click_selects_clicked_page_before_opening_context_menu() {
        let (mut app, mut ui, _nudge_rx) = make_ui_test_harness();
        app.pages.push(OnenotePageSummary {
            page_key: "me|p2".to_string(),
            id: "p2".to_string(),
            scope: OnenoteScope::Me,
            section_id: "sec1".to_string(),
            section_name: Some("Notes".to_string()),
            notebook_id: Some("nb1".to_string()),
            notebook_name: Some("Eng".to_string()),
            title: "Page 2".to_string(),
            level: 0,
            order: 1,
            created_at: None,
            last_modified_at: None,
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: false,
        });
        assert_eq!(app.active_page_key.as_deref(), Some("me|p1"));

        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        let p2_rect = ui
            .hitmap
            .region_of(HitTarget::OnenotePageItem(1))
            .expect("second page row must be registered");
        let rclick = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right),
            column: p2_rect.x + 1,
            row: p2_rect.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let act = crate::ui::event::mouse_to_action(rclick, &ui.hitmap).unwrap();
        dispatch(act, &mut app, &mut ui);

        assert_eq!(
            app.active_page_key.as_deref(),
            Some("me|p2"),
            "right-click must select the clicked page before opening context menu"
        );
        assert_eq!(ui.focus, Focus::Pages);
        match ui.overlays.last() {
            Some(Overlay::ContextMenu(menu)) => {
                assert_eq!(menu.context, ContextTarget::PageItem);
            }
            other => panic!("expected PageItem ContextMenu, got {other:?}"),
        }

        // Redraw with ContextMenu open, then right-click on NavRow(0) -> replaces ContextMenu
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();
        let nav_rect = ui
            .hitmap
            .region_of(HitTarget::NavRow(0))
            .expect("nav row must remain underneath backdrop");
        let rclick_nav = crossterm::event::MouseEvent {
            kind: crossterm::event::MouseEventKind::Down(crossterm::event::MouseButton::Right),
            column: nav_rect.x + 1,
            row: nav_rect.y,
            modifiers: crossterm::event::KeyModifiers::NONE,
        };
        let act_nav = crate::ui::event::mouse_to_action(rclick_nav, &ui.hitmap).unwrap();
        dispatch(act_nav, &mut app, &mut ui);
        assert_eq!(ui.overlays.len(), 1);
        assert_eq!(ui.focus, Focus::Nav);
        match ui.overlays.last() {
            Some(Overlay::ContextMenu(menu)) => {
                assert_eq!(menu.context, ContextTarget::Global);
            }
            other => panic!("expected Global ContextMenu replacement, got {other:?}"),
        }
    }

    #[test]
    fn test_section_ribbon_hitmap_exact_alignment_with_multiple_tabs() {
        let (mut app, mut ui, _nudge_rx) = make_ui_test_harness();
        app.sections.clear();
        for (i, name) in ["Alpha", "Beta", "Gamma", "Delta"].iter().enumerate() {
            app.sections.push(OnenoteSection {
                section_key: format!("me|sec{i}"),
                id: format!("sec{i}"),
                scope: OnenoteScope::Me,
                notebook_id: Some("nb1".to_string()),
                notebook_name: Some("Eng".to_string()),
                parent_section_group_id: None,
                parent_section_group_name: None,
                display_name: (*name).to_string(),
                color: None,
                is_default: i == 0,
                pages_url: None,
                created_at: None,
                last_modified_at: None,
                web_url: None,
            });
        }
        app.active_section_key = Some("me|sec0".to_string());

        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        let tab0 = ui.hitmap.region_of(HitTarget::OnenoteSectionTab(0)).unwrap();
        let buf = terminal.backend().buffer();
        let row_text: String = (0..120)
            .map(|x| buf[(x, tab0.y)].symbol().to_string())
            .collect();

        for (i, name) in ["Alpha", "Beta", "Gamma", "Delta"].iter().enumerate() {
            let rect = ui
                .hitmap
                .region_of(HitTarget::OnenoteSectionTab(i))
                .expect("section tab must be registered");
            let rendered_slice: String = (rect.x..rect.right())
                .map(|x| buf[(x, rect.y)].symbol().to_string())
                .collect();
            assert_eq!(
                rendered_slice,
                format!("[ ▌{name} ]"),
                "tab {i} hitmap must match exact rendered cells (row: {row_text:?})"
            );
        }

        let add_rect = ui
            .hitmap
            .region_of(HitTarget::Button(ButtonId::OnenoteNewSection))
            .expect("[+Section S] must be registered");
        let add_slice: String = (add_rect.x..add_rect.right())
            .map(|x| buf[(x, add_rect.y)].symbol().to_string())
            .collect();
        assert_eq!(
            add_slice, "[+Section S]",
            "[+Section S] hitmap must align with zero drift"
        );
    }

    #[test]
    fn test_select_section_updates_active_notebook_key_and_survives_hierarchy_refresh() {
        let (mut app, mut ui, _nudge_rx) = make_ui_test_harness();
        let nb2 = OnenoteNotebook {
            notebook_key: "me|nb2".to_string(),
            id: "nb2".to_string(),
            scope: OnenoteScope::Me,
            display_name: "Personal".to_string(),
            color: None,
            is_default: false,
            is_shared: false,
            user_role: None,
            created_at: None,
            last_modified_at: None,
            created_by: None,
            web_url: None,
        };
        let sec2 = OnenoteSection {
            section_key: "me|sec2".to_string(),
            id: "sec2".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb2".to_string()),
            notebook_name: Some("Personal".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "Journal".to_string(),
            color: None,
            is_default: false,
            pages_url: None,
            created_at: None,
            last_modified_at: None,
            web_url: None,
        };
        app.notebooks.push(nb2);
        app.sections.push(sec2);
        app.storage.upsert_onenote_notebooks(&app.notebooks).unwrap();
        app.storage.upsert_onenote_sections(&app.sections).unwrap();

        dispatch(Action::SelectSection("me|sec2".to_string()), &mut app, &mut ui);
        assert_eq!(app.active_notebook_key.as_deref(), Some("me|nb2"));
        assert_eq!(app.active_section_key.as_deref(), Some("me|sec2"));

        // Subsequent hierarchy update must NOT clobber active_section_key back to nb1/sec1
        let nbs = app.notebooks.clone();
        let secs = app.sections.clone();
        app.handle_core_event(
            &mut ui,
            crate::models::CoreEvent::OnenoteHierarchyUpdated {
                notebooks: nbs,
                section_groups: Vec::new(),
                sections: secs,
            },
        );
        assert_eq!(app.active_notebook_key.as_deref(), Some("me|nb2"));
        assert_eq!(app.active_section_key.as_deref(), Some("me|sec2"));
    }

    #[test]
    fn test_search_overlay_clears_stale_hits_allows_refresh_and_palette_slash_prefill() {
        let (mut app, mut ui, mut nudge_rx) = make_ui_test_harness();
        app.storage.upsert_onenote_notebooks(&app.notebooks).unwrap();
        app.storage.upsert_onenote_sections(&app.sections).unwrap();
        app.storage.upsert_onenote_pages(&app.pages).unwrap();

        // Seed stale search result
        app.search_results.push(crate::models::OnenoteSearchHit {
            page_key: "me|p1".to_string(),
            page_id: "p1".to_string(),
            scope: OnenoteScope::Me,
            notebook_name: Some("Eng".to_string()),
            section_id: "sec1".to_string(),
            section_name: Some("Notes".to_string()),
            title: "Stale Hit".to_string(),
            last_modified_at: None,
            snippet: "stale".to_string(),
            web_url: None,
        });

        // Opening fresh SearchOverlay clears stale results and Enter does NOT jump
        dispatch(Action::OpenSearch, &mut app, &mut ui);
        assert!(app.search_results.is_empty());
        dispatch(Action::Activate, &mut app, &mut ui);
        assert!(
            matches!(ui.overlays.last(), Some(Overlay::Search(_))),
            "Enter on empty search query must not jump to stale hit"
        );

        // Ctrl+R / RefreshAll works inside SearchOverlay
        dispatch(Action::RefreshAll, &mut app, &mut ui);
        assert_eq!(nudge_rx.try_recv().ok(), Some(SyncCommand::RefreshAll));
        assert!(matches!(ui.overlays.last(), Some(Overlay::Search(_))));
        dispatch(Action::CloseOverlay, &mut app, &mut ui);

        // CommandPalette `/Page` opens SearchOverlay prefilled with `Page` and runs search
        dispatch(
            Action::OpenOverlay(OverlayKind::CommandPalette),
            &mut app,
            &mut ui,
        );
        for ch in "/Page".chars() {
            dispatch(Action::Input(ch), &mut app, &mut ui);
        }
        dispatch(Action::Activate, &mut app, &mut ui);
        match ui.overlays.last() {
            Some(Overlay::Search(s)) => assert_eq!(s.query, "Page"),
            other => panic!("expected prefilled SearchOverlay, got {other:?}"),
        }
        assert_eq!(app.search_results.len(), 1);
        assert_eq!(app.search_results[0].page_key, "me|p1");
    }

    #[test]
    fn test_pane_hover_scroll_focus_keys_html_close_pill_and_mutation_hygiene() {
        let (mut app, mut ui, _nudge_rx) = make_ui_test_harness();
        app.pages.push(OnenotePageSummary {
            page_key: "me|p2".to_string(),
            id: "p2".to_string(),
            scope: OnenoteScope::Me,
            section_id: "sec1".to_string(),
            section_name: Some("Notes".to_string()),
            notebook_id: Some("nb1".to_string()),
            notebook_name: Some("Eng".to_string()),
            title: "Page 2".to_string(),
            level: 0,
            order: 1,
            created_at: None,
            last_modified_at: None,
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: false,
        });

        let mut terminal = Terminal::new(TestBackend::new(120, 34)).unwrap();
        terminal
            .draw(|frame| {
                ui::draw(frame, &mut app, &mut ui);
            })
            .unwrap();

        // Hover-scroll over empty space in Pages pane selects next page instead of scrolling canvas
        let pages_pane = ui
            .hitmap
            .region_of(HitTarget::PanePages)
            .expect("PanePages container must be registered");
        let empty_y = pages_pane.bottom().saturating_sub(2);
        assert_eq!(ui.hitmap.hit(empty_y, pages_pane.x + 1), Some(HitTarget::PanePages));
        dispatch(
            Action::MouseScroll {
                delta: 3,
                column: pages_pane.x + 1,
                row: empty_y,
            },
            &mut app,
            &mut ui,
        );
        assert_eq!(app.active_page_key.as_deref(), Some("me|p2"));
        assert_eq!(ui.onenote_scroll, 0);

        // ScrollTop / ScrollBottom respect Focus::Pages
        ui.focus = Focus::Pages;
        dispatch(Action::ScrollTop, &mut app, &mut ui);
        assert_eq!(app.active_page_key.as_deref(), Some("me|p1"));
        dispatch(Action::ScrollBottom, &mut app, &mut ui);
        assert_eq!(app.active_page_key.as_deref(), Some("me|p2"));

        // HtmlPreview `[✕ Esc]` closes immediately even while editing `<input>`
        let mut html_ov = HtmlPreviewOverlay::new(
            Some("Form".into()),
            r#"<input type="text" name="q" value="abc" />"#.into(),
        );
        html_ov.handle(&Action::Tab, &ui);
        html_ov.handle(&Action::Activate, &ui);
        assert!(html_ov.accepts_text(), "should be editing input");
        ui.overlays.push(Overlay::HtmlPreview(html_ov));
        dispatch(
            Action::MouseClick {
                target: HitTarget::Button(ButtonId::ConfirmCancel),
                col: 1,
                row: 1,
            },
            &mut app,
            &mut ui,
        );
        assert!(
            ui.overlays.is_empty(),
            "[✕ Esc] must close HtmlPreview immediately even while editing <input>"
        );

        // PageMutated for ToggleTodo (cleared_draft_key: None) must NOT clear append draft
        app.drafts.set_note(
            "append:me|p2",
            crate::config::NoteDraftEntry {
                title: String::new(),
                body: "Keep my append draft".to_string(),
                base_content: None,
            },
        );
        let detail_p2 = crate::parser::parse_onenote_page_xhtml(
            app.pages[1].clone(),
            "<html><body><p>Body</p></body></html>",
        );
        app.handle_ui_msg(
            &mut ui,
            UiMsg::PageMutated {
                task_id: 1,
                detail: Box::new(detail_p2),
                message: "Marked to-do completed".to_string(),
                cleared_draft_key: None,
            },
        );
        assert!(
            app.drafts.get_note("append:me|p2").is_some(),
            "ToggleTodo PageMutated must preserve append draft"
        );

        // PageDeleted resets scroll/selection, clears drafts, and evicts image_cache
        ui.onenote_scroll = 15;
        ui.onenote_selected_block = 4;
        let png = composer::encode_rgba_to_png(2, 2, vec![255; 16]).unwrap();
        let rendered =
            crate::img_preview::decode_and_render_halfblocks(&png, 4, 2, (0, 0, 0)).unwrap();
        app.image_cache.insert("thumb:me|p2:0:32:12".to_string(), rendered);
        app.handle_ui_msg(
            &mut ui,
            UiMsg::PageDeleted {
                task_id: 2,
                page_key: "me|p2".to_string(),
            },
        );
        assert_eq!(ui.onenote_scroll, 0);
        assert_eq!(ui.onenote_selected_block, 0);
        assert!(!app.image_cache.contains_key("thumb:me|p2:0:32:12"));
        assert!(app.drafts.get_note("append:me|p2").is_none());
    }
}
