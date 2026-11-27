//! Unit tests for `AppState`, `UiState`, cache eviction, inline editing, and `UiMsg` handlers.

use std::sync::Arc;

use tokio::sync::mpsc;

use crate::{
    auth::AuthManager,
    config::{AppConfig, ImageProtocolPreference, NoteDraftEntry},
    graph::GraphClient,
    img_preview::{TerminalGraphicsCaps, TerminalImageProtocol},
    models::{
        CoreEvent, OnenoteNotebook, OnenotePageDetail, OnenotePageSummary, OnenoteScope,
        OnenoteSection, OutgoingOnenotePage,
    },
    parser,
    storage::Storage,
    sync::{FocusTarget, SyncCommand, SyncController},
    ui::overlays::Overlay,
};

use super::state::apply_ui_settings;
use super::*;
use crate::auth::StoredTokens;
use tokio::sync::watch;

fn make_test_app_with_cached_summary() -> (
    AppState,
    UiState,
    watch::Receiver<FocusTarget>,
    mpsc::UnboundedReceiver<SyncCommand>,
) {
    let cfg = AppConfig::default();
    let tokens = StoredTokens {
        access_token: "test-token".to_string(),
        refresh_token: None,
        expires_at: chrono::Utc::now().timestamp() + 3600,
        client_id: "cid".to_string(),
        tenant_id: "tid".to_string(),
        user_id: Some("user-1".to_string()),
        user_display_name: Some("Tester".to_string()),
        user_email: Some("tester@example.com".to_string()),
        auth_mode: "access_token".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tokens);
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();

    storage
        .upsert_onenote_notebooks(&[OnenoteNotebook {
            notebook_key: "me|nb-1".to_string(),
            id: "nb-1".to_string(),
            scope: OnenoteScope::Me,
            display_name: "Engineering".to_string(),
            color: None,
            is_default: true,
            is_shared: false,
            user_role: Some("Owner".to_string()),
            created_at: None,
            last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
            created_by: None,
            web_url: None,
        }])
        .unwrap();
    storage
        .upsert_onenote_sections(&[OnenoteSection {
            section_key: "me|sec-1".to_string(),
            id: "sec-1".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Engineering".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "Specs".to_string(),
            color: None,
            is_default: true,
            pages_url: None,
            created_at: None,
            last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
            web_url: None,
        }])
        .unwrap();
    storage
        .upsert_onenote_pages(&[OnenotePageSummary {
            page_key: "me|page-1".to_string(),
            id: "page-1".to_string(),
            scope: OnenoteScope::Me,
            section_id: "sec-1".to_string(),
            section_name: Some("Specs".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Engineering".to_string()),
            title: "Initial Spec".to_string(),
            level: 0,
            order: 0,
            created_at: Some("2026-11-18T09:00:00Z".to_string()),
            last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: false,
        }])
        .unwrap();

    let (focus_tx, focus_rx) = watch::channel(FocusTarget::default());
    let (nudge_tx, nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let (ui_tx, _ui_rx) = mpsc::unbounded_channel::<UiMsg>();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);

    let app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let ui = UiState::from_config(&cfg);
    (app, ui, focus_rx, nudge_rx)
}

#[test]
fn test_recent_page_cache_reopens_evicted_details_and_isolates_late_updates() {
    let (mut app, mut ui, _, _) = make_test_app_with_cached_summary();
    app.load_cached_initial();

    let make_detail = |mut summary: OnenotePageSummary, id: &str, body: &str| {
        summary.id = id.to_string();
        summary.page_key = format!("me|{id}");
        parser::parse_onenote_page_xhtml(
            summary,
            &format!("<html><body><p>{body}</p></body></html>"),
        )
    };
    let summary = app.active_page_summary().unwrap().clone();
    let a = make_detail(summary.clone(), "page-1", "First cached body");
    let b = make_detail(summary.clone(), "page-2", "Second cached body");
    for detail in [&a, &b] {
        app.storage.upsert_onenote_page_detail(detail).unwrap();
    }

    app.select_page(a.summary.page_key.clone(), &mut ui);
    app.select_page(b.summary.page_key.clone(), &mut ui);
    assert_eq!(app.page_details.len(), 2);

    let mut form = crate::ui::overlays::onenote_form::OnenoteFormOverlay::edit_block(
        b.summary.page_key.clone(),
        b.summary.title.clone(),
        "p:1".into(),
        0,
        b.body_plain.clone(),
    );
    form.body = "Unsaved draft".into();
    ui.overlays.push(Overlay::OnenoteForm(Box::new(form)));

    app.select_page("me|page-3".into(), &mut ui);
    assert!(!app.page_details.contains_key(&a.summary.page_key));
    for (id, body) in [
        ("page-1", "Late evicted update"),
        ("page-2", "Late previous update"),
    ] {
        let detail = make_detail(summary.clone(), id, body);
        app.storage.upsert_onenote_page_detail(&detail).unwrap();
        app.handle_core_event(&mut ui, CoreEvent::OnenotePageDetailUpdated(Box::new(detail)));
    }
    assert_eq!(app.active_page_key.as_deref(), Some("me|page-3"));
    assert!(app.loading_onenote_detail);
    assert_eq!(app.page_details.len(), 1);
    assert_eq!(app.page_details["me|page-2"].body_plain, "Late previous update");
    let Some(Overlay::OnenoteForm(form)) = ui.overlays.last() else {
        panic!("expected edit form");
    };
    assert_eq!(form.body, "Unsaved draft");
    assert!(form.error.is_none());

    app.select_page(a.summary.page_key.clone(), &mut ui);
    assert_eq!(app.active_page_detail().unwrap().body_plain, "Late evicted update");
    assert!(!app.loading_onenote_detail);
    assert_eq!(app.page_details.len(), 1);

    for i in 3..=21 {
        let detail = make_detail(summary.clone(), &format!("page-{i}"), "Visited body");
        app.select_page(detail.summary.page_key.clone(), &mut ui);
        app.storage.upsert_onenote_page_detail(&detail).unwrap();
        app.handle_core_event(&mut ui, CoreEvent::OnenotePageDetailUpdated(Box::new(detail)));
        assert_eq!(app.page_details.len(), 2);
    }
    app.select_page(a.summary.page_key, &mut ui);
    assert_eq!(app.active_page_detail().unwrap().body_plain, "Late evicted update");
    assert_eq!(app.page_details.len(), 2);
    let stored_keys = app.storage.list_onenote_page_keys("me|sec-1").unwrap();
    assert_eq!(stored_keys.len(), 21);
    assert!(stored_keys.iter().all(|key| app.storage.get_onenote_page_detail(key).unwrap().is_some()));
}

#[test]
fn test_startup_pushes_focus_hydrates_initial_page_and_rehydrates_on_remote_edit() {
    let (mut app, mut ui, mut focus_rx, mut nudge_rx) = make_test_app_with_cached_summary();

    // 1. load_cached_initial must push initial focus and hydrate the selected summary-only page
    app.load_cached_initial();
    assert!(focus_rx.has_changed().unwrap());
    {
        let focus = focus_rx.borrow_and_update();
        assert_eq!(focus.section_key.as_deref(), Some("me|sec-1"));
        assert_eq!(focus.page_key.as_deref(), Some("me|page-1"));
    }
    assert!(app.loading_onenote_detail);
    assert_eq!(
        nudge_rx.try_recv().ok(),
        Some(SyncCommand::HydratePage {
            page_key: "me|page-1".to_string()
        })
    );

    // 2. Deliver hydrated page detail at T1
    let summary_t1 = OnenotePageSummary {
        page_key: "me|page-1".to_string(),
        id: "page-1".to_string(),
        scope: OnenoteScope::Me,
        section_id: "sec-1".to_string(),
        section_name: Some("Specs".to_string()),
        notebook_id: Some("nb-1".to_string()),
        notebook_name: Some("Engineering".to_string()),
        title: "Initial Spec".to_string(),
        level: 0,
        order: 0,
        created_at: Some("2026-11-18T09:00:00Z".to_string()),
        last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
        content_url: None,
        web_url: None,
        preview_text: Some("Old body".to_string()),
        has_todo: false,
    };
    let detail_t1 = OnenotePageDetail {
        summary: summary_t1.clone(),
        raw_xhtml: "<html><body><p>Old body</p></body></html>".to_string(),
        body_markdown: "Old body".to_string(),
        body_plain: "Old body".to_string(),
        blocks: Vec::new(),
        resources: Vec::new(),
        primary_div_id: None,
        synced_at: Some("2026-11-18T10:00:00Z".to_string()),
    };
    app.handle_core_event(&mut ui, CoreEvent::OnenotePageDetailUpdated(Box::new(detail_t1)));
    assert!(!app.loading_onenote_detail);
    assert!(app.page_details.contains_key("me|page-1"));

    // 3. Unchanged OnenotePagesUpdated at T1 keeps cached detail, avoids redundant HydratePage,
    //    and does not re-notify the focus watch channel.
    app.handle_core_event(
        &mut ui,
        CoreEvent::OnenotePagesUpdated {
            section_key: "me|sec-1".to_string(),
            pages: vec![summary_t1.clone()],
            notify_candidate: None,
        },
    );
    assert!(app.page_details.contains_key("me|page-1"));
    assert!(nudge_rx.try_recv().is_err());
    assert!(!focus_rx.has_changed().unwrap());

    // 4. Remote edit arrives with newer last_modified_at = T2 -> evicts stale detail and queues HydratePage
    let mut summary_t2 = summary_t1;
    summary_t2.last_modified_at = Some("2026-11-18T11:00:00Z".to_string());
    app.handle_core_event(
        &mut ui,
        CoreEvent::OnenotePagesUpdated {
            section_key: "me|sec-1".to_string(),
            pages: vec![summary_t2],
            notify_candidate: None,
        },
    );
    assert!(!app.page_details.contains_key("me|page-1"));
    assert!(app.loading_onenote_detail);
    assert_eq!(
        nudge_rx.try_recv().ok(),
        Some(SyncCommand::HydratePage {
            page_key: "me|page-1".to_string()
        })
    );

    // 5. Action::RefreshAll sets syncing state, pushes focus, and requests RefreshAll
    crate::ui::action::dispatch(crate::ui::action::Action::RefreshAll, &mut app, &mut ui);
    assert!(app.syncing);
    assert_eq!(app.sync_summary, "Syncing OneNote...");
    assert_eq!(nudge_rx.try_recv().ok(), Some(SyncCommand::RefreshAll));
}

#[test]
fn test_draft_reopen_canonical_keys_conflict_preservation_and_inactive_section_eviction() {
    let (mut app, mut ui, mut focus_rx, _nudge_rx) = make_test_app_with_cached_summary();

    // Add an inactive section + page so we can verify inactive section eviction and move handling
    app.storage
        .upsert_onenote_sections(&[OnenoteSection {
            section_key: "me|sec-2".to_string(),
            id: "sec-2".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Engineering".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "InactiveSec".to_string(),
            color: None,
            is_default: false,
            pages_url: None,
            created_at: None,
            last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
            web_url: None,
        }])
        .unwrap();
    let p2_summary = OnenotePageSummary {
        page_key: "me|page-2".to_string(),
        id: "page-2".to_string(),
        scope: OnenoteScope::Me,
        section_id: "sec-2".to_string(),
        section_name: Some("InactiveSec".to_string()),
        notebook_id: Some("nb-1".to_string()),
        notebook_name: Some("Engineering".to_string()),
        title: "Inactive Page v1".to_string(),
        level: 0,
        order: 0,
        created_at: None,
        last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
        content_url: None,
        web_url: None,
        preview_text: Some("Old inactive body".to_string()),
        has_todo: false,
    };
    let p2_detail = crate::parser::parse_onenote_page_xhtml(
        p2_summary.clone(),
        r#"<html><head><title>Inactive Page v1</title></head><body><div id="div:2"><p id="p:2">Old inactive body</p></div></body></html>"#,
    );
    app.storage.upsert_onenote_page_detail(&p2_detail).unwrap();

    let p1_summary = app.storage.get_onenote_page_summary("me|page-1").unwrap().unwrap();
    let p1_detail = crate::parser::parse_onenote_page_xhtml(
        p1_summary,
        r#"<html><head><title>Initial Spec</title></head><body><div id="div:1"><p id="p:1">Alpha paragraph</p></div></body></html>"#,
    );
    app.storage.upsert_onenote_page_detail(&p1_detail).unwrap();

    app.load_cached_initial();
    let _ = focus_rx.borrow_and_update();
    app.page_details.insert("me|page-2".to_string(), p2_detail);
    assert!(app.page_details.contains_key("me|page-2"));

    // 1. CreatePage draft stashed via canonical key ("create:me|sec-1") restores on reopen
    let mut create_form = crate::ui::overlays::onenote_form::OnenoteFormOverlay::create_page(
        "me|sec-1".to_string(),
        "Specs".to_string(),
        0,
    );
    create_form.title = "Draft Page Title".to_string();
    create_form.body = "Unsent draft body".to_string();
    let (ckey, centry) = create_form.draft_entry_snapshot().unwrap();
    assert_eq!(ckey, "create:me|sec-1");
    app.drafts.set_note(ckey, centry);

    crate::ui::action::dispatch(
        crate::ui::action::Action::OpenOnenoteCreatePage { level: 0 },
        &mut app,
        &mut ui,
    );
    match ui.overlays.pop() {
        Some(Overlay::OnenoteForm(form)) => {
            assert_eq!(form.title, "Draft Page Title");
            assert_eq!(form.body, "Unsent draft body");
        }
        other => panic!("expected create form overlay, got {other:?}"),
    }

    // 2. EditBlock draft saved via Esc or conflict restores on reopen & flags remote drift
    crate::ui::action::dispatch(
        crate::ui::action::Action::OpenOnenoteEditBlock,
        &mut app,
        &mut ui,
    );
    if let Some(Overlay::OnenoteForm(ref mut form)) = ui.overlays.last_mut() {
        form.body = "Local unsent edit".to_string();
    }
    // Simulate Esc close -> stashes edit draft with base_content = "Alpha paragraph"
    crate::ui::action::dispatch(crate::ui::action::Action::CloseOverlay, &mut app, &mut ui);
    let stashed = app.drafts.get_note("edit:me|page-1:p:1").unwrap();
    assert_eq!(stashed.body, "Local unsent edit");
    assert_eq!(stashed.base_content.as_deref(), Some("Alpha paragraph"));

    // Simulate remote edit arriving via EditConflict -> preserves local draft & updates detail
    let mut remote_detail = p1_detail.clone();
    remote_detail.summary.title = "Remote Renamed Spec".to_string();
    remote_detail.summary.last_modified_at = Some("2026-11-18T12:00:00Z".to_string());
    remote_detail = crate::parser::parse_onenote_page_xhtml(
        remote_detail.summary.clone(),
        r#"<html><head><title>Remote Renamed Spec</title></head><body><div id="div:1"><p id="p:1">Remote edit by Alice</p></div></body></html>"#,
    );
    app.handle_ui_msg(
        &mut ui,
        UiMsg::EditConflict {
            task_id: 1,
            detail: Box::new(remote_detail),
            draft_to_preserve: Some((
                "edit:me|page-1:p:1".to_string(),
                NoteDraftEntry {
                    title: String::new(),
                    body: "Local unsent edit".to_string(),
                    base_content: Some("Alpha paragraph".to_string()),
                },
            )),
            message: "Conflict: block changed remotely.".to_string(),
        },
    );
    assert_eq!(app.pages[0].title, "Remote Renamed Spec");
    assert!(app.drafts.get_note("edit:me|page-1:p:1").is_some());

    // Reopen EditBlock -> restores local draft AND shows remote drift warning
    crate::ui::action::dispatch(
        crate::ui::action::Action::OpenOnenoteEditBlock,
        &mut app,
        &mut ui,
    );
    match ui.overlays.pop() {
        Some(Overlay::OnenoteForm(form)) => {
            assert_eq!(form.body, "Local unsent edit");
            assert!(form
                .error
                .as_deref()
                .is_some_and(|e| e.contains("Remote block changed")));
        }
        other => panic!("expected edit block overlay, got {other:?}"),
    }

    // 3. Inactive section page modified in SQLite -> OnenoteHierarchyUpdated evicts RAM page_details
    let mut p2_newer = p2_summary;
    p2_newer.last_modified_at = Some("2026-11-18T13:00:00Z".to_string());
    p2_newer.title = "Inactive Page v2".to_string();
    app.storage.upsert_onenote_pages(&[p2_newer]).unwrap();
    let nbs = app.storage.list_onenote_notebooks().unwrap();
    let secs = app.storage.list_onenote_sections(None).unwrap();
    app.handle_core_event(
        &mut ui,
        CoreEvent::OnenoteHierarchyUpdated {
            notebooks: nbs,
            section_groups: Vec::new(),
            sections: secs,
        },
    );
    assert!(
        !app.page_details.contains_key("me|page-2"),
        "inactive section stale detail must be evicted on hierarchy update"
    );

    // 4. PageDeleted pushes updated focus to sync engine
    let _ = focus_rx.borrow_and_update();
    app.handle_ui_msg(
        &mut ui,
        UiMsg::PageDeleted {
            task_id: 2,
            page_key: "me|page-1".to_string(),
        },
    );
    assert!(focus_rx.has_changed().unwrap());
    assert_eq!(focus_rx.borrow_and_update().page_key, None);
}

#[tokio::test]
async fn test_tui_spawn_edit_block_and_toggle_todo_preflight_conflict_and_success() {
    use axum::{
        body::Bytes,
        extract::State,
        http::StatusCode,
        routing::get,
        Json, Router,
    };
    use serde_json::json;
    use std::sync::{
        atomic::{AtomicUsize, Ordering},
        Arc, Mutex,
    };

    #[derive(Clone)]
    struct TuiMockState {
        xhtml: Arc<Mutex<String>>,
        patch_hits: Arc<AtomicUsize>,
    }

    let state = TuiMockState {
        xhtml: Arc::new(Mutex::new(
            r#"<!DOCTYPE html><html><head><title>Initial Spec</title></head><body><div id="div:{root}"><p id="p:{b1}">REMOTE EDIT BY ALICE</p><p id="p:{todo1}" data-tag="to-do">Ship sync audit</p></div></body></html>"#
                .to_string(),
        )),
        patch_hits: Arc::new(AtomicUsize::new(0)),
    };

    let app_router = Router::new()
        .route(
            "/v1.0/me/onenote/pages/page-1",
            get(|| async {
                Json(json!({
                    "id": "page-1",
                    "title": "Initial Spec",
                    "level": 0,
                    "order": 0,
                    "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                    "parentSection": { "id": "sec-1", "displayName": "Specs" }
                }))
            }),
        )
        .route(
            "/v1.0/me/onenote/pages/page-1/content",
            get(|State(st): State<TuiMockState>| async move {
                let body = st.xhtml.lock().unwrap().clone();
                (StatusCode::OK, [("content-type", "text/html")], body)
            })
            .patch(|State(st): State<TuiMockState>, body: Bytes| async move {
                st.patch_hits.fetch_add(1, Ordering::SeqCst);
                let cmds: Vec<serde_json::Value> =
                    serde_json::from_slice(&body).unwrap_or_default();
                let mut html = st.xhtml.lock().unwrap();
                for cmd in cmds {
                    let target = cmd["target"].as_str().unwrap_or("");
                    let content = cmd["content"].as_str().unwrap_or("");
                    if target == "p:{b1}" {
                        if let Some(start) = html.find(r#"<p id="p:{b1}""#) {
                            if let Some(rel_end) = html[start..].find("</p>") {
                                let repl = content.replacen("<p>", r#"<p id="p:{b1}">"#, 1);
                                html.replace_range(start..start + rel_end + 4, &repl);
                            }
                        }
                    } else if target == "p:{todo1}" {
                        if let Some(start) = html.find(r#"<p id="p:{todo1}""#) {
                            if let Some(rel_end) = html[start..].find("</p>") {
                                html.replace_range(start..start + rel_end + 4, content);
                            }
                        }
                    }
                }
                StatusCode::NO_CONTENT
            }),
        )
        .with_state(state.clone());

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, app_router).await;
    });

    let cfg = AppConfig {
        graph_base_url: format!("http://{addr}/v1.0"),
        ..AppConfig::default()
    };
    let tokens = crate::auth::StoredTokens {
        access_token: "tui-test-token".to_string(),
        refresh_token: None,
        expires_at: chrono::Utc::now().timestamp() + 3600,
        client_id: "cid".to_string(),
        tenant_id: "tid".to_string(),
        user_id: Some("user-1".to_string()),
        user_display_name: Some("Tester".to_string()),
        user_email: None,
        auth_mode: "access_token".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tokens);
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMsg>();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);

    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);

    // Seed TUI with stale local snapshot (before Alice edited p:{b1})
    let stale_summary = OnenotePageSummary {
        page_key: "me|page-1".to_string(),
        id: "page-1".to_string(),
        scope: OnenoteScope::Me,
        section_id: "sec-1".to_string(),
        section_name: Some("Specs".to_string()),
        notebook_id: Some("nb-1".to_string()),
        notebook_name: Some("Engineering".to_string()),
        title: "Initial Spec".to_string(),
        level: 0,
        order: 0,
        created_at: None,
        last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
        content_url: None,
        web_url: None,
        preview_text: None,
        has_todo: true,
    };
    let stale_detail = parser::parse_onenote_page_xhtml(
        stale_summary.clone(),
        r#"<!DOCTYPE html><html><head><title>Initial Spec</title></head><body><div id="div:{root}"><p id="p:{b1}">Old local text</p><p id="p:{todo1}" data-tag="to-do">Ship sync audit</p></div></body></html>"#,
    );
    app.storage.upsert_onenote_page_detail(&stale_detail).unwrap();
    app.pages = vec![stale_summary];
    app.active_page_key = Some("me|page-1".to_string());
    app.page_details.insert("me|page-1".to_string(), stale_detail);

    // 1. Stale edit on p:{b1} -> EditConflict, 0 PATCH, remote preserved, draft preserved
    app.spawn_edit_block(
        "me|page-1".to_string(),
        "p:{b1}".to_string(),
        "Local stale edit".to_string(),
        "Old local text".to_string(),
        &mut ui,
    );
    let msg1 = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(msg1, UiMsg::EditConflict { .. }));
    app.handle_ui_msg(&mut ui, msg1);
    assert_eq!(state.patch_hits.load(Ordering::SeqCst), 0);
    assert!(state.xhtml.lock().unwrap().contains("REMOTE EDIT BY ALICE"));
    assert_eq!(
        app.drafts
            .get_note("edit:me|page-1:p:{b1}")
            .map(|d| d.body.as_str()),
        Some("Local stale edit")
    );

    // 2. Different-paragraph to-do toggle (block index 1 = p:{todo1}) -> succeeds, BOTH edits kept
    app.spawn_toggle_todo(1, &mut ui);
    let msg2 = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(msg2, UiMsg::PageMutated { .. }));
    app.handle_ui_msg(&mut ui, msg2);
    assert_eq!(state.patch_hits.load(Ordering::SeqCst), 1);
    let after_todo = state.xhtml.lock().unwrap().clone();
    assert!(after_todo.contains("REMOTE EDIT BY ALICE"));
    assert!(after_todo.contains("to-do:completed"));

    // 3. Matching/new baseline edit on p:{b1} -> succeeds and clears edit draft
    app.spawn_edit_block(
        "me|page-1".to_string(),
        "p:{b1}".to_string(),
        "Merged edit on top of Alice".to_string(),
        "REMOTE EDIT BY ALICE".to_string(),
        &mut ui,
    );
    let msg3 = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(msg3, UiMsg::PageMutated { .. }));
    app.handle_ui_msg(&mut ui, msg3);
    assert_eq!(state.patch_hits.load(Ordering::SeqCst), 2);
    assert!(state
        .xhtml
        .lock()
        .unwrap()
        .contains("Merged edit on top of Alice"));
    assert!(app.drafts.get_note("edit:me|page-1:p:{b1}").is_none());

    srv.abort();
}

