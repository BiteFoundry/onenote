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
use onenote_cli::graph::copy::CopyPageStatus;

const SOURCE_XHTML: &str =
    "<html><head><title>Release note</title></head><body><p>Keep this.</p></body></html>";
const SOURCE_LINK: &str = "https://onenote.example/source-page";

#[derive(Clone, Copy, PartialEq, Eq)]
enum CopyBehavior {
    Complete,
    Fail,
    GatewayError,
    RedirectSubmission,
    ForeignOperation,
    RedirectOperation,
    WrongDestination,
}

#[derive(Default)]
struct CopyLog {
    requests: Vec<(String, String)>,
    polls: u32,
    copy_exists: bool,
}

async fn fixture(behavior: CopyBehavior) -> (GraphClient, Arc<Mutex<CopyLog>>) {
    let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
    let base = format!("http://{}/v1.0", listener.local_addr().unwrap());
    let log = Arc::new(Mutex::new(CopyLog::default()));
    let app = Router::new().fallback({
        let base = base.clone();
        let log = log.clone();
        move |request: Request| {
            let base = base.clone();
            let log = log.clone();
            async move {
                let method = request.method().as_str().to_owned();
                let path = request.uri().path().to_owned();
                assert_eq!(request.headers()["Authorization"], "Bearer e2e-test-token");
                let body = axum::body::to_bytes(request.into_body(), 4096).await.unwrap();
                let mut log = log.lock().unwrap();
                log.requests.push((method.clone(), path.clone()));
                match (method.as_str(), path.as_str()) {
                    ("POST", "/v1.0/me/onenote/pages/source/copyToSection") => {
                        assert_eq!(
                            serde_json::from_slice::<Value>(&body).unwrap(),
                            json!({"id":"target", "groupId":"team"})
                        );
                        match behavior {
                            CopyBehavior::GatewayError => StatusCode::BAD_GATEWAY.into_response(),
                            CopyBehavior::RedirectSubmission => (
                                StatusCode::TEMPORARY_REDIRECT,
                                [("Location", format!("{base}/unexpected"))],
                            ).into_response(),
                            CopyBehavior::ForeignOperation => (
                                StatusCode::ACCEPTED,
                                [("Operation-Location", "http://127.0.0.1:1/v1.0/me/onenote/operations/job"), ("Retry-After", "0")],
                            ).into_response(),
                            _ => (
                                StatusCode::ACCEPTED,
                                [("Operation-Location", "/v1.0/me/onenote/operations/job"), ("Retry-After", "0")],
                            ).into_response(),
                        }
                    }
                    ("GET", "/v1.0/me/onenote/operations/job") => {
                        log.polls += 1;
                        if behavior == CopyBehavior::RedirectOperation {
                            return (
                                StatusCode::TEMPORARY_REDIRECT,
                                [("Location", format!("{base}/unexpected"))],
                            ).into_response();
                        }
                        let payload = if log.polls == 1 {
                            json!({"id":"job","status":"Running"})
                        } else if behavior == CopyBehavior::Fail {
                            json!({"id":"job","status":"Failed","error":{"code":"copyDenied","message":"Permission denied"}})
                        } else {
                            log.copy_exists = true;
                            json!({
                                "id":"job", "status":"Completed",
                                "resourceLocation":format!("{base}/groups/team/onenote/pages/copy"),
                                "resourceId":"copy"
                            })
                        };
                        ([("Retry-After", "0")], Json(payload)).into_response()
                    }
                    ("GET", "/v1.0/me/onenote/pages/source") => Json(json!({
                        "id":"source","title":"Release note","parentSection":{"id":"original"},
                        "links":{"oneNoteWebUrl":{"href":SOURCE_LINK}}
                    })).into_response(),
                    ("GET", "/v1.0/me/onenote/pages/source/content") => SOURCE_XHTML.into_response(),
                    ("GET", "/v1.0/groups/team/onenote/pages/copy") if log.copy_exists => {
                        let section = if behavior == CopyBehavior::WrongDestination {"other"} else {"target"};
                        Json(json!({"id":"copy","title":"Release note","parentSection":{"id":section}})).into_response()
                    }
                    ("GET", "/v1.0/groups/team/onenote/pages/copy/content") if log.copy_exists => SOURCE_XHTML.into_response(),
                    _ => StatusCode::NOT_FOUND.into_response(),
                }
            }
        }
    });
    tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });
    let dir = tempfile::tempdir().unwrap();
    let (_, graph, _) = make_test_runtime(base, dir.path());
    (graph, log)
}

