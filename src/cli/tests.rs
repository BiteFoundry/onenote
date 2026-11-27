//! Unit tests for CLI parsing, normalization, formatting, and domain helpers.

use crate::models::{
    OnenoteBlock, OnenotePageDetail, OnenotePageSummary, OnenoteResource, OnenoteScope,
};

use super::format::*;
use super::parse::*;
use super::runtime::*;
use super::*;

#[test]
fn parses_default_tui_and_core_onenote_commands() {
    let cli = Cli::try_parse_from(["onenote"]).unwrap();
    assert!(cli.command.is_none());

    let cli = Cli::try_parse_from(["onenote", "notebooks", "--include-teams", "--json"])
        .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Notebooks {
            include_teams: true,
            json: true,
            ..
        }
    ));

    let cli = Cli::try_parse_from([
        "onenote",
        "create",
        "Work/Specs",
        "--title",
        "RFC-01",
        "# Heading",
        "--level",
        "1",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Create {
            level: Some(1),
            json: true,
            ..
        }
    ));

    let cli = Cli::try_parse_from(["onenote", "resource", "me|p1", "--index", "0", "--json"])
        .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Resource {
            index: Some(0),
            json: true,
            ..
        }
    ));

    let cli = Cli::try_parse_from(["onenote", "serve"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Serve { port: 8793, .. }
    ));

    let cli = Cli::try_parse_from(["onenote", "serve", "--port", "8788"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Serve { port: 8788, .. }
    ));

    let cli = Cli::try_parse_from([
        "onenote",
        "serve",
        "--allow-keyless",
        "--allowed-host",
        "notes.corp.example",
        "--allowed-host",
        "10.0.0.5",
        "--cors-origin",
        "https://app.example.com",
    ])
    .unwrap();
    match cli.command.unwrap() {
        Command::Serve {
            api_key,
            allow_keyless,
            allowed_hosts,
            cors_origins,
            ..
        } => {
            assert_eq!(api_key, None);
            assert!(allow_keyless);
            assert_eq!(allowed_hosts, vec!["notes.corp.example", "10.0.0.5"]);
            assert_eq!(cors_origins, vec!["https://app.example.com"]);
        }
        other => panic!("unexpected command: {other:?}"),
    }
    assert!(
        Cli::try_parse_from(["onenote", "serve", "--token", "k", "--allow-keyless"]).is_err()
    );

    let cli = Cli::try_parse_from(["onenote", "update", "--check", "--json"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Update {
            page: None,
            check: true,
            json: true,
            ..
        }
    ));

    let cli = Cli::try_parse_from([
        "onenote",
        "update",
        "me|p1",
        "--block-id",
        "p:123",
        "--body",
        "Updated",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Update {
            page: Some(_),
            block_id: Some(_),
            ..
        }
    ));
}

