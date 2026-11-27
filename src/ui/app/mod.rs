//! Core TUI application state (`AppState`), presentation state (`UiState`), and async `UiMsg` handlers.

mod cache;
mod events;
mod inline_edit;
mod messages;
mod navigation;
mod requests_media;
mod requests_mutations;
mod state;
#[cfg(test)]
mod tests;

pub use state::{
    is_html_resource_meta, is_markdown_resource_meta, is_markup_resource_meta,
    sniff_preview_bytes_as_html, AppState, Focus, NarrowPane, PendingPixelGraphic, RunningTask,
    TaskTracker, UiMsg, UiState, MAX_RAW_IMAGE_CACHE_BYTES, MAX_RAW_IMAGE_CACHE_ENTRIES,
};
