//! Argv pre-processing, hyphen-text guardrails, and fast help/version exit.

use std::io::{self, Write};

use anyhow::Result;
use clap::CommandFactory;

use super::args::Cli;

impl Cli {
    pub fn parse() -> Self {
        Self::try_parse_from(std::env::args_os()).unwrap_or_else(|e| e.exit())
    }

    pub fn try_parse_from<I, T>(itr: I) -> std::result::Result<Self, clap::Error>
    where
        I: IntoIterator<Item = T>,
        T: Into<std::ffi::OsString> + Clone,
    {
        let normalized = normalize_hyphen_text_args(itr);
        <Self as clap::Parser>::try_parse_from(normalized)
    }
}

fn find_subcommand<'a>(cmd: &'a clap::Command, name: &str) -> Option<&'a clap::Command> {
    cmd.get_subcommands()
        .find(|sub| sub.get_name() == name || sub.get_all_aliases().any(|alias| alias == name))
}

fn find_long_arg<'a>(subcmd: &'a clap::Command, long_name: &str) -> Option<&'a clap::Arg> {
    subcmd.get_arguments().find(|arg| {
        arg.get_long() == Some(long_name)
            || arg
                .get_all_aliases()
                .is_some_and(|aliases| aliases.contains(&long_name))
    })
}

fn is_known_short_token(subcmd: &clap::Command, short_rest: &str) -> bool {
    !short_rest.is_empty()
        && short_rest.chars().all(|ch| {
            matches!(ch, 'h' | 'V')
                || subcmd.get_arguments().any(|arg| {
                    arg.get_short() == Some(ch)
                        || arg
                            .get_all_short_aliases()
                            .is_some_and(|aliases| aliases.contains(&ch))
                })
        })
}

fn is_known_flag_token(subcmd: &clap::Command, raw: &str) -> bool {
    if let Some(long_body) = raw.strip_prefix("--") {
        let name = long_body.split_once('=').map_or(long_body, |(name, _)| name);
        return matches!(name, "help" | "version") || find_long_arg(subcmd, name).is_some();
    }
    if let Some(short_rest) = raw.strip_prefix('-') {
        return is_known_short_token(subcmd, short_rest);
    }
    false
}

