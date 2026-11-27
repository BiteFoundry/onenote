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

use axum::{extract::Request, response::IntoResponse};

#[tokio::test]
async fn regression_onenote_bug_08_ambiguous_page_cli_and_http() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");
    let requests: Arc<Mutex<Vec<(String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let deleted: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
    let reqs_clone = requests.clone();
    let del_clone = deleted.clone();
    let base_clone = base_url.clone();

    let app = Router::new().fallback(move |req: Request| {
        let reqs = reqs_clone.clone();
        let dels = del_clone.clone();
        let base = base_clone.clone();
        async move {
            let method = req.method().as_str().to_string();
            let path = req.uri().path().to_string();
            reqs.lock().unwrap().push((method.clone(), path.clone()));

            match (method.as_str(), path.as_str()) {
                ("GET", "/v1.0/me/onenote/notebooks") => Json(json!({
                    "value": [{
                        "id": "nb-1",
                        "displayName": "Engineering",
                        "isDefault": true,
                        "isShared": false
                    }]
                }))
                .into_response(),
                ("GET", "/v1.0/me/onenote/notebooks/nb-1/sectionGroups")
                | ("GET", "/v1.0/me/onenote/sectionGroups") => {
                    Json(json!({ "value": [] })).into_response()
                }
                ("GET", "/v1.0/me/onenote/sections")
                | ("GET", "/v1.0/me/onenote/notebooks/nb-1/sections") => Json(json!({
                    "value": [
                        {
                            "id": "sec-alpha",
                            "displayName": "Alpha",
                            "isDefault": true,
                            "parentNotebook": { "id": "nb-1", "displayName": "Engineering" }
                        },
                        {
                            "id": "sec-beta",
                            "displayName": "Beta",
                            "isDefault": false,
                            "parentNotebook": { "id": "nb-1", "displayName": "Engineering" }
                        }
                    ]
                }))
                .into_response(),
                ("GET", "/v1.0/me/onenote/sections/sec-alpha/pages") => Json(json!({
                    "value": [
                        {
                            "id": "page-standup-a",
                            "title": "Standup notes",
                            "level": 0,
                            "order": 0,
                            "contentUrl": format!("{base}/me/onenote/pages/page-standup-a/content"),
                            "parentSection": { "id": "sec-alpha", "displayName": "Alpha" }
                        },
                        {
                            "id": "page-unique",
                            "title": "Quarterly Roadmap",
                            "level": 0,
                            "order": 1,
                            "contentUrl": format!("{base}/me/onenote/pages/page-unique/content"),
                            "parentSection": { "id": "sec-alpha", "displayName": "Alpha" }
                        }
                    ]
                }))
                .into_response(),
                ("GET", "/v1.0/me/onenote/sections/sec-beta/pages") => Json(json!({
                    "value": [
                        {
                            "id": "page-standup-b",
                            "title": "Standup notes",
                            "level": 0,
                            "order": 0,
                            "contentUrl": format!("{base}/me/onenote/pages/page-standup-b/content"),
                            "parentSection": { "id": "sec-beta", "displayName": "Beta" }
                        }
                    ]
                }))
                .into_response(),
                ("GET", "/v1.0/me/onenote/pages/page-standup-a") => Json(json!({
                    "id": "page-standup-a",
                    "title": "Standup notes",
                    "parentSection": { "id": "sec-alpha", "displayName": "Alpha" }
                }))
                .into_response(),
                ("GET", "/v1.0/me/onenote/pages/page-standup-a/content") => (
                    StatusCode::OK,
                    [("content-type", "text/html")],
                    r#"<!DOCTYPE html><html><head><title>Standup notes</title></head><body><div id="div:a"><p id="p:a">Alpha standup body</p></div></body></html>"#,
                )
                    .into_response(),
                ("GET", "/v1.0/me/onenote/pages/page-standup-b") => Json(json!({
                    "id": "page-standup-b",
                    "title": "Standup notes",
                    "parentSection": { "id": "sec-beta", "displayName": "Beta" }
                }))
                .into_response(),
                ("GET", "/v1.0/me/onenote/pages/page-standup-b/content") => (
                    StatusCode::OK,
                    [("content-type", "text/html")],
                    r#"<!DOCTYPE html><html><head><title>Standup notes</title></head><body><div id="div:b"><p id="p:b">Beta standup body</p></div></body></html>"#,
                )
                    .into_response(),
                ("GET", "/v1.0/me/onenote/pages/page-unique") => Json(json!({
                    "id": "page-unique",
                    "title": "Quarterly Roadmap",
                    "parentSection": { "id": "sec-alpha", "displayName": "Alpha" }
                }))
                .into_response(),
                ("GET", "/v1.0/me/onenote/pages/page-unique/content") => (
                    StatusCode::OK,
                    [("content-type", "text/html")],
                    r#"<!DOCTYPE html><html><head><title>Quarterly Roadmap</title></head><body><div id="div:u"><p id="p:u">Roadmap body</p></div></body></html>"#,
                )
                    .into_response(),
                ("GET", "/v1.0/me/onenote/pages/page-remote-only") => Json(json!({
                    "id": "page-remote-only",
                    "title": "Remote Only Note",
                    "parentSection": { "id": "sec-beta", "displayName": "Beta" }
                }))
                .into_response(),
                ("GET", "/v1.0/me/onenote/pages/page-remote-only/content") => (
                    StatusCode::OK,
                    [("content-type", "text/html")],
                    r#"<!DOCTYPE html><html><head><title>Remote Only Note</title></head><body><div id="div:r"><p id="p:r">Remote only body</p></div></body></html>"#,
                )
                    .into_response(),
                ("DELETE", p) if p.starts_with("/v1.0/me/onenote/pages/") => {
                    let id = p.trim_start_matches("/v1.0/me/onenote/pages/").to_string();
                    dels.lock().unwrap().push(id);
                    StatusCode::NO_CONTENT.into_response()
                }
                ("PATCH", p) if p.ends_with("/content") => StatusCode::NO_CONTENT.into_response(),
                _ => (
                    StatusCode::NOT_FOUND,
                    Json(json!({ "error": { "code": "ResourceNotFound", "message": format!("Not found: {path}") } })),
                )
                    .into_response(),
            }
        }
    });
    let graph_server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let (cfg, graph, storage) = make_test_runtime(base_url, dir.path());

    // Populate cache with both sections and hydrate both duplicate pages
    cli::load_onenote_notebooks(&storage, &graph, false, true, false)
        .await
        .unwrap();
    cli::load_onenote_sections(&storage, &graph, Some("Engineering"), true, false)
        .await
        .unwrap();
    cli::list_onenote_pages_helper(&storage, &graph, "Alpha", 20, true, false)
        .await
        .unwrap();
    cli::list_onenote_pages_helper(&storage, &graph, "Beta", 20, true, false)
        .await
        .unwrap();
    cli::load_onenote_page_detail(&storage, &graph, "me|page-standup-a", false)
        .await
        .unwrap();
    cli::load_onenote_page_detail(&storage, &graph, "me|page-standup-b", false)
        .await
        .unwrap();

    // Start local HTTP API (`onenote serve`) sharing the same runtime
    let state = Arc::new(ServeState {
        config: cfg.clone(),
        graph: graph.clone(),
        storage: storage.clone(),
        api_key: None,
    });
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, serve::build_router(state)).await;
    });
    let http = reqwest::Client::new();

    requests.lock().unwrap().clear();

    let assert_ambiguous_msg = |msg: &str, ctx: &str| {
        assert!(
            msg.to_ascii_lowercase().contains("ambiguous"),
            "{ctx}: expected ambiguity error, got: {msg}"
        );
        assert!(
            msg.contains("Standup notes")
                && msg.contains("Alpha")
                && msg.contains("Beta")
                && msg.contains("me|page-standup-a")
                && msg.contains("me|page-standup-b"),
            "{ctx}: error must include title, sections, and both scoped page keys, got: {msg}"
        );
    };

    for query in ["Standup notes", "Standup"] {
        // CLI helpers: read (online), read (offline), append, update, delete
        let err = cli::load_onenote_page_detail(&storage, &graph, query, false)
            .await
            .unwrap_err();
        assert_ambiguous_msg(&err.to_string(), &format!("read online '{query}'"));

        let err = cli::load_onenote_page_detail(&storage, &graph, query, true)
            .await
            .unwrap_err();
        assert_ambiguous_msg(&err.to_string(), &format!("read offline '{query}'"));

        let err = cli::append_onenote_page_helper(&storage, &graph, query, "Extra", &[])
            .await
            .unwrap_err();
        assert_ambiguous_msg(&err.to_string(), &format!("append '{query}'"));

        let err = cli::update_onenote_page_helper(
            &storage,
            &graph,
            query,
            Some("Renamed"),
            None,
            None,
        )
        .await
        .unwrap_err();
        assert_ambiguous_msg(&err.to_string(), &format!("update '{query}'"));

        let err = cli::delete_onenote_page_helper(&storage, &graph, query)
            .await
            .unwrap_err();
        assert_ambiguous_msg(&err.to_string(), &format!("delete '{query}'"));

        // HTTP API: GET, GET?offline=true, PATCH, POST /append, DELETE
        let encoded = query.replace(' ', "%20");
        let r_get = http
            .get(format!("http://{api_addr}/v1/onenote/pages/{encoded}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r_get.status(), StatusCode::BAD_REQUEST, "HTTP GET '{query}'");
        let v_get: Value = r_get.json().await.unwrap();
        assert_ambiguous_msg(
            v_get["error"].as_str().unwrap_or(""),
            &format!("HTTP GET '{query}'"),
        );

        let r_off = http
            .get(format!("http://{api_addr}/v1/onenote/pages/{encoded}?offline=true"))
            .send()
            .await
            .unwrap();
        assert_eq!(r_off.status(), StatusCode::BAD_REQUEST, "HTTP GET offline '{query}'");
        let v_off: Value = r_off.json().await.unwrap();
        assert_ambiguous_msg(
            v_off["error"].as_str().unwrap_or(""),
            &format!("HTTP GET offline '{query}'"),
        );

        let r_del = http
            .delete(format!("http://{api_addr}/v1/onenote/pages/{encoded}"))
            .send()
            .await
            .unwrap();
        assert_eq!(r_del.status(), StatusCode::BAD_REQUEST, "HTTP DELETE '{query}'");
        let v_del: Value = r_del.json().await.unwrap();
        assert_ambiguous_msg(
            v_del["error"].as_str().unwrap_or(""),
            &format!("HTTP DELETE '{query}'"),
        );
    }

    // Zero Graph requests must have been sent for any ambiguous query
    assert!(
        requests.lock().unwrap().is_empty(),
        "ambiguous queries must send 0 Graph requests, got: {:?}",
        *requests.lock().unwrap()
    );
    assert!(deleted.lock().unwrap().is_empty());

    // Regression controls:
    // 1. Scoped key selects only the specified duplicate page
    let detail_a = cli::load_onenote_page_detail(&storage, &graph, "me|page-standup-a", false)
        .await
        .unwrap();
    assert_eq!(detail_a.summary.page_key, "me|page-standup-a");
    assert!(detail_a.body_plain.contains("Alpha standup body"));

    let detail_b = cli::load_onenote_page_detail(&storage, &graph, "me|page-standup-b", true)
        .await
        .unwrap();
    assert_eq!(detail_b.summary.page_key, "me|page-standup-b");
    assert!(detail_b.body_plain.contains("Beta standup body"));

    // 2. Unique title & unique substring continue working
    let by_title = cli::load_onenote_page_detail(&storage, &graph, "Quarterly Roadmap", false)
        .await
        .unwrap();
    assert_eq!(by_title.summary.page_key, "me|page-unique");

    let by_sub = cli::load_onenote_page_detail(&storage, &graph, "Roadmap", false)
        .await
        .unwrap();
    assert_eq!(by_sub.summary.page_key, "me|page-unique");

    // 3. Unseen real remote ID still uses online fallback
    let remote_only =
        cli::load_onenote_page_detail(&storage, &graph, "page-remote-only", false)
            .await
            .unwrap();
    assert_eq!(remote_only.summary.page_key, "me|page-remote-only");
    assert!(remote_only.body_plain.contains("Remote only body"));

    // 4. Deleting via explicit scoped key deletes only that page
    let del_key = cli::delete_onenote_page_helper(&storage, &graph, "me|page-standup-a")
        .await
        .unwrap();
    assert_eq!(del_key, "me|page-standup-a");
    assert_eq!(*deleted.lock().unwrap(), vec!["page-standup-a".to_string()]);

    api_server.abort();
    graph_server.abort();
}

#[tokio::test]
async fn regression_onenote_bug_01_full_body_repeated_chars_and_failure_safety() {
    use scraper::{Html, Selector};

    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let mut initial_xhtml = std::collections::HashMap::new();
    initial_xhtml.insert(
        "page-abs".to_string(),
        r#"<!DOCTYPE html><html><head><title>Abs Page</title></head><body><div id="div:abs" style="position:absolute;left:48px;top:120px"><p id="p:agenda" data-id="agenda-1" style="font-size:12pt">Meeting agenda</p><p id="p:score" data-id="score-1" style="color:#333333">Total 1000 vs 2000: 40 points</p><p id="p:sib" data-id="sib-1">Sibling untouched</p></div><div id="div:side" style="position:absolute;left:500px;top:120px"><p id="p:side">Side region</p></div></body></html>"#.to_string(),
    );
    initial_xhtml.insert(
        "page-plain".to_string(),
        r#"<!DOCTYPE html><html><head><title>Plain Page</title></head><body><div id="div:plain"><p id="p:agenda" data-id="agenda-2" style="font-size:12pt">Meeting agenda</p><p id="p:score" data-id="score-2" style="color:#333333">Total 1000 vs 2000: 40 points</p><p id="p:sib" data-id="sib-2">Sibling untouched</p></div></body></html>"#.to_string(),
    );
    initial_xhtml.insert(
        "page-ambig-plain".to_string(),
        r#"<!DOCTYPE html><html><head><title>Ambig Plain</title></head><body><div id="div:plain-amb"><p id="p:fmt" data-id="fmt-1" style="color:#111111"><b>alpha</b> alpha</p><p id="p:keep" data-id="keep-1">Keep sibling</p></div></body></html>"#.to_string(),
    );
    initial_xhtml.insert(
        "page-ambig-abs".to_string(),
        r#"<!DOCTYPE html><html><head><title>Ambig Abs</title></head><body><div id="div:abs-amb" style="position:absolute;left:48px"><p id="p:fmt" data-id="fmt-2" style="color:#111111"><b>alpha</b> alpha</p><p id="p:keep" data-id="keep-2">Keep sibling</p></div></body></html>"#.to_string(),
    );
    initial_xhtml.insert(
        "page-ctrl".to_string(),
        r#"<!DOCTYPE html><html><head><title>Control Title</title></head><body><div id="div:ctrl" style="position:absolute;left:48px"><p id="p:status" data-id="st-1" style="color:green">Status READY</p></div></body></html>"#.to_string(),
    );

    type PatchLog = Arc<Mutex<Vec<(String, Vec<Value>)>>>;
    let page_xhtml = Arc::new(Mutex::new(initial_xhtml));
    let patches: PatchLog = Arc::new(Mutex::new(Vec::new()));
    let xhtml_clone = page_xhtml.clone();
    let patches_clone = patches.clone();

    let app = Router::new().fallback(move |req: Request| {
        let xhtml_map = xhtml_clone.clone();
        let patch_log = patches_clone.clone();
        async move {
            let method = req.method().as_str().to_string();
            let path = req.uri().path().to_string();
            let body = axum::body::to_bytes(req.into_body(), 1024 * 1024)
                .await
                .unwrap();
            if method == "GET" && path.starts_with("/v1.0/me/onenote/pages/") {
                let rest = path.trim_start_matches("/v1.0/me/onenote/pages/");
                if let Some(id) = rest.strip_suffix("/content") {
                    if let Some(html) = xhtml_map.lock().unwrap().get(id).cloned() {
                        return (StatusCode::OK, [("content-type", "text/html")], html)
                            .into_response();
                    }
                } else if let Some(html) = xhtml_map.lock().unwrap().get(rest).cloned() {
                    let title = html
                        .split("<title>")
                        .nth(1)
                        .and_then(|s| s.split("</title>").next())
                        .unwrap_or("Page")
                        .to_string();
                    return Json(json!({
                        "id": rest,
                        "title": title,
                        "parentSection": { "id": "sec-1", "displayName": "Specs" }
                    }))
                    .into_response();
                }
            } else if method == "PATCH" && path.ends_with("/content") {
                let id = path
                    .trim_start_matches("/v1.0/me/onenote/pages/")
                    .trim_end_matches("/content")
                    .to_string();
                let cmds: Vec<Value> = serde_json::from_slice(&body).unwrap();
                patch_log.lock().unwrap().push((id.clone(), cmds.clone()));
                if let Some(html) = xhtml_map.lock().unwrap().get_mut(&id) {
                    for cmd in &cmds {
                        let target = cmd["target"].as_str().unwrap_or("");
                        let action = cmd["action"].as_str().unwrap_or("");
                        let content = cmd["content"].as_str().unwrap_or("");
                        if action == "replace" && target == "title" {
                            if let (Some(s), Some(e)) =
                                (html.find("<title>"), html.find("</title>"))
                            {
                                html.replace_range(
                                    s..e + "</title>".len(),
                                    &format!("<title>{content}</title>"),
                                );
                            }
                        } else if action == "replace" {
                            let needle = format!(r#"id="{target}""#);
                            if let Some(id_pos) = html.find(&needle) {
                                if let Some(open_lt) = html[..id_pos].rfind('<') {
                                    let tag: String = html[open_lt + 1..]
                                        .chars()
                                        .take_while(|c| c.is_ascii_alphanumeric())
                                        .collect();
                                    let close_tag = format!("</{tag}>");
                                    if let Some(rel_end) = html[open_lt..].find(&close_tag) {
                                        html.replace_range(
                                            open_lt..open_lt + rel_end + close_tag.len(),
                                            content,
                                        );
                                    }
                                }
                            }
                        }
                    }
                }
                return StatusCode::NO_CONTENT.into_response();
            }
            StatusCode::NOT_FOUND.into_response()
        }
    });
    let graph_server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let (_cfg, graph, storage) = make_test_runtime(base_url, dir.path());

    let assert_p_dom = |html: &str,
                        expected_id: &str,
                        expected_data_id: &str,
                        expected_style: &str,
                        expected_text: &str| {
        let frag = Html::parse_fragment(html);
        let p_sel = Selector::parse("p").unwrap();
        let p = frag.select(&p_sel).next().expect("patch must contain <p>");
        assert_eq!(p.value().attr("id"), Some(expected_id));
        assert_eq!(p.value().attr("data-id"), Some(expected_data_id));
        assert_eq!(p.value().attr("style"), Some(expected_style));
        let text: String = p.text().collect();
        assert_eq!(text, expected_text);
    };

    // 1 & 2. Both position:absolute container and plain div:
    //    - "Meeting agenda" -> "Meeting agendas"
    //    - "40" -> "45" in sentence containing repeated '0's
    for (page_id, agenda_did, score_did, sib_did) in [
        ("page-abs", "agenda-1", "score-1", "sib-1"),
        ("page-plain", "agenda-2", "score-2", "sib-2"),
    ] {
        let key = format!("me|{page_id}");
        let before = cli::load_onenote_page_detail(&storage, &graph, &key, false)
            .await
            .unwrap();

        // Edit 1: "Meeting agenda" -> "Meeting agendas"
        let md1 = before
            .body_markdown
            .replace("Meeting agenda", "Meeting agendas");
        let count_before = patches.lock().unwrap().len();
        let after1 = cli::update_onenote_page_helper(&storage, &graph, &key, None, Some(&md1), None)
            .await
            .unwrap_or_else(|e| panic!("{page_id} agenda->agendas failed: {e}"));
        {
            let log = patches.lock().unwrap();
            assert_eq!(log.len(), count_before + 1, "{page_id} expected 1 PATCH");
            let (pid, cmds) = log.last().unwrap();
            assert_eq!(pid, page_id);
            assert_eq!(cmds.len(), 1, "{page_id} expected 1 command");
            assert_eq!(cmds[0]["target"], "p:agenda", "{page_id} must target p:agenda, not parent div");
            assert_eq!(cmds[0]["action"], "replace");
            assert_p_dom(
                cmds[0]["content"].as_str().unwrap(),
                "p:agenda",
                agenda_did,
                "font-size:12pt",
                "Meeting agendas",
            );
        }

        // Edit 2: "40" -> "45" in sentence with repeated 0s
        let md2 = after1.body_markdown.replace("40 points", "45 points");
        let count_before = patches.lock().unwrap().len();
        let after2 = cli::update_onenote_page_helper(&storage, &graph, &key, None, Some(&md2), None)
            .await
            .unwrap_or_else(|e| panic!("{page_id} 40->45 failed: {e}"));
        {
            let log = patches.lock().unwrap();
            assert_eq!(log.len(), count_before + 1, "{page_id} expected 1 PATCH for 40->45");
            let (_, cmds) = log.last().unwrap();
            assert_eq!(cmds.len(), 1);
            assert_eq!(cmds[0]["target"], "p:score", "{page_id} must target p:score, not parent div");
            assert_p_dom(
                cmds[0]["content"].as_str().unwrap(),
                "p:score",
                score_did,
                "color:#333333",
                "Total 1000 vs 2000: 45 points",
            );
        }

        // Verify DOM preserves sibling <p> and child order
        let parsed_doc = Html::parse_document(&after2.raw_xhtml);
        let p_sel = Selector::parse("p").unwrap();
        let p_ids: Vec<_> = parsed_doc
            .select(&p_sel)
            .filter_map(|el| el.value().attr("id"))
            .collect();
        assert!(p_ids.starts_with(&["p:agenda", "p:score", "p:sib"]));
        let sib = parsed_doc
            .select(&Selector::parse(r#"p[id="p:sib"]"#).unwrap())
            .next()
            .unwrap();
        assert_eq!(sib.value().attr("data-id"), Some(sib_did));
        assert_eq!(sib.text().collect::<String>(), "Sibling untouched");
    }

    // 3. Ambiguous edit across formatting nodes (BOTH plain div and position:absolute div)
    //    must fail with edit guidance and send 0 PATCH requests (must NOT replace parent div!).
    for page_id in ["page-ambig-plain", "page-ambig-abs"] {
        let key = format!("me|{page_id}");
        let before = cli::load_onenote_page_detail(&storage, &graph, &key, false)
            .await
            .unwrap();
        let count_before = patches.lock().unwrap().len();
        let ambig_md = before
            .body_markdown
            .replacen("**alpha** alpha", "**alphas** alpha", 1);
        let err = cli::update_onenote_page_helper(&storage, &graph, &key, None, Some(&ambig_md), None)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("--block-id"),
            "{page_id} ambiguous cross-node edit must return edit guidance, got: {err}"
        );
        assert_eq!(
            patches.lock().unwrap().len(),
            count_before,
            "{page_id} ambiguous edit must send 0 PATCH requests"
        );

        // Also crossing formatting boundary ("**alpha** alpha" -> "replaced") must send 0 PATCH requests
        let cross_md = before
            .body_markdown
            .replacen("**alpha** alpha", "replaced", 1);
        let err2 = cli::update_onenote_page_helper(&storage, &graph, &key, None, Some(&cross_md), None)
            .await
            .unwrap_err();
        assert!(
            err2.to_string().contains("--block-id"),
            "{page_id} cross-boundary edit must return edit guidance, got: {err2}"
        );
        assert_eq!(
            patches.lock().unwrap().len(),
            count_before,
            "{page_id} cross-boundary edit must send 0 PATCH requests"
        );
    }

    // 4. Regression controls: unique word replacement & title-only update
    let ctrl_before = cli::load_onenote_page_detail(&storage, &graph, "me|page-ctrl", false)
        .await
        .unwrap();
    let count_before = patches.lock().unwrap().len();
    let ctrl_md = ctrl_before.body_markdown.replace("READY", "APPROVED");
    cli::update_onenote_page_helper(&storage, &graph, "me|page-ctrl", None, Some(&ctrl_md), None)
        .await
        .unwrap();
    {
        let log = patches.lock().unwrap();
        assert_eq!(log.len(), count_before + 1);
        assert_eq!(log.last().unwrap().1[0]["target"], "p:status");
    }

    let count_before = patches.lock().unwrap().len();
    cli::update_onenote_page_helper(&storage, &graph, "me|page-ctrl", Some("New Control Title"), None, None)
        .await
        .unwrap();
    {
        let log = patches.lock().unwrap();
        assert_eq!(log.len(), count_before + 1);
        assert_eq!(log.last().unwrap().1[0]["target"], "title");
        assert_eq!(log.last().unwrap().1[0]["content"], "New Control Title");
    }

    graph_server.abort();
}

#[tokio::test]
async fn regression_onenote_bug_05_nested_table_cell_order_and_list_todos() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let complex_xhtml = format!(
        r#"<!DOCTYPE html><html><head><title>Nested And Todos</title></head><body>
<div id="div:x" style="position:absolute;left:48px;top:115px">
  <table id="table:o" style="border:1px solid">
<tr>
  <td id="td:1"><p>앞</p><img id="img:c" alt="Logo" src="{base_url}/me/onenote/resources/res-9/$value" data-src-type="image/png" /><table id="table:i"><tr><td>INNERCELLZEBRA</td><td>77</td></tr></table><p>뒤</p></td>
</tr>
<tr>
  <td id="td:2">Row2A</td>
</tr>
  </table>
  <table id="table:simple">
<tr><th>Module</th><th>Owner</th></tr>
<tr><td>Parser</td><td>Core</td></tr>
  </table>
  <ul id="ul:1">
<li id="li:span1"><span id="span:1" data-tag="to-do">Order paint</span></li>
<li id="li:p1"><p id="p:la" data-tag="to-do" style="margin-top:0pt">Buy filters</p></li>
<li id="li:ctrl" data-tag="to-do">Control li tag</li>
<li id="li:span2"><span data-tag="to-do:completed">Sand wall</span></li>
  </ul>
</div>
</body></html>"#
    );

    let page_xhtml = Arc::new(Mutex::new(complex_xhtml));
    let patches: Arc<Mutex<Vec<Vec<Value>>>> = Arc::new(Mutex::new(Vec::new()));
    let xhtml_clone = page_xhtml.clone();
    let patches_clone = patches.clone();

    let app = Router::new().fallback(move |req: Request| {
        let xhtml_ref = xhtml_clone.clone();
        let patch_log = patches_clone.clone();
        async move {
            let method = req.method().as_str().to_string();
            let path = req.uri().path().to_string();
            let body = axum::body::to_bytes(req.into_body(), 1024 * 1024)
                .await
                .unwrap();
            match (method.as_str(), path.as_str()) {
                ("GET", "/v1.0/me/onenote/pages/pg-nested") => Json(json!({
                    "id": "pg-nested",
                    "title": "Nested And Todos",
                    "parentSection": { "id": "sec-1", "displayName": "Specs" }
                }))
                .into_response(),
                ("GET", "/v1.0/me/onenote/pages/pg-nested/content") => (
                    StatusCode::OK,
                    [("content-type", "text/html")],
                    xhtml_ref.lock().unwrap().clone(),
                )
                    .into_response(),
                ("PATCH", "/v1.0/me/onenote/pages/pg-nested/content") => {
                    let cmds: Vec<Value> = serde_json::from_slice(&body).unwrap();
                    patch_log.lock().unwrap().push(cmds.clone());
                    let mut html = xhtml_ref.lock().unwrap();
                    for cmd in &cmds {
                        let target = cmd["target"].as_str().unwrap_or("");
                        let content = cmd["content"].as_str().unwrap_or("");
                        let needle = format!(r#"id="{target}""#);
                        if let Some(id_pos) = html.find(&needle) {
                            if let Some(open_lt) = html[..id_pos].rfind('<') {
                                let tag: String = html[open_lt + 1..]
                                    .chars()
                                    .take_while(|c| c.is_ascii_alphanumeric())
                                    .collect();
                                let close_tag = format!("</{tag}>");
                                if let Some(rel_end) = html[open_lt..].find(&close_tag) {
                                    html.replace_range(
                                        open_lt..open_lt + rel_end + close_tag.len(),
                                        content,
                                    );
                                }
                            }
                        }
                    }
                    StatusCode::NO_CONTENT.into_response()
                }
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    let graph_server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let (_cfg, graph, storage) = make_test_runtime(base_url, dir.path());

    // 1. Read & verify DOM order in cell, nested table non-duplication, simple table control, and search
    let detail = cli::load_onenote_page_detail(&storage, &graph, "me|pg-nested", false)
        .await
        .unwrap();
    assert!(
        patches.lock().unwrap().is_empty(),
        "read must not modify remote page"
    );

    // First block must be outer table (NOT hoisted img:c!)
    assert_eq!(detail.blocks[0].element_id.as_deref(), Some("table:o"));
    if let onenote_cli::models::OnenoteBlockKind::Table { headers, rows } =
        &detail.blocks[0].kind
    {
        assert!(headers.is_empty());
        assert_eq!(
            rows.len(),
            2,
            "outer table must have 2 rows (not absorb inner table <tr>), got {}",
            rows.len()
        );
    } else {
        panic!("expected outer Table block first, got {:?}", detail.blocks[0]);
    }

    // Verify resource for cell image is still preserved
    assert_eq!(detail.resources.len(), 1);
    assert!(detail.resources[0].is_image);
    assert_eq!(detail.resources[0].alt_text.as_deref(), Some("Logo"));

    // Verify reading order in both Markdown and plain text: 앞 < Logo < INNERCELLZEBRA < 77 < 뒤 < Row2A
    for (label, text) in [
        ("body_markdown", detail.body_markdown.as_str()),
        ("body_plain", detail.body_plain.as_str()),
    ] {
        let pos_front = text
            .find("앞")
            .unwrap_or_else(|| panic!("{label} missing '앞': {text}"));
        let pos_logo = text
            .find("Logo")
            .unwrap_or_else(|| panic!("{label} missing 'Logo': {text}"));
        let pos_zebra = text
            .find("INNERCELLZEBRA")
            .unwrap_or_else(|| panic!("{label} missing 'INNERCELLZEBRA': {text}"));
        // The preceding image URL can also contain 77 in its ephemeral port.
        let pos_77 = text[pos_zebra..]
            .find("77")
            .map(|offset| pos_zebra + offset)
            .unwrap_or_else(|| panic!("{label} missing table cell '77' after INNERCELLZEBRA: {text}"));
        let pos_back = text
            .find("뒤")
            .unwrap_or_else(|| panic!("{label} missing '뒤': {text}"));
        let pos_row2 = text
            .find("Row2A")
            .unwrap_or_else(|| panic!("{label} missing 'Row2A': {text}"));
        assert!(
            pos_front < pos_logo
                && pos_logo < pos_zebra
                && pos_zebra < pos_77
                && pos_77 < pos_back
                && pos_back < pos_row2,
            "{label} did not preserve DOM order (앞={pos_front}, Logo={pos_logo}, INNERCELLZEBRA={pos_zebra}, 77={pos_77}, 뒤={pos_back}, Row2A={pos_row2}):\n{text}"
        );
        assert_eq!(
            text.matches("INNERCELLZEBRA").count(),
            1,
            "{label} must include nested table cell text exactly once:\n{text}"
        );
    }

    // Control: simple table still parsed cleanly
    assert!(detail.body_markdown.contains("| Module | Owner |"));
    assert!(detail.body_markdown.contains("| Parser | Core |"));

    // Search finds INNERCELLZEBRA once and sends 0 remote writes
    let hits = cli::search_onenote_helper(&storage, &graph, "INNERCELLZEBRA", None, false, 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1, "search for INNERCELLZEBRA must return 1 hit");
    assert_eq!(hits[0].page_key, "me|pg-nested");
    assert!(patches.lock().unwrap().is_empty());

    // 2. Verify li>span, li>p, and control li[data-tag] todos appear in blocks/markdown/has_todo
    assert!(detail.summary.has_todo);
    let todo_blocks: Vec<_> = detail
        .blocks
        .iter()
        .filter(|b| b.tag.as_ref().is_some_and(|t| t.is_todo()))
        .collect();
    assert_eq!(
        todo_blocks.len(),
        4,
        "expected 4 todo items (li>span, li>p, li, li>span:completed), got: {todo_blocks:?}"
    );
    assert!(detail.body_markdown.contains("- [ ] Order paint"));
    assert!(detail.body_markdown.contains("- [ ] Buy filters"));
    assert!(detail.body_markdown.contains("- [ ] Control li tag"));
    assert!(detail.body_markdown.contains("- [x] Sand wall"));

    // 3. Check index 0 (li>span) -> targets li:span1 (Graph-replaceable) and strips stale child tag
    let (after0, bid0, done0) =
        cli::toggle_onenote_todo_helper(&storage, &graph, "me|pg-nested", None, Some(0))
            .await
            .unwrap();
    assert!(done0);
    assert_eq!(bid0, "li:span1");
    assert!(after0.body_markdown.contains("- [x] Order paint"));
    {
        let log = patches.lock().unwrap();
        let cmd = &log.last().unwrap()[0];
        assert_eq!(cmd["target"], "li:span1");
        let content = cmd["content"].as_str().unwrap();
        assert!(content.contains(r#"data-tag="to-do:completed""#));
        assert!(
            !content.contains(r#"data-tag="to-do""#),
            "stale unchecked tag must not remain on child span: {content}"
        );
    }

    // 4. Check index 1 (li>p with id=p:la) -> targets p:la (actual tagged replaceable <p>)
    let (after1, bid1, done1) =
        cli::toggle_onenote_todo_helper(&storage, &graph, "me|pg-nested", None, Some(1))
            .await
            .unwrap();
    assert!(done1);
    assert_eq!(bid1, "p:la");
    assert!(after1.body_markdown.contains("- [x] Buy filters"));
    {
        let log = patches.lock().unwrap();
        let cmd = &log.last().unwrap()[0];
        assert_eq!(cmd["target"], "p:la");
        let content = cmd["content"].as_str().unwrap();
        assert!(content.contains(r#"data-tag="to-do:completed""#));
        assert!(content.contains(r#"style="margin-top:0pt""#));
    }

    // 5. Check index 2 (control li[data-tag]) -> targets li:ctrl
    let (after2, bid2, done2) =
        cli::toggle_onenote_todo_helper(&storage, &graph, "me|pg-nested", None, Some(2))
            .await
            .unwrap();
    assert!(done2);
    assert_eq!(bid2, "li:ctrl");
    assert!(after2.body_markdown.contains("- [x] Control li tag"));

    // 6. Check index 3 (li>span already completed) -> toggles to unchecked on li:span2
    let (after3, bid3, done3) =
        cli::toggle_onenote_todo_helper(&storage, &graph, "me|pg-nested", None, Some(3))
            .await
            .unwrap();
    assert!(!done3);
    assert_eq!(bid3, "li:span2");
    assert!(after3.body_markdown.contains("- [ ] Sand wall"));
    {
        let log = patches.lock().unwrap();
        let cmd = &log.last().unwrap()[0];
        assert_eq!(cmd["target"], "li:span2");
        let content = cmd["content"].as_str().unwrap();
        assert!(content.contains(r#"data-tag="to-do""#));
        assert!(!content.contains("to-do:completed"));
    }

    // 7. Check --block-id using outer li:p1 resolves to the tagged p:la target
    let (after4, bid4, done4) = cli::change_onenote_todo_helper_with_guard(
        &storage,
        &graph,
        "me|pg-nested",
        Some("li:p1"),
        None,
        Some(false),
        None,
        true,
    )
    .await
    .unwrap();
    assert!(!done4);
    assert_eq!(bid4, "p:la");
    assert!(after4.body_markdown.contains("- [ ] Buy filters"));

    graph_server.abort();
}

#[tokio::test]
async fn regression_onenote_bug_03_http_api_contract_and_compat() {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let addr = listener.local_addr().unwrap();
    let base_url = format!("http://{addr}/v1.0");

    let png_a = sample_png_bytes();
    let png_b = {
        let mut out = Vec::new();
        let img = image::RgbaImage::from_pixel(3, 3, image::Rgba([20, 180, 90, 255]));
        let mut cursor = std::io::Cursor::new(&mut out);
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .unwrap();
        out
    };
    let png_beta = {
        let mut out = Vec::new();
        let img = image::RgbaImage::from_pixel(4, 2, image::Rgba([10, 40, 220, 255]));
        let mut cursor = std::io::Cursor::new(&mut out);
        image::DynamicImage::ImageRgba8(img)
            .write_to(&mut cursor, image::ImageFormat::Png)
            .unwrap();
        out
    };

    let res1_url = format!("{base_url}/me/onenote/resources/res-1/content");
    let res2_url = format!("{base_url}/me/onenote/resources/res-2/content");
    let res_alpha_url = format!("{base_url}/me/onenote/resources/res-alpha/content");
    let res_beta_url = format!("{base_url}/me/onenote/resources/res-beta/content");

    let mut xhtml_map = std::collections::HashMap::new();
    xhtml_map.insert(
        "page-1".to_string(),
        format!(
            r#"<!DOCTYPE html><html><head><title>API Page</title></head><body><div id="div:1"><p id="p:todo-1" data-tag="to-do">Ship API parity</p><img id="img:1" src="{res1_url}" data-src-type="image/png" alt="first" /><img id="img:2" src="{res2_url}" data-src-type="image/png" alt="second" /></div></body></html>"#
        ),
    );
    xhtml_map.insert(
        "page-sa".to_string(),
        format!(
            r#"<!DOCTYPE html><html><head><title>Standup notes</title></head><body><div id="div:a"><img id="img:a" src="{res_alpha_url}" data-src-type="image/png" alt="alpha" /></div></body></html>"#
        ),
    );
    xhtml_map.insert(
        "page-sb".to_string(),
        format!(
            r#"<!DOCTYPE html><html><head><title>Standup notes</title></head><body><div id="div:b"><img id="img:b" src="{res_beta_url}" data-src-type="image/png" alt="beta" /></div></body></html>"#
        ),
    );

    let page_xhtml = Arc::new(Mutex::new(xhtml_map));
    let patches: Arc<Mutex<Vec<(String, String, String)>>> = Arc::new(Mutex::new(Vec::new()));
    let resource_hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));

    let xhtml_c = page_xhtml.clone();
    let patches_c = patches.clone();
    let res_hits_c = resource_hits.clone();
    let base_c = base_url.clone();
    let png_a_c = png_a.clone();
    let png_b_c = png_b.clone();
    let png_beta_c = png_beta.clone();

    let app = Router::new().fallback(move |req: Request| {
        let xhtml = xhtml_c.clone();
        let patch_log = patches_c.clone();
        let res_hits = res_hits_c.clone();
        let base = base_c.clone();
        let bytes_a = png_a_c.clone();
        let bytes_b = png_b_c.clone();
        let bytes_beta = png_beta_c.clone();
        async move {
            let method = req.method().as_str().to_string();
            let path = req.uri().path().to_string();
            let ct = req
                .headers()
                .get("content-type")
                .and_then(|v| v.to_str().ok())
                .unwrap_or("")
                .to_string();
            let body = axum::body::to_bytes(req.into_body(), 2 * 1024 * 1024)
                .await
                .unwrap();

            match (method.as_str(), path.as_str()) {
                ("GET", "/v1.0/me/onenote/notebooks") => Json(json!({
                    "value": [{
                        "id": "nb-personal",
                        "displayName": "Personal",
                        "isDefault": true,
                        "isShared": false
                    }]
                }))
                .into_response(),
                ("GET", "/v1.0/me/joinedTeams") => Json(json!({
                    "value": [{ "id": "grp-ops", "displayName": "Ops Team" }]
                }))
                .into_response(),
                ("GET", "/v1.0/groups/grp-ops/onenote/notebooks") => Json(json!({
                    "value": [{
                        "id": "nb-ops",
                        "displayName": "Ops Notebook",
                        "isDefault": false,
                        "isShared": true
                    }]
                }))
                .into_response(),
                ("GET", "/v1.0/me/onenote/notebooks/nb-personal/sectionGroups")
                | ("GET", "/v1.0/groups/grp-ops/onenote/notebooks/nb-ops/sectionGroups")
                | ("GET", "/v1.0/me/onenote/sectionGroups") => {
                    Json(json!({ "value": [] })).into_response()
                }
                ("GET", "/v1.0/me/onenote/sections")
                | ("GET", "/v1.0/me/onenote/notebooks/nb-personal/sections") => Json(json!({
                    "value": [
                        {
                            "id": "sec-alpha",
                            "displayName": "Alpha",
                            "isDefault": true,
                            "parentNotebook": { "id": "nb-personal", "displayName": "Personal" }
                        },
                        {
                            "id": "sec-beta",
                            "displayName": "Beta",
                            "isDefault": false,
                            "parentNotebook": { "id": "nb-personal", "displayName": "Personal" }
                        }
                    ]
                }))
                .into_response(),
                ("GET", "/v1.0/groups/grp-ops/onenote/sections")
                | ("GET", "/v1.0/groups/grp-ops/onenote/notebooks/nb-ops/sections") => {
                    Json(json!({ "value": [] })).into_response()
                }
                ("GET", "/v1.0/me/onenote/sections/sec-alpha/pages") => Json(json!({
                    "value": [
                        {
                            "id": "page-1",
                            "title": "API Page",
                            "contentUrl": format!("{base}/me/onenote/pages/page-1/content"),
                            "parentSection": { "id": "sec-alpha", "displayName": "Alpha" }
                        },
                        {
                            "id": "page-sa",
                            "title": "Standup notes",
                            "contentUrl": format!("{base}/me/onenote/pages/page-sa/content"),
                            "parentSection": { "id": "sec-alpha", "displayName": "Alpha" }
                        }
                    ]
                }))
                .into_response(),
                ("GET", "/v1.0/me/onenote/sections/sec-beta/pages") => Json(json!({
                    "value": [{
                        "id": "page-sb",
                        "title": "Standup notes",
                        "contentUrl": format!("{base}/me/onenote/pages/page-sb/content"),
                        "parentSection": { "id": "sec-beta", "displayName": "Beta" }
                    }]
                }))
                .into_response(),
                ("GET", p) if p.starts_with("/v1.0/me/onenote/pages/") && p.ends_with("/content") => {
                    let id = p
                        .trim_start_matches("/v1.0/me/onenote/pages/")
                        .trim_end_matches("/content");
                    match xhtml.lock().unwrap().get(id).cloned() {
                        Some(html) => (StatusCode::OK, [("content-type", "text/html")], html)
                            .into_response(),
                        None => StatusCode::NOT_FOUND.into_response(),
                    }
                }
                ("GET", p) if p.starts_with("/v1.0/me/onenote/pages/") => {
                    let id = p.trim_start_matches("/v1.0/me/onenote/pages/");
                    let (title, sec_id, sec_name) = match id {
                        "page-1" => ("API Page", "sec-alpha", "Alpha"),
                        "page-sa" => ("Standup notes", "sec-alpha", "Alpha"),
                        "page-sb" => ("Standup notes", "sec-beta", "Beta"),
                        _ => return StatusCode::NOT_FOUND.into_response(),
                    };
                    Json(json!({
                        "id": id,
                        "title": title,
                        "parentSection": { "id": sec_id, "displayName": sec_name }
                    }))
                    .into_response()
                }
                ("PATCH", p) if p.starts_with("/v1.0/me/onenote/pages/") && p.ends_with("/content") => {
                    let id = p
                        .trim_start_matches("/v1.0/me/onenote/pages/")
                        .trim_end_matches("/content")
                        .to_string();
                    let raw = String::from_utf8_lossy(&body).into_owned();
                    patch_log.lock().unwrap().push((id.clone(), ct, raw.clone()));
                    let mut map = xhtml.lock().unwrap();
                    if let Some(html) = map.get_mut(&id) {
                        if raw.contains("to-do:completed") {
                            *html = html.replace(r#"data-tag="to-do""#, r#"data-tag="to-do:completed""#);
                        } else if raw.contains(r#"data-tag=\"to-do\""#) {
                            *html = html.replace(r#"data-tag="to-do:completed""#, r#"data-tag="to-do""#);
                        }
                    }
                    StatusCode::NO_CONTENT.into_response()
                }
                ("GET", p) if p.starts_with("/v1.0/me/onenote/resources/") => {
                    res_hits.lock().unwrap().push(p.to_string());
                    let data = if p.contains("res-1") {
                        bytes_a
                    } else if p.contains("res-2") {
                        bytes_b
                    } else if p.contains("res-beta") {
                        bytes_beta
                    } else {
                        bytes_a
                    };
                    (StatusCode::OK, [("content-type", "image/png")], data).into_response()
                }
                _ => StatusCode::NOT_FOUND.into_response(),
            }
        }
    });
    let graph_server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let dir = tempfile::tempdir().unwrap();
    let (cfg, graph, storage) = make_test_runtime(base_url.clone(), dir.path());
    let state = Arc::new(ServeState {
        config: cfg.clone(),
        graph: graph.clone(),
        storage: storage.clone(),
        api_key: None,
    });
    let api_listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = api_listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(api_listener, serve::build_router(state)).await;
    });
    let http = reqwest::Client::new();

    // 1. Both documented `/todo` and legacy `/toggle-todo`:
    //    send [true, true, false, false] starting from unchecked -> returns [true, true, false, false] with 2 actual PATCHes
    for route in [
        format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/todo"),
        format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/toggle-todo"),
    ] {
        let writes_before = patches.lock().unwrap().len();
        let mut results = Vec::new();
        for desired in [true, true, false, false] {
            let resp = http
                .post(&route)
                .json(&json!({ "block_id": "p:todo-1", "completed": desired }))
                .send()
                .await
                .unwrap();
            assert_eq!(resp.status(), StatusCode::OK, "route={route} desired={desired}");
            let val: Value = resp.json().await.unwrap();
            assert_eq!(val["ok"], true);
            assert_eq!(val["block_id"], "p:todo-1");
            results.push(val["completed"].as_bool().unwrap());
        }
        assert_eq!(results, vec![true, true, false, false], "route={route}");
        assert_eq!(
            patches.lock().unwrap().len() - writes_before,
            2,
            "route={route}: idempotent true,true,false,false must perform exactly 2 writes"
        );
    }

    // Omitting `completed` still toggles, and existing index/expected_content/force still work
    let writes_before = patches.lock().unwrap().len();
    let tog1: Value = http
        .post(format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/todo"))
        .json(&json!({ "index": 0 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(tog1["completed"], true);
    assert_eq!(patches.lock().unwrap().len(), writes_before + 1);

    let conflict_resp = http
        .post(format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/todo"))
        .json(&json!({
            "block_id": "p:todo-1",
            "completed": false,
            "expected_content": "Wrong baseline text"
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(conflict_resp.status(), StatusCode::CONFLICT);
    assert_eq!(patches.lock().unwrap().len(), writes_before + 1);

    let forced_resp = http
        .post(format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/todo"))
        .json(&json!({
            "block_id": "p:todo-1",
            "completed": false,
            "expected_content": "Wrong baseline text",
            "force": true
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(forced_resp.status(), StatusCode::OK);
    assert_eq!(patches.lock().unwrap().len(), writes_before + 2);

    // 2. Resource download: legacy route vs documented `/v1/onenote/resources/download`
    let out_legacy = dir.path().join("dl_legacy");
    let out_doc_idx = dir.path().join("dl_doc_idx");
    let out_doc_url = dir.path().join("dl_doc_url");
    let out_doc_both = dir.path().join("dl_doc_both");
    let out_reject = dir.path().join("dl_reject");
    let out_beta = dir.path().join("dl_beta");
    std::fs::create_dir_all(&out_reject).unwrap();

    let legacy_dl: Value = http
        .post(format!(
            "http://{api_addr}/v1/onenote/pages/me%7Cpage-1/resources/download"
        ))
        .json(&json!({ "index": 1, "out_dir": out_legacy.to_string_lossy() }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(legacy_dl["ok"], true);
    let legacy_bytes = std::fs::read(legacy_dl["path"].as_str().unwrap()).unwrap();
    assert_eq!(legacy_bytes, png_b);

    let doc_idx_resp = http
        .post(format!("http://{api_addr}/v1/onenote/resources/download"))
        .json(&json!({
            "page": "me|page-1",
            "index": 1,
            "out": out_doc_idx.to_string_lossy()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(doc_idx_resp.status(), StatusCode::OK);
    let doc_idx_dl: Value = doc_idx_resp.json().await.unwrap();
    assert_eq!(doc_idx_dl["ok"], true);
    assert_eq!(doc_idx_dl["mime_type"], legacy_dl["mime_type"]);
    assert_eq!(doc_idx_dl["size_bytes"], legacy_dl["size_bytes"]);
    assert_eq!(doc_idx_dl["is_image"], legacy_dl["is_image"]);
    assert_eq!(
        std::fs::read(doc_idx_dl["path"].as_str().unwrap()).unwrap(),
        legacy_bytes
    );

    let doc_url_dl: Value = http
        .post(format!("http://{api_addr}/v1/onenote/resources/download"))
        .json(&json!({
            "page": "me|page-1",
            "url": res2_url,
            "out": out_doc_url.to_string_lossy()
        }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(doc_url_dl["ok"], true);
    assert_eq!(
        std::fs::read(doc_url_dl["path"].as_str().unwrap()).unwrap(),
        legacy_bytes
    );

    // Matching index + url succeeds
    let doc_both_resp = http
        .post(format!("http://{api_addr}/v1/onenote/resources/download"))
        .json(&json!({
            "page": "me|page-1",
            "index": 1,
            "url": res2_url,
            "out": out_doc_both.to_string_lossy()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(doc_both_resp.status(), StatusCode::OK);
    let doc_both_dl: Value = doc_both_resp.json().await.unwrap();
    assert_eq!(
        std::fs::read(doc_both_dl["path"].as_str().unwrap()).unwrap(),
        legacy_bytes
    );

    // Mismatched index + url, unknown url, and duplicate out+out_dir are rejected with 0 downloads & 0 files written
    let hits_before_reject = resource_hits.lock().unwrap().len();
    let mismatch = http
        .post(format!("http://{api_addr}/v1/onenote/resources/download"))
        .json(&json!({
            "page": "me|page-1",
            "index": 0,
            "url": res2_url,
            "out": out_reject.to_string_lossy()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(mismatch.status(), StatusCode::BAD_REQUEST);

    let missing_url = http
        .post(format!("http://{api_addr}/v1/onenote/resources/download"))
        .json(&json!({
            "page": "me|page-1",
            "url": format!("{base_url}/me/onenote/resources/not-on-page/content"),
            "out": out_reject.to_string_lossy()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(missing_url.status(), StatusCode::NOT_FOUND);

    let dup_out = http
        .post(format!("http://{api_addr}/v1/onenote/resources/download"))
        .json(&json!({
            "page": "me|page-1",
            "index": 0,
            "out": out_reject.to_string_lossy(),
            "out_dir": out_reject.to_string_lossy()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(dup_out.status(), StatusCode::BAD_REQUEST);

    assert_eq!(
        resource_hits.lock().unwrap().len(),
        hits_before_reject,
        "rejected download requests must perform 0 resource downloads"
    );
    assert_eq!(
        std::fs::read_dir(&out_reject).unwrap().count(),
        0,
        "rejected download requests must write 0 files"
    );

    // Section disambiguation for duplicate page titles ("Standup notes" in Beta)
    let beta_dl_resp = http
        .post(format!("http://{api_addr}/v1/onenote/resources/download"))
        .json(&json!({
            "page": "Standup notes",
            "section": "Beta",
            "index": 0,
            "out": out_beta.to_string_lossy()
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(beta_dl_resp.status(), StatusCode::OK);
    let beta_dl: Value = beta_dl_resp.json().await.unwrap();
    assert_eq!(
        std::fs::read(beta_dl["path"].as_str().unwrap()).unwrap(),
        png_beta
    );

    // 3. Append `images` alias vs `image_paths` and duplicate-field 400
    let sample_img_file = dir.path().join("attach.png");
    std::fs::write(&sample_img_file, &png_a).unwrap();
    let img_str = sample_img_file.to_string_lossy().into_owned();

    let p_before = patches.lock().unwrap().len();
    let r_images = http
        .post(format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/append"))
        .json(&json!({ "markdown": "Photo via images", "images": [img_str] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r_images.status(), StatusCode::OK);

    let r_image_paths = http
        .post(format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/append"))
        .json(&json!({ "markdown": "Photo via image_paths", "image_paths": [img_str] }))
        .send()
        .await
        .unwrap();
    assert_eq!(r_image_paths.status(), StatusCode::OK);

    {
        let log = patches.lock().unwrap();
        assert_eq!(log.len(), p_before + 2);
        let (_, ct1, body1) = &log[p_before];
        let (_, ct2, body2) = &log[p_before + 1];
        assert!(
            ct1.starts_with("multipart/form-data; boundary="),
            "images alias must send multipart, got: {ct1}"
        );
        assert!(
            ct2.starts_with("multipart/form-data; boundary="),
            "image_paths must send multipart, got: {ct2}"
        );
        assert!(body1.contains("filename=\"attach.png\"") && body1.contains("name:img1"));
        assert!(body2.contains("filename=\"attach.png\"") && body2.contains("name:img1"));
    }

    let p_before_dup = patches.lock().unwrap().len();
    let r_dup = http
        .post(format!("http://{api_addr}/v1/onenote/pages/me%7Cpage-1/append"))
        .json(&json!({
            "markdown": "Both keys",
            "images": [img_str],
            "image_paths": [img_str]
        }))
        .send()
        .await
        .unwrap();
    assert_eq!(r_dup.status(), StatusCode::BAD_REQUEST);
    assert_eq!(patches.lock().unwrap().len(), p_before_dup);

    // 4. Notebooks `include_teams`: omitted and false return personal only; true includes group notebooks
    let nb_default: Value = http
        .get(format!("http://{api_addr}/v1/onenote/notebooks?refresh=true"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let default_ids: Vec<&str> = nb_default["notebooks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["id"].as_str())
        .collect();
    assert_eq!(default_ids, vec!["nb-personal"]);

    let nb_false: Value = http
        .get(format!(
            "http://{api_addr}/v1/onenote/notebooks?include_teams=false&refresh=true"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let false_ids: Vec<&str> = nb_false["notebooks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["id"].as_str())
        .collect();
    assert_eq!(false_ids, vec!["nb-personal"]);

    let nb_true: Value = http
        .get(format!(
            "http://{api_addr}/v1/onenote/notebooks?include_teams=true&refresh=true"
        ))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    let mut true_ids: Vec<&str> = nb_true["notebooks"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|n| n["id"].as_str())
        .collect();
    true_ids.sort();
    assert_eq!(true_ids, vec!["nb-ops", "nb-personal"]);

    // Verify docs/API.md and docs/openapi.yaml reflect include_teams default: false
    let api_md = std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/API.md"))
        .unwrap();
    assert!(
        api_md.contains("?include_teams=false"),
        "docs/API.md must document include_teams=false default"
    );
    let openapi_yaml =
        std::fs::read_to_string(concat!(env!("CARGO_MANIFEST_DIR"), "/docs/openapi.yaml"))
            .unwrap();
    let include_teams_block = openapi_yaml
        .split("name: include_teams")
        .nth(1)
        .expect("openapi.yaml must define include_teams");
    let block_prefix: String = include_teams_block.chars().take(120).collect();
    assert!(
        block_prefix.contains("default: false"),
        "docs/openapi.yaml must document include_teams default: false, got: {block_prefix}"
    );

    api_server.abort();
    graph_server.abort();
}