#[test]
fn parses_hyphen_values_across_free_text_commands() {
    const ITEM: &str = "- 목록 항목";
    const TWO_LINE_LIST: &str = "- first\n- second";
    const KO_TWO_LINE: &str = "- 첫째 항목\n- 둘째 항목";

    // 1. create positional body + --title (Korean, two-line list, trailing --json)
    let cli = Cli::try_parse_from(["onenote", "create", "sec", "--title", "Test", ITEM])
        .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Create {
            ref section,
            ref title,
            markdown_body: Some(ref body),
            json: false,
            ..
        } if section == "sec" && title == "Test" && body == ITEM
    ));

    let cli = Cli::try_parse_from([
        "onenote",
        "create",
        "sec",
        "--title",
        ITEM,
        TWO_LINE_LIST,
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Create {
            ref section,
            ref title,
            markdown_body: Some(ref body),
            json: true,
            ..
        } if section == "sec" && title == ITEM && body == TWO_LINE_LIST
    ));

    // 2. append positional body (Korean, two-line list, trailing --json, positional --)
    let cli = Cli::try_parse_from(["onenote", "append", "page", ITEM, "--json"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Append {
            ref page,
            markdown_body: Some(ref body),
            json: true,
            ..
        } if page == "page" && body == ITEM
    ));

    let cli = Cli::try_parse_from(["onenote", "append", "page", KO_TWO_LINE]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Append {
            markdown_body: Some(ref body),
            ..
        } if body == KO_TWO_LINE
    ));

    let cli = Cli::try_parse_from(["onenote", "append", "page", "---"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Append {
            markdown_body: Some(ref body),
            ..
        } if body == "---"
    ));

    let cli = Cli::try_parse_from(["onenote", "append", "page", "--", "--dry-run"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Append {
            markdown_body: Some(ref body),
            ..
        } if body == "--dry-run"
    ));

    let cli = Cli::try_parse_from(["onenote", "append", "page", "--json"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Append {
            markdown_body: None,
            json: true,
            ..
        }
    ));

    // 3. update --body, --expected-content (--base-content), --replace-text, --with-text, --title
    let cli = Cli::try_parse_from([
        "onenote",
        "update",
        "page",
        "--title",
        ITEM,
        "--body",
        TWO_LINE_LIST,
        "--expected-content",
        ITEM,
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Update {
            page: Some(ref page),
            title: Some(ref title),
            body: Some(ref body),
            expected_content: Some(ref exp),
            json: true,
            ..
        } if page == "page" && title == ITEM && body == TWO_LINE_LIST && exp == ITEM
    ));

    let cli = Cli::try_parse_from([
        "onenote",
        "update",
        "page",
        "--block-id",
        "block",
        "--replace-text",
        "old",
        "--with-text",
        ITEM,
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Update {
            replace_text: Some(ref from),
            with_text: Some(ref to),
            json: true,
            ..
        } if from == "old" && to == ITEM
    ));

    let cli = Cli::try_parse_from([
        "onenote",
        "update",
        "page",
        "--block-id",
        "block",
        "--replace-text",
        ITEM,
        "--with-text",
        "new",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Update {
            replace_text: Some(ref from),
            with_text: Some(ref to),
            ..
        } if from == ITEM && to == "new"
    ));

    let cli = Cli::try_parse_from([
        "onenote",
        "update",
        "page",
        "--body",
        "Text",
        "--base-content",
        ITEM,
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Update {
            expected_content: Some(ref exp),
            ..
        } if exp == ITEM
    ));

    // 4. check --expected-content
    let cli = Cli::try_parse_from([
        "onenote",
        "check",
        "page",
        "--expected-content",
        "- [ ] task",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Check {
            expected_content: Some(ref exp),
            json: true,
            ..
        } if exp == "- [ ] task"
    ));

    // 5. section & notebook names + search
    let cli = Cli::try_parse_from([
        "onenote",
        "rename-section",
        "sec",
        "--name",
        ITEM,
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::RenameSection {
            ref section,
            ref name,
            json: true,
        } if section == "sec" && name == ITEM
    ));

    let cli =
        Cli::try_parse_from(["onenote", "create-section", "nb", ITEM, "--json"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::CreateSection {
            ref notebook,
            ref name,
            json: true,
        } if notebook == "nb" && name == ITEM
    ));

    let cli = Cli::try_parse_from(["onenote", "create-notebook", ITEM, "--json"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::CreateNotebook {
            ref name,
            json: true,
            ..
        } if name == ITEM
    ));

    let cli = Cli::try_parse_from([
        "onenote",
        "rename-notebook",
        "--drive-id",
        "d1",
        "--item-id",
        "i1",
        "--name",
        ITEM,
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::RenameNotebook {
            ref name,
            json: true,
            ..
        } if name == ITEM
    ));

    let cli = Cli::try_parse_from(["onenote", "search", ITEM, "--json"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Search {
            ref query,
            json: true,
            ..
        } if query == ITEM
    ));

    // 6. Explicit `--flag=value` accepts flag-looking text literally
    let cli = Cli::try_parse_from([
        "onenote",
        "update",
        "page",
        "--body=--dry-run",
        "--title=--help",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Update {
            title: Some(ref title),
            body: Some(ref body),
            json: true,
            ..
        } if title == "--help" && body == "--dry-run"
    ));

    let cli = Cli::try_parse_from([
        "onenote",
        "rename-section",
        "sec",
        "--name=--json",
        "--json",
    ])
    .unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::RenameSection {
            ref name,
            json: true,
            ..
        } if name == "--json"
    ));
}

