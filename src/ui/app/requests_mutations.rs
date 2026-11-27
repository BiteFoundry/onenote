//! Background OneNote mutation tasks (create/append/edit/todo/delete/download/update).

use crate::{
    config::NoteDraftEntry,
    models::{InlineImageUpload, OnenoteScope, OutgoingOnenotePage},
    parser,
};

use super::state::{AppState, UiMsg, UiState};

impl AppState {
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

}