#[test]
fn test_focus_gained_moved_page_reconnect_and_live_draft_preservation() {
    let (mut app, mut ui, mut focus_rx, mut nudge_rx) = make_test_app_with_cached_summary();

    app.storage
        .upsert_onenote_sections(&[OnenoteSection {
            section_key: "me|sec-2".to_string(),
            id: "sec-2".to_string(),
            scope: OnenoteScope::Me,
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("Engineering".to_string()),
            parent_section_group_id: None,
            parent_section_group_name: None,
            display_name: "MovedTarget".to_string(),
            color: None,
            is_default: false,
            pages_url: None,
            created_at: None,
            last_modified_at: Some("2026-11-18T10:00:00Z".to_string()),
            web_url: None,
        }])
        .unwrap();

    let p1_summary = app.storage.get_onenote_page_summary("me|page-1").unwrap().unwrap();
    let p1_detail = parser::parse_onenote_page_xhtml(
        p1_summary.clone(),
        r#"<html><head><title>Initial Spec</title></head><body><div id="div:1"><p id="p:1">Alpha paragraph</p></div></body></html>"#,
    );
    app.storage.upsert_onenote_page_detail(&p1_detail).unwrap();
    app.load_cached_initial();
    let _ = focus_rx.borrow_and_update();
    while nudge_rx.try_recv().is_ok() {}

    // 1. Terminal focus gained unconditionally requests HydratePage even when detail is cached.
    assert!(app.page_details.contains_key("me|page-1"));
    app.on_terminal_focus_gained();
    assert_eq!(
        nudge_rx.try_recv().ok(),
        Some(SyncCommand::HydratePage {
            page_key: "me|page-1".to_string()
        })
    );

    // 2. Open EditBlock form, type unsaved text, then receive remote OnenotePageDetailUpdated:
    //    unsaved form.body & base_content must stay untouched while drift error notice appears.
    crate::ui::action::dispatch(
        crate::ui::action::Action::OpenOnenoteEditBlock,
        &mut app,
        &mut ui,
    );
    if let Some(Overlay::OnenoteForm(ref mut form)) = ui.overlays.last_mut() {
        form.body = "Alpha paragraph LOCAL_UNSAVED".to_string();
    }
    let remote_detail = parser::parse_onenote_page_xhtml(
        p1_summary.clone(),
        r#"<html><head><title>Initial Spec</title></head><body><div id="div:1"><p id="p:1">Remote edit while editing</p></div></body></html>"#,
    );
    app.handle_core_event(
        &mut ui,
        CoreEvent::OnenotePageDetailUpdated(Box::new(remote_detail)),
    );
    match ui.overlays.last() {
        Some(Overlay::OnenoteForm(form)) => {
            assert_eq!(form.body, "Alpha paragraph LOCAL_UNSAVED");
            assert!(form
                .error
                .as_deref()
                .is_some_and(|e| e.contains("Remote block changed")));
        }
        other => panic!("expected OnenoteForm overlay, got {other:?}"),
    }
    ui.overlays.pop();

    // 3. Page moved from sec-1 -> sec-2 in SQLite, then sec-1 OnenotePagesUpdated arrives without me|page-1:
    //    AppState reconnects active_section_key to me|sec-2 and preserves me|page-1 open!
    let mut moved_summary = p1_summary;
    moved_summary.section_id = "sec-2".to_string();
    moved_summary.section_name = Some("MovedTarget".to_string());
    app.storage.upsert_onenote_pages(&[moved_summary]).unwrap();
    let _ = focus_rx.borrow_and_update();

    app.handle_core_event(
        &mut ui,
        CoreEvent::OnenotePagesUpdated {
            section_key: "me|sec-1".to_string(),
            pages: Vec::new(),
            notify_candidate: None,
        },
    );
    assert_eq!(app.active_section_key.as_deref(), Some("me|sec-2"));
    assert_eq!(app.active_page_key.as_deref(), Some("me|page-1"));
    assert!(app.page_details.contains_key("me|page-1"));
    assert_eq!(focus_rx.borrow_and_update().section_key.as_deref(), Some("me|sec-2"));
}

