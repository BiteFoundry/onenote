//! Notebook, section, and page selection, cache hydration, search refresh, and sync focus.

use crate::{
    models::{OnenoteNotebook, OnenotePageDetail, OnenotePageSummary, OnenoteSection},
    sync::FocusTarget,
    ui::overlays::Overlay,
};

use super::state::{AppState, UiState};

impl AppState {
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
        self.sections_for_notebook(nb)
    }

    /// Sections shown for `nb`; a single notebook whose sections carry no
    /// notebook ID falls back to every section (same rule as the ribbon).
    pub fn sections_for_notebook(&self, nb: &OnenoteNotebook) -> Vec<&OnenoteSection> {
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

    pub(super) fn set_active_page(&mut self, page_key: Option<String>) {
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
                    self.bump_canvas_gen();
                }
            }
        }
    }

    pub(super) fn cache_page_detail(&mut self, detail: OnenotePageDetail) {
        let key = &detail.summary.page_key;
        if Some(key) == self.active_page_key.as_ref() {
            self.loading_onenote_detail = false;
        } else if Some(key) != self.previous_page_key.as_ref() {
            // Sync and mutation handlers have already written the detail to SQLite.
            return;
        }
        self.page_details.insert(key.clone(), detail);
        self.bump_canvas_gen();
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


    pub fn build_context_menu(
        &self,
        target: crate::ui::overlays::context_menu::ContextTarget,
        anchor: (u16, u16),
    ) -> crate::ui::overlays::context_menu::ContextMenu {
        let has_resources = self
            .active_page_detail()
            .is_some_and(|d| !d.resources.is_empty());
        crate::ui::overlays::context_menu::ContextMenu::build(target, anchor, has_resources)
    }
}
