//! Native asynchronous OneNote page copies.

use std::time::Duration;

use anyhow::{bail, ensure, Context, Result};
use reqwest::header::{HeaderMap, RETRY_AFTER};
use serde::{Deserialize, Serialize};
use tokio::time::{sleep, timeout};
use url::Url;

use super::GraphClient;
use crate::models::{split_onenote_entity_key, OnenoteScope};

pub const DEFAULT_COPY_WAIT_SECONDS: u32 = 20;
pub const MAX_COPY_WAIT_SECONDS: u32 = 300;
const POLL_INTERVAL: Duration = Duration::from_secs(1);

#[derive(Clone, Copy, Debug, Serialize)]
#[serde(rename_all = "snake_case")]
pub enum CopyPageStatus {
    Completed,
    Pending,
    Failed,
    StatusUnavailable,
    VerificationFailed,
}

#[derive(Debug, Serialize)]
pub struct CopyPageOutcome {
    pub ok: bool,
    pub status: CopyPageStatus,
    pub source_page_id: String,
    pub source_page_key: String,
    pub destination_section_id: String,
    pub destination_section_key: String,
    pub operation_id: String,
    pub operation_url: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_page_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub new_page_key: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct GraphOperation {
    id: Option<String>,
    status: String,
    resource_id: Option<String>,
    resource_location: Option<String>,
    error: Option<OperationError>,
}

#[derive(Deserialize)]
struct OperationError {
    code: Option<String>,
    message: Option<String>,
}

#[derive(Deserialize)]
#[serde(rename_all = "camelCase")]
struct PageIdentity {
    id: String,
    parent_section: SectionIdentity,
}

#[derive(Deserialize)]
struct SectionIdentity {
    id: String,
}

impl GraphClient {
    /// Submit a native copy once, or resume a previously accepted operation by URL.
    pub async fn copy_onenote_page(
        &self,
        page: &str,
        section: &str,
        operation_url: Option<&str>,
        wait_seconds: u32,
    ) -> Result<CopyPageOutcome> {
        ensure!(
            wait_seconds <= MAX_COPY_WAIT_SECONDS,
            "Copy wait must be between 0 and {MAX_COPY_WAIT_SECONDS} seconds"
        );
        let (source_scope, source_id) = copy_entity(page, "page")?;
        let (destination_scope, section_id) = copy_entity(section, "section")?;
        let base = Url::parse(&format!(
            "{}/",
            self.config().graph_base_url.trim_end_matches('/')
        ))?;

        let (operation_url, operation_id, delay) = if let Some(location) = operation_url {
            let (url, id) = entity_location(&base, location, "operations")?;
            (url, id, Duration::ZERO)
        } else {
            let path = format!(
                "{}/pages/{}/copyToSection",
                source_scope.graph_base_path(),
                urlencoding::encode(&source_id)
            );
            let mut body = serde_json::json!({ "id": section_id });
            if let OnenoteScope::Group { group_id, .. } = &destination_scope {
                body["groupId"] = serde_json::json!(group_id);
            }
            let headers = self.start_operation(&path, body).await.map_err(|err| {
                anyhow::anyhow!(
                    "Copy submission returned no usable operation handle ({err:#}). Check the destination before submitting another copy."
                )
            })?;
            let location = headers
                .get("Operation-Location")
                .and_then(|value| value.to_str().ok())
                .context("Graph accepted the copy without a usable Operation-Location. Check the destination before submitting another copy.")?;
            let (url, id) = entity_location(&base, location, "operations").map_err(|err| {
                anyhow::anyhow!("Graph accepted the copy but its Operation-Location could not be followed ({err}). Check the destination before submitting another copy.")
            })?;
            (url, id, poll_delay(&headers))
        };

        let mut outcome = CopyPageOutcome {
            ok: false,
            status: CopyPageStatus::Pending,
            source_page_key: source_scope.make_entity_key(&source_id),
            source_page_id: source_id,
            destination_section_key: destination_scope.make_entity_key(&section_id),
            destination_section_id: section_id,
            operation_id,
            operation_url: operation_url.to_string(),
            new_page_id: None,
            new_page_key: None,
            error: None,
        };
        if wait_seconds == 0 {
            return Ok(outcome);
        }

        let poll = self.poll_copy_operation(&operation_url, &outcome.operation_id, delay);
        let operation = match timeout(Duration::from_secs(wait_seconds.into()), poll).await {
            Err(_) => return Ok(outcome),
            Ok(Ok(operation)) => operation,
            Ok(Err(err)) => {
                outcome.status = CopyPageStatus::StatusUnavailable;
                outcome.error = Some(format!("{err:#}"));
                return Ok(outcome);
            }
        };

        if operation.status.eq_ignore_ascii_case("failed") {
            outcome.status = CopyPageStatus::Failed;
            outcome.error = Some(match operation.error {
                Some(error) => [error.code, error.message]
                    .into_iter()
                    .flatten()
                    .collect::<Vec<_>>()
                    .join(": "),
                None => "Graph reported a failed copy".to_owned(),
            });
            return Ok(outcome);
        }

        match self
            .verify_copied_page(&base, operation, &destination_scope, &outcome)
            .await
        {
            Ok(id) => {
                outcome.ok = true;
                outcome.status = CopyPageStatus::Completed;
                outcome.new_page_key = Some(destination_scope.make_entity_key(&id));
                outcome.new_page_id = Some(id);
            }
            Err(err) => {
                outcome.status = CopyPageStatus::VerificationFailed;
                outcome.error = Some(format!("{err:#}"));
            }
        }
        Ok(outcome)
    }

