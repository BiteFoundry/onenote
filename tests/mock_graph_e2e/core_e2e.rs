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
async fn e2e_onenote_crud_xhtml_fts5_resource_download() {
    let (base_url, mock_state, server) = spawn_mock_graph().await;
    let dir = tempfile::tempdir().unwrap();
    let (cfg, graph, storage) = make_test_runtime(base_url, dir.path());

    // 1. Notebooks, sections, pages
    let notebooks = cli::load_onenote_notebooks(&storage, &graph, false, true, false)
        .await
        .unwrap();
    assert_eq!(notebooks.len(), 1);
    assert_eq!(notebooks[0].display_name, "Engineering");

    let (_nb, sections) = cli::load_onenote_sections(&storage, &graph, Some("Engineering"), true, false)
        .await
        .unwrap();
    assert_eq!(sections.len(), 1);
    assert_eq!(sections[0].display_name, "Specs");

    let (_sec, pages) = cli::list_onenote_pages_helper(&storage, &graph, "Engineering/Specs", 20, true, false)
        .await
        .unwrap();
    assert_eq!(pages.len(), 1);
    assert_eq!(pages[0].title, "Architecture Spec");

    // 2. Read page detail with parsed blocks, todo, and embedded image resource
    let detail = cli::load_onenote_page_detail(&storage, &graph, "me|page-1", false)
        .await
        .unwrap();
    assert_eq!(detail.summary.title, "Architecture Spec");
    assert!(detail.summary.has_todo);
    assert_eq!(detail.resources.len(), 1);
    assert!(detail.resources[0].is_image);

    // 3. FTS5 search finds cached page body
    let hits = cli::search_onenote_helper(&storage, &graph, "standalone", None, false, 10)
        .await
        .unwrap();
    assert_eq!(hits.len(), 1);
    assert_eq!(hits[0].page_id, "page-1");

    // 4. Surgical to-do toggle PATCH
    let (after_todo, block_id, completed) =
        cli::toggle_onenote_todo_helper(&storage, &graph, "me|page-1", None, Some(0))
            .await
            .unwrap();
    assert!(completed);
    assert!(block_id.contains("todo-1"));
    assert!(after_todo.raw_xhtml.contains("to-do:completed"));

    // 5. Surgical block update & append
    let after_update = cli::update_onenote_page_helper(
        &storage,
        &graph,
        "me|page-1",
        None,
        Some("Updated Block Copy"),
        Some("p:{body-1}"),
    )
    .await
    .unwrap();
    assert!(after_update.body_plain.contains("Updated Block Copy"));

    let after_append = cli::append_onenote_page_helper(
        &storage,
        &graph,
        "me|page-1",
        "Appended Section",
        &[],
    )
    .await
    .unwrap();
    assert!(after_append.body_plain.contains("Appended Section"));

    // 6. Resource download returns canonical `{ ok, path, file_name, mime_type, size_bytes, is_image }`
    let (_d, _idx, dl) =
        cli::download_onenote_resource_helper(&cfg, &storage, &graph, "me|page-1", Some(0), None)
            .await
            .unwrap();
    assert!(dl.ok);
    assert!(dl.is_image);
    assert_eq!(dl.mime_type, "image/png");
    assert!(dl.size_bytes > 0);
    assert!(std::path::Path::new(&dl.path).is_file());

    // 7. Create notebook, section, page, and delete page
    let created_nb = graph
        .create_onenote_notebook(&OnenoteScope::Me, "Personal")
        .await
        .unwrap();
    assert_eq!(created_nb.display_name, "Personal");

    let (_sec2, created_page) = cli::create_onenote_page_helper(
        &storage,
        &graph,
        "Specs",
        "Created E2E Page",
        "## Notes\n- [ ] Verify CI",
        Vec::new(),
        Some(0),
    )
    .await
    .unwrap();
    let del_key = cli::delete_onenote_page_helper(&storage, &graph, &created_page.page_key)
        .await
        .unwrap();
    assert_eq!(del_key, created_page.page_key);
    assert!(mock_state
        .deleted_pages
        .lock()
        .unwrap()
        .contains(&created_page.id));

    server.abort();
}