#[test]
fn preserves_known_flags_help_and_missing_value_errors() {
    use clap::error::ErrorKind;

    const ITEM: &str = "- 목록 항목";

    // Actual help invocations (standalone and trailing after completed hyphen values) return DisplayHelp
    for argv in [
        vec!["onenote", "create", "--help"],
        vec!["onenote", "create", "sec", "--title", "Test", ITEM, "--help"],
        vec!["onenote", "create", "sec", "--title", ITEM, "--help"],
        vec!["onenote", "append", "--help"],
        vec!["onenote", "append", "page", ITEM, "--help"],
        vec!["onenote", "update", "--help"],
        vec!["onenote", "update", "page", "--body", ITEM, "--help"],
        vec![
            "onenote",
            "update",
            "page",
            "--block-id",
            "blk",
            "--replace-text",
            "old",
            "--with-text",
            ITEM,
            "--help",
        ],
        vec![
            "onenote",
            "update",
            "page",
            "--block-id",
            "blk",
            "--replace-text",
            ITEM,
            "--with-text",
            "new",
            "--help",
        ],
        vec![
            "onenote",
            "update",
            "page",
            "--body",
            "Text",
            "--expected-content",
            ITEM,
            "--help",
        ],
        vec!["onenote", "rename-section", "sec", "--name", ITEM, "--help"],
        vec!["onenote", "search", ITEM, "--help"],
        vec!["onenote", "search", "--help"],
        vec!["onenote", "search", "-h"],
    ] {
        let err = Cli::try_parse_from(argv.clone()).unwrap_err();
        assert_eq!(
            err.kind(),
            ErrorKind::DisplayHelp,
            "expected DisplayHelp for {argv:?}, got: {err}"
        );
    }

    // Missing option/positional values followed by known or flag-looking tokens must fail (never swallow flags)
    for argv in [
        vec!["onenote", "create", "sec", "--title", "--json"],
        vec!["onenote", "create", "sec", "--title", "--dry-run"],
        vec!["onenote", "create", "sec", "--title", "--help"],
        vec!["onenote", "create", "sec", "--title", "-h"],
        vec!["onenote", "append", "page", "--dry-run"],
        vec!["onenote", "update", "page", "--body", "--json"],
        vec!["onenote", "update", "page", "--body", "--dry-run"],
        vec!["onenote", "update", "page", "--body", "--help"],
        vec!["onenote", "update", "page", "--body", "-h"],
        vec!["onenote", "update", "page", "--body", "--"],
        vec![
            "onenote",
            "update",
            "page",
            "--block-id",
            "blk",
            "--replace-text",
            "--with-text",
            "new",
        ],
        vec![
            "onenote",
            "update",
            "page",
            "--block-id",
            "blk",
            "--replace-text",
            "old",
            "--with-text",
            "--json",
        ],
        vec![
            "onenote",
            "update",
            "page",
            "--body",
            "Text",
            "--expected-content",
            "--json",
        ],
        vec!["onenote", "check", "page", "--expected-content", "--json"],
        vec!["onenote", "rename-section", "sec", "--name", "--json"],
        vec!["onenote", "search", "--json"],
        vec!["onenote", "search", "--dry-run"],
        // Non-free-text fields keep their existing strict parsers
        vec!["onenote", "create", "sec", "--title", "Test", "--level", "-1"],
        vec!["onenote", "read", "page", "--format", "-bad"],
        vec!["onenote", "update", "page", "--block-id", "-bad", "--body", "Text"],
    ] {
        let err = Cli::try_parse_from(argv.clone()).unwrap_err();
        assert_ne!(
            err.kind(),
            ErrorKind::DisplayHelp,
            "expected parse error (not help) for {argv:?}"
        );
    }
}

#[test]
fn skips_second_clap_command_tree_when_argv_has_no_hyphen_text_candidates() {
    let to_os = |args: &[&str]| -> Vec<std::ffi::OsString> {
        args.iter().map(std::ffi::OsString::from).collect()
    };

    for ordinary in [
        &["onenote"][..],
        &["onenote", "notebooks", "--include-teams", "--json"][..],
        &["onenote", "create", "sec", "--title", "RFC-01", "# Heading", "--json"][..],
        &["onenote", "create", "sec", "--title=- 목록 항목", "--json"][..],
        &["onenote", "append", "page", "Plain body", "--json"][..],
        &["onenote", "append", "page", "--", "- 목록 항목"][..],
        &["onenote", "update", "page", "--body", "Updated", "--json"][..],
        &["onenote", "update", "page", "--body", "--json"][..],
        &["onenote", "update", "page", "--body", "--dry-run"][..],
        &["onenote", "search", "architecture", "--help"][..],
    ] {
        assert!(
            !argv_needs_hyphen_normalization(&to_os(ordinary)),
            "should skip normalization for ordinary argv: {ordinary:?}"
        );
    }

    for hyphen_text in [
        &["onenote", "create", "sec", "--title", "- 목록 항목"][..],
        &["onenote", "append", "page", "- first\n- second", "--json"][..],
        &["onenote", "append", "page", "---"][..],
        &["onenote", "update", "page", "--body", "- 목록 항목"][..],
        &["onenote", "rename-section", "sec", "--name", "- 목록 항목"][..],
        &["onenote", "search", "- 목록 항목", "--json"][..],
    ] {
        assert!(
            argv_needs_hyphen_normalization(&to_os(hyphen_text)),
            "should normalize hyphen text argv: {hyphen_text:?}"
        );
    }
}