#[tokio::test]
async fn copy_resumes_and_preserves_source_link_title_and_body() {
    let (graph, log) = fixture(CopyBehavior::Complete).await;
    let source = OnenoteScope::Me;
    let destination = OnenoteScope::from_scope_key("group:team", None).unwrap();
    let original = graph
        .get_onenote_page_summary(&source, "source", None)
        .await
        .unwrap();
    let original_html = graph
        .get_onenote_page_content_xhtml(&source, "source")
        .await
        .unwrap();
    assert_eq!(original.web_url.as_deref(), Some(SOURCE_LINK));

    let pending = graph
        .copy_onenote_page("me|source", "group:team|target", None, 0)
        .await
        .unwrap();
    assert!(matches!(pending.status, CopyPageStatus::Pending));
    assert_eq!(pending.operation_id, "job");
    assert!(!pending.ok);
    assert_eq!(log.lock().unwrap().polls, 0);

    let copied = graph
        .copy_onenote_page(
            "me|source",
            "group:team|target",
            Some(&pending.operation_url),
            2,
        )
        .await
        .unwrap();
    assert!(matches!(copied.status, CopyPageStatus::Completed));
    assert!(copied.ok);
    assert_eq!(copied.source_page_id, "source");
    assert_eq!(copied.destination_section_id, "target");
    assert_eq!(copied.new_page_key.as_deref(), Some("group:team|copy"));
    assert_eq!(copied.new_page_id.as_deref(), Some("copy"));
    assert_eq!(copied.operation_url, pending.operation_url);

    let retained = graph
        .get_onenote_page_summary(&source, "source", None)
        .await
        .unwrap();
    let duplicate = graph
        .get_onenote_page_summary(&destination, "copy", None)
        .await
        .unwrap();
    assert_eq!(retained, original);
    assert_eq!(duplicate.title, original.title);
    assert_eq!(duplicate.section_id, "target");
    assert_eq!(
        graph
            .get_onenote_page_content_xhtml(&source, "source")
            .await
            .unwrap(),
        original_html
    );
    assert_eq!(
        graph
            .get_onenote_page_content_xhtml(&destination, "copy")
            .await
            .unwrap(),
        original_html
    );
    let log = log.lock().unwrap();
    assert_eq!(log.polls, 2);
    assert_eq!(
        log.requests
            .iter()
            .filter(|(method, _)| method != "GET")
            .count(),
        1
    );
}

#[tokio::test]
async fn copy_reports_terminal_failure_and_verification_failure_with_handles() {
    for (behavior, expected, error) in [
        (
            CopyBehavior::Fail,
            "failed",
            "copyDenied: Permission denied",
        ),
        (
            CopyBehavior::WrongDestination,
            "verification_failed",
            "expected page copy in section target",
        ),
        (CopyBehavior::RedirectOperation, "status_unavailable", "307"),
    ] {
        let (graph, log) = fixture(behavior).await;
        let outcome = graph
            .copy_onenote_page("source", "group:team|target", None, 2)
            .await
            .unwrap();
        assert_eq!(serde_json::to_value(&outcome).unwrap()["status"], expected);
        assert!(!outcome.ok);
        assert_eq!(outcome.operation_id, "job");
        assert!(outcome.operation_url.ends_with("/operations/job"));
        assert!(
            outcome.error.as_deref().unwrap().contains(error),
            "{outcome:?}"
        );
        assert!(outcome.new_page_id.is_none());
        assert_eq!(
            graph
                .get_onenote_page_content_xhtml(&OnenoteScope::Me, "source")
                .await
                .unwrap(),
            SOURCE_XHTML
        );
        let log = log.lock().unwrap();
        assert_eq!(
            log.requests
                .iter()
                .filter(|(method, _)| method != "GET")
                .count(),
            1
        );
        assert!(!log
            .requests
            .iter()
            .any(|(_, path)| path.ends_with("/unexpected")));
    }
}

#[tokio::test]
async fn copy_submission_does_not_replay_or_follow_foreign_and_redirected_locations() {
    for behavior in [
        CopyBehavior::GatewayError,
        CopyBehavior::RedirectSubmission,
        CopyBehavior::ForeignOperation,
    ] {
        let (graph, log) = fixture(behavior).await;
        let error = graph
            .copy_onenote_page("source", "group:team|target", None, 2)
            .await
            .unwrap_err();
        assert!(error
            .to_string()
            .contains("Check the destination before submitting another copy"));
        assert_eq!(log.lock().unwrap().requests.len(), 1);
    }

    let (graph, log) = fixture(CopyBehavior::Complete).await;
    let error = graph
        .copy_onenote_page(
            "source",
            "group:team|target",
            Some("http://127.0.0.1:1/v1.0/me/onenote/operations/job"),
            2,
        )
        .await
        .unwrap_err();
    assert!(error.to_string().contains("Untrusted Graph"));
    assert!(log.lock().unwrap().requests.is_empty());
}