#[tokio::test]
async fn e2e_http_serve_and_mcp_tools() {
    let (base_url, mock, graph_server) = spawn_mock_graph().await;
    let dir = tempfile::tempdir().unwrap();
    let (cfg, graph, storage) = make_test_runtime(base_url, dir.path());

    // --- Part A: HTTP API (`onenote serve`) ---
    let state = Arc::new(ServeState {
        config: cfg.clone(),
        graph: graph.clone(),
        storage: storage.clone(),
        api_key: Some("secret-key".to_string()),
    });
    let app = serve::build_router(state);
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let api_addr = listener.local_addr().unwrap();
    let api_server = tokio::spawn(async move {
        let _ = axum::serve(listener, app).await;
    });

    let http = reqwest::Client::new();
    let health: Value = http
        .get(format!("http://{api_addr}/health"))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(health["ok"], true);
    assert_eq!(health["service"], "onenote-cli");

    // Unauthorized without API key
    let unauth = http
        .get(format!("http://{api_addr}/v1/onenote/notebooks"))
        .send()
        .await
        .unwrap();
    assert_eq!(unauth.status(), StatusCode::UNAUTHORIZED);

    // Authorized notebooks, sections, pages, resource download
    let nb_resp: Value = http
        .get(format!("http://{api_addr}/v1/onenote/notebooks?refresh=true"))
        .header("x-api-key", "secret-key")
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(nb_resp["notebooks"].as_array().unwrap().len(), 1);

    let dl_resp: Value = http
        .post(format!(
            "http://{api_addr}/v1/onenote/pages/me%7Cpage-1/resources/download"
        ))
        .header("Authorization", "Bearer secret-key")
        .json(&json!({ "index": 0 }))
        .send()
        .await
        .unwrap()
        .json()
        .await
        .unwrap();
    assert_eq!(dl_resp["ok"], true);
    assert_eq!(dl_resp["is_image"], true);
    assert_eq!(dl_resp["mime_type"], "image/png");
    assert!(dl_resp["size_bytes"].as_u64().unwrap() > 0);
    assert!(dl_resp["path"].as_str().is_some());
    assert!(dl_resp["file_name"].as_str().is_some());

    let original_pages = mock.pages.lock().unwrap().clone();
    let original_xhtml = mock.page_xhtml.lock().unwrap().clone();
    let unsupported = http
        .post(format!("http://{api_addr}/v1/onenote/pages"))
        .header("Authorization", "Bearer secret-key")
        .json(&json!({ "section": "me|sec-1", "title": "Subpage", "level": 1 }))
        .send()
        .await
        .unwrap();
    assert_eq!(unsupported.status(), StatusCode::BAD_REQUEST);
    let error: Value = unsupported.json().await.unwrap();
    assert!(error["error"].as_str().unwrap().contains("top-level"));

    let unsupported = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({
            "jsonrpc": "2.0", "id": 10, "method": "tools/call",
            "params": { "name": "create_page", "arguments": {
                "section": "me|sec-1", "title": "Subpage", "level": 2
            }}
        }),
    )
    .await
    .unwrap();
    assert_eq!(unsupported["result"]["isError"], true);
    assert!(unsupported["result"]["content"][0]["text"]
        .as_str()
        .unwrap()
        .contains("top-level"));
    assert_eq!(*mock.pages.lock().unwrap(), original_pages);
    assert_eq!(*mock.page_xhtml.lock().unwrap(), original_xhtml);
    assert!(mock.patches_received.lock().unwrap().is_empty());

    // --- Part B: MCP JSON-RPC 2.0 server ---
    let init = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({ "jsonrpc": "2.0", "id": 1, "method": "initialize", "params": {} }),
    )
    .await
    .unwrap();
    assert_eq!(init["result"]["serverInfo"]["name"], "onenote-cli");

    let listed = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({ "jsonrpc": "2.0", "id": 2, "method": "tools/list", "params": {} }),
    )
    .await
    .unwrap();
    let tool_names: Vec<&str> = listed["result"]["tools"]
        .as_array()
        .unwrap()
        .iter()
        .filter_map(|t| t["name"].as_str())
        .collect();
    for expected in [
        "whoami",
        "list_notebooks",
        "list_sections",
        "list_pages",
        "get_page",
        "create_notebook",
        "create_section",
        "rename_section",
        "create_page",
        "append_page",
        "update_block",
        "update_page",
        "toggle_todo",
        "delete_page",
        "search_pages",
        "download_resource",
    ] {
        assert!(
            tool_names.contains(&expected),
            "missing required MCP tool: {expected}"
        );
    }

    let patch_start = mock.patches_received.lock().unwrap().len();
    for (requested, expected, writes) in [
        (Some(true), true, 1),
        (Some(true), true, 1),
        (Some(false), false, 2),
        (None, true, 3),
    ] {
        let mut arguments =
            json!({ "page": "me|page-1", "block_id": "p:{todo-1}", "compact": false });
        if let Some(state) = requested {
            arguments["completed"] = json!(state);
        }
        let response = mcp::handle_request_value(
            &cfg,
            &graph,
            &storage,
            json!({
                "jsonrpc": "2.0", "id": 9, "method": "tools/call",
                "params": { "name": "toggle_todo", "arguments": arguments }
            }),
        )
        .await
        .unwrap();
        assert_eq!(response["result"]["isError"], false, "{response}");
        let value = &response["result"]["structuredContent"];
        assert_eq!(value["completed"], expected);
        assert_eq!(
            value["page"]["body_markdown"]
                .as_str()
                .unwrap()
                .contains("- [x]"),
            expected
        );
        assert_eq!(
            mock.patches_received.lock().unwrap().len() - patch_start,
            writes
        );
    }

    // Call MCP `download_resource` and check canonical JSON fields
    let mcp_dl = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 3,
            "method": "tools/call",
            "params": {
                "name": "download_resource",
                "arguments": { "page": "me|page-1", "index": 0 }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_dl["result"]["isError"], false);
    let sc = &mcp_dl["result"]["structuredContent"];
    assert_eq!(sc["ok"], true);
    assert_eq!(sc["is_image"], true);
    assert_eq!(sc["mime_type"], "image/png");
    assert!(sc["size_bytes"].as_u64().unwrap() > 0);
    assert!(std::path::Path::new(sc["path"].as_str().unwrap()).is_file());

    // Call MCP `search_pages`
    let mcp_search = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 4,
            "method": "tools/call",
            "params": {
                "name": "search_pages",
                "arguments": { "query": "Architecture" }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_search["result"]["isError"], false);
    assert!(
        !mcp_search["result"]["structuredContent"]["hits"]
            .as_array()
            .unwrap()
            .is_empty()
    );

    // Call MCP `delete_page` without confirm: true -> must return isError: true
    let mcp_del_guard = mcp::handle_request_value(
        &cfg,
        &graph,
        &storage,
        json!({
            "jsonrpc": "2.0",
            "id": 5,
            "method": "tools/call",
            "params": {
                "name": "delete_page",
                "arguments": { "page": "me|page-1", "confirm": false }
            }
        }),
    )
    .await
    .unwrap();
    assert_eq!(mcp_del_guard["result"]["isError"], true);

    api_server.abort();
    graph_server.abort();
}