#[test]
fn read_json_parse_forms() {
    let cli = Cli::try_parse_from(["onenote", "read", "p", "--json"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Read {
            format: None,
            full: false,
            compact: false,
            json: true,
            ..
        }
    ));

    let cli =
        Cli::try_parse_from(["onenote", "read", "p", "--format", "md", "--json"]).unwrap();
    assert!(matches!(
        cli.command.unwrap(),
        Command::Read {
            format: Some(ref f),
            full: false,
            compact: false,
            json: true,
            ..
        } if f == "md"
    ));
}

#[test]
fn read_json_shape_matrix() {
    assert!(read_json_is_compact(false, false, false));
    assert!(read_json_is_compact(false, true, false));
    assert!(!read_json_is_compact(false, false, true));
    assert!(!read_json_is_compact(true, false, false));
    assert!(read_json_is_compact(true, true, false));
    assert!(!read_json_is_compact(true, false, true));
}

#[test]
fn read_full_conflicts_with_compact() {
    use clap::error::ErrorKind;
    let err = Cli::try_parse_from([
        "onenote", "read", "p", "--json", "--compact", "--full",
    ])
    .unwrap_err();
    assert_eq!(err.kind(), ErrorKind::ArgumentConflict);
}

#[test]
fn read_selectors_require_json() {
    use clap::error::ErrorKind;
    let err_full = Cli::try_parse_from(["onenote", "read", "p", "--full"]).unwrap_err();
    assert_eq!(err_full.kind(), ErrorKind::MissingRequiredArgument);

    let err_compact = Cli::try_parse_from(["onenote", "read", "p", "--compact"]).unwrap_err();
    assert_eq!(err_compact.kind(), ErrorKind::MissingRequiredArgument);
}

#[test]
fn read_format_with_json_parses() {
    assert!(
        Cli::try_parse_from(["onenote", "read", "p", "--format", "xhtml", "--json"]).is_ok()
    );
    assert!(Cli::try_parse_from([
        "onenote", "read", "p", "--format", "xhtml", "--json", "--compact",
    ])
    .is_ok());
    assert!(Cli::try_parse_from(["onenote", "read", "p", "--format", "bogus"]).is_err());
}

#[test]
fn list_limit_bounds() {
    assert!(Cli::try_parse_from(["onenote", "sections", "--limit", "0"]).is_err());
    assert!(Cli::try_parse_from(["onenote", "sections", "--limit", "1001"]).is_err());
    assert!(
        Cli::try_parse_from(["onenote", "sections", "--limit", "1000", "--cursor", "5"])
            .is_ok()
    );
    assert!(Cli::try_parse_from(["onenote", "sections", "--cursor", "x"]).is_err());
}

#[test]
fn page_slice_boundaries() {
    let base = vec![1, 2, 3, 4, 5];
    let (s, m) = page_slice(base.clone(), 0, 5);
    assert_eq!((s.len(), m), (5, false));
    let (s, m) = page_slice(base.clone(), 0, 4);
    assert_eq!((s.len(), m), (4, true));
    let (s, m) = page_slice(base.clone(), 4, 1);
    assert_eq!((s.len(), m), (1, false));
    let (s, m) = page_slice(base.clone(), 5, 1);
    assert_eq!((s.len(), m), (0, false));
    let (s, m) = page_slice(base, 9, 2);
    assert_eq!((s.len(), m), (0, false));
}

