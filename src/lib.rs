pub mod auth;
pub mod cli;
pub mod config;
pub mod daemon;
#[cfg(windows)]
mod desktop;
pub mod graph;
mod http;
pub mod img_preview;
pub mod mcp;
pub mod models;
pub mod parser;
pub mod resolver;
pub mod serve;
pub mod storage;
pub mod sync;
pub mod tui;
pub mod ui;
pub mod updater;