#[tokio::test]
async fn e2e_cli_default_zero_ttl_fetches_fresh_and_cached_flag_uses_cache() {
    let (base_url, mock_state, server) = spawn_mock_graph().await;
    let dir = tempfile::tempdir().unwrap();
    let (cfg, graph, storage) = make_test_runtime(base_url.clone(), dir.path());
    assert_eq!(cfg.cli_cache_ttl_secs, 0);

    // 1. Default invocation (cli_cache_ttl_secs = 0, refresh = false, cached = false) fetches 1 notebook
    let first = cli::load_onenote_notebooks_with_cache(&storage, &graph, false, false, false, false)
        .await
        .unwrap();
    assert_eq!(first.len(), 1);
    assert_eq!(*mock_state.notebook_list_hits.lock().unwrap(), 1);

    // 2. Add a 2nd personal notebook on MockGraph; next default call MUST fetch fresh without --refresh
    mock_state.notebooks.lock().unwrap().push(json!({
        "id": "nb-2",
        "displayName": "Product Specs",
        "isDefault": false,
        "isShared": false,
        "userRole": "Owner",
        "createdDateTime": "2026-11-19T00:00:00Z",
        "lastModifiedDateTime": "2026-11-19T10:00:00Z"
    }));
    let second =
        cli::load_onenote_notebooks_with_cache(&storage, &graph, false, false, false, false)
            .await
            .unwrap();
    assert_eq!(second.len(), 2);
    assert_eq!(*mock_state.notebook_list_hits.lock().unwrap(), 2);

    // 3. Add a 3rd notebook on MockGraph; --cached returns cached 2 immediately without hitting Graph
    mock_state.notebooks.lock().unwrap().push(json!({
        "id": "nb-3",
        "displayName": "Design Notes",
        "isDefault": false,
        "isShared": false,
        "userRole": "Owner"
    }));
    let from_cache =
        cli::load_onenote_notebooks_with_cache(&storage, &graph, false, false, false, true)
            .await
            .unwrap();
    assert_eq!(from_cache.len(), 2);
    assert_eq!(*mock_state.notebook_list_hits.lock().unwrap(), 2);

    // 4. --cached on an EMPTY cache falls back to Graph (unlike --offline which stays network-free)
    let empty_storage = Storage::open_in_memory().unwrap();
    let offline_empty = cli::load_onenote_notebooks_with_cache(
        &empty_storage,
        &graph,
        false,
        false,
        true,
        false,
    )
    .await
    .unwrap();
    assert!(offline_empty.is_empty());
    assert_eq!(*mock_state.notebook_list_hits.lock().unwrap(), 2);

    let cached_fallback = cli::load_onenote_notebooks_with_cache(
        &empty_storage,
        &graph,
        false,
        false,
        false,
        true,
    )
    .await
    .unwrap();
    assert_eq!(cached_fallback.len(), 3);
    assert_eq!(*mock_state.notebook_list_hits.lock().unwrap(), 3);

    // 5. Non-zero TTL serves personal cache, while --include-teams still fetches group notebooks!
    mock_state.joined_teams.lock().unwrap().push(json!({
        "id": "grp-1",
        "displayName": "Platform Team"
    }));
    mock_state.group_notebooks.lock().unwrap().insert(
        "grp-1".to_string(),
        vec![json!({
            "id": "nb-team-1",
            "displayName": "Platform Team Notebook",
            "isDefault": false,
            "isShared": true,
            "userRole": "Contributor"
        })],
    );

    let mut ttl_cfg = cfg.clone();
    ttl_cfg.cli_cache_ttl_secs = 300;
    let ttl_tokens = StoredTokens {
        access_token: "e2e-test-token".to_string(),
        refresh_token: None,
        expires_at: chrono::Utc::now().timestamp() + 3600,
        client_id: "cid".to_string(),
        tenant_id: "tid".to_string(),
        user_id: Some("user-1".to_string()),
        user_display_name: Some("Alice Engineer".to_string()),
        user_email: Some("alice@contoso.com".to_string()),
        auth_mode: "access_token".to_string(),
    };
    let ttl_auth = AuthManager::from_tokens(ttl_cfg.clone(), ttl_tokens);
    let ttl_graph = GraphClient::new(ttl_cfg, ttl_auth)
        .unwrap()
        .with_my_user_id("user-1");
    let ttl_storage = Storage::open_in_memory().unwrap();

    // Fetch personal notebooks (populates personal cursor only)
    let personal_nbs =
        cli::load_onenote_notebooks(&ttl_storage, &ttl_graph, false, false, false)
            .await
            .unwrap();
    assert_eq!(personal_nbs.len(), 3);
    let hits_after_personal = *mock_state.notebook_list_hits.lock().unwrap();

    // Repeat personal call within TTL -> cache hit (no extra Graph request)
    let personal_cached =
        cli::load_onenote_notebooks(&ttl_storage, &ttl_graph, false, false, false)
            .await
            .unwrap();
    assert_eq!(personal_cached.len(), 3);
    assert_eq!(
        *mock_state.notebook_list_hits.lock().unwrap(),
        hits_after_personal
    );

    // Now request --include-teams -> must fetch group notebooks instead of returning personal-only cache
    let with_teams =
        cli::load_onenote_notebooks(&ttl_storage, &ttl_graph, true, false, false)
            .await
            .unwrap();
    assert_eq!(with_teams.len(), 4);
    assert!(with_teams.iter().any(|n| n.id == "nb-team-1"));
    assert_eq!(
        *mock_state.notebook_list_hits.lock().unwrap(),
        hits_after_personal + 1
    );

    server.abort();
}