#[test]
fn compact_page_json_keys() {
    use crate::models::{OnenoteBlockKind, RichSpan};

    let make_block = |id: &str| OnenoteBlock {
        element_id: Some(id.to_string()),
        tag: None,
        raw_html: Some(format!("<p id=\"{id}\">text</p>")),
        kind: OnenoteBlockKind::Paragraph {
            spans: vec![RichSpan::plain("text")],
        },
    };
    let detail = OnenotePageDetail {
        summary: OnenotePageSummary {
            page_key: "group:G1|gpage-1".to_string(),
            id: "gpage-1".to_string(),
            scope: OnenoteScope::Group {
                group_id: "G1".to_string(),
                group_name: None,
            },
            section_id: "gsec-1".to_string(),
            section_name: Some("Specs".to_string()),
            notebook_id: Some("gnb-1".to_string()),
            notebook_name: Some("Engineering".to_string()),
            title: "Architecture Spec".to_string(),
            level: 0,
            order: 1,
            created_at: Some("2026-11-01T00:00:00Z".to_string()),
            last_modified_at: Some("2026-11-02T00:00:00Z".to_string()),
            content_url: Some("https://example.invalid/content".to_string()),
            web_url: Some("https://example.invalid/gpage-1".to_string()),
            preview_text: Some("preview".to_string()),
            has_todo: true,
        },
        raw_xhtml: "<html><body><p>text</p></body></html>".to_string(),
        body_markdown: "# Architecture Spec".to_string(),
        body_plain: "Architecture Spec".to_string(),
        blocks: vec![make_block("p:1"), make_block("p:2"), make_block("p:3")],
        resources: vec![OnenoteResource {
            element_id: Some("img:1".to_string()),
            resource_url: "https://example.invalid/res-1".to_string(),
            fullres_url: None,
            mime_type: "image/png".to_string(),
            file_name: Some("diagram.png".to_string()),
            is_image: true,
            alt_text: None,
        }],
        primary_div_id: Some("div:1".to_string()),
        synced_at: Some("2026-11-21T11:07:40Z".to_string()),
    };

    let cli_compact = page_content_json(&detail, true, PageSurface::Cli);
    assert_eq!(cli_compact["page_key"], "group:G1|gpage-1");
    assert_eq!(cli_compact["scope"]["scope_type"], "group");
    assert_eq!(cli_compact["scope"]["group_id"], "G1");
    assert_eq!(cli_compact["omitted"]["blocks"], 3);
    assert_eq!(cli_compact["omitted"]["resources"], 1);
    assert!(cli_compact.get("blocks").is_none());
    assert!(cli_compact.get("raw_xhtml").is_none());
    assert!(cli_compact["hint"].as_str().unwrap().contains("--full"));

    let mcp_compact = page_content_json(&detail, true, PageSurface::Mcp);
    assert_eq!(mcp_compact["page_key"], "group:G1|gpage-1");
    assert_eq!(mcp_compact["scope"]["group_id"], "G1");
    assert_eq!(mcp_compact["omitted"]["blocks"], 3);
    assert_eq!(mcp_compact["omitted"]["resources"], 1);
    assert!(mcp_compact.get("blocks").is_none());
    assert!(mcp_compact.get("raw_xhtml").is_none());
    assert!(mcp_compact["hint"]
        .as_str()
        .unwrap()
        .contains("\"compact\":false"));

    assert_eq!(page_content_json(&detail, false, PageSurface::Cli), json!(detail));
    assert_eq!(page_content_json(&detail, false, PageSurface::Mcp), json!(detail));
}

#[test]
fn delete_prompt_names_the_resolved_page_or_flags_a_raw_id() {
    let page = OnenotePageSummary {
        page_key: "me|pg-1".to_string(),
        id: "pg-1".to_string(),
        scope: crate::models::OnenoteScope::Me,
        section_id: "sec-1".to_string(),
        section_name: Some("Specs".to_string()),
        notebook_id: Some("nb-1".to_string()),
        notebook_name: Some("Engineering".to_string()),
        title: "Release Plan".to_string(),
        level: 0,
        order: 0,
        created_at: None,
        last_modified_at: Some("2026-11-20T09:00:00Z".to_string()),
        content_url: None,
        web_url: None,
        preview_text: None,
        has_todo: false,
    };
    let text = delete_prompt_text("me|pg-1", Some(&page));
    assert!(text.contains("Title:         Release Plan"));
    assert!(text.contains("Location:      Engineering / Specs"));
    assert!(text.contains("Last modified: 2026-11-20T09:00:00Z"));
    assert!(text.contains("Page key:      me|pg-1"));
    assert!(text.ends_with("[y/N]: "));

    // Missing names fall back to the section ID instead of an empty location.
    let bare = OnenotePageSummary {
        section_name: None,
        notebook_name: None,
        last_modified_at: None,
        ..page
    };
    let bare_text = delete_prompt_text("me|pg-1", Some(&bare));
    assert!(bare_text.contains("Location:      (unknown notebook) / sec-1"));
    assert!(bare_text.contains("Last modified: unknown"));

    let raw = delete_prompt_text("0-abc!1-def", None);
    assert!(raw.contains("not in the local cache"));
    assert!(raw.contains("raw page ID"));
    assert!(raw.contains("Delete OneNote page ID '0-abc!1-def'?"));
    assert!(raw.ends_with("[y/N]: "));
}
