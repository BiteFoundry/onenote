//! Core sync event (`CoreEvent`) reducer for `AppState`.

use crate::{
    models::{CoreEvent, POLL_RETRY_SUMMARY},
    parser,
    ui::{notification, overlays::Overlay},
};

use super::state::{AppState, UiState};

impl AppState {
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
}