#[test]
fn apply_ui_settings_keeps_auto_image_protocol_until_the_user_picks_one() {
    let mut config = AppConfig {
        image_protocol: ImageProtocolPreference::Auto,
        notify: true,
        ..AppConfig::default()
    };
    let mut ui = UiState::from_config(&config);
    ui.graphics_caps.protocol = TerminalImageProtocol::Sixel;
    ui.notify = false;

    // An unrelated setting change saves the UI flags but leaves `auto` alone.
    apply_ui_settings(&mut config, &ui);
    assert!(!config.notify);
    assert_eq!(config.image_protocol, ImageProtocolPreference::Auto);

    // Cycling the protocol in the UI is an explicit choice and is stored.
    ui.image_protocol_user_chosen = true;
    apply_ui_settings(&mut config, &ui);
    assert_eq!(config.image_protocol, ImageProtocolPreference::Sixel);

    // An explicit stored preference keeps tracking the protocol in use.
    ui.image_protocol_user_chosen = false;
    ui.graphics_caps.protocol = TerminalImageProtocol::Kitty;
    apply_ui_settings(&mut config, &ui);
    assert_eq!(config.image_protocol, ImageProtocolPreference::Kitty);
}

#[tokio::test]
async fn raw_image_cache_hit_skips_refetch_on_zoom_and_protocol_cycle() {
    use axum::{extract::State, http::StatusCode, routing::get, Router};
    use image::{DynamicImage, ImageBuffer, ImageFormat, Rgba, RgbaImage};
    use std::io::Cursor;
    use std::sync::atomic::{AtomicUsize, Ordering};

    // 1. Verify LRU count cap, byte budget cap, oversized skip, and page eviction
    let (mut lru_app, _, _, _) = make_test_app_with_cached_summary();
    for i in 0..(MAX_RAW_IMAGE_CACHE_ENTRIES + 3) {
        lru_app.insert_raw_image_cache(
            format!("https://example.com/img-{i}.png"),
            Arc::from(vec![i as u8; 1024]),
        );
    }
    assert_eq!(lru_app.image_raw_cache.len(), MAX_RAW_IMAGE_CACHE_ENTRIES);
    assert_eq!(lru_app.image_raw_bytes, MAX_RAW_IMAGE_CACHE_ENTRIES * 1024);
    assert!(!lru_app.image_raw_cache.contains_key("https://example.com/img-0.png"));

    let six_mib = 6 * 1024 * 1024;
    for i in 0..4 {
        lru_app.insert_raw_image_cache(
            format!("https://example.com/big-{i}.png"),
            Arc::from(vec![1u8; six_mib]),
        );
    }
    assert!(lru_app.image_raw_bytes <= MAX_RAW_IMAGE_CACHE_BYTES);

    // Oversized (> 16 MiB) entry is ignored without corrupting cache budget
    lru_app.insert_raw_image_cache(
        "https://example.com/huge.png".to_string(),
        Arc::from(vec![0u8; MAX_RAW_IMAGE_CACHE_BYTES + 1]),
    );
    assert!(!lru_app.image_raw_cache.contains_key("https://example.com/huge.png"));
    assert!(lru_app.image_raw_bytes <= MAX_RAW_IMAGE_CACHE_BYTES);
    lru_app.clear_raw_image_cache();
    assert_eq!(lru_app.image_raw_cache.len(), 0);
    assert_eq!(lru_app.image_raw_bytes, 0);

    // 2. Live mock server verifying zoom (+/- /0) and protocol cycle (p) hit raw_image_cache with 0 refetches
    let img: RgbaImage = ImageBuffer::from_pixel(32, 32, Rgba([40, 120, 220, 255]));
    let mut png_buf = Cursor::new(Vec::new());
    DynamicImage::ImageRgba8(img)
        .write_to(&mut png_buf, ImageFormat::Png)
        .unwrap();
    let png_bytes = Arc::new(png_buf.into_inner());
    let fetch_hits = Arc::new(AtomicUsize::new(0));

    let app_router = Router::new()
        .route(
            "/res/pic.png",
            get(
                |State((hits, body)): State<(Arc<AtomicUsize>, Arc<Vec<u8>>)>| async move {
                    hits.fetch_add(1, Ordering::SeqCst);
                    (StatusCode::OK, [("content-type", "image/png")], (*body).clone())
                },
            ),
        )
        .with_state((Arc::clone(&fetch_hits), Arc::clone(&png_bytes)));

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, app_router).await;
    });

    let cfg = AppConfig {
        graph_base_url: format!("http://{addr}/v1.0"),
        ..AppConfig::default()
    };
    let tokens = StoredTokens {
        access_token: "raw-cache-token".to_string(),
        refresh_token: None,
        expires_at: chrono::Utc::now().timestamp() + 3600,
        client_id: "cid".to_string(),
        tenant_id: "tid".to_string(),
        user_id: Some("user-1".to_string()),
        user_display_name: Some("Tester".to_string()),
        user_email: None,
        auth_mode: "access_token".to_string(),
    };
    let auth = AuthManager::from_tokens(cfg.clone(), tokens);
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let storage = Storage::open_in_memory().unwrap();
    let (focus_tx, _focus_rx) = watch::channel(FocusTarget::default());
    let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
    let (ui_tx, mut ui_rx) = mpsc::unbounded_channel::<UiMsg>();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);

    let mut app = AppState::new(cfg.clone(), auth, graph, storage, sync_ctrl, ui_tx);
    let mut ui = UiState::from_config(&cfg);
    ui.term_width = 120;
    ui.term_height = 34;
    ui.graphics_caps = TerminalGraphicsCaps {
        protocol: TerminalImageProtocol::Halfblock,
        cell_px: (10, 20),
        cell_px_exact: true,
    };

    let resource_url = format!("http://{addr}/res/pic.png");
    let summary = OnenotePageSummary {
        page_key: "me|page-raw".to_string(),
        id: "page-raw".to_string(),
        scope: OnenoteScope::Me,
        section_id: "sec-1".to_string(),
        section_name: Some("Specs".to_string()),
        notebook_id: Some("nb-1".to_string()),
        notebook_name: Some("Eng".to_string()),
        title: "Raw Cache Page".to_string(),
        level: 0,
        order: 0,
        created_at: None,
        last_modified_at: None,
        content_url: None,
        web_url: None,
        preview_text: None,
        has_todo: false,
    };
    let detail = OnenotePageDetail {
        summary,
        raw_xhtml: String::new(),
        body_markdown: String::new(),
        body_plain: String::new(),
        blocks: Vec::new(),
        resources: vec![crate::models::OnenoteResource {
            element_id: Some("img:1".to_string()),
            resource_url: resource_url.clone(),
            fullres_url: None,
            mime_type: "image/png".to_string(),
            file_name: Some("pic.png".to_string()),
            is_image: true,
            alt_text: None,
        }],
        primary_div_id: None,
        synced_at: None,
    };
    app.active_page_key = Some("me|page-raw".to_string());
    app.page_details.insert("me|page-raw".to_string(), detail);

    // Open preview modal -> initial fetch (hits == 1)
    crate::ui::action::dispatch(crate::ui::action::Action::OpenImagePreview, &mut app, &mut ui);
    let mut saw_bytes = false;
    let mut saw_loaded = false;
    while !saw_bytes || !saw_loaded {
        let msg = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
            .await
            .unwrap()
            .unwrap();
        match &msg {
            UiMsg::ImageBytesFetched { .. } => saw_bytes = true,
            UiMsg::ImageLoaded { .. } => saw_loaded = true,
            other => panic!("unexpected msg during initial load: {other:?}"),
        }
        app.handle_ui_msg(&mut ui, msg);
    }
    assert_eq!(fetch_hits.load(Ordering::SeqCst), 1);
    assert!(app.image_raw_cache.contains_key(&resource_url));

    // Zoom in (`+`) -> renders from raw_image_cache with 0 additional HTTP fetches
    crate::ui::action::dispatch(crate::ui::action::Action::Input('+'), &mut app, &mut ui);
    let msg_zoom = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(msg_zoom, UiMsg::ImageLoaded { .. }));
    app.handle_ui_msg(&mut ui, msg_zoom);
    assert_eq!(fetch_hits.load(Ordering::SeqCst), 1);

    // Cycle protocol (`p`: Halfblock -> Kitty) -> renders from raw_image_cache with 0 additional HTTP fetches
    crate::ui::action::dispatch(crate::ui::action::Action::Input('p'), &mut app, &mut ui);
    let msg_proto = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(msg_proto, UiMsg::ImageLoaded { .. }));
    app.handle_ui_msg(&mut ui, msg_proto);
    assert_eq!(fetch_hits.load(Ordering::SeqCst), 1);

    // Resize / cell_px change -> renders from raw_image_cache with 0 additional HTTP fetches
    ui.term_width = 160;
    ui.term_height = 44;
    ui.graphics_caps.cell_px = (12, 24);
    crate::ui::action::dispatch(crate::ui::action::Action::LoadPreviewImage, &mut app, &mut ui);
    let msg_resize = tokio::time::timeout(std::time::Duration::from_secs(5), ui_rx.recv())
        .await
        .unwrap()
        .unwrap();
    assert!(matches!(msg_resize, UiMsg::ImageLoaded { .. }));
    app.handle_ui_msg(&mut ui, msg_resize);
    assert_eq!(fetch_hits.load(Ordering::SeqCst), 1);

    // Evicting page removes both rendered and raw byte cache entries for that page
    app.evict_image_cache_for_page("me|page-raw");
    assert!(!app.image_raw_cache.contains_key(&resource_url));
    assert_eq!(app.image_raw_bytes, 0);

    srv.abort();
}

