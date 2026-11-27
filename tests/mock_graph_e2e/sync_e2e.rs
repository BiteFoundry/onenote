#![allow(unused_imports)]

use std::sync::{Arc, Mutex};

use axum::{
    body::Bytes,
    extract::{Path as AxumPath, State},
    http::{HeaderMap, StatusCode},
    routing::get,
    Json, Router,
};
use serde_json::{json, Value};
use tokio::net::TcpListener;

use onenote_cli::{
    auth::{AuthManager, StoredTokens},
    cli,
    config::AppConfig,
    graph::GraphClient,
    mcp,
    models::OnenoteScope,
    serve::{self, AppState as ServeState},
    storage::Storage,
};

use crate::server::*;

#[tokio::test]
async fn e2e_bidirectional_sync_fixes_s1_s3_s4_s6_and_section4() {
    use std::collections::HashMap;
    use std::sync::atomic::{AtomicBool, Ordering};

    #[derive(Clone)]
    struct SyncFixState {
        fail_group_notebooks: Arc<AtomicBool>,
        notebooks: Arc<Mutex<Vec<Value>>>,
        joined_teams: Arc<Mutex<Vec<Value>>>,
        group_notebooks: Arc<Mutex<HashMap<String, Vec<Value>>>>,
        group_sections: Arc<Mutex<HashMap<String, Vec<Value>>>>,
        section_groups: Arc<Mutex<Vec<Value>>>,
        sections: Arc<Mutex<Vec<Value>>>,
        pages: Arc<Mutex<HashMap<String, Value>>>,
        page_xhtml: Arc<Mutex<HashMap<String, String>>>,
        patch_count: Arc<Mutex<usize>>,
    }

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let mut pages_map = HashMap::new();
    pages_map.insert(
        "p-1".to_string(),
        json!({
            "id": "p-1",
            "title": "Spec v1",
            "level": 0,
            "order": 0,
            "lastModifiedDateTime": "2026-11-18T10:00:00Z",
            "parentSection": { "id": "sec-1", "displayName": "Specs" }
        }),
    );
    pages_map.insert(
        "p-2".to_string(),
        json!({
            "id": "p-2",
            "title": "Ghost Candidate",
            "level": 0,
            "order": 1,
            "lastModifiedDateTime": "2026-11-18T10:01:00Z",
            "parentSection": { "id": "sec-1", "displayName": "Specs" }
        }),
    );
    pages_map.insert(
        "p-team-1".to_string(),
        json!({
            "id": "p-team-1",
            "title": "Team Runbook",
            "level": 0,
            "order": 0,
            "lastModifiedDateTime": "2026-11-18T10:00:00Z",
            "parentSection": { "id": "sec-team-1", "displayName": "OnCall" }
        }),
    );

    let mut xhtml_map = HashMap::new();
    xhtml_map.insert(
        "p-1".to_string(),
        r#"<!DOCTYPE html><html><head><title>Spec v1</title></head><body><div id="div:{root}"><p id="p:{b1}">Alpha initial body text</p><p id="p:{todo1}" data-tag="to-do">Ship sync audit</p></div></body></html>"#
            .to_string(),
    );
    xhtml_map.insert(
        "p-2".to_string(),
        r#"<!DOCTYPE html><html><head><title>Ghost Candidate</title></head><body><div id="div:{r2}"><p id="p:{b2}">GhostBodyTokenXYZ</p></div></body></html>"#
            .to_string(),
    );
    xhtml_map.insert(
        "p-team-1".to_string(),
        r#"<!DOCTYPE html><html><head><title>Team Runbook</title></head><body><div id="div:{tr}"><p id="p:{tb1}">Team incident steps</p></div></body></html>"#
            .to_string(),
    );

    let mut group_nbs = HashMap::new();
    group_nbs.insert(
        "team-1".to_string(),
        vec![json!({
            "id": "nb-team-1",
            "displayName": "Team Runbooks",
            "isDefault": false,
            "isShared": true,
            "lastModifiedDateTime": "2026-11-18T10:00:00Z"
        })],
    );
    let mut group_secs = HashMap::new();
    group_secs.insert(
        "team-1".to_string(),
        vec![json!({
            "id": "sec-team-1",
            "displayName": "OnCall",
            "isDefault": false,
            "parentNotebook": { "id": "nb-team-1", "displayName": "Team Runbooks" },
            "lastModifiedDateTime": "2026-11-18T10:00:00Z"
        })],
    );

    let st = SyncFixState {
        fail_group_notebooks: Arc::new(AtomicBool::new(false)),
        notebooks: Arc::new(Mutex::new(vec![
            json!({
                "id": "nb-1",
                "displayName": "Engineering",
                "isDefault": true,
                "isShared": false,
                "lastModifiedDateTime": "2026-11-18T10:00:00Z"
            }),
            json!({
                "id": "nb-2",
                "displayName": "ArchiveNB",
                "isDefault": false,
                "isShared": false,
                "lastModifiedDateTime": "2026-11-18T10:00:00Z"
            }),
        ])),
        joined_teams: Arc::new(Mutex::new(vec![json!({
            "id": "team-1",
            "displayName": "Platform Team"
        })])),
        group_notebooks: Arc::new(Mutex::new(group_nbs)),
        group_sections: Arc::new(Mutex::new(group_secs)),
        section_groups: Arc::new(Mutex::new(vec![json!({
            "id": "sg-1",
            "displayName": "Q4 Group",
            "parentNotebook": { "id": "nb-1", "displayName": "Engineering" }
        })])),
        sections: Arc::new(Mutex::new(vec![
            json!({
                "id": "sec-1",
                "displayName": "Specs",
                "isDefault": true,
                "parentNotebook": { "id": "nb-1", "displayName": "Engineering" },
                "parentSectionGroup": { "id": "sg-1", "displayName": "Q4 Group" },
                "lastModifiedDateTime": "2026-11-18T10:00:00Z"
            }),
            json!({
                "id": "sec-2",
                "displayName": "DeprecatedSec",
                "isDefault": false,
                "parentNotebook": { "id": "nb-2", "displayName": "ArchiveNB" },
                "lastModifiedDateTime": "2026-11-18T10:00:00Z"
            }),
            json!({
                "id": "sec-3",
                "displayName": "TargetSec",
                "isDefault": false,
                "parentNotebook": { "id": "nb-1", "displayName": "Engineering" },
                "lastModifiedDateTime": "2026-11-18T10:00:00Z"
            }),
        ])),
        pages: Arc::new(Mutex::new(pages_map)),
        page_xhtml: Arc::new(Mutex::new(xhtml_map)),
        patch_count: Arc::new(Mutex::new(0)),
    };

    let app = Router::new()
        .route(
            "/v1.0/me/joinedTeams",
            get(|State(s): State<SyncFixState>| async move {
                Json(json!({ "value": s.joined_teams.lock().unwrap().clone() }))
            }),
        )
        .route(
            "/v1.0/me/onenote/notebooks",
            get(|State(s): State<SyncFixState>| async move {
                Json(json!({ "value": s.notebooks.lock().unwrap().clone() }))
            }),
        )
        .route(
            "/v1.0/groups/{gid}/onenote/notebooks",
            get(
                |State(s): State<SyncFixState>, AxumPath(gid): AxumPath<String>| async move {
                    if s.fail_group_notebooks.load(Ordering::SeqCst) {
                        return (
                            StatusCode::INTERNAL_SERVER_ERROR,
                            Json(json!({ "error": { "code": "InternalServerError" } })),
                        );
                    }
                    let items = s
                        .group_notebooks
                        .lock()
                        .unwrap()
                        .get(&gid)
                        .cloned()
                        .unwrap_or_default();
                    (StatusCode::OK, Json(json!({ "value": items })))
                },
            ),
        )
        .route(
            "/v1.0/sites/{sid}/onenote/notebooks",
            axum::routing::post(
                |AxumPath(sid): AxumPath<String>, Json(body): Json<Value>| async move {
                    let nb = json!({
                        "id": format!("nb-site-{sid}"),
                        "displayName": body["displayName"].as_str().unwrap_or("SiteNB"),
                        "isDefault": false,
                        "isShared": true,
                        "lastModifiedDateTime": "2026-11-19T12:00:00Z"
                    });
                    (StatusCode::CREATED, Json(nb))
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/sectionGroups",
            get(|State(s): State<SyncFixState>| async move {
                Json(json!({ "value": s.section_groups.lock().unwrap().clone() }))
            }),
        )
        .route(
            "/v1.0/groups/{gid}/onenote/sectionGroups",
            get(|| async { Json(json!({ "value": [] })) }),
        )
        .route(
            "/v1.0/me/onenote/sections",
            get(|State(s): State<SyncFixState>| async move {
                Json(json!({ "value": s.sections.lock().unwrap().clone() }))
            }),
        )
        .route(
            "/v1.0/groups/{gid}/onenote/sections",
            get(
                |State(s): State<SyncFixState>, AxumPath(gid): AxumPath<String>| async move {
                    let items = s
                        .group_sections
                        .lock()
                        .unwrap()
                        .get(&gid)
                        .cloned()
                        .unwrap_or_default();
                    Json(json!({ "value": items }))
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/sections/{sec_id}/pages",
            get(
                |State(s): State<SyncFixState>, AxumPath(sec_id): AxumPath<String>| async move {
                    let exists = s
                        .sections
                        .lock()
                        .unwrap()
                        .iter()
                        .any(|sec| sec["id"].as_str() == Some(sec_id.as_str()));
                    if !exists {
                        return (
                            StatusCode::NOT_FOUND,
                            Json(json!({ "error": { "code": "ResourceNotFound", "message": "Section not found" } })),
                        );
                    }
                    let items: Vec<Value> = s
                        .pages
                        .lock()
                        .unwrap()
                        .values()
                        .filter(|p| p["parentSection"]["id"].as_str() == Some(sec_id.as_str()))
                        .cloned()
                        .collect();
                    (StatusCode::OK, Json(json!({ "value": items })))
                },
            ),
        )
        .route(
            "/v1.0/groups/{gid}/onenote/sections/{sec_id}/pages",
            get(
                |State(s): State<SyncFixState>, AxumPath((_gid, sec_id)): AxumPath<(String, String)>| async move {
                    let items: Vec<Value> = s
                        .pages
                        .lock()
                        .unwrap()
                        .values()
                        .filter(|p| p["parentSection"]["id"].as_str() == Some(sec_id.as_str()))
                        .cloned()
                        .collect();
                    Json(json!({ "value": items }))
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/pages/{pid}",
            get(
                |State(s): State<SyncFixState>, AxumPath(pid): AxumPath<String>| async move {
                    match s.pages.lock().unwrap().get(&pid).cloned() {
                        Some(p) => (StatusCode::OK, Json(p)),
                        None => (
                            StatusCode::NOT_FOUND,
                            Json(json!({ "error": { "code": "ResourceNotFound", "message": "Page not found" } })),
                        ),
                    }
                },
            ),
        )
        .route(
            "/v1.0/me/onenote/pages/{pid}/content",
            get(
                |State(s): State<SyncFixState>, AxumPath(pid): AxumPath<String>| async move {
                    match s.page_xhtml.lock().unwrap().get(&pid).cloned() {
                        Some(x) => (StatusCode::OK, [("content-type", "text/html")], x),
                        None => (
                            StatusCode::NOT_FOUND,
                            [("content-type", "application/json")],
                            r#"{"error":{"code":"ResourceNotFound","message":"Page not found"}}"#.to_string(),
                        ),
                    }
                },
            )
            .patch(
                |State(s): State<SyncFixState>, AxumPath(pid): AxumPath<String>, body: Bytes| async move {
                    *s.patch_count.lock().unwrap() += 1;
                    let cmds: Vec<Value> = serde_json::from_slice(&body).unwrap_or_default();
                    for cmd in cmds {
                        let target = cmd["target"].as_str().unwrap_or("");
                        let content = cmd["content"].as_str().unwrap_or("");
                        if target == "title" {
                            if let Some(p) = s.pages.lock().unwrap().get_mut(&pid) {
                                let old_t = p["title"].as_str().unwrap_or("").to_string();
                                p["title"] = json!(content);
                                p["lastModifiedDateTime"] = json!("2026-11-20T12:11:00Z");
                                if let Some(x) = s.page_xhtml.lock().unwrap().get_mut(&pid) {
                                    *x = x.replace(&format!("<title>{old_t}</title>"), &format!("<title>{content}</title>"));
                                }
                            }
                        } else if target == "p:{b1}" {
                            if let Some(x) = s.page_xhtml.lock().unwrap().get_mut(&pid) {
                                if let Some(start) = x.find(r#"<p id="p:{b1}""#) {
                                    if let Some(rel_end) = x[start..].find("</p>") {
                                        let repl = content.replacen("<p>", r#"<p id="p:{b1}">"#, 1);
                                        x.replace_range(start..start + rel_end + 4, &repl);
                                    }
                                }
                            }
                            if let Some(p) = s.pages.lock().unwrap().get_mut(&pid) {
                                p["lastModifiedDateTime"] = json!("2026-11-20T12:12:00Z");
                            }
                        } else if target == "p:{todo1}" {
                            if let Some(x) = s.page_xhtml.lock().unwrap().get_mut(&pid) {
                                if let Some(start) = x.find(r#"<p id="p:{todo1}""#) {
                                    if let Some(rel_end) = x[start..].find("</p>") {
                                        x.replace_range(start..start + rel_end + 4, content);
                                    }
                                }
                            }
                            if let Some(p) = s.pages.lock().unwrap().get_mut(&pid) {
                                p["lastModifiedDateTime"] = json!("2026-11-20T12:13:00Z");
                            }
                        }
                    }
                    StatusCode::NO_CONTENT
                },
            ),
        )
        .with_state(st.clone());

    let server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let (cfg, graph, storage) = make_test_runtime(base_url, dir.path());

    // Initial bootstrap + read p-1 and p-2
    onenote_cli::sync::bootstrap_onenote(&graph, &storage)
        .await
        .unwrap();
    let _ = cli::load_onenote_page_detail(&storage, &graph, "me|p-1", false)
        .await
        .unwrap();
    let _ = cli::load_onenote_page_detail(&storage, &graph, "me|p-2", false)
        .await
        .unwrap();

    // S1: Remote edit title + lastModifiedDateTime + body -> direct read updates all fields + FTS5
    {
        let mut pm = st.pages.lock().unwrap();
        let p1 = pm.get_mut("p-1").unwrap();
        p1["title"] = json!("Spec v2 RemoteTitle");
        p1["lastModifiedDateTime"] = json!("2026-11-19T15:00:00Z");
        st.page_xhtml.lock().unwrap().insert(
            "p-1".to_string(),
            r#"<!DOCTYPE html><html><head><title>Spec v2 RemoteTitle</title></head><body><div id="div:{root}"><p id="p:{b1}">Beta remote content edited natively</p><p id="p:{todo1}" data-tag="to-do">Ship sync audit</p></div></body></html>"#
                .to_string(),
        );
    }
    let s1 = cli::load_onenote_page_detail(&storage, &graph, "me|p-1", false)
        .await
        .unwrap();
    assert_eq!(s1.summary.title, "Spec v2 RemoteTitle");
    assert_eq!(
        s1.summary.last_modified_at.as_deref(),
        Some("2026-11-19T15:00:00Z")
    );
    assert!(s1
        .summary
        .preview_text
        .as_deref()
        .is_some_and(|t| t.contains("Beta remote content")));
    assert_eq!(
        storage
            .search_onenote_pages("Beta remote content", None, 10)
            .unwrap()
            .len(),
        1
    );

    // S2: Local title update returns live title + timestamp and preserves FTS5 hits after sync
    let s2 = cli::update_onenote_page_helper(
        &storage,
        &graph,
        "me|p-1",
        Some("Spec v3 LocalRename"),
        None,
        None,
    )
    .await
    .unwrap();
    assert_eq!(s2.summary.title, "Spec v3 LocalRename");
    assert_eq!(
        s2.summary.last_modified_at.as_deref(),
        Some("2026-11-20T12:11:00Z")
    );
    onenote_cli::sync::bootstrap_onenote(&graph, &storage)
        .await
        .unwrap();
    assert_eq!(
        storage
            .search_onenote_pages("Beta remote content", None, 10)
            .unwrap()
            .len(),
        1,
        "FTS5 body hits must survive sync after local update"
    );

    // S3: Same-block remote edit blocks stale overwrite; different-block edit succeeds!
    let _ = cli::load_onenote_page_detail(&storage, &graph, "me|p-1", false)
        .await
        .unwrap();
    {
        let mut pm = st.pages.lock().unwrap();
        pm.get_mut("p-1").unwrap()["lastModifiedDateTime"] = json!("2026-11-20T09:00:00Z");
        st.page_xhtml.lock().unwrap().insert(
            "p-1".to_string(),
            r#"<!DOCTYPE html><html><head><title>Spec v3 LocalRename</title></head><body><div id="div:{root}"><p id="p:{b1}">CRITICAL REMOTE EDIT BY ALICE</p><p id="p:{todo1}" data-tag="to-do">Ship sync audit</p></div></body></html>"#
                .to_string(),
        );
    }
    let patches_before_s3 = *st.patch_count.lock().unwrap();
    let stale_err = cli::update_onenote_page_helper(
        &storage,
        &graph,
        "me|p-1",
        None,
        Some("Local stale edit"),
        Some("p:{b1}"),
    )
    .await
    .unwrap_err();
    assert!(stale_err.to_string().to_lowercase().contains("conflict"));
    assert_eq!(*st.patch_count.lock().unwrap(), patches_before_s3);
    assert!(st
        .page_xhtml
        .lock()
        .unwrap()
        .get("p-1")
        .unwrap()
        .contains("CRITICAL REMOTE EDIT BY ALICE"));

    // W1 regression: retrying the same implicit-baseline update must keep failing with Conflict
    // and never overwrite remote edits until the caller explicitly re-reads (or passes --force).
    let stale_err_retry = cli::update_onenote_page_helper_with_guard(
        &storage,
        &graph,
        "me|p-1",
        None,
        Some("Local stale edit"),
        Some("p:{b1}"),
        None,
        false,
    )
    .await
    .unwrap_err();
    assert!(stale_err_retry.to_string().to_lowercase().contains("conflict"));
    assert_eq!(*st.patch_count.lock().unwrap(), patches_before_s3);
    assert!(st
        .page_xhtml
        .lock()
        .unwrap()
        .get("p-1")
        .unwrap()
        .contains("CRITICAL REMOTE EDIT BY ALICE"));

    // Same implicit-baseline protection for `check` (`change_onenote_todo_helper_with_guard`):
    // edit p:{todo1} remotely while keeping the earlier cached baseline, verify 2x Conflict + 0 PATCH.
    let saved_xhtml_before_todo_conflict = st.page_xhtml.lock().unwrap().get("p-1").unwrap().clone();
    st.page_xhtml.lock().unwrap().insert(
        "p-1".to_string(),
        r#"<!DOCTYPE html><html><head><title>Spec v3 LocalRename</title></head><body><div id="div:{root}"><p id="p:{b1}">CRITICAL REMOTE EDIT BY ALICE</p><p id="p:{todo1}" data-tag="to-do">Ship sync audit (REMOTE EDIT)</p></div></body></html>"#
            .to_string(),
    );
    for attempt in 1..=2 {
        let todo_conflict = cli::change_onenote_todo_helper_with_guard(
            &storage,
            &graph,
            "me|p-1",
            Some("p:{todo1}"),
            None,
            Some(true),
            None,
            false,
        )
        .await
        .unwrap_err();
        assert!(
            todo_conflict.to_string().to_lowercase().contains("conflict"),
            "attempt {attempt}: {todo_conflict}"
        );
        assert_eq!(
            *st.patch_count.lock().unwrap(),
            patches_before_s3,
            "attempt {attempt} must not send PATCH"
        );
    }
    // Restore remote p:{todo1} to the original text for the control group below, then verify
    // re-reading allows the update on p:{b1} to proceed after the control group.
    st.page_xhtml
        .lock()
        .unwrap()
        .insert("p-1".to_string(), saved_xhtml_before_todo_conflict);

    // Different-paragraph concurrent edit (toggle p:{todo1} while p:{b1} changed remotely) MUST succeed!
    let (after_todo, _, completed) = cli::toggle_onenote_todo_helper_with_guard(
        &storage,
        &graph,
        "me|p-1",
        Some("p:{todo1}"),
        None,
        Some("- [ ] Ship sync audit"),
        false,
    )
    .await
    .unwrap();
    assert!(completed);
    assert!(after_todo.raw_xhtml.contains("CRITICAL REMOTE EDIT BY ALICE"));
    assert!(after_todo.raw_xhtml.contains("to-do:completed"));

    // After an explicit re-read refreshes the cache baseline, the same update succeeds.
    // Re-stage remote change on p:{b1}, confirm 2x Conflict, then re-read and succeed.
    st.page_xhtml.lock().unwrap().insert(
        "p-1".to_string(),
        r#"<!DOCTYPE html><html><head><title>Spec v3 LocalRename</title></head><body><div id="div:{root}"><p id="p:{b1}">REMOTE V2 BY ALICE</p><p id="p:{todo1}" data-tag="to-do:completed">Ship sync audit</p></div></body></html>"#
            .to_string(),
    );
    let patches_before_reread_check = *st.patch_count.lock().unwrap();
    for attempt in 1..=2 {
        let err = cli::update_onenote_page_helper_with_guard(
            &storage,
            &graph,
            "me|p-1",
            None,
            Some("Local edit after reread"),
            Some("p:{b1}"),
            None,
            false,
        )
        .await
        .unwrap_err();
        assert!(
            err.to_string().to_lowercase().contains("conflict"),
            "attempt {attempt}: {err}"
        );
        assert_eq!(*st.patch_count.lock().unwrap(), patches_before_reread_check);
    }
    let _ = cli::load_onenote_page_detail(&storage, &graph, "me|p-1", false)
        .await
        .unwrap();
    let after_reread = cli::update_onenote_page_helper_with_guard(
        &storage,
        &graph,
        "me|p-1",
        None,
        Some("CRITICAL REMOTE EDIT BY ALICE"),
        Some("p:{b1}"),
        None,
        false,
    )
    .await
    .unwrap();
    assert_eq!(
        *st.patch_count.lock().unwrap(),
        patches_before_reread_check + 1
    );
    assert!(after_reread
        .raw_xhtml
        .contains("CRITICAL REMOTE EDIT BY ALICE"));

    // S4: Remote deletions pruned by --refresh on sections, notebooks, pages, and 404 read
    st.notebooks
        .lock()
        .unwrap()
        .retain(|n| n["id"].as_str() != Some("nb-2"));
    st.sections
        .lock()
        .unwrap()
        .retain(|s| s["id"].as_str() != Some("sec-2"));
    st.pages.lock().unwrap().remove("p-2");
    st.page_xhtml.lock().unwrap().remove("p-2");

    let (_, secs_refreshed) = cli::load_onenote_sections(&storage, &graph, None, true, false)
        .await
        .unwrap();
    assert!(!secs_refreshed.iter().any(|s| s.id == "sec-2"));

    let nbs_refreshed = cli::load_onenote_notebooks(&storage, &graph, false, true, false)
        .await
        .unwrap();
    assert!(!nbs_refreshed.iter().any(|n| n.id == "nb-2"));
    assert!(!storage
        .list_onenote_notebooks()
        .unwrap()
        .iter()
        .any(|n| n.id == "nb-2"));

    let (_, pages_refreshed) =
        cli::list_onenote_pages_helper(&storage, &graph, "Engineering/Specs", 50, true, false)
            .await
            .unwrap();
    assert!(!pages_refreshed.iter().any(|p| p.id == "p-2"));
    assert!(storage
        .search_onenote_pages("GhostBodyTokenXYZ", None, 10)
        .unwrap()
        .is_empty());

    // S6: Partial group 500 failure preserves group:team-1 AND site:site-99 caches
    let site_nb = graph
        .create_onenote_notebook(
            &OnenoteScope::Site {
                site_id: "site-99".to_string(),
                site_name: None,
            },
            "SharePoint Site NB",
        )
        .await
        .unwrap();
    storage
        .upsert_onenote_notebooks(std::slice::from_ref(&site_nb))
        .unwrap();
    st.fail_group_notebooks.store(true, Ordering::SeqCst);
    onenote_cli::sync::bootstrap_onenote(&graph, &storage)
        .await
        .unwrap();
    st.fail_group_notebooks.store(false, Ordering::SeqCst);
    let nbs_after_s6 = storage.list_onenote_notebooks().unwrap();
    assert!(nbs_after_s6
        .iter()
        .any(|n| n.notebook_key == "site:site-99|nb-site-site-99"));
    assert!(nbs_after_s6
        .iter()
        .any(|n| n.notebook_key == "group:team-1|nb-team-1"));
    assert!(!storage
        .list_onenote_pages("group:team-1|sec-team-1")
        .unwrap()
        .is_empty());

    // Section 4.1 & 4.2: Section moved out of sectionGroup, Page moved across sections,
    // and Recreated section with same name ("TargetSec": sec-3 -> sec-3-new)
    {
        let mut secs = st.sections.lock().unwrap();
        for s in secs.iter_mut() {
            if s["id"].as_str() == Some("sec-1") {
                s.as_object_mut().unwrap().remove("parentSectionGroup");
            }
        }
        secs.retain(|s| s["id"].as_str() != Some("sec-3"));
        secs.push(json!({
            "id": "sec-3-new",
            "displayName": "TargetSec",
            "isDefault": false,
            "parentNotebook": { "id": "nb-1", "displayName": "Engineering" },
            "lastModifiedDateTime": "2026-11-20T10:30:00Z"
        }));
        let mut pm = st.pages.lock().unwrap();
        let p1 = pm.get_mut("p-1").unwrap();
        p1["parentSection"] = json!({ "id": "sec-3-new", "displayName": "TargetSec" });
    }
    // Resolve recreated section by name with refresh=true -> must find sec-3-new and return p-1
    let (resolved_sec, moved_pages) =
        cli::list_onenote_pages_helper(&storage, &graph, "TargetSec", 50, true, false)
            .await
            .unwrap();
    assert_eq!(resolved_sec.id, "sec-3-new");
    assert_eq!(moved_pages.len(), 1);
    assert_eq!(moved_pages[0].id, "p-1");

    // Verify get_onenote_page_detail reflects moved section_id/section_name
    let cached_detail = storage.get_onenote_page_detail("me|p-1").unwrap().unwrap();
    assert_eq!(cached_detail.summary.section_id, "sec-3-new");
    assert_eq!(
        cached_detail.summary.section_name.as_deref(),
        Some("TargetSec")
    );

    // Verify sec-1 parent_section_group_id was cleared to None on hierarchy sync
    onenote_cli::sync::bootstrap_onenote(&graph, &storage)
        .await
        .unwrap();
    let sec1 = storage
        .list_onenote_sections(None)
        .unwrap()
        .into_iter()
        .find(|s| s.id == "sec-1")
        .unwrap();
    assert_eq!(sec1.parent_section_group_id, None);
    assert_eq!(sec1.parent_section_group_name, None);

    // HTTP 409 Conflict & MCP conflict verification
    let serve_router = serve::build_router(Arc::new(ServeState {
        config: cfg.clone(),
        graph: graph.clone(),
        storage: storage.clone(),
        api_key: Some("k".to_string()),
    }));
    let serve_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let serve_addr = serve_listener.local_addr().unwrap();
    let serve_handle = tokio::spawn(async move {
        let _ = axum::serve(serve_listener, serve_router).await;
    });

    let http = reqwest::Client::new();
    let conflict_resp = http
        .patch(format!("http://{serve_addr}/v1/onenote/pages/me%7Cp-1"))
        .header("Authorization", "Bearer k")
        .json(&json!({
            "block_id": "p:{b1}",
            "markdown": "HTTP stale overwrite",
            "expected_content": "Old stale baseline text"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict_resp.status(), reqwest::StatusCode::CONFLICT);

    let mcp_conflict = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 99,
            "method": "tools/call",
            "params": {
                "name": "update_block",
                "arguments": {
                    "page": "me|p-1",
                    "block_id": "p:{b1}",
                    "markdown": "MCP stale overwrite",
                    "expected_content": "Old stale baseline text"
                }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_conflict["result"]["isError"], true);
    assert!(mcp_conflict["result"]["content"][0]["text"]
        .as_str()
        .unwrap_or("")
        .to_lowercase()
        .contains("conflict"));

    // Matching/new baseline preflight succeeds on HTTP (200) and MCP (isError: false)
    let ok_http = http
        .patch(format!("http://{serve_addr}/v1/onenote/pages/me%7Cp-1"))
        .header("Authorization", "Bearer k")
        .json(&json!({
            "block_id": "p:{b1}",
            "markdown": "HTTP fresh preflight edit",
            "expected_content": "CRITICAL REMOTE EDIT BY ALICE"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(ok_http.status(), reqwest::StatusCode::OK);

    let ok_mcp = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 100,
            "method": "tools/call",
            "params": {
                "name": "update_block",
                "arguments": {
                    "page": "me|p-1",
                    "block_id": "p:{b1}",
                    "markdown": "MCP fresh preflight edit",
                    "expected_content": "HTTP fresh preflight edit",
                    "compact": false
                }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(ok_mcp["result"]["isError"], false);
    assert!(ok_mcp["result"]["structuredContent"]["page"]["body_plain"]
        .as_str()
        .unwrap_or("")
        .contains("MCP fresh preflight edit"));

    serve_handle.abort();
    server.abort();
}

#[tokio::test]
async fn e2e_non_404_with_404_text_preserves_cached_page_while_404_and_410_evict() {
    use std::sync::atomic::{AtomicU16, Ordering};

    let status_code = Arc::new(AtomicU16::new(200));
    let sc_meta = status_code.clone();
    let sc_content = status_code.clone();

    let app = Router::new()
        .route(
            "/v1.0/me/onenote/pages/p-keep",
            get(move || {
                let sc = sc_meta.clone();
                async move {
                    match sc.load(Ordering::SeqCst) {
                        200 => (
                            StatusCode::OK,
                            Json(json!({
                                "id": "p-keep",
                                "title": "Preserved Page",
                                "level": 0,
                                "order": 0,
                                "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                                "parentSection": { "id": "sec-1", "displayName": "Specs" }
                            })),
                        ),
                        503 => (
                            StatusCode::SERVICE_UNAVAILABLE,
                            Json(json!({
                                "error": {
                                    "code": "ResourceNotFound",
                                    "message": "transient gateway 404 backend shard not found"
                                }
                            })),
                        ),
                        410 => (
                            StatusCode::GONE,
                            Json(json!({
                                "error": { "code": "Gone", "message": "Page permanently removed" }
                            })),
                        ),
                        _ => (
                            StatusCode::NOT_FOUND,
                            Json(json!({
                                "error": { "code": "ResourceNotFound", "message": "Page not found" }
                            })),
                        ),
                    }
                }
            }),
        )
        .route(
            "/v1.0/me/onenote/pages/p-keep/content",
            get(move || {
                let sc = sc_content.clone();
                async move {
                    match sc.load(Ordering::SeqCst) {
                        200 => (
                            StatusCode::OK,
                            [("Retry-After", "0")],
                            r#"<!DOCTYPE html><html><head><title>Preserved Page</title></head><body><div id="div:1"><p id="p:1">PreserveCacheToken999</p></div></body></html>"#,
                        ),
                        503 => (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("Retry-After", "0")],
                            r#"{"error":{"code":"ResourceNotFound","message":"transient gateway 404 backend shard not found"}}"#,
                        ),
                        410 => (
                            StatusCode::GONE,
                            [("Retry-After", "0")],
                            r#"{"error":{"code":"Gone","message":"Page permanently removed"}}"#,
                        ),
                        _ => (
                            StatusCode::NOT_FOUND,
                            [("Retry-After", "0")],
                            r#"{"error":{"code":"ResourceNotFound","message":"Page not found"}}"#,
                        ),
                    }
                }
            }),
        );

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let srv = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let (_cfg, graph, storage) = make_test_runtime(format!("http://{addr}/v1.0"), dir.path());

    // 1. Seed cache via 200 OK read
    let detail = cli::load_onenote_page_detail(&storage, &graph, "me|p-keep", false)
        .await
        .unwrap();
    assert_eq!(detail.summary.title, "Preserved Page");
    assert_eq!(
        storage
            .search_onenote_pages("PreserveCacheToken999", None, 10)
            .unwrap()
            .len(),
        1
    );

    // 2. Non-404 (503) whose error body mentions "404" / "ResourceNotFound" / "not found"
    //    must fail the request WITHOUT deleting the cached page or its FTS5 entry.
    status_code.store(503, Ordering::SeqCst);
    let err_503 = cli::load_onenote_page_detail(&storage, &graph, "me|p-keep", false)
        .await
        .unwrap_err();
    assert!(err_503.to_string().contains("503"));
    assert!(
        storage.get_onenote_page_detail("me|p-keep").unwrap().is_some(),
        "503 with 404/not-found text must preserve cached page detail"
    );
    assert_eq!(
        storage
            .search_onenote_pages("PreserveCacheToken999", None, 10)
            .unwrap()
            .len(),
        1,
        "503 with 404/not-found text must preserve FTS5 search hit"
    );

    // 3. Authoritative HTTP 410 Gone evicts the cached page & FTS5 entry
    status_code.store(410, Ordering::SeqCst);
    let _ = cli::load_onenote_page_detail(&storage, &graph, "me|p-keep", false)
        .await
        .unwrap_err();
    assert!(storage
        .get_onenote_page_detail("me|p-keep")
        .unwrap()
        .is_none());
    assert!(storage
        .search_onenote_pages("PreserveCacheToken999", None, 10)
        .unwrap()
        .is_empty());

    srv.abort();
}
