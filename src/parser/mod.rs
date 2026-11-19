//! Bidirectional parser between Microsoft OneNote XHTML, Rich Terminal AST, GFM Markdown,
//! and surgical Graph `PATCH` payloads.

pub mod onenote;

pub use onenote::{
    build_append_markdown_payload, build_replace_block_patch, build_replace_page_body_commands,
    build_set_tag_patch, build_toggle_todo_patch, compile_new_page_payload,
    markdown_to_onenote_html_fragment, onenote_blocks_to_markdown, onenote_blocks_to_plain,
    parse_onenote_page_xhtml,
};
