//! Direct in-place Note Canvas inline editor open & conflict-guarded flush logic.

use crate::{config::NoteDraftEntry, models::OutgoingOnenotePage};

use super::state::{AppState, Focus, NarrowPane, UiMsg, UiState};

impl AppState {
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

}