/// NEW-01: the HTTP API must not be reachable from a web page (cross-origin
/// requests, DNS rebinding) and must not act without the API key.
#[tokio::test]
async fn e2e_http_serve_guard_blocks_cross_site_and_rebinding() {
    let (base_url, mock, graph_server) = spawn_mock_graph().await;
    let dir = tempfile::tempdir().unwrap();
    let (cfg, graph, storage) = make_test_runtime(base_url, dir.path());
    let state = |api_key: Option<&str>| {
        Arc::new(ServeState {
            config: cfg.clone(),
            graph: graph.clone(),
            storage: storage.clone(),
            api_key: api_key.map(str::to_string),
        })
    };
    async fn spawn_api(app: Router) -> (std::net::SocketAddr, tokio::task::JoinHandle<()>) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        (addr, handle)
    }
    let http = reqwest::Client::new();

    // --- Default (loopback) guard with a key ---
    let (addr, keyed_server) = spawn_api(serve::build_router(state(Some("secret-key")))).await;

    // (a) Preflight from a foreign origin gets no CORS grant.
    let preflight = http
        .request(
            reqwest::Method::OPTIONS,
            format!("http://{addr}/v1/onenote/pages/me%7Cpage-1"),
        )
        .header("Origin", "https://evil.example")
        .header("Access-Control-Request-Method", "DELETE")
        .header("Access-Control-Request-Headers", "authorization")
        .send()
        .await
        .unwrap();
    assert_eq!(preflight.status(), StatusCode::FORBIDDEN);
    assert!(preflight
        .headers()
        .get("access-control-allow-origin")
        .is_none());

    // (b) DNS rebinding: an attacker-controlled Host is refused, key or not.
    for host in ["evil.example", "127.0.0.1.evil.example:8793"] {
        let rebound = http
            .get(format!("http://{addr}/v1/onenote/notebooks"))
            .header("Host", host)
            .header("x-api-key", "secret-key")
            .send()
            .await
            .unwrap();
        assert_eq!(rebound.status(), StatusCode::MISDIRECTED_REQUEST, "{host}");
        let body: Value = rebound.json().await.unwrap();
        assert_eq!(body["ok"], false);
    }
    let rebound_health = http
        .get(format!("http://{addr}/health"))
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(rebound_health.status(), StatusCode::MISDIRECTED_REQUEST);

    // (c) A "simple" cross-site GET is refused even with a valid key, and
    // never reaches Graph.
    let hits_before = *mock.notebook_list_hits.lock().unwrap();
    let cross_site = http
        .get(format!("http://{addr}/v1/onenote/notebooks?refresh=true"))
        .header("Origin", "https://evil.example")
        .header("x-api-key", "secret-key")
        .send()
        .await
        .unwrap();
    assert_eq!(cross_site.status(), StatusCode::FORBIDDEN);
    assert!(cross_site
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    assert_eq!(*mock.notebook_list_hits.lock().unwrap(), hits_before);

    // (d) Destructive call without (or with a wrong) key: 401, nothing deleted.
    for key in [None, Some("secret-kez"), Some("")] {
        let mut request = http.delete(format!("http://{addr}/v1/onenote/pages/me%7Cpage-1"));
        if let Some(key) = key {
            request = request.header("Authorization", format!("Bearer {key}"));
        }
        assert_eq!(
            request.send().await.unwrap().status(),
            StatusCode::UNAUTHORIZED
        );
    }
    assert!(mock.deleted_pages.lock().unwrap().is_empty());

    // (e) /health stays key-free for a valid Host (IP and name).
    let health = http
        .get(format!("http://{addr}/health"))
        .send()
        .await
        .unwrap();
    assert_eq!(health.status(), StatusCode::OK);
    assert!(health
        .headers()
        .get("access-control-allow-origin")
        .is_none());
    let health_by_name = http
        .get(format!("http://{addr}/health"))
        .header("Host", format!("LOCALHOST:{}", addr.port()))
        .send()
        .await
        .unwrap();
    assert_eq!(health_by_name.status(), StatusCode::OK);
    keyed_server.abort();

    // An empty configured key must not mean "no key".
    let (addr, empty_key_server) = spawn_api(serve::build_router(state(Some("")))).await;
    let empty = http
        .get(format!("http://{addr}/v1/onenote/notebooks"))
        .header("x-api-key", "")
        .send()
        .await
        .unwrap();
    assert_eq!(empty.status(), StatusCode::UNAUTHORIZED);
    empty_key_server.abort();

    // A keyless router (`--allow-keyless`) keeps the Host and Origin checks.
    let (addr, keyless_server) = spawn_api(serve::build_router(state(None))).await;
    let keyless_ok = http
        .get(format!("http://{addr}/v1/onenote/notebooks"))
        .send()
        .await
        .unwrap();
    assert_eq!(keyless_ok.status(), StatusCode::OK);
    let keyless_rebound = http
        .get(format!("http://{addr}/v1/onenote/notebooks"))
        .header("Host", "evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(keyless_rebound.status(), StatusCode::MISDIRECTED_REQUEST);
    let keyless_cross_site = http
        .delete(format!("http://{addr}/v1/onenote/pages/me%7Cpage-1"))
        .header("Origin", "https://evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(keyless_cross_site.status(), StatusCode::FORBIDDEN);
    assert!(mock.deleted_pages.lock().unwrap().is_empty());
    keyless_server.abort();

    // (f) `--cors-origin` echoes exactly the configured origin and nothing else.
    let guard = serve::ServeGuard::new(
        "127.0.0.1",
        &["notes.corp.example".to_string()],
        &["https://app.example.com".to_string()],
    )
    .unwrap();
    let (addr, cors_server) = spawn_api(serve::build_router_with_guard(
        state(Some("secret-key")),
        guard,
    ))
    .await;
    let allowed = http
        .get(format!("http://{addr}/v1/onenote/notebooks"))
        .header("Origin", "https://app.example.com")
        .header("x-api-key", "secret-key")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed.status(), StatusCode::OK);
    assert_eq!(
        allowed.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );
    let allowed_preflight = http
        .request(
            reqwest::Method::OPTIONS,
            format!("http://{addr}/v1/onenote/pages/me%7Cpage-1"),
        )
        .header("Origin", "https://app.example.com")
        .header("Access-Control-Request-Method", "DELETE")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed_preflight.status(), StatusCode::NO_CONTENT);
    assert_eq!(
        allowed_preflight.headers()["access-control-allow-origin"],
        "https://app.example.com"
    );
    assert!(allowed_preflight.headers()["access-control-allow-methods"]
        .to_str()
        .unwrap()
        .contains("DELETE"));
    // The allowed origin still needs the key.
    let allowed_no_key = http
        .get(format!("http://{addr}/v1/onenote/notebooks"))
        .header("Origin", "https://app.example.com")
        .send()
        .await
        .unwrap();
    assert_eq!(allowed_no_key.status(), StatusCode::UNAUTHORIZED);
    let other_origin = http
        .get(format!("http://{addr}/health"))
        .header("Origin", "https://app.example.com.evil.example")
        .send()
        .await
        .unwrap();
    assert_eq!(other_origin.status(), StatusCode::FORBIDDEN);
    let extra_host = http
        .get(format!("http://{addr}/health"))
        .header("Host", "notes.corp.example:8793")
        .send()
        .await
        .unwrap();
    assert_eq!(extra_host.status(), StatusCode::OK);
    cors_server.abort();

    graph_server.abort();
}
