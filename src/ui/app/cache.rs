//! Image caches, draft persistence, config saving, and cached page-detail reconciliation.

use std::{collections::HashSet, sync::Arc};

use crate::{
    config::NoteDraftEntry,
    models::OnenotePageSummary,
};

use super::state::{
    apply_ui_settings, AppState, UiState, MAX_RAW_IMAGE_CACHE_BYTES, MAX_RAW_IMAGE_CACHE_ENTRIES,
};

impl AppState {
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

    pub(super) fn touch_raw_image_cache(&mut self, resource_url: &str) -> Option<Arc<[u8]>> {
        let bytes = self.image_raw_cache.get(resource_url).cloned()?;
        self.image_raw_order.retain(|k| k != resource_url);
        self.image_raw_order.push_back(resource_url.to_string());
        Some(bytes)
    }

    pub(super) fn evict_raw_image_urls(&mut self, urls: &HashSet<String>) {
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
        self.bump_canvas_gen();
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


    pub(super) fn evict_stale_page_details(&mut self, incoming: &[OnenotePageSummary]) {
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
                self.bump_canvas_gen();
            }
        }
    }

    pub(super) fn evict_removed_section_page_details(
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
                    self.bump_canvas_gen();
                }
                Ok(None) => {
                    self.evict_image_cache_for_page(&pk);
                    self.page_details.remove(&pk);
                    self.bump_canvas_gen();
                }
                Err(_) => {}
            }
        }
    }

    pub(super) fn reconnect_active_page_section(&mut self, moved_section_key: String) {
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

    pub(super) fn reconcile_cached_page_details_from_storage(&mut self) {
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

    pub(super) fn sync_detail_summary_into_pages(&mut self, summary: &OnenotePageSummary) {
        if let Some(existing) = self
            .pages
            .iter_mut()
            .find(|p| p.page_key == summary.page_key)
        {
            existing.title = summary.title.clone();
            existing.level = summary.level;
            existing.order = summary.order;
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
}
