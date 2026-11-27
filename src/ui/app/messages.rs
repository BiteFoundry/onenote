//! Async `UiMsg` router and domain handlers for background task completions.

use crate::{
    config::NoteDraftEntry,
    models::{OnenotePageDetail, OnenotePageSummary, UpdateCheckResult},
    parser,
    ui::overlays::Overlay,
};

use super::state::{AppState, UiMsg, UiState};

impl AppState {
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
                self.sync_ctrl.request(crate::sync::SyncCommand::Bootstrap);
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
                self.handle_notebook_created(ui, task_id, notebook);
            }
            UiMsg::SectionCreated { task_id, section } => {
                self.handle_section_created(ui, task_id, section);
            }
            UiMsg::PageCreated {
                task_id,
                section_key,
                page,
                preview_detail,
            } => {
                self.handle_page_created(ui, task_id, section_key, page, *preview_detail);
            }
            UiMsg::PageMutated {
                task_id,
                detail,
                message,
                cleared_draft_key,
            } => {
                self.handle_page_mutated(ui, task_id, *detail, message, cleared_draft_key);
            }
            UiMsg::EditConflict {
                task_id,
                detail,
                draft_to_preserve,
                message,
            } => {
                self.handle_edit_conflict(ui, task_id, *detail, draft_to_preserve, message);
            }
            UiMsg::PageDeleted { task_id, page_key } => {
                self.handle_page_deleted(ui, task_id, page_key);
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
                self.handle_image_load_failed(ui, cache_key, error);
            }
            UiMsg::ImageRoutedToHtml {
                cache_key,
                title,
                html,
            } => {
                self.handle_image_routed_to_html(ui, cache_key, title, html);
            }
            UiMsg::HtmlResourceLoaded {
                task_id,
                seq,
                page_key,
                title,
                mime_type,
                html,
            } => {
                self.handle_html_resource_loaded(ui, task_id, seq, page_key, title, mime_type, html);
            }
            UiMsg::HtmlServed { url } => match open::that(&url) {
                Ok(()) => ui.success(format!("Opened local HTML preview: {url}")),
                Err(err) => ui.error(format!("Served at {url}, but browser open failed: {err}")),
            },
            UiMsg::UpdateResult { task_id, result } => {
                self.handle_update_result(ui, task_id, result);
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
                self.handle_mutation_failed(ui, task_id, page_key, error, rollback_detail);
            }
            UiMsg::InlineSaved {
                task_id,
                detail,
                cleared_draft_key,
            } => {
                self.handle_inline_saved(ui, task_id, *detail, cleared_draft_key);
            }
            UiMsg::InlineConflict {
                task_id,
                detail,
                draft_to_preserve,
                message,
            } => {
                self.handle_inline_conflict(ui, task_id, *detail, draft_to_preserve, message);
            }
            UiMsg::Info(msg) => ui.info(msg),
            UiMsg::Error(msg) => ui.error(msg),
        }
    }

    fn handle_notebook_created(
        &mut self,
        ui: &mut UiState,
        task_id: u64,
        notebook: crate::models::OnenoteNotebook,
    ) {
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

    fn handle_section_created(
        &mut self,
        ui: &mut UiState,
        task_id: u64,
        section: crate::models::OnenoteSection,
    ) {
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

    fn handle_page_created(
        &mut self,
        ui: &mut UiState,
        task_id: u64,
        section_key: String,
        page: OnenotePageSummary,
        preview_detail: OnenotePageDetail,
    ) {
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
            self.page_details.insert(pk.clone(), preview_detail);
            self.select_page(pk, ui);
        }
        self.refresh_active_search(ui);
        ui.success(format!("Created page '{title}'"));
    }

    fn handle_page_mutated(
        &mut self,
        ui: &mut UiState,
        task_id: u64,
        detail: OnenotePageDetail,
        message: String,
        cleared_draft_key: Option<String>,
    ) {
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
        self.cache_page_detail(detail);
        self.refresh_active_search(ui);
        ui.success(message);
    }

    fn handle_edit_conflict(
        &mut self,
        ui: &mut UiState,
        task_id: u64,
        detail: OnenotePageDetail,
        draft_to_preserve: Option<(String, NoteDraftEntry)>,
        message: String,
    ) {
        ui.tasks.finish(task_id);
        if let Some((draft_key, entry)) = draft_to_preserve {
            self.save_draft_entry(draft_key, entry);
        }
        self.evict_image_cache_for_page(&detail.summary.page_key);
        self.sync_detail_summary_into_pages(&detail.summary);
        self.cache_page_detail(detail);
        ui.warn(message);
    }

    fn handle_page_deleted(&mut self, ui: &mut UiState, task_id: u64, page_key: String) {
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

    fn handle_image_load_failed(&mut self, ui: &mut UiState, cache_key: String, error: String) {
        self.inflight_images.remove(&cache_key);
        if let Some(Overlay::ImagePreview(ref mut preview)) = ui.overlays.last_mut() {
            let expected =
                preview.current_cache_key(ui.term_width, ui.term_height, ui.graphics_caps);
            if expected.as_deref() == Some(cache_key.as_str()) {
                preview.error = Some(error);
            }
        }
    }

    fn handle_image_routed_to_html(
        &mut self,
        ui: &mut UiState,
        cache_key: String,
        title: String,
        html: String,
    ) {
        self.inflight_images.remove(&cache_key);
        let matches_active_preview = matches!(
            ui.overlays.last(),
            Some(Overlay::ImagePreview(preview))
                if preview
                    .current_cache_key(ui.term_width, ui.term_height, ui.graphics_caps)
                    .as_deref()
                    == Some(cache_key.as_str())
        );
        if matches_active_preview {
            ui.overlays.pop();
            ui.overlays.push(Overlay::HtmlPreview(Box::new(
                crate::ui::overlays::html_preview::HtmlPreviewOverlay::new(Some(title), html),
            )));
            ui.info("Resource returned HTML — opened in interactive HTML viewer");
        }
    }

    #[allow(clippy::too_many_arguments)]
    fn handle_html_resource_loaded(
        &mut self,
        ui: &mut UiState,
        task_id: u64,
        seq: u64,
        page_key: String,
        title: String,
        mime_type: Option<String>,
        html: String,
    ) {
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
                crate::ui::overlays::html_preview::HtmlPreviewOverlay::new(Some(title), html)
            };
            ui.overlays.push(Overlay::HtmlPreview(Box::new(overlay)));
        }
    }

    fn handle_update_result(&mut self, ui: &mut UiState, task_id: u64, result: UpdateCheckResult) {
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
            ui.info(format!("onenote {} is up to date", result.current_version));
        }
    }

    fn handle_mutation_failed(
        &mut self,
        ui: &mut UiState,
        task_id: u64,
        page_key: String,
        error: String,
        rollback_detail: Option<Box<OnenotePageDetail>>,
    ) {
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

    fn handle_inline_saved(
        &mut self,
        ui: &mut UiState,
        task_id: u64,
        detail: OnenotePageDetail,
        cleared_draft_key: Option<String>,
    ) {
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
                    crate::ui::inline_editor::InlineEditTarget::PageBody { .. } => Some(new_md),
                    crate::ui::inline_editor::InlineEditTarget::Block { element_id, .. } => {
                        parser::find_block_by_element_id(&detail, element_id)
                            .map(parser::block_baseline_content)
                    }
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
        self.cache_page_detail(detail);
        self.refresh_active_search(ui);
    }

    fn handle_inline_conflict(
        &mut self,
        ui: &mut UiState,
        task_id: u64,
        detail: OnenotePageDetail,
        draft_to_preserve: Option<(String, NoteDraftEntry)>,
        message: String,
    ) {
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
        self.cache_page_detail(detail);
        ui.warn(message);
    }
}