    async fn poll_copy_operation(
        &self,
        url: &Url,
        id: &str,
        mut delay: Duration,
    ) -> Result<GraphOperation> {
        loop {
            if !delay.is_zero() {
                // Cap the sleep below Instant's range. The caller's poll deadline can be shorter.
                sleep(delay.min(Duration::from_secs(u64::from(MAX_COPY_WAIT_SECONDS) + 1))).await;
            }
            let (operation, headers) = self
                .get_operation_json::<GraphOperation>(url.as_str())
                .await?;
            ensure!(
                operation
                    .id
                    .as_deref()
                    .is_none_or(|returned| returned == id),
                "Graph operation ID did not match Operation-Location"
            );
            match operation.status.to_ascii_lowercase().as_str() {
                "completed" | "failed" => return Ok(operation),
                "notstarted" | "running" | "inprogress" => delay = poll_delay(&headers),
                status => bail!("Unrecognized Graph operation status {status:?}"),
            }
        }
    }

    async fn verify_copied_page(
        &self,
        graph_base: &Url,
        operation: GraphOperation,
        scope: &OnenoteScope,
        outcome: &CopyPageOutcome,
    ) -> Result<String> {
        let location_id = operation
            .resource_location
            .as_deref()
            .map(|location| entity_location(graph_base, location, "pages").map(|(_, id)| id))
            .transpose()?;
        let id = operation.resource_id.as_deref().filter(|id| !id.is_empty());
        if let (Some(id), Some(location_id)) = (id, &location_id) {
            ensure!(
                id == location_id,
                "Graph copy returned different IDs in resourceId and resourceLocation"
            );
        }
        let id = id
            .or(location_id.as_deref())
            .context("Graph copy returned no new page ID")?;
        ensure!(
            valid_id(id) && id != outcome.source_page_id,
            "Graph copy did not return a distinct new page ID"
        );
        let path = format!(
            "{}/pages/{}",
            scope.graph_base_path(),
            urlencoding::encode(id)
        );
        let (page, _) = self.get_operation_json::<PageIdentity>(&path).await?;
        ensure!(
            page.id == id && page.parent_section.id == outcome.destination_section_id,
            "Graph page {} is in section {}; expected page {id} in section {}",
            page.id,
            page.parent_section.id,
            outcome.destination_section_id
        );
        Ok(id.to_string())
    }
}

fn valid_id(id: &str) -> bool {
    !id.is_empty()
        && !matches!(id, "." | "..")
        && !id.contains(['/', '\\', '|'])
        && !id.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn copy_entity(value: &str, kind: &str) -> Result<(OnenoteScope, String)> {
    let (scope, id) = split_onenote_entity_key(value.trim());
    match &scope {
        OnenoteScope::Me => {}
        OnenoteScope::Group { group_id, .. } => {
            ensure!(
                valid_id(group_id) && !group_id.contains(['%', '?', '#']),
                "Invalid Microsoft 365 group ID"
            );
        }
        OnenoteScope::Site { .. } => bail!(
            "Graph copyToSection supports me and Microsoft 365 group {kind} keys in this client"
        ),
    }
    ensure!(valid_id(&id), "Enter a {kind} ID or a scoped {kind} key");
    Ok((scope, id))
}

fn entity_location(base: &Url, location: &str, collection: &str) -> Result<(Url, String)> {
    let url = base.join(location).context("Invalid Graph location URL")?;
    ensure!(
        url.origin() == base.origin()
            && url.username().is_empty()
            && url.password().is_none()
            && url.fragment().is_none(),
        "Untrusted Graph {collection} URL origin or credentials"
    );
    let path = url
        .path()
        .strip_prefix(base.path())
        .context("Graph location is outside the configured API path")?;
    let segments: Vec<_> = path.split('/').collect();
    let id = match segments.as_slice() {
        ["me", "onenote", kind, id] if *kind == collection => id,
        ["users" | "groups" | "sites", owner, "onenote", kind, id]
            if *kind == collection && urlencoding::decode(owner).is_ok_and(|s| valid_id(&s)) =>
        {
            id
        }
        _ => bail!("Expected a Graph OneNote {collection} URL"),
    };
    let id = urlencoding::decode(id).context("Invalid Graph location ID")?;
    ensure!(valid_id(&id), "Invalid Graph location ID");
    let id = id.into_owned();
    Ok((url, id))
}

fn poll_delay(headers: &HeaderMap) -> Duration {
    let Some(value) = headers
        .get(RETRY_AFTER)
        .and_then(|header| header.to_str().ok())
    else {
        return POLL_INTERVAL;
    };
    if let Ok(seconds) = value.parse::<u64>() {
        return Duration::from_secs(seconds);
    }
    chrono::DateTime::parse_from_rfc2822(value)
        .map(|date| {
            date.signed_duration_since(chrono::Utc::now())
                .to_std()
                .unwrap_or(Duration::ZERO)
        })
        .unwrap_or(POLL_INTERVAL)
}