#[test]
fn page_created_renders_preview_immediately_without_loading_blank() {
    let storage = Storage::open_in_memory().unwrap();
    let cfg = AppConfig::default();
    let auth = crate::auth::AuthManager::from_tokens(
        cfg.clone(),
        crate::auth::StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("u1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".to_string(),
        },
    );
    let graph = GraphClient::new(cfg.clone(), auth.clone()).unwrap();
    let (focus_tx, _focus_rx) = tokio::sync::watch::channel(FocusTarget::default());
    let (nudge_tx, mut nudge_rx) = tokio::sync::mpsc::unbounded_channel();
    let sync_ctrl = SyncController::from_channels(focus_tx, nudge_tx);
    let (ui_tx, _ui_rx) = tokio::sync::mpsc::unbounded_channel();
    let mut app = AppState::new(cfg, auth, graph, storage, sync_ctrl, ui_tx);
    app.active_section_key = Some("me|sec-1".to_string());
    let mut ui = UiState::new();

    let page = OnenotePageSummary {
        page_key: "me|p-new".to_string(),
        id: "p-new".to_string(),
        scope: crate::models::OnenoteScope::Me,
        section_id: "sec-1".to_string(),
        section_name: Some("Notes".to_string()),
        notebook_id: Some("nb-1".to_string()),
        notebook_name: Some("Personal".to_string()),
        title: "Instant Page".to_string(),
        level: 0,
        order: 0,
        created_at: None,
        last_modified_at: None,
        content_url: None,
        web_url: None,
        preview_text: None,
        has_todo: false,
    };
    let outgoing = OutgoingOnenotePage {
        title: "Instant Page".to_string(),
        markdown_body: "Hello instant preview".to_string(),
        inline_images: Vec::new(),
        level: None,
    };
    let preview_xhtml = crate::parser::onenote::compile_new_page_xhtml(&outgoing);
    let preview_detail =
        crate::parser::onenote::parse_onenote_page_xhtml(page.clone(), &preview_xhtml);

    let task_id = ui.tasks.start("Creating page");
    app.handle_ui_msg(
        &mut ui,
        UiMsg::PageCreated {
            task_id,
            section_key: "me|sec-1".to_string(),
            page,
            preview_detail: Box::new(preview_detail),
        },
    );

    assert_eq!(app.active_page_key.as_deref(), Some("me|p-new"));
    assert!(
        !app.loading_onenote_detail,
        "newly created page must display immediately instead of showing loading blank"
    );
    let detail = app
        .active_page_detail()
        .expect("preview detail must be present immediately");
    assert!(detail.body_plain.contains("Hello instant preview"));

    // Background HydratePage must still be queued to fetch authoritative Graph element IDs.
    let mut saw_hydrate = false;
    while let Ok(cmd) = nudge_rx.try_recv() {
        if matches!(cmd, SyncCommand::HydratePage { ref page_key } if page_key == "me|p-new") {
            saw_hydrate = true;
        }
    }
    assert!(saw_hydrate, "background HydratePage must still be requested");
}