fn looks_like_long_flag(long_body: &str) -> bool {
    if long_body.contains(['\n', '\r']) {
        return false;
    }
    let name = long_body.split_once('=').map_or(long_body, |(name, _)| name);
    let mut chars = name.chars();
    matches!(chars.next(), Some(c) if c.is_ascii_alphabetic())
        && chars.all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

fn is_hyphen_text_literal(subcmd: &clap::Command, raw: &str) -> bool {
    let Some(rest) = raw.strip_prefix('-') else {
        return false;
    };
    if rest.is_empty() || raw == "--" || is_known_flag_token(subcmd, raw) {
        return false;
    }
    if let Some(long_body) = raw.strip_prefix("--") {
        return !looks_like_long_flag(long_body);
    }
    true
}

fn subcommand_has_free_text(subcmd_name: &str) -> bool {
    matches!(
        subcmd_name,
        "create"
            | "append"
            | "update"
            | "check"
            | "rename-section"
            | "create-section"
            | "desktop-rename-section"
            | "create-notebook"
            | "rename-notebook"
            | "search"
    )
}

fn is_free_text_option(subcmd_name: &str, arg_id: &str) -> bool {
    match subcmd_name {
        "create" => arg_id == "title",
        "update" => matches!(
            arg_id,
            "title" | "body" | "replace_text" | "with_text" | "expected_content"
        ),
        "check" => arg_id == "expected_content",
        "rename-section" | "rename-notebook" | "desktop-rename-section" => arg_id == "name",
        _ => false,
    }
}

fn is_free_text_positional(subcmd_name: &str, pos_index: usize) -> bool {
    matches!(
        (subcmd_name, pos_index),
        ("create", 1)
            | ("append", 1)
            | ("create-section", 1)
            | ("create-notebook", 0)
            | ("search", 0)
    )
}

fn may_be_hyphen_text_literal(raw: &str) -> bool {
    let Some(rest) = raw.strip_prefix('-') else {
        return false;
    };
    if rest.is_empty() || raw == "--" || matches!(raw, "-h" | "-V") {
        return false;
    }
    if let Some(long_body) = raw.strip_prefix("--") {
        return !looks_like_long_flag(long_body);
    }
    true
}

pub(super) fn argv_needs_hyphen_normalization(raw: &[std::ffi::OsString]) -> bool {
    if raw.len() <= 2 {
        return false;
    }
    let Some(sub_arg) = raw[1].to_str() else {
        return false;
    };
    if !subcommand_has_free_text(sub_arg) {
        return false;
    }
    for tok in &raw[2..] {
        let Some(tok_str) = tok.to_str() else {
            continue;
        };
        if tok_str == "--" {
            break;
        }
        if may_be_hyphen_text_literal(tok_str) {
            return true;
        }
    }
    false
}

pub(super) fn normalize_hyphen_text_args<I, T>(itr: I) -> Vec<std::ffi::OsString>
where
    I: IntoIterator<Item = T>,
    T: Into<std::ffi::OsString>,
{
    let raw: Vec<std::ffi::OsString> = itr.into_iter().map(Into::into).collect();
    if !argv_needs_hyphen_normalization(&raw) {
        return raw;
    }
    let Some(sub_arg) = raw[1].to_str() else {
        return raw;
    };
    let mut root_cmd = <Cli as clap::CommandFactory>::command();
    root_cmd.build();
    let Some(subcmd) = find_subcommand(&root_cmd, sub_arg) else {
        return raw;
    };
    let sub_name = subcmd.get_name();

    let mut before_dash = Vec::with_capacity(raw.len() + 1);
    before_dash.push(raw[0].clone());
    before_dash.push(raw[1].clone());
    let mut deferred_pos: Vec<std::ffi::OsString> = Vec::new();
    let mut pos_index = 0usize;
    let mut i = 2usize;

    while i < raw.len() {
        let tok = &raw[i];
        let Some(tok_str) = tok.to_str() else {
            if deferred_pos.is_empty() {
                before_dash.push(tok.clone());
            } else {
                deferred_pos.push(tok.clone());
            }
            pos_index += 1;
            i += 1;
            continue;
        };

        if tok_str == "--" {
            if deferred_pos.is_empty() {
                before_dash.extend_from_slice(&raw[i..]);
            } else {
                deferred_pos.extend_from_slice(&raw[i + 1..]);
            }
            break;
        }

        if let Some(long_body) = tok_str.strip_prefix("--") {
            if let Some((long_name, _)) = long_body.split_once('=') {
                if find_long_arg(subcmd, long_name).is_some() || looks_like_long_flag(long_body) {
                    before_dash.push(tok.clone());
                    i += 1;
                    continue;
                }
            } else if let Some(arg) = find_long_arg(subcmd, long_body) {
                if arg.get_action().takes_values() {
                    if let Some(next) = raw.get(i + 1) {
                        match next.to_str() {
                            Some(next_str)
                                if is_free_text_option(sub_name, arg.get_id().as_str())
                                    && is_hyphen_text_literal(subcmd, next_str) =>
                            {
                                let mut combined = std::ffi::OsString::from(tok_str);
                                combined.push("=");
                                combined.push(next);
                                before_dash.push(combined);
                                i += 2;
                                continue;
                            }
                            Some(next_str) if next_str != "-" && next_str.starts_with('-') => {
                                before_dash.push(tok.clone());
                                i += 1;
                                continue;
                            }
                            _ => {
                                before_dash.push(tok.clone());
                                before_dash.push(next.clone());
                                i += 2;
                                continue;
                            }
                        }
                    }
                }
                before_dash.push(tok.clone());
                i += 1;
                continue;
            }
            if is_free_text_positional(sub_name, pos_index)
                && is_hyphen_text_literal(subcmd, tok_str)
            {
                deferred_pos.push(tok.clone());
                pos_index += 1;
            } else {
                before_dash.push(tok.clone());
            }
            i += 1;
            continue;
        }

        if tok_str.len() > 1 && tok_str.starts_with('-') {
            if is_known_flag_token(subcmd, tok_str) {
                before_dash.push(tok.clone());
                i += 1;
                continue;
            }
            if is_free_text_positional(sub_name, pos_index)
                && is_hyphen_text_literal(subcmd, tok_str)
            {
                deferred_pos.push(tok.clone());
                pos_index += 1;
            } else {
                before_dash.push(tok.clone());
            }
            i += 1;
            continue;
        }

        if deferred_pos.is_empty() {
            before_dash.push(tok.clone());
        } else {
            deferred_pos.push(tok.clone());
        }
        pos_index += 1;
        i += 1;
    }

    if !deferred_pos.is_empty() {
        before_dash.push(std::ffi::OsString::from("--"));
        before_dash.extend(deferred_pos);
    }
    before_dash
}

/// Fast-path for `--version` / `-V` / `--help` / `-h` before Tokio runtime spin-up (<1ms).
pub fn try_fast_help_or_version() -> Option<Result<()>> {
    let mut args = std::env::args_os();
    let _bin = args.next();
    let first = args.next()?;
    if args.next().is_some() {
        return None;
    }
    match first.to_str()? {
        "-V" | "--version" => {
            let mut out = io::stdout().lock();
            let res = writeln!(out, "onenote {}", env!("CARGO_PKG_VERSION"));
            Some(res.map_err(Into::into))
        }
        "-h" | "--help" => Some(Cli::command().print_long_help().map_err(Into::into)),
        _ => None,
    }
}
