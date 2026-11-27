//! Microsoft Graph v1.0 OneNote operations across `/me/onenote`, `/groups/{id}/onenote`, and `/sites/{id}/onenote`.

use std::collections::{HashMap, HashSet, VecDeque};

use anyhow::{bail, Result};
use futures_util::future::join_all;
use serde::{Deserialize, Serialize};

use super::{
    client::{GraphIdentitySet, GraphListPage},
    GraphClient,
};
use crate::models::{
    split_onenote_entity_key, InlineImageUpload, OnenoteNotebook, OnenotePageDetail,
    OnenotePageSummary, OnenotePatchCommand, OnenoteScope, OnenoteSection, OnenoteSectionGroup,
    OutgoingOnenotePage,
};

/// Joined Teams queried for notebooks by explicit CLI / MCP / HTTP calls.
/// `ONENOTE_MAX_TEAMS` overrides it (clamped to 1..=`MAX_JOINED_TEAMS_LIMIT`).
const DEFAULT_MAX_JOINED_TEAMS_FOR_ONENOTE: usize = 100;
/// Background sync runs under the automatic request budget (one request per
/// Team), so it keeps the smaller cap.
const BUDGETED_MAX_JOINED_TEAMS_FOR_ONENOTE: usize = 25;
const MAX_JOINED_TEAMS_LIMIT: usize = 500;
/// Shown when Teams notebooks were asked for but the sign-in cannot list Teams.
pub const TEAMS_SKIPPED_MISSING_ORG_SCOPES_WARNING: &str = "Teams notebooks were skipped: this sign-in has no Team.ReadBasic.All / Notes.Read.All permission. Sign in with your organization's tenant: onenote login --tenant-id <TENANT_ID>.";
const ONENOTE_SCOPE_CONCURRENCY: usize = 4;
const MAX_SECTION_GROUP_TRAVERSAL: usize = 256;

type PrefetchedMeSections = (
    Result<Vec<OnenoteSectionGroup>>,
    Result<Vec<OnenoteSection>>,
);

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphOnenoteLink {
    #[serde(default)]
    href: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphOnenoteLinks {
    #[serde(rename = "oneNoteWebUrl", default)]
    onenote_web_url: Option<GraphOnenoteLink>,
}

impl GraphOnenoteLinks {
    fn web_url(self) -> Option<String> {
        self.onenote_web_url
            .and_then(|l| l.href)
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphParentEntity {
    #[serde(default)]
    id: Option<String>,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
}

impl GraphParentEntity {
    fn into_id_and_name(self) -> (Option<String>, Option<String>) {
        let id = self
            .id
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        let name = self
            .display_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty());
        (id, name)
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphJoinedTeamLite {
    #[serde(default)]
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphOnenoteNotebook {
    #[serde(default)]
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    color: Option<String>,
    #[serde(rename = "isDefault", default)]
    is_default: bool,
    #[serde(rename = "isShared", default)]
    is_shared: bool,
    #[serde(rename = "userRole", default)]
    user_role: Option<String>,
    #[serde(rename = "createdDateTime", default)]
    created_date_time: Option<String>,
    #[serde(rename = "lastModifiedDateTime", default)]
    last_modified_date_time: Option<String>,
    #[serde(rename = "createdBy", default)]
    created_by: Option<GraphIdentitySet>,
    #[serde(default)]
    links: Option<GraphOnenoteLinks>,
}

impl GraphOnenoteNotebook {
    fn into_notebook(self, scope: &OnenoteScope) -> OnenoteNotebook {
        let id = self.id;
        let notebook_key = scope.make_entity_key(&id);
        let display_name = self
            .display_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| id.clone());
        let created_by = self
            .created_by
            .as_ref()
            .and_then(|c| c.primary_name())
            .map(str::to_string);

        OnenoteNotebook {
            notebook_key,
            id,
            scope: scope.clone(),
            display_name,
            color: self
                .color
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            is_default: self.is_default,
            is_shared: self.is_shared,
            user_role: self
                .user_role
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            created_at: self.created_date_time.filter(|s| !s.trim().is_empty()),
            last_modified_at: self
                .last_modified_date_time
                .filter(|s| !s.trim().is_empty()),
            created_by,
            web_url: self.links.and_then(GraphOnenoteLinks::web_url),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphOnenoteSectionGroup {
    #[serde(default)]
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(rename = "createdDateTime", default)]
    created_date_time: Option<String>,
    #[serde(rename = "lastModifiedDateTime", default)]
    last_modified_date_time: Option<String>,
    #[serde(rename = "parentNotebook", default)]
    parent_notebook: Option<GraphParentEntity>,
    #[serde(rename = "parentSectionGroup", default)]
    parent_section_group: Option<GraphParentEntity>,
}

impl GraphOnenoteSectionGroup {
    fn into_section_group(
        self,
        scope: &OnenoteScope,
        fallback_notebook_id: Option<&str>,
    ) -> OnenoteSectionGroup {
        let id = self.id;
        let section_group_key = scope.make_entity_key(&id);
        let notebook_id = self
            .parent_notebook
            .and_then(|p| p.into_id_and_name().0)
            .or_else(|| fallback_notebook_id.map(str::to_string));
        let parent_section_group_id = self
            .parent_section_group
            .and_then(|p| p.into_id_and_name().0);
        let display_name = self
            .display_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| id.clone());

        OnenoteSectionGroup {
            section_group_key,
            id,
            scope: scope.clone(),
            notebook_id,
            parent_section_group_id,
            display_name,
            created_at: self.created_date_time.filter(|s| !s.trim().is_empty()),
            last_modified_at: self
                .last_modified_date_time
                .filter(|s| !s.trim().is_empty()),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphOnenoteSection {
    #[serde(default)]
    id: String,
    #[serde(rename = "displayName", default)]
    display_name: Option<String>,
    #[serde(default)]
    color: Option<String>,
    #[serde(rename = "isDefault", default)]
    is_default: bool,
    #[serde(rename = "pagesUrl", default)]
    pages_url: Option<String>,
    #[serde(rename = "createdDateTime", default)]
    created_date_time: Option<String>,
    #[serde(rename = "lastModifiedDateTime", default)]
    last_modified_date_time: Option<String>,
    #[serde(rename = "parentNotebook", default)]
    parent_notebook: Option<GraphParentEntity>,
    #[serde(rename = "parentSectionGroup", default)]
    parent_section_group: Option<GraphParentEntity>,
    #[serde(default)]
    links: Option<GraphOnenoteLinks>,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "camelCase")]
struct NotebookDriveItem {
    id: String,
    name: String,
    #[serde(rename = "eTag")]
    etag: Option<String>,
    package: Option<DriveItemPackage>,
    parent_reference: Option<DriveItemParent>,
}

#[derive(Debug, Deserialize)]
struct DriveItemPackage {
    #[serde(rename = "type")]
    kind: String,
}

#[derive(Debug, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "camelCase")]
struct DriveItemParent {
    drive_id: String,
    id: String,
}

#[derive(Debug, Serialize)]
pub struct RenamedNotebookStorage {
    pub drive_id: String,
    pub item_id: String,
    pub parent_id: String,
    pub name: String,
    pub changed: bool,
}

impl GraphOnenoteSection {
    fn into_section(
        self,
        scope: &OnenoteScope,
        fallback_notebook_id: Option<&str>,
    ) -> OnenoteSection {
        let id = self.id;
        let section_key = scope.make_entity_key(&id);
        let (nb_id, notebook_name) = self
            .parent_notebook
            .map(GraphParentEntity::into_id_and_name)
            .unwrap_or((None, None));
        let notebook_id = nb_id.or_else(|| fallback_notebook_id.map(str::to_string));
        let (parent_section_group_id, parent_section_group_name) = self
            .parent_section_group
            .map(GraphParentEntity::into_id_and_name)
            .unwrap_or((None, None));
        let display_name = self
            .display_name
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| id.clone());

        OnenoteSection {
            section_key,
            id,
            scope: scope.clone(),
            notebook_id,
            notebook_name,
            parent_section_group_id,
            parent_section_group_name,
            display_name,
            color: self
                .color
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            is_default: self.is_default,
            pages_url: self
                .pages_url
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            created_at: self.created_date_time.filter(|s| !s.trim().is_empty()),
            last_modified_at: self
                .last_modified_date_time
                .filter(|s| !s.trim().is_empty()),
            web_url: self.links.and_then(GraphOnenoteLinks::web_url),
        }
    }
}

#[derive(Debug, Clone, Default, Deserialize)]
struct GraphOnenotePage {
    #[serde(default)]
    id: String,
    #[serde(default)]
    title: Option<String>,
    #[serde(default)]
    level: Option<i32>,
    #[serde(default)]
    order: Option<i32>,
    #[serde(rename = "createdDateTime", default)]
    created_date_time: Option<String>,
    #[serde(rename = "lastModifiedDateTime", default)]
    last_modified_date_time: Option<String>,
    #[serde(rename = "contentUrl", default)]
    content_url: Option<String>,
    #[serde(default)]
    links: Option<GraphOnenoteLinks>,
    #[serde(rename = "parentSection", default)]
    parent_section: Option<GraphParentEntity>,
    #[serde(rename = "parentNotebook", default)]
    parent_notebook: Option<GraphParentEntity>,
}

impl GraphOnenotePage {
    fn into_page_summary(
        self,
        scope: &OnenoteScope,
        fallback_section_id: Option<&str>,
        fallback_order: i32,
    ) -> OnenotePageSummary {
        let id = self.id;
        let page_key = scope.make_entity_key(&id);
        let (sec_id, section_name) = self
            .parent_section
            .map(GraphParentEntity::into_id_and_name)
            .unwrap_or((None, None));
        let section_id = sec_id
            .or_else(|| fallback_section_id.map(str::to_string))
            .unwrap_or_default();
        let (notebook_id, notebook_name) = self
            .parent_notebook
            .map(GraphParentEntity::into_id_and_name)
            .unwrap_or((None, None));
        let title = self
            .title
            .map(|s| s.trim().to_string())
            .filter(|s| !s.is_empty())
            .unwrap_or_else(|| "(Untitled)".to_string());

        OnenotePageSummary {
            page_key,
            id,
            scope: scope.clone(),
            section_id,
            section_name,
            notebook_id,
            notebook_name,
            title,
            level: self.level.unwrap_or(0).max(0),
            order: self.order.unwrap_or(fallback_order),
            created_at: self.created_date_time.filter(|s| !s.trim().is_empty()),
            last_modified_at: self
                .last_modified_date_time
                .filter(|s| !s.trim().is_empty()),
            content_url: self
                .content_url
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
            web_url: self.links.and_then(GraphOnenoteLinks::web_url),
            preview_text: None,
            has_todo: false,
        }
    }
}

fn clean_raw_onenote_id(id_or_composite: &str) -> String {
    let trimmed = id_or_composite.trim();
    if trimmed.contains('|') {
        split_onenote_entity_key(trimmed).1
    } else {
        trimmed.to_string()
    }
}

fn merge_fetched_page_summary(
    raw_meta: GraphOnenotePage,
    scope: &OnenoteScope,
    cached_summary: Option<OnenotePageSummary>,
) -> OnenotePageSummary {
    let fallback_sec = cached_summary
        .as_ref()
        .map(|s| s.section_id.as_str())
        .filter(|s| !s.is_empty());
    let fallback_order = cached_summary.as_ref().map(|s| s.order).unwrap_or(0);
    // Without `pagelevel=true` Graph omits `level`; a placeholder 0 must not
    // flatten a cached subpage.
    let level_omitted = raw_meta.level.is_none();
    let mut fresh = raw_meta.into_page_summary(scope, fallback_sec, fallback_order);
    if let Some(cached) = cached_summary {
        let same_section = !fresh.section_id.is_empty()
            && !cached.section_id.is_empty()
            && fresh.section_id == cached.section_id;
        if same_section {
            if level_omitted {
                fresh.level = cached.level;
            }
            if fresh.section_name.is_none() {
                fresh.section_name = cached.section_name;
            }
            if fresh.notebook_id.is_none() {
                fresh.notebook_id = cached.notebook_id;
            }
            if fresh.notebook_name.is_none() {
                fresh.notebook_name = cached.notebook_name;
            }
        }
        if fresh.web_url.is_none() {
            fresh.web_url = cached.web_url;
        }
    }
    fresh
}

fn is_pagelevel_bad_request(err: &anyhow::Error) -> bool {
    let full = err.to_string();
    let detail = full
        .split_once("): ")
        .map(|(_, d)| d)
        .unwrap_or(full.as_str())
        .to_ascii_lowercase();
    if !detail.contains("400") || detail.contains("10008") {
        return false;
    }
    detail.contains("pagelevel")
        || detail.contains("20152")
        || detail.contains("unsupported query parameter")
        || detail.contains("query parameter is not supported")
}

fn is_unauthorized_error(err: &anyhow::Error) -> bool {
    let full = err.to_string();
    let detail = full
        .split_once("): ")
        .map(|(_, d)| d)
        .unwrap_or(full.as_str())
        .to_ascii_lowercase();
    detail.contains("401")
        || detail.contains("unauthorized")
        || detail.contains("invalidauthenticationtoken")
}

/// Returns `true` ONLY when Microsoft Graph returned an authoritative HTTP 404 or 410 status.
/// Non-404/410 statuses (e.g. 500/503 whose body mentions "404" or "not found") return `false`
/// so transient backend failures never evict cached local pages.
pub fn is_not_found_error(err: &anyhow::Error) -> bool {
    matches!(
        crate::graph::client::graph_http_status(err),
        Some(404 | 410)
    )
}

/// Scope-aware notebook fetch result tracking which scopes were completely fetched without error.
#[derive(Debug, Clone)]
pub struct ScopedNotebooksResult {
    pub notebooks: Vec<OnenoteNotebook>,
    pub complete_scopes: HashSet<String>,
    /// Human-readable notes about notebooks that may be missing from the
    /// result (Teams skipped, Teams cap reached, Teams listing failed).
    pub warnings: Vec<String>,
}

/// Whether `/me/joinedTeams` should be queried for the current sign-in.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum JoinedTeamsDecision {
    Query,
    /// Personal Microsoft account: Teams do not exist, stay silent.
    SkipConsumer,
    /// Work or school token that carries scope claims but none that can list
    /// Teams (typically a `/common` sign-in without the organization scopes).
    SkipMissingOrgScopes,
}

pub(crate) fn joined_teams_decision(
    tokens: Option<&crate::auth::StoredTokens>,
) -> JoinedTeamsDecision {
    let Some(tok) = tokens else {
        return JoinedTeamsDecision::Query;
    };
    if crate::auth::is_consumer_msa_token(tok) {
        return JoinedTeamsDecision::SkipConsumer;
    }
    let diag = crate::auth::diagnose_access_token_scopes(&tok.access_token);
    if diag.has_scope_claims && !diag.can_read_groups {
        return JoinedTeamsDecision::SkipMissingOrgScopes;
    }
    JoinedTeamsDecision::Query
}

/// The warning to show when Teams notebooks cannot be listed with this
/// sign-in, or `None` when they can (or when the account has no Teams).
pub fn teams_skipped_warning(tokens: Option<&crate::auth::StoredTokens>) -> Option<&'static str> {
    (joined_teams_decision(tokens) == JoinedTeamsDecision::SkipMissingOrgScopes)
        .then_some(TEAMS_SKIPPED_MISSING_ORG_SCOPES_WARNING)
}

/// Parses `ONENOTE_MAX_TEAMS`; anything missing or invalid gives the default.
fn max_joined_teams_from(value: Option<&str>) -> usize {
    value
        .and_then(|v| v.trim().parse::<usize>().ok())
        .map(|n| n.clamp(1, MAX_JOINED_TEAMS_LIMIT))
        .unwrap_or(DEFAULT_MAX_JOINED_TEAMS_FOR_ONENOTE)
}

/// One section's page list as returned by [`GraphClient::list_onenote_page_listing`].
#[derive(Debug, Clone, Default)]
pub struct OnenotePageListing {
    pub pages: Vec<OnenotePageSummary>,
    /// More pages exist past the requested limit: callers must not prune.
    pub has_more: bool,
    /// `false` when the section rejected `pagelevel=true`: every `level` is a
    /// placeholder `0` and `order` the list position, so callers keep the
    /// cached hierarchy instead of storing these values.
    pub levels_known: bool,
}

/// Scope-aware section fetch result tracking which scopes were completely fetched without error.
#[derive(Debug, Clone)]
pub struct ScopedSectionsResult {
    pub section_groups: Vec<OnenoteSectionGroup>,
    pub sections: Vec<OnenoteSection>,
    pub complete_scopes: HashSet<String>,
}

fn backfill_section_hierarchy(
    notebooks: &[OnenoteNotebook],
    all_groups: &mut [OnenoteSectionGroup],
    all_sections: &mut [OnenoteSection],
) {
    let mut nb_name_by_scoped_id: HashMap<(String, String), String> = HashMap::new();
    let mut nb_name_by_id: HashMap<String, String> = HashMap::new();
    for nb in notebooks {
        nb_name_by_scoped_id.insert(
            (nb.scope.scope_key(), nb.id.clone()),
            nb.display_name.clone(),
        );
        nb_name_by_id
            .entry(nb.id.clone())
            .or_insert_with(|| nb.display_name.clone());
    }

    type SectionGroupInfoMap = HashMap<(String, String), (Option<String>, Option<String>, String)>;
    let mut sg_info: SectionGroupInfoMap = HashMap::new();
    for sg in all_groups.iter() {
        sg_info.insert(
            (sg.scope.scope_key(), sg.id.clone()),
            (
                sg.notebook_id.clone(),
                sg.parent_section_group_id.clone(),
                sg.display_name.clone(),
            ),
        );
    }

    let resolve_sg_notebook_id =
        |scope_key: &str, start_sg_id: &str, info: &SectionGroupInfoMap| -> Option<String> {
        let mut current = start_sg_id.to_string();
        let mut visited = HashSet::new();
        for _ in 0..32 {
            if !visited.insert(current.clone()) {
                break;
            }
            let Some((nb_id, parent_sg_id, _)) = info.get(&(scope_key.to_string(), current)) else {
                break;
            };
            if let Some(nb) = nb_id {
                return Some(nb.clone());
            }
            match parent_sg_id {
                Some(parent) => current = parent.clone(),
                None => break,
            }
        }
        None
    };

    for sg in all_groups.iter_mut() {
        if sg.notebook_id.is_none() {
            if let Some(ref parent_sg_id) = sg.parent_section_group_id {
                if let Some(nb_id) =
                    resolve_sg_notebook_id(&sg.scope.scope_key(), parent_sg_id, &sg_info)
                {
                    sg.notebook_id = Some(nb_id.clone());
                    if let Some(entry) = sg_info.get_mut(&(sg.scope.scope_key(), sg.id.clone())) {
                        entry.0 = Some(nb_id);
                    }
                }
            }
        }
    }

    for sec in all_sections.iter_mut() {
        let sk = sec.scope.scope_key();
        if sec.notebook_id.is_none() {
            if let Some(ref sg_id) = sec.parent_section_group_id {
                sec.notebook_id = resolve_sg_notebook_id(&sk, sg_id, &sg_info);
            }
        }
        if sec.notebook_name.is_none() {
            if let Some(ref nb_id) = sec.notebook_id {
                sec.notebook_name = nb_name_by_scoped_id
                    .get(&(sk.clone(), nb_id.clone()))
                    .or_else(|| nb_name_by_id.get(nb_id))
                    .cloned();
            }
        }
        if sec.parent_section_group_name.is_none() {
            if let Some(ref sg_id) = sec.parent_section_group_id {
                sec.parent_section_group_name = sg_info
                    .get(&(sk, sg_id.clone()))
                    .map(|(_, _, name)| name.clone());
            }
        }
    }
}

fn notebook_richness(nb: &OnenoteNotebook) -> usize {
    let mut score = 0usize;
    if !nb.scope.is_personal() {
        score += 2;
    }
    if nb.color.is_some() {
        score += 1;
    }
    if nb.user_role.is_some() {
        score += 1;
    }
    if nb.created_at.is_some() {
        score += 1;
    }
    if nb.last_modified_at.is_some() {
        score += 1;
    }
    if nb.created_by.is_some() {
        score += 1;
    }
    if nb.web_url.is_some() {
        score += 1;
    }
    score
}

fn merge_duplicate_notebook(existing: &mut OnenoteNotebook, mut candidate: OnenoteNotebook) {
    if notebook_richness(&candidate) > notebook_richness(existing) {
        if candidate.color.is_none() {
            candidate.color = existing.color.clone();
        }
        if candidate.user_role.is_none() {
            candidate.user_role = existing.user_role.clone();
        }
        if candidate.created_at.is_none() {
            candidate.created_at = existing.created_at.clone();
        }
        if candidate.last_modified_at.is_none() {
            candidate.last_modified_at = existing.last_modified_at.clone();
        }
        if candidate.created_by.is_none() {
            candidate.created_by = existing.created_by.clone();
        }
        if candidate.web_url.is_none() {
            candidate.web_url = existing.web_url.clone();
        }
        candidate.is_default |= existing.is_default;
        candidate.is_shared |= existing.is_shared;
        *existing = candidate;
    } else {
        if existing.color.is_none() {
            existing.color = candidate.color;
        }
        if existing.user_role.is_none() {
            existing.user_role = candidate.user_role;
        }
        if existing.created_at.is_none() {
            existing.created_at = candidate.created_at;
        }
        if existing.last_modified_at.is_none() {
            existing.last_modified_at = candidate.last_modified_at;
        }
        if existing.created_by.is_none() {
            existing.created_by = candidate.created_by;
        }
        if existing.web_url.is_none() {
            existing.web_url = candidate.web_url;
        }
        existing.is_default |= candidate.is_default;
        existing.is_shared |= candidate.is_shared;
    }
}

pub(crate) fn validate_page_creation_level(level: Option<i32>) -> Result<()> {
    // Graph exposes page level through GET metadata. Creation accepts XHTML.
    if level.is_some_and(|level| level != 0) {
        bail!("Microsoft Graph supports creating top-level OneNote pages. Omit the level or use 0, then change indentation in OneNote for Windows.");
    }
    Ok(())
}

impl GraphClient {
    /// Lists up to `max_teams` joined Teams. The flag is `true` only when at
    /// least one more Team (or another result page) exists beyond the cap.
    async fn list_joined_teams_lite(
        &self,
        max_teams: usize,
    ) -> Result<(Vec<(String, String)>, bool)> {
        let mut next_url = Some("/me/joinedTeams".to_string());
        let mut out = Vec::new();

        while let Some(url) = next_url {
            let page: GraphListPage<GraphJoinedTeamLite> = self.get_json(&url).await?;
            for raw in page.value {
                if raw.id.trim().is_empty() {
                    continue;
                }
                if out.len() >= max_teams {
                    return Ok((out, true));
                }
                let name = raw
                    .display_name
                    .map(|s| s.trim().to_string())
                    .filter(|s| !s.is_empty())
                    .unwrap_or_else(|| raw.id.clone());
                out.push((raw.id, name));
            }
            next_url = page.next_link;
            if out.len() >= max_teams && next_url.is_some() {
                return Ok((out, true));
            }
        }

        Ok((out, false))
    }

    pub async fn list_onenote_notebooks(
        &self,
        scope: &OnenoteScope,
    ) -> Result<Vec<OnenoteNotebook>> {
        let mut next_url = Some(format!("{}/notebooks?$top=100", scope.graph_base_path()));
        let mut notebooks = Vec::new();

        while let Some(url) = next_url {
            let page: GraphListPage<GraphOnenoteNotebook> = self.get_json(&url).await?;
            for raw in page.value {
                if !raw.id.trim().is_empty() {
                    notebooks.push(raw.into_notebook(scope));
                }
            }
            next_url = page.next_link;
        }

        Ok(notebooks)
    }

    /// Teams cap for this client: explicit calls use the configurable cap,
    /// budgeted background sync keeps the small one.
    fn max_joined_teams(&self) -> usize {
        if self.request_allowance.is_some() {
            BUDGETED_MAX_JOINED_TEAMS_FOR_ONENOTE
        } else {
            max_joined_teams_from(std::env::var("ONENOTE_MAX_TEAMS").ok().as_deref())
        }
    }

    pub async fn list_all_accessible_notebooks(
        &self,
        include_teams: bool,
    ) -> Result<Vec<OnenoteNotebook>> {
        Ok(self
            .list_all_accessible_notebooks_scoped(include_teams)
            .await?
            .notebooks)
    }

    pub async fn list_all_accessible_notebooks_scoped(
        &self,
        include_teams: bool,
    ) -> Result<ScopedNotebooksResult> {
        self.list_all_accessible_notebooks_with_team_cap(include_teams, self.max_joined_teams())
            .await
    }

    async fn list_all_accessible_notebooks_with_team_cap(
        &self,
        include_teams: bool,
        max_teams: usize,
    ) -> Result<ScopedNotebooksResult> {
        if !include_teams {
            let notebooks = self.list_onenote_notebooks(&OnenoteScope::Me).await?;
            let mut complete_scopes = HashSet::new();
            complete_scopes.insert(OnenoteScope::Me.scope_key());
            return Ok(ScopedNotebooksResult {
                notebooks,
                complete_scopes,
                warnings: Vec::new(),
            });
        }
        let mut warnings: Vec<String> = Vec::new();

        let tokens = self.auth.current_tokens().await;
        let teams_decision = joined_teams_decision(tokens.as_ref());
        let (me_res, teams_list_res) = match teams_decision {
            JoinedTeamsDecision::Query => {
                let (me_res, joined) = tokio::join!(
                    self.list_onenote_notebooks(&OnenoteScope::Me),
                    self.list_joined_teams_lite(max_teams)
                );
                (me_res, Some(joined))
            }
            JoinedTeamsDecision::SkipMissingOrgScopes => {
                warnings.push(TEAMS_SKIPPED_MISSING_ORG_SCOPES_WARNING.to_string());
                (self.list_onenote_notebooks(&OnenoteScope::Me).await, None)
            }
            JoinedTeamsDecision::SkipConsumer => {
                (self.list_onenote_notebooks(&OnenoteScope::Me).await, None)
            }
        };

        let mut merged: Vec<OnenoteNotebook> = Vec::new();
        let mut index_by_id: HashMap<String, usize> = HashMap::new();
        let mut complete_scopes: HashSet<String> = HashSet::new();
        let mut any_succeeded = false;
        let mut me_err = None;

        match me_res {
            Ok(personal) => {
                any_succeeded = true;
                complete_scopes.insert(OnenoteScope::Me.scope_key());
                for nb in personal {
                    if let Some(&pos) = index_by_id.get(&nb.id) {
                        merge_duplicate_notebook(&mut merged[pos], nb);
                    } else {
                        index_by_id.insert(nb.id.clone(), merged.len());
                        merged.push(nb);
                    }
                }
            }
            Err(err) => {
                me_err = Some(err);
            }
        }

        let teams = match teams_list_res {
            Some(Ok((teams, truncated))) => {
                if truncated {
                    warnings.push(format!(
                        "Showing notebooks for the first {} of your joined Teams; set ONENOTE_MAX_TEAMS to raise the limit.",
                        teams.len()
                    ));
                }
                Some(teams)
            }
            Some(Err(err)) => {
                warnings.push(format!(
                    "Teams notebooks were skipped: listing your joined Teams failed ({err})."
                ));
                None
            }
            None => None,
        };
        if let Some(teams) = teams {
            let scopes: Vec<OnenoteScope> = teams
                .into_iter()
                .map(|(group_id, group_name)| OnenoteScope::Group {
                    group_id,
                    group_name: Some(group_name),
                })
                .collect();

            for chunk in scopes.chunks(ONENOTE_SCOPE_CONCURRENCY) {
                let futs = chunk.iter().map(|s| async move {
                    let scope_key = s.scope_key();
                    (scope_key, self.list_onenote_notebooks(s).await)
                });
                let results = join_all(futs).await;
                for (scope_key, res) in results {
                    if let Ok(group_nbs) = res {
                        complete_scopes.insert(scope_key);
                        if !group_nbs.is_empty() {
                            any_succeeded = true;
                        }
                        for nb in group_nbs {
                            if let Some(&pos) = index_by_id.get(&nb.id) {
                                merge_duplicate_notebook(&mut merged[pos], nb);
                            } else {
                                index_by_id.insert(nb.id.clone(), merged.len());
                                merged.push(nb);
                            }
                        }
                    }
                }
            }
        }

        if (!any_succeeded || merged.is_empty()) && me_err.is_some() {
            if let Some(err) = me_err {
                return Err(err);
            }
        }

        Ok(ScopedNotebooksResult {
            notebooks: merged,
            complete_scopes,
            warnings,
        })
    }

    pub async fn create_onenote_notebook(
        &self,
        scope: &OnenoteScope,
        display_name: &str,
    ) -> Result<OnenoteNotebook> {
        let name = display_name.trim();
        if name.is_empty() {
            bail!("OneNote notebook name is required.");
        }
        let path = format!("{}/notebooks", scope.graph_base_path());
        let raw: GraphOnenoteNotebook = self
            .post_json(&path, &serde_json::json!({ "displayName": name }))
            .await?;
        Ok(raw.into_notebook(scope))
    }

    pub async fn list_onenote_section_groups(
        &self,
        scope: &OnenoteScope,
        notebook_id: Option<&str>,
    ) -> Result<Vec<OnenoteSectionGroup>> {
        let nb_clean = notebook_id
            .map(clean_raw_onenote_id)
            .filter(|s| !s.is_empty());
        let base = scope.graph_base_path();
        let mut next_url = Some(match nb_clean.as_deref() {
            Some(nb_id) => format!(
                "{base}/notebooks/{}/sectionGroups?$top=100",
                urlencoding::encode(nb_id)
            ),
            None => format!("{base}/sectionGroups?$top=100"),
        });

        let mut groups = Vec::new();
        while let Some(url) = next_url {
            let page: GraphListPage<GraphOnenoteSectionGroup> = self.get_json(&url).await?;
            for raw in page.value {
                if !raw.id.trim().is_empty() {
                    groups.push(raw.into_section_group(scope, nb_clean.as_deref()));
                }
            }
            next_url = page.next_link;
        }

        Ok(groups)
    }

    async fn list_onenote_sections_direct(
        &self,
        scope: &OnenoteScope,
        notebook_id: Option<&str>,
    ) -> Result<Vec<OnenoteSection>> {
        let nb_clean = notebook_id
            .map(clean_raw_onenote_id)
            .filter(|s| !s.is_empty());
        let base = scope.graph_base_path();
        let mut next_url = Some(match nb_clean.as_deref() {
            Some(nb_id) => format!(
                "{base}/notebooks/{}/sections?$top=100",
                urlencoding::encode(nb_id)
            ),
            None => format!("{base}/sections?$top=100"),
        });

        let mut sections = Vec::new();
        while let Some(url) = next_url {
            let page: GraphListPage<GraphOnenoteSection> = self.get_json(&url).await?;
            for raw in page.value {
                if !raw.id.trim().is_empty() {
                    sections.push(raw.into_section(scope, nb_clean.as_deref()));
                }
            }
            next_url = page.next_link;
        }

        Ok(sections)
    }

    async fn list_section_group_sections(
        &self,
        scope: &OnenoteScope,
        section_group_id: &str,
        fallback_notebook_id: Option<&str>,
    ) -> Result<Vec<OnenoteSection>> {
        let sg_clean = clean_raw_onenote_id(section_group_id);
        if sg_clean.is_empty() {
            return Ok(Vec::new());
        }
        let base = scope.graph_base_path();
        let mut next_url = Some(format!(
            "{base}/sectionGroups/{}/sections?$top=100",
            urlencoding::encode(&sg_clean)
        ));

        let mut sections = Vec::new();
        while let Some(url) = next_url {
            let page: GraphListPage<GraphOnenoteSection> = self.get_json(&url).await?;
            for raw in page.value {
                if !raw.id.trim().is_empty() {
                    let mut sec = raw.into_section(scope, fallback_notebook_id);
                    if sec.parent_section_group_id.is_none() {
                        sec.parent_section_group_id = Some(sg_clean.clone());
                    }
                    sections.push(sec);
                }
            }
            next_url = page.next_link;
        }

        Ok(sections)
    }

    async fn list_child_section_groups(
        &self,
        scope: &OnenoteScope,
        section_group_id: &str,
        fallback_notebook_id: Option<&str>,
    ) -> Result<Vec<OnenoteSectionGroup>> {
        let sg_clean = clean_raw_onenote_id(section_group_id);
        if sg_clean.is_empty() {
            return Ok(Vec::new());
        }
        let base = scope.graph_base_path();
        let mut next_url = Some(format!(
            "{base}/sectionGroups/{}/sectionGroups?$top=100",
            urlencoding::encode(&sg_clean)
        ));

        let mut groups = Vec::new();
        while let Some(url) = next_url {
            let page: GraphListPage<GraphOnenoteSectionGroup> = self.get_json(&url).await?;
            for raw in page.value {
                if !raw.id.trim().is_empty() {
                    let mut sg = raw.into_section_group(scope, fallback_notebook_id);
                    if sg.parent_section_group_id.is_none() {
                        sg.parent_section_group_id = Some(sg_clean.clone());
                    }
                    groups.push(sg);
                }
            }
            next_url = page.next_link;
        }

        Ok(groups)
    }

    /// Lists every notebook's section groups (and nested groups) into
    /// `groups`. Returns `false` when any listing failed or the walk stopped
    /// at `MAX_SECTION_GROUP_TRAVERSAL`, so the caller must not prune.
    async fn walk_notebook_section_groups(
        &self,
        scope: &OnenoteScope,
        notebooks: &[OnenoteNotebook],
        groups: &mut Vec<OnenoteSectionGroup>,
    ) -> bool {
        let mut complete = true;
        let mut visited_sg_ids: HashSet<String> = HashSet::new();
        for nb in notebooks {
            let nb_sgs = match self.list_onenote_section_groups(scope, Some(&nb.id)).await {
                Ok(nb_sgs) => nb_sgs,
                Err(_) => {
                    complete = false;
                    continue;
                }
            };
            let mut queue: VecDeque<OnenoteSectionGroup> = VecDeque::new();
            let mut admit = |sg: OnenoteSectionGroup,
                             queue: &mut VecDeque<OnenoteSectionGroup>,
                             complete: &mut bool|
             -> bool {
                if visited_sg_ids.contains(&sg.id) {
                    return true;
                }
                if visited_sg_ids.len() >= MAX_SECTION_GROUP_TRAVERSAL {
                    *complete = false;
                    return false;
                }
                visited_sg_ids.insert(sg.id.clone());
                groups.push(sg.clone());
                queue.push_back(sg);
                true
            };
            for sg in nb_sgs {
                if !admit(sg, &mut queue, &mut complete) {
                    break;
                }
            }
            while let Some(sg) = queue.pop_front() {
                let eff_nb = sg.notebook_id.as_deref().or(Some(nb.id.as_str()));
                match self.list_child_section_groups(scope, &sg.id, eff_nb).await {
                    Ok(children) => {
                        for child in children {
                            if !admit(child, &mut queue, &mut complete) {
                                break;
                            }
                        }
                    }
                    Err(_) => complete = false,
                }
            }
        }
        complete
    }

    async fn traverse_section_groups(
        &self,
        scope: &OnenoteScope,
        initial_groups: Vec<OnenoteSectionGroup>,
        fallback_notebook_id: Option<&str>,
        visited_sg_ids: &mut HashSet<String>,
    ) -> (Vec<OnenoteSectionGroup>, Vec<OnenoteSection>, bool) {
        let mut queue: VecDeque<OnenoteSectionGroup> = VecDeque::new();
        let mut collected_groups: Vec<OnenoteSectionGroup> = Vec::new();
        let mut collected_sections: Vec<OnenoteSection> = Vec::new();
        let mut seen_sections: HashSet<String> = HashSet::new();
        // `false` as soon as any listing fails or the traversal cap stops the
        // walk: the caller must then not prune cached sections.
        let mut complete = true;

        for g in initial_groups {
            if visited_sg_ids.contains(&g.id) {
                continue;
            }
            if visited_sg_ids.len() >= MAX_SECTION_GROUP_TRAVERSAL {
                complete = false;
                break;
            }
            if visited_sg_ids.insert(g.id.clone()) {
                collected_groups.push(g.clone());
                queue.push_back(g);
            }
        }

        while let Some(sg) = queue.pop_front() {
            let eff_nb_id = sg.notebook_id.as_deref().or(fallback_notebook_id);
            let (secs_res, child_groups_res) = tokio::join!(
                self.list_section_group_sections(scope, &sg.id, eff_nb_id),
                self.list_child_section_groups(scope, &sg.id, eff_nb_id)
            );

            match secs_res {
                Ok(secs) => {
                    for mut sec in secs {
                        if sec.parent_section_group_name.is_none() {
                            sec.parent_section_group_name = Some(sg.display_name.clone());
                        }
                        if seen_sections.insert(sec.section_key.clone()) {
                            collected_sections.push(sec);
                        }
                    }
                }
                Err(_) => complete = false,
            }

            match child_groups_res {
                Ok(children) => {
                    for child in children {
                        if visited_sg_ids.contains(&child.id) {
                            continue;
                        }
                        if visited_sg_ids.len() >= MAX_SECTION_GROUP_TRAVERSAL {
                            complete = false;
                            break;
                        }
                        if visited_sg_ids.insert(child.id.clone()) {
                            collected_groups.push(child.clone());
                            queue.push_back(child);
                        }
                    }
                }
                Err(_) => complete = false,
            }
        }

        (collected_groups, collected_sections, complete)
    }

    async fn collect_notebook_section_groups_and_sections(
        &self,
        scope: &OnenoteScope,
        nb_id: &str,
        visited_sg_ids: &mut HashSet<String>,
    ) -> Result<(Vec<OnenoteSectionGroup>, Vec<OnenoteSection>, bool)> {
        let (direct_secs_res, direct_groups_res) = tokio::join!(
            self.list_onenote_sections_direct(scope, Some(nb_id)),
            self.list_onenote_section_groups(scope, Some(nb_id))
        );

        let mut sections: Vec<OnenoteSection> = Vec::new();
        let mut seen_sections: HashSet<String> = HashSet::new();
        let mut direct_err = None;

        match direct_secs_res {
            Ok(direct_secs) => {
                for sec in direct_secs {
                    if seen_sections.insert(sec.section_key.clone()) {
                        sections.push(sec);
                    }
                }
            }
            Err(err) => {
                direct_err = Some(err);
            }
        }

        let (groups, grouped_secs, groups_complete) = match direct_groups_res {
            Ok(top_groups) => {
                self.traverse_section_groups(scope, top_groups, Some(nb_id), visited_sg_ids)
                    .await
            }
            Err(_) => (Vec::new(), Vec::new(), false),
        };
        // Partial when the direct section list failed (but grouped sections
        // were found) or any part of the section-group walk failed.
        let complete = direct_err.is_none() && groups_complete;

        for sec in grouped_secs {
            if seen_sections.insert(sec.section_key.clone()) {
                sections.push(sec);
            }
        }

        if let Some(err) = direct_err {
            if sections.is_empty() {
                return Err(err);
            }
        }

        Ok((groups, sections, complete))
    }

    pub async fn list_onenote_sections(
        &self,
        scope: &OnenoteScope,
        notebook_id: Option<&str>,
    ) -> Result<Vec<OnenoteSection>> {
        Ok(self
            .list_onenote_sections_scoped(scope, notebook_id)
            .await?
            .0)
    }

    /// Like [`Self::list_onenote_sections`], plus whether the list is complete.
    /// `false` means some section or section-group request failed (or the
    /// traversal cap was reached): the sections returned are valid, but
    /// cached sections missing from the list must not be pruned.
    pub async fn list_onenote_sections_scoped(
        &self,
        scope: &OnenoteScope,
        notebook_id: Option<&str>,
    ) -> Result<(Vec<OnenoteSection>, bool)> {
        let nb_clean = notebook_id
            .map(clean_raw_onenote_id)
            .filter(|s| !s.is_empty());
        match nb_clean.as_deref() {
            Some(nb_id) => {
                let mut visited = HashSet::new();
                let (mut groups, mut sections, complete) = self
                    .collect_notebook_section_groups_and_sections(scope, nb_id, &mut visited)
                    .await?;
                backfill_section_hierarchy(&[], &mut groups, &mut sections);
                Ok((sections, complete))
            }
            None => Ok((self.list_onenote_sections_direct(scope, None).await?, true)),
        }
    }

    pub async fn list_all_accessible_sections(
        &self,
        notebooks: &[OnenoteNotebook],
    ) -> Result<(Vec<OnenoteSectionGroup>, Vec<OnenoteSection>)> {
        let res = self
            .list_all_accessible_sections_scoped(notebooks, None)
            .await?;
        Ok((res.section_groups, res.sections))
    }

    /// Fetches accessible notebooks, section groups, and sections together,
    /// overlapping personal (`/me/onenote`) notebooks, sectionGroups, and sections
    /// into a single parallel round-trip instead of two serial hops.
    pub async fn list_all_accessible_hierarchy_scoped(
        &self,
        include_teams: bool,
    ) -> Result<(ScopedNotebooksResult, ScopedSectionsResult)> {
        let (nb_res, me_groups_res, me_secs_res) = tokio::join!(
            self.list_all_accessible_notebooks_scoped(include_teams),
            self.list_onenote_section_groups(&OnenoteScope::Me, None),
            self.list_onenote_sections_direct(&OnenoteScope::Me, None),
        );
        let nb_res = nb_res?;
        let sec_res = self
            .list_all_accessible_sections_scoped_inner(
                &nb_res.notebooks,
                Some(&nb_res.complete_scopes),
                Some((me_groups_res, me_secs_res)),
            )
            .await?;
        Ok((nb_res, sec_res))
    }

    pub async fn list_all_accessible_sections_scoped(
        &self,
        notebooks: &[OnenoteNotebook],
        extra_complete_scopes: Option<&HashSet<String>>,
    ) -> Result<ScopedSectionsResult> {
        self.list_all_accessible_sections_scoped_inner(notebooks, extra_complete_scopes, None)
            .await
    }

    async fn list_all_accessible_sections_scoped_inner(
        &self,
        notebooks: &[OnenoteNotebook],
        extra_complete_scopes: Option<&HashSet<String>>,
        mut prefetched_me: Option<PrefetchedMeSections>,
    ) -> Result<ScopedSectionsResult> {
        let mut unique_scopes: Vec<OnenoteScope> = Vec::new();
        let mut scope_pos_by_key: HashMap<String, usize> = HashMap::new();
        let mut notebooks_by_scope: HashMap<String, Vec<OnenoteNotebook>> = HashMap::new();

        if notebooks.is_empty() && extra_complete_scopes.is_none_or(HashSet::is_empty) {
            unique_scopes.push(OnenoteScope::Me);
            scope_pos_by_key.insert(OnenoteScope::Me.scope_key(), 0);
        } else {
            for nb in notebooks {
                let sk = nb.scope.scope_key();
                notebooks_by_scope
                    .entry(sk.clone())
                    .or_default()
                    .push(nb.clone());
                if let Some(&pos) = scope_pos_by_key.get(&sk) {
                    let replace = matches!(
                        (&unique_scopes[pos], &nb.scope),
                        (
                            OnenoteScope::Group {
                                group_name: None,
                                ..
                            },
                            OnenoteScope::Group {
                                group_name: Some(_),
                                ..
                            }
                        ) | (
                            OnenoteScope::Site {
                                site_name: None,
                                ..
                            },
                            OnenoteScope::Site {
                                site_name: Some(_),
                                ..
                            }
                        )
                    );
                    if replace {
                        unique_scopes[pos] = nb.scope.clone();
                    }
                } else {
                    scope_pos_by_key.insert(sk, unique_scopes.len());
                    unique_scopes.push(nb.scope.clone());
                }
            }
            if let Some(extra) = extra_complete_scopes {
                for sk in extra {
                    if !scope_pos_by_key.contains_key(sk) {
                        if let Some(scope) = OnenoteScope::from_scope_key(sk, None) {
                            scope_pos_by_key.insert(sk.clone(), unique_scopes.len());
                            unique_scopes.push(scope);
                        }
                    }
                }
            }
        }

        let mut all_groups: Vec<OnenoteSectionGroup> = Vec::new();
        let mut seen_groups: HashMap<String, usize> = HashMap::new();
        let mut all_sections: Vec<OnenoteSection> = Vec::new();
        let mut seen_sections: HashMap<String, usize> = HashMap::new();
        let mut complete_scopes: HashSet<String> = HashSet::new();
        let mut any_scope_succeeded = false;
        let mut me_err: Option<anyhow::Error> = None;
        let mut first_err: Option<anyhow::Error> = None;

        for chunk in unique_scopes.chunks(ONENOTE_SCOPE_CONCURRENCY) {
            let futs: Vec<_> = chunk
                .iter()
                .map(|scope| {
                    let scope_nbs = notebooks_by_scope
                        .get(&scope.scope_key())
                        .cloned()
                        .unwrap_or_default();
                    let pre = if scope.is_personal() {
                        prefetched_me.take()
                    } else {
                        None
                    };
                    async move {
                        let (groups_res, secs_res) = match pre {
                            Some(res) => res,
                            None => tokio::join!(
                                self.list_onenote_section_groups(scope, None),
                                self.list_onenote_sections_direct(scope, None)
                            ),
                        };
                        (scope.clone(), scope_nbs, groups_res, secs_res)
                    }
                })
                .collect();
            let results = join_all(futs).await;
            for (scope, scope_nbs, groups_res, secs_res) in results {
                let mut scope_groups: Vec<OnenoteSectionGroup> = Vec::new();
                let mut scope_sections: Vec<OnenoteSection> = Vec::new();
                let mut scope_succeeded = false;
                let mut scope_complete = false;
                let mut scope_err: Option<anyhow::Error> = None;

                let top_groups_from_scope = match groups_res {
                    Ok(groups) => {
                        scope_groups.extend(groups.clone());
                        Some(groups)
                    }
                    Err(_) => None,
                };

                match secs_res {
                    Ok(secs) => {
                        scope_succeeded = true;
                        scope_sections.extend(secs);
                        // The scope is complete (and may prune cached groups and
                        // sections) only when the section-group listing is whole
                        // too: either the scope-wide list succeeded, or every
                        // per-notebook walk below succeeded without hitting the
                        // traversal cap.
                        scope_complete = if top_groups_from_scope.is_some() {
                            true
                        } else if scope_nbs.is_empty() {
                            false
                        } else {
                            self.walk_notebook_section_groups(&scope, &scope_nbs, &mut scope_groups)
                                .await
                        };
                    }
                    Err(err) => {
                        if !is_unauthorized_error(&err) {
                            let mut visited_sg_ids: HashSet<String> = HashSet::new();
                            let mut fallback_ok = false;
                            let mut all_nbs_ok = !scope_nbs.is_empty();

                            for nb in &scope_nbs {
                                match self
                                    .collect_notebook_section_groups_and_sections(
                                        &scope,
                                        &nb.id,
                                        &mut visited_sg_ids,
                                    )
                                    .await
                                {
                                    Ok((nb_groups, nb_secs, nb_complete)) => {
                                        fallback_ok = true;
                                        if !nb_complete {
                                            all_nbs_ok = false;
                                        }
                                        scope_groups.extend(nb_groups);
                                        scope_sections.extend(nb_secs);
                                    }
                                    Err(_) => {
                                        all_nbs_ok = false;
                                    }
                                }
                            }

                            if let Some(top_groups) = top_groups_from_scope {
                                let remaining: Vec<OnenoteSectionGroup> = top_groups
                                    .into_iter()
                                    .filter(|g| !visited_sg_ids.contains(&g.id))
                                    .collect();
                                if !remaining.is_empty() {
                                    let (extra_groups, extra_secs, extra_complete) = self
                                        .traverse_section_groups(
                                            &scope,
                                            remaining,
                                            None,
                                            &mut visited_sg_ids,
                                        )
                                        .await;
                                    if !extra_complete {
                                        all_nbs_ok = false;
                                    }
                                    if !extra_secs.is_empty() {
                                        fallback_ok = true;
                                    }
                                    scope_groups.extend(extra_groups);
                                    scope_sections.extend(extra_secs);
                                }
                            }

                            if fallback_ok {
                                scope_succeeded = true;
                                scope_complete = all_nbs_ok;
                            } else {
                                scope_err = Some(err);
                            }
                        } else {
                            scope_err = Some(err);
                        }
                    }
                }

                if scope_succeeded {
                    any_scope_succeeded = true;
                    if scope_complete {
                        complete_scopes.insert(scope.scope_key());
                    }
                } else if let Some(err) = scope_err {
                    if scope.is_personal() && me_err.is_none() {
                        me_err = Some(err);
                    } else if first_err.is_none() {
                        first_err = Some(err);
                    }
                }

                for g in scope_groups {
                    if !seen_groups.contains_key(&g.section_group_key) {
                        seen_groups.insert(g.section_group_key.clone(), all_groups.len());
                        all_groups.push(g);
                    }
                }
                for s in scope_sections {
                    if !seen_sections.contains_key(&s.section_key) {
                        seen_sections.insert(s.section_key.clone(), all_sections.len());
                        all_sections.push(s);
                    }
                }
            }
        }

        if all_sections.is_empty() {
            if let Some(err) = me_err {
                return Err(err);
            }
            if !any_scope_succeeded {
                if let Some(err) = first_err {
                    return Err(err);
                }
            }
        }

        backfill_section_hierarchy(notebooks, &mut all_groups, &mut all_sections);

        Ok(ScopedSectionsResult {
            section_groups: all_groups,
            sections: all_sections,
            complete_scopes,
        })
    }

    pub async fn create_onenote_section(
        &self,
        scope: &OnenoteScope,
        notebook_id: &str,
        display_name: &str,
    ) -> Result<OnenoteSection> {
        let nb_id = clean_raw_onenote_id(notebook_id);
        if nb_id.is_empty() {
            bail!("OneNote notebook ID is required.");
        }
        let name = display_name.trim();
        if name.is_empty() {
            bail!("OneNote section name is required.");
        }
        let path = format!(
            "{}/notebooks/{}/sections",
            scope.graph_base_path(),
            urlencoding::encode(&nb_id)
        );
        let raw: GraphOnenoteSection = self
            .post_json(&path, &serde_json::json!({ "displayName": name }))
            .await?;
        Ok(raw.into_section(scope, Some(&nb_id)))
    }

    /// Rename a section addressed by the signed-in user's OneNote route.
    pub async fn rename_onenote_section(
        &self,
        scope: &OnenoteScope,
        section_id: &str,
        display_name: &str,
    ) -> Result<OnenoteSection> {
        if !scope.is_personal() {
            bail!("Section rename accepts a section ID under /me/onenote. Use OneNote for Windows for group or site routes.");
        }
        let id = section_id.trim();
        let name = display_name.trim();
        if id.is_empty() || name.is_empty() {
            bail!("Section ID and new name are required.");
        }
        let path = format!(
            "{}/sections/{}",
            scope.graph_base_path(),
            urlencoding::encode(id)
        );
        let expand_path = format!("{path}?$expand=parentNotebook,parentSectionGroup");
        let before: GraphOnenoteSection = self.get_json(&expand_path).await?;
        if before.id != id || before.display_name.is_none() {
            bail!("Graph did not return the requested section identity and name.");
        }
        let before = before.into_section(scope, None);
        if before.notebook_id.is_none() {
            bail!("Graph did not return the section's parent notebook.");
        }
        if before.display_name == name {
            return Ok(before);
        }
        self.patch_json(&path, &serde_json::json!({ "displayName": name }))
            .await?;
        let after = self
            .get_json::<GraphOnenoteSection>(&expand_path)
            .await
            .map_err(|err| {
                anyhow::anyhow!("Graph accepted section rename for '{id}', but read-back failed: {err}")
            })?;
        let name_matches = after.display_name.as_deref() == Some(name);
        let after = after.into_section(scope, None);
        if after.id != before.id
            || after.notebook_id != before.notebook_id
            || after.parent_section_group_id != before.parent_section_group_id
            || !name_matches
        {
            bail!("Graph accepted section rename for '{id}', but the read-back did not confirm the name and unchanged identity and parents.");
        }
        Ok(after)
    }

    /// Rename the existing drive item's OneNote package by its Files API identity.
    pub async fn rename_notebook_storage(
        &self,
        drive_id: &str,
        item_id: &str,
        name: &str,
    ) -> Result<RenamedNotebookStorage> {
        let (drive_id, item_id, name) = (drive_id.trim(), item_id.trim(), name.trim());
        if drive_id.is_empty() || item_id.is_empty() || name.is_empty() {
            bail!("Drive ID, item ID and new notebook name are required.");
        }
        let path = format!(
            "/drives/{}/items/{}",
            urlencoding::encode(drive_id),
            urlencoding::encode(item_id)
        );
        let before: NotebookDriveItem = self.get_json(&path).await?;
        let is_notebook = |item: &NotebookDriveItem| {
            item.id == item_id && item.package.as_ref().is_some_and(|pkg| pkg.kind == "oneNote")
        };
        if !is_notebook(&before) {
            bail!("The requested drive item is not a verified OneNote notebook package.");
        }
        let parent = before
            .parent_reference
            .filter(|parent| parent.drive_id == drive_id && !parent.id.is_empty())
            .ok_or_else(|| anyhow::anyhow!("Graph did not confirm the notebook's parent in the requested drive."))?;
        let changed = name != before.name;
        if changed {
            self.patch_json_with_etag(
                &path,
                &serde_json::json!({ "name": name }),
                before.etag.as_deref(),
            )
            .await?;
            let after: NotebookDriveItem = self.get_json(&path).await.map_err(|err| {
                anyhow::anyhow!("Graph accepted the notebook storage rename for '{item_id}', but read-back failed: {err}")
            })?;
            if !is_notebook(&after)
                || after.parent_reference.as_ref() != Some(&parent)
                || after.name != name
            {
                bail!("Graph accepted the notebook storage rename for '{item_id}', but read-back did not confirm the name, drive item ID, and parent.");
            }
        }
        Ok(RenamedNotebookStorage {
            drive_id: drive_id.to_owned(),
            item_id: item_id.to_owned(),
            parent_id: parent.id,
            name: name.to_owned(),
            changed,
        })
    }

    pub async fn list_onenote_pages(
        &self,
        scope: &OnenoteScope,
        section_id: &str,
        limit: usize,
    ) -> Result<Vec<OnenotePageSummary>> {
        Ok(self
            .list_onenote_pages_with_more(scope, section_id, limit)
            .await?
            .0)
    }

    /// Same as `list_onenote_pages`, also reporting whether the section holds
    /// more pages than `limit` (another row or a `nextLink` remained when the
    /// limit was reached). Callers must not prune cached pages when it is true.
    pub async fn list_onenote_pages_with_more(
        &self,
        scope: &OnenoteScope,
        section_id: &str,
        limit: usize,
    ) -> Result<(Vec<OnenotePageSummary>, bool)> {
        let listing = self
            .list_onenote_page_listing(scope, section_id, limit)
            .await?;
        Ok((listing.pages, listing.has_more))
    }

    /// Same as [`Self::list_onenote_pages_with_more`], also reporting whether
    /// Graph returned page levels (`pagelevel=true` was accepted for this
    /// section). A 400 on `pagelevel` is remembered for this section only.
    pub async fn list_onenote_page_listing(
        &self,
        scope: &OnenoteScope,
        section_id: &str,
        limit: usize,
    ) -> Result<OnenotePageListing> {
        if limit == 0 {
            // Nothing was fetched, so the list cannot be treated as complete.
            return Ok(OnenotePageListing {
                pages: Vec::new(),
                has_more: true,
                levels_known: true,
            });
        }
        let sec_id = clean_raw_onenote_id(section_id);
        if sec_id.is_empty() {
            bail!("OneNote section ID is required.");
        }
        let top = limit.clamp(1, 100);
        let base = scope.graph_base_path();
        let sec_enc = urlencoding::encode(&sec_id);
        let section_key = scope.make_entity_key(&sec_id);

        let fallback_url = format!("{base}/sections/{sec_enc}/pages?$top={top}");
        let (first_page, levels_known): (GraphListPage<GraphOnenotePage>, bool) =
            if self.is_pagelevel_unsupported(&section_key) {
                (self.get_json(&fallback_url).await?, false)
            } else {
                let url_with_level = format!("{fallback_url}&pagelevel=true");
                match self.get_json(&url_with_level).await {
                    Ok(page) => (page, true),
                    Err(err) if is_pagelevel_bad_request(&err) => {
                        self.mark_pagelevel_unsupported(&section_key);
                        (self.get_json(&fallback_url).await?, false)
                    }
                    Err(err) => return Err(err),
                }
            };

        let mut pages = Vec::new();
        let mut batch = first_page;
        loop {
            let next_link = batch.next_link;
            let mut rows = batch
                .value
                .into_iter()
                .filter(|raw| !raw.id.trim().is_empty());
            for raw in rows.by_ref() {
                let idx = pages.len() as i32;
                pages.push(raw.into_page_summary(scope, Some(&sec_id), idx));
                if pages.len() >= limit {
                    break;
                }
            }
            if pages.len() >= limit {
                let has_more = rows.next().is_some() || next_link.is_some();
                return Ok(OnenotePageListing {
                    pages,
                    has_more,
                    levels_known,
                });
            }
            match next_link {
                Some(url) => batch = self.get_json(&url).await?,
                None => {
                    return Ok(OnenotePageListing {
                        pages,
                        has_more: false,
                        levels_known,
                    })
                }
            }
        }
    }

    pub async fn get_onenote_page_content_xhtml(
        &self,
        scope: &OnenoteScope,
        page_id: &str,
    ) -> Result<String> {
        let pid = clean_raw_onenote_id(page_id);
        if pid.is_empty() {
            bail!("OneNote page ID is required.");
        }
        let base = scope.graph_base_path();
        let pid_enc = urlencoding::encode(&pid);
        let content_path = format!("{base}/pages/{pid_enc}/content?includeIDs=true");
        self.get_text(&content_path).await
    }

    pub async fn get_onenote_page_summary(
        &self,
        scope: &OnenoteScope,
        page_id: &str,
        cached_summary: Option<OnenotePageSummary>,
    ) -> Result<OnenotePageSummary> {
        let pid = clean_raw_onenote_id(page_id);
        if pid.is_empty() {
            bail!("OneNote page ID is required.");
        }
        let raw_meta = self
            .get_page_meta(scope, &pid, cached_summary.as_ref())
            .await?;
        Ok(merge_fetched_page_summary(raw_meta, scope, cached_summary))
    }

    /// `GET /pages/{id}` with `pagelevel=true`, unless the page's section
    /// (from `cached_summary`) already rejected it. A 400 on the parameter
    /// retries without it and marks only that page's section.
    async fn get_page_meta(
        &self,
        scope: &OnenoteScope,
        pid: &str,
        cached_summary: Option<&OnenotePageSummary>,
    ) -> Result<GraphOnenotePage> {
        let base = scope.graph_base_path();
        let fallback_meta = format!("{base}/pages/{}", urlencoding::encode(pid));
        let section_key = cached_summary
            .map(|s| clean_raw_onenote_id(&s.section_id))
            .filter(|id| !id.is_empty())
            .map(|id| scope.make_entity_key(&id));
        if section_key
            .as_deref()
            .is_some_and(|key| self.is_pagelevel_unsupported(key))
        {
            return self.get_json(&fallback_meta).await;
        }
        let meta_with_level = format!("{fallback_meta}?pagelevel=true");
        match self.get_json::<GraphOnenotePage>(&meta_with_level).await {
            Ok(meta) => Ok(meta),
            Err(err) if is_pagelevel_bad_request(&err) => {
                let meta: GraphOnenotePage = self.get_json(&fallback_meta).await?;
                let section_key = section_key.or_else(|| {
                    meta.parent_section
                        .as_ref()
                        .and_then(|p| p.id.as_deref())
                        .map(str::trim)
                        .filter(|id| !id.is_empty())
                        .map(|id| scope.make_entity_key(id))
                });
                if let Some(key) = section_key {
                    self.mark_pagelevel_unsupported(&key);
                }
                Ok(meta)
            }
            Err(err) => Err(err),
        }
    }

    /// Fetches only page XHTML content (`GET /{scope}/pages/{id}/content?includeIDs=true`)
    /// and parses it with a known summary (1 HTTP request instead of 2).
    pub async fn get_onenote_page_detail_content_only(
        &self,
        scope: &OnenoteScope,
        page_id: &str,
        mut cached_summary: OnenotePageSummary,
    ) -> Result<OnenotePageDetail> {
        let raw_xhtml = self.get_onenote_page_content_xhtml(scope, page_id).await?;
        cached_summary.preview_text = None;
        let mut detail = crate::parser::onenote::parse_onenote_page_xhtml(cached_summary, &raw_xhtml);
        detail.synced_at = Some(chrono::Utc::now().to_rfc3339());
        Ok(detail)
    }

    pub async fn get_onenote_page_detail(
        &self,
        scope: &OnenoteScope,
        page_id: &str,
        cached_summary: Option<OnenotePageSummary>,
    ) -> Result<OnenotePageDetail> {
        let pid = clean_raw_onenote_id(page_id);
        if pid.is_empty() {
            bail!("OneNote page ID is required.");
        }
        let base = scope.graph_base_path();
        let pid_enc = urlencoding::encode(&pid);

        let meta_fut = self.get_page_meta(scope, &pid, cached_summary.as_ref());
        let content_path = format!("{base}/pages/{pid_enc}/content?includeIDs=true");
        let content_fut = self.get_text(&content_path);

        let (meta_res, content_res) = tokio::join!(meta_fut, content_fut);

        let raw_xhtml = match content_res {
            Ok(xhtml) => xhtml,
            Err(err) => return Err(err),
        };

        let summary = match meta_res {
            Ok(raw_meta) => merge_fetched_page_summary(raw_meta, scope, cached_summary),
            Err(err) => {
                if is_not_found_error(&err) {
                    return Err(err);
                }
                match cached_summary {
                    Some(mut cached) => {
                        cached.preview_text = None;
                        cached
                    }
                    None => return Err(err),
                }
            }
        };

        let mut detail = crate::parser::onenote::parse_onenote_page_xhtml(summary, &raw_xhtml);
        detail.synced_at = Some(chrono::Utc::now().to_rfc3339());
        Ok(detail)
    }

    pub async fn create_onenote_page(
        &self,
        scope: &OnenoteScope,
        section_id: &str,
        outgoing: &OutgoingOnenotePage,
    ) -> Result<OnenotePageSummary> {
        validate_page_creation_level(outgoing.level)?;
        let sec_id = clean_raw_onenote_id(section_id);
        if sec_id.is_empty() {
            bail!("OneNote section ID is required.");
        }
        let (bytes, content_type) = crate::parser::onenote::compile_new_page_payload(outgoing);
        let path = format!(
            "{}/sections/{}/pages",
            scope.graph_base_path(),
            urlencoding::encode(&sec_id)
        );
        let raw: GraphOnenotePage = self.post_bytes_json(&path, bytes, &content_type).await?;
        let had_wire_title = raw
            .title
            .as_deref()
            .map(|t| !t.trim().is_empty())
            .unwrap_or(false);
        let mut summary = raw.into_page_summary(scope, Some(&sec_id), 0);
        if !had_wire_title && !outgoing.title.trim().is_empty() {
            summary.title = outgoing.title.trim().to_string();
        }
        if summary.section_id.is_empty() {
            summary.section_id = sec_id;
        }
        Ok(summary)
    }

    pub async fn patch_onenote_page_commands(
        &self,
        scope: &OnenoteScope,
        page_id: &str,
        commands: &[OnenotePatchCommand],
    ) -> Result<()> {
        if commands.is_empty() {
            return Ok(());
        }
        let pid = clean_raw_onenote_id(page_id);
        if pid.is_empty() {
            bail!("OneNote page ID is required.");
        }
        let bytes = serde_json::to_vec(commands)?;
        self.patch_bytes_empty(
            &format!(
                "{}/pages/{}/content",
                scope.graph_base_path(),
                urlencoding::encode(&pid)
            ),
            bytes,
            "application/json",
        )
        .await
    }

    pub async fn append_onenote_page_markdown(
        &self,
        scope: &OnenoteScope,
        page_id: &str,
        primary_div_id: Option<&str>,
        markdown: &str,
        inline_images: &[InlineImageUpload],
    ) -> Result<()> {
        let pid = clean_raw_onenote_id(page_id);
        if pid.is_empty() {
            bail!("OneNote page ID is required.");
        }
        let (bytes, ct) = crate::parser::onenote::build_append_markdown_payload(
            primary_div_id,
            markdown,
            inline_images,
        );
        self.patch_bytes_empty(
            &format!(
                "{}/pages/{}/content",
                scope.graph_base_path(),
                urlencoding::encode(&pid)
            ),
            bytes,
            &ct,
        )
        .await
    }

    pub async fn delete_onenote_page(&self, scope: &OnenoteScope, page_id: &str) -> Result<()> {
        let pid = clean_raw_onenote_id(page_id);
        if pid.is_empty() {
            bail!("OneNote page ID is required.");
        }
        self.delete_req(&format!(
            "{}/pages/{}",
            scope.graph_base_path(),
            urlencoding::encode(&pid)
        ))
        .await
    }

    pub async fn download_onenote_resource_bytes(&self, resource_url: &str) -> Result<Vec<u8>> {
        self.get_bytes(resource_url).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        auth::{AuthManager, StoredTokens},
        config::AppConfig,
    };
    use axum::{
        body::Bytes,
        extract::{Path as AxumPath, Query, State},
        http::{HeaderMap, StatusCode},
        routing::{get, post},
        Json, Router,
    };
    use std::sync::{Arc, Mutex};

    fn test_graph_client(base_url: String) -> GraphClient {
        let cfg = AppConfig {
            graph_base_url: base_url,
            ..AppConfig::default()
        };
        let auth = AuthManager::from_tokens(
            cfg.clone(),
            StoredTokens {
                access_token: "test-token".to_string(),
                refresh_token: None,
                expires_at: chrono::Utc::now().timestamp() + 3600,
                client_id: "cid".to_string(),
                tenant_id: "tid".to_string(),
                user_id: Some("me-1".to_string()),
                user_display_name: Some("Tester".to_string()),
                user_email: Some("tester@example.com".to_string()),
                auth_mode: "access_token".to_string(),
            },
        );
        GraphClient::new(cfg, auth).unwrap().with_my_user_id("me-1")
    }

    #[tokio::test]
    async fn section_rename_requires_read_back_of_name_and_parent() {
        let writes = Arc::new(Mutex::new(Vec::<String>::new()));
        let app = Router::new()
            .route(
                "/v1.0/me/onenote/sections/sec-1",
                get(
                    |State(writes): State<Arc<Mutex<Vec<String>>>>,
                     Query(params): Query<HashMap<String, String>>| async move {
                        assert_eq!(params["$expand"], "parentNotebook,parentSectionGroup");
                        let writes = writes.lock().unwrap();
                        let name = writes.last().map(String::as_str).unwrap_or("Original");
                        Json(serde_json::json!({
                            "id": "sec-1",
                            "displayName": name,
                            "parentNotebook": { "id": "nb-1", "displayName": "Notebook" },
                            "parentSectionGroup": { "id": if name == "Wrong parent" { "group-other" } else { "group-1" } }
                        }))
                    },
                )
                .patch(
                    |State(writes): State<Arc<Mutex<Vec<String>>>>,
                     Json(body): Json<serde_json::Value>| async move {
                        let name = body["displayName"].as_str().unwrap();
                        assert_eq!(body, serde_json::json!({ "displayName": name }));
                        writes.lock().unwrap().push(name.to_string());
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .with_state(writes.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = test_graph_client(format!("http://{}/v1.0", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        for _ in 0..2 {
            let section = client
                .rename_onenote_section(&OnenoteScope::Me, "sec-1", "Renamed")
                .await
                .unwrap();
            assert_eq!(section.section_key, "me|sec-1");
            assert_eq!(section.notebook_id.as_deref(), Some("nb-1"));
            assert_eq!(section.parent_section_group_id.as_deref(), Some("group-1"));
            assert_eq!(section.display_name, "Renamed");
        }
        assert_eq!(*writes.lock().unwrap(), ["Renamed"]);
        let err = client
            .rename_onenote_section(&OnenoteScope::Me, "sec-1", "Wrong parent")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("did not confirm"));
        assert_eq!(*writes.lock().unwrap(), ["Renamed", "Wrong parent"]);
        server.abort();
    }

    #[tokio::test]
    async fn storage_rename_checks_package_identity_and_etag() {
        let writes = Arc::new(Mutex::new(Vec::<String>::new()));
        let app = Router::new()
            .route(
                "/v1.0/drives/drive-1/items/{id}",
                get(
                    |State(writes): State<Arc<Mutex<Vec<String>>>>,
                     AxumPath(id): AxumPath<String>| async move {
                        let writes = writes.lock().unwrap();
                        let name = writes.last().map(String::as_str).unwrap_or("Original");
                        Json(serde_json::json!({
                            "id": id,
                            "name": name,
                            "eTag": "\"fixture-etag\"",
                            "package": (id == "notebook-1").then(|| serde_json::json!({ "type": "oneNote" })),
                            "parentReference": { "driveId": "drive-1", "id": "folder-1" }
                        }))
                    },
                )
                .patch(
                    |State(writes): State<Arc<Mutex<Vec<String>>>>,
                     AxumPath(id): AxumPath<String>,
                     headers: HeaderMap,
                     Json(body): Json<serde_json::Value>| async move {
                        assert_eq!(id, "notebook-1");
                        assert_eq!(headers["if-match"], "\"fixture-etag\"");
                        let name = body["name"].as_str().unwrap();
                        assert_eq!(body, serde_json::json!({ "name": name }));
                        writes.lock().unwrap().push(name.to_string());
                        StatusCode::OK
                    },
                ),
            )
            .with_state(writes.clone());
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let client = test_graph_client(format!("http://{}/v1.0", listener.local_addr().unwrap()));
        let server = tokio::spawn(async move { axum::serve(listener, app).await.unwrap() });

        let err = client
            .rename_notebook_storage("drive-1", "ordinary-file", "New name")
            .await
            .unwrap_err();
        assert!(err.to_string().contains("verified OneNote notebook package"));
        assert!(writes.lock().unwrap().is_empty());

        for changed in [true, false] {
            let renamed = client
                .rename_notebook_storage("drive-1", "notebook-1", "New name")
                .await
                .unwrap();
            assert_eq!(renamed.item_id, "notebook-1");
            assert_eq!(renamed.drive_id, "drive-1");
            assert_eq!(renamed.parent_id, "folder-1");
            assert_eq!(renamed.name, "New name");
            assert_eq!(renamed.changed, changed);
        }
        assert_eq!(*writes.lock().unwrap(), ["New name"]);
        server.abort();
    }

    #[tokio::test]
    async fn test_onenote_hierarchy_pages_fallback_and_mutations() {
        #[derive(Clone, Default)]
        struct MockState {
            created_pages: Arc<Mutex<Vec<String>>>,
            content_patches: Arc<Mutex<Vec<(String, String)>>>,
            deleted_pages: Arc<Mutex<Vec<String>>>,
        }

        let state = MockState::default();
        let state_check = state.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/onenote/notebooks",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            {
                                "id": "nb-me",
                                "displayName": "개인 노트",
                                "isDefault": true,
                                "isShared": false,
                                "userRole": "Owner",
                                "createdBy": { "user": { "displayName": "홍길동" } },
                                "links": { "oneNoteWebUrl": { "href": "https://onenote.example.com/nb-me" } }
                            },
                            {
                                "id": "nb-dup",
                                "displayName": "공유 노트"
                            }
                        ]
                    }))
                })
                .post(|Json(body): Json<serde_json::Value>| async move {
                    (
                        StatusCode::CREATED,
                        Json(serde_json::json!({
                            "id": "nb-created",
                            "displayName": body["displayName"],
                            "isDefault": false,
                            "isShared": false
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/me/joinedTeams",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            { "id": "team-ok", "displayName": "플랫폼팀" },
                            { "id": "team-404", "displayName": "미개설팀" }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/groups/{gid}/onenote/notebooks",
                get(|AxumPath(gid): AxumPath<String>| async move {
                    if gid == "team-ok" {
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "value": [
                                    {
                                        "id": "nb-dup",
                                        "displayName": "공유 노트",
                                        "isShared": true,
                                        "userRole": "Contributor",
                                        "links": { "oneNoteWebUrl": { "href": "https://onenote.example.com/nb-dup" } }
                                    }
                                ]
                            })),
                        )
                    } else {
                        (
                            StatusCode::NOT_FOUND,
                            Json(serde_json::json!({
                                "error": { "code": "ResourceNotFound", "message": "No notebook" }
                            })),
                        )
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            {
                                "id": "sg-1",
                                "displayName": "2026 아카이브",
                                "parentNotebook": { "id": "nb-me", "displayName": "개인 노트" }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/groups/{gid}/onenote/sectionGroups",
                get(|AxumPath(_gid): AxumPath<String>| async move {
                    Json(serde_json::json!({ "value": [] }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sections",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [
                            {
                                "id": "sec-1",
                                "displayName": "주간 회의",
                                "isDefault": true,
                                "parentNotebook": { "id": "nb-me" }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/groups/{gid}/onenote/sections",
                get(|AxumPath(_gid): AxumPath<String>| async move {
                    Json(serde_json::json!({
                        "value": [
                            {
                                "id": "sec-team",
                                "displayName": "런북",
                                "parentNotebook": { "id": "nb-dup" }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/notebooks/{nb_id}/sections",
                post(
                    |AxumPath(nb_id): AxumPath<String>, Json(body): Json<serde_json::Value>| async move {
                        assert_eq!(nb_id, "nb-me");
                        (
                            StatusCode::CREATED,
                            Json(serde_json::json!({
                                "id": "sec-created",
                                "displayName": body["displayName"]
                            })),
                        )
                    },
                ),
            )
            .route(
                "/v1.0/me/onenote/sections/{sec_id}/pages",
                get(
                    |AxumPath(sec_id): AxumPath<String>,
                     Query(params): Query<HashMap<String, String>>| async move {
                        assert_eq!(sec_id, "sec-1");
                        if params.contains_key("pagelevel") {
                            return (
                                StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({
                                    "error": { "code": "20152", "message": "pagelevel is not supported here" }
                                })),
                            );
                        }
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "value": [
                                    {
                                        "id": "pg-1",
                                        "title": "",
                                        "createdDateTime": "2026-11-15T09:00:00Z",
                                        "lastModifiedDateTime": "2026-11-15T10:00:00Z"
                                    }
                                ]
                            })),
                        )
                    },
                )
                .post(
                    |State(st): State<MockState>,
                     AxumPath(sec_id): AxumPath<String>,
                     headers: HeaderMap,
                     body: Bytes| async move {
                        assert_eq!(sec_id, "sec-1");
                        assert!(!body.is_empty());
                        assert!(headers.contains_key("content-type"));
                        st.created_pages.lock().unwrap().push(sec_id);
                        (
                            StatusCode::CREATED,
                            Json(serde_json::json!({
                                "id": "pg-new",
                                "title": "새 페이지"
                            })),
                        )
                    },
                ),
            )
            .route(
                "/v1.0/me/onenote/pages/{page_id}",
                get(|AxumPath(page_id): AxumPath<String>| async move {
                    Json(serde_json::json!({
                        "id": page_id,
                        "title": "아키텍처 설계",
                        "level": 1,
                        "order": 2,
                        "parentSection": { "id": "sec-1", "displayName": "주간 회의" }
                    }))
                })
                .delete(
                    |State(st): State<MockState>, AxumPath(page_id): AxumPath<String>| async move {
                        st.deleted_pages.lock().unwrap().push(page_id);
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .route(
                "/v1.0/me/onenote/pages/{page_id}/content",
                get(|AxumPath(_page_id): AxumPath<String>| async move {
                    (
                        StatusCode::OK,
                        r#"<html xmlns="http://www.w3.org/1999/xhtml"><head><title>아키텍처 설계</title></head><body><div id="div:{1111}{32}"><p id="p:{2222}{40}" data-tag="to-do">마이그레이션 점검</p></div></body></html>"#,
                    )
                })
                .patch(
                    |State(st): State<MockState>,
                     AxumPath(page_id): AxumPath<String>,
                     headers: HeaderMap,
                     body: Bytes| async move {
                        let ct = headers
                            .get("content-type")
                            .and_then(|v| v.to_str().ok())
                            .unwrap_or("")
                            .to_string();
                        assert!(!body.is_empty());
                        st.content_patches.lock().unwrap().push((page_id, ct));
                        StatusCode::NO_CONTENT
                    },
                ),
            )
            .route(
                "/v1.0/me/onenote/resources/{res_id}/content",
                get(|AxumPath(res_id): AxumPath<String>| async move {
                    assert_eq!(res_id, "res-1");
                    vec![0x89u8, b'P', b'N', b'G']
                }),
            )
            .with_state(state);

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(format!("http://{addr}/v1.0"));

        // 1. Accessible notebooks across Me + Joined Teams (deduped, 404 ignored)
        let notebooks = client.list_all_accessible_notebooks(true).await.unwrap();
        assert_eq!(notebooks.len(), 2);
        assert_eq!(notebooks[0].notebook_key, "me|nb-me");
        assert_eq!(notebooks[0].created_by.as_deref(), Some("홍길동"));
        assert_eq!(notebooks[1].notebook_key, "group:team-ok|nb-dup");
        assert_eq!(notebooks[1].scope.owner_label(), "플랫폼팀");

        // 2. Accessible section groups & sections with notebook_name enrichment
        let (sgs, secs) = client
            .list_all_accessible_sections(&notebooks)
            .await
            .unwrap();
        assert_eq!(sgs.len(), 1);
        assert_eq!(sgs[0].section_group_key, "me|sg-1");
        assert_eq!(secs.len(), 2);
        assert_eq!(secs[0].notebook_name.as_deref(), Some("개인 노트"));
        assert_eq!(secs[1].notebook_name.as_deref(), Some("공유 노트"));

        // 3. Create notebook & section
        let created_nb = client
            .create_onenote_notebook(&OnenoteScope::Me, "신규 노트")
            .await
            .unwrap();
        assert_eq!(created_nb.notebook_key, "me|nb-created");
        let created_sec = client
            .create_onenote_section(&OnenoteScope::Me, "nb-me", "스프린트")
            .await
            .unwrap();
        assert_eq!(created_sec.notebook_id.as_deref(), Some("nb-me"));

        // 4. List pages with automatic fallback on 400 pagelevel error + untitled fallback
        let pages = client
            .list_onenote_pages(&OnenoteScope::Me, "sec-1", 20)
            .await
            .unwrap();
        assert_eq!(pages.len(), 1);
        assert_eq!(pages[0].page_key, "me|pg-1");
        assert_eq!(pages[0].title, "(Untitled)");
        assert_eq!(pages[0].section_id, "sec-1");

        // 5. Page detail fetch + XHTML parse
        let detail = client
            .get_onenote_page_detail(&OnenoteScope::Me, "pg-1", None)
            .await
            .unwrap();
        assert_eq!(detail.summary.id, "pg-1");
        assert!(detail.body_plain.contains("마이그레이션 점검"));
        assert!(detail.synced_at.is_some());

        // 6. Reject unsupported subpage creation before the POST.
        let mut outgoing = OutgoingOnenotePage {
            title: "새 페이지".into(),
            markdown_body: "내용입니다".into(),
            inline_images: Vec::new(),
            level: Some(1),
        };
        let err = client
            .create_onenote_page(&OnenoteScope::Me, "sec-1", &outgoing)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("top-level"));
        assert!(state_check.created_pages.lock().unwrap().is_empty());
        assert_eq!(detail.summary.level, 1);

        outgoing.level = Some(0);
        let new_page = client
            .create_onenote_page(
                &OnenoteScope::Me,
                "sec-1",
                &outgoing,
            )
            .await
            .unwrap();
        assert_eq!(new_page.page_key, "me|pg-new");
        assert_eq!(new_page.section_id, "sec-1");
        assert_eq!(new_page.level, 0);
        assert_eq!(*state_check.created_pages.lock().unwrap(), ["sec-1"]);

        client
            .patch_onenote_page_commands(
                &OnenoteScope::Me,
                "pg-1",
                &[OnenotePatchCommand {
                    target: "p:{2222}{40}".into(),
                    action: "replace".into(),
                    position: None,
                    content: "<p data-tag=\"to-do:completed\">마이그레이션 점검</p>".into(),
                }],
            )
            .await
            .unwrap();

        client
            .append_onenote_page_markdown(
                &OnenoteScope::Me,
                "pg-1",
                detail.primary_div_id.as_deref(),
                "추가 메모",
                &[],
            )
            .await
            .unwrap();
        assert_eq!(state_check.content_patches.lock().unwrap().len(), 2);

        let bytes = client
            .download_onenote_resource_bytes(&format!(
                "http://{addr}/v1.0/me/onenote/resources/res-1/content"
            ))
            .await
            .unwrap();
        assert_eq!(bytes, vec![0x89u8, b'P', b'N', b'G']);

        client
            .delete_onenote_page(&OnenoteScope::Me, "pg-1")
            .await
            .unwrap();
        assert_eq!(
            *state_check.deleted_pages.lock().unwrap(),
            vec!["pg-1".to_string()]
        );
    }

    fn make_test_jwt(payload: &serde_json::Value) -> String {
        use base64::{engine::general_purpose::URL_SAFE_NO_PAD, Engine as _};
        let header = URL_SAFE_NO_PAD.encode(r#"{"alg":"RS256","typ":"JWT"}"#);
        let body = URL_SAFE_NO_PAD.encode(serde_json::to_vec(payload).unwrap());
        format!("{header}.{body}.sig")
    }

    fn test_graph_client_with_token(
        base_url: String,
        access_token: String,
        tenant_id: String,
    ) -> GraphClient {
        let cfg = AppConfig {
            graph_base_url: base_url,
            tenant_id: tenant_id.clone(),
            ..AppConfig::default()
        };
        let auth = AuthManager::from_tokens(
            cfg.clone(),
            StoredTokens {
                access_token,
                refresh_token: None,
                expires_at: chrono::Utc::now().timestamp() + 3600,
                client_id: "cid".to_string(),
                tenant_id,
                user_id: Some("me-1".to_string()),
                user_display_name: Some("Tester".to_string()),
                user_email: Some("tester@example.com".to_string()),
                auth_mode: "access_token".to_string(),
            },
        );
        GraphClient::new(cfg, auth).unwrap().with_my_user_id("me-1")
    }

    #[tokio::test]
    async fn test_list_sections_propagates_500_and_falls_back_on_10008() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        // --- Part A: HTTP 400 Error 10008 fallback + recursive sectionGroups + transitive backfill ---
        let page_10008_hits = Arc::new(AtomicUsize::new(0));
        let page_10008_clone = page_10008_hits.clone();

        let app_10008 = Router::new()
            .route(
                "/v1.0/me/onenote/sections",
                get(|| async {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": {
                                "code": "10008",
                                "message": "One or more of the document libraries on the user or group's OneDrive contains more than 5,000 OneNote items."
                            }
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups",
                get(|| async {
                    (
                        StatusCode::BAD_REQUEST,
                        Json(serde_json::json!({
                            "error": {
                                "code": "10008",
                                "message": "One or more of the document libraries contains more than 5,000 OneNote items."
                            }
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/me/onenote/notebooks/{nb_id}/sections",
                get(|AxumPath(nb_id): AxumPath<String>| async move {
                    assert_eq!(nb_id, "nb-big");
                    Json(serde_json::json!({
                        "value": [
                            {
                                "id": "sec-direct",
                                "displayName": "직접 섹션",
                                "isDefault": true
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/notebooks/{nb_id}/sectionGroups",
                get(|AxumPath(nb_id): AxumPath<String>| async move {
                    assert_eq!(nb_id, "nb-big");
                    Json(serde_json::json!({
                        "value": [
                            {
                                "id": "sg-1",
                                "displayName": "상위 그룹",
                                "parentNotebook": { "id": "nb-big" }
                            }
                        ]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups/{sg_id}/sections",
                get(|AxumPath(sg_id): AxumPath<String>| async move {
                    if sg_id == "sg-1" {
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "sec-in-sg1",
                                    "displayName": "그룹1 섹션",
                                    "parentSectionGroup": { "id": "sg-1" }
                                }
                            ]
                        }))
                    } else if sg_id == "sg-2" {
                        // Graph omits parentNotebook AND parentSectionGroup displayName here
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "sec-in-sg2",
                                    "displayName": "중첩 그룹 섹션"
                                }
                            ]
                        }))
                    } else {
                        Json(serde_json::json!({ "value": [] }))
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups/{sg_id}/sectionGroups",
                get(|AxumPath(sg_id): AxumPath<String>| async move {
                    if sg_id == "sg-1" {
                        // Nested section group omitting parentNotebook
                        Json(serde_json::json!({
                            "value": [
                                {
                                    "id": "sg-2",
                                    "displayName": "하위 그룹",
                                    "parentSectionGroup": { "id": "sg-1" }
                                }
                            ]
                        }))
                    } else {
                        Json(serde_json::json!({ "value": [] }))
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/sections/sec-10008/pages",
                get(move || {
                    let hits = page_10008_clone.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::BAD_REQUEST,
                            Json(serde_json::json!({
                                "error": {
                                    "code": "10008",
                                    "message": "One or more of the document libraries contains more than 5,000 OneNote items."
                                }
                            })),
                        )
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app_10008).await;
        });

        let client = test_graph_client(format!("http://{addr}/v1.0"));
        let nb = OnenoteNotebook {
            notebook_key: "me|nb-big".into(),
            id: "nb-big".into(),
            scope: OnenoteScope::Me,
            display_name: "대용량 필기장".into(),
            color: None,
            is_default: true,
            is_shared: false,
            user_role: None,
            created_at: None,
            last_modified_at: None,
            created_by: None,
            web_url: None,
        };

        let (groups, secs) = client
            .list_all_accessible_sections(std::slice::from_ref(&nb))
            .await
            .unwrap();
        assert_eq!(groups.len(), 2);
        assert_eq!(secs.len(), 3);
        let ids: Vec<&str> = secs.iter().map(|s| s.id.as_str()).collect();
        assert!(ids.contains(&"sec-direct"));
        assert!(ids.contains(&"sec-in-sg1"));
        assert!(ids.contains(&"sec-in-sg2"));
        for sec in &secs {
            assert_eq!(sec.notebook_id.as_deref(), Some("nb-big"), "sec={}", sec.id);
            assert_eq!(
                sec.notebook_name.as_deref(),
                Some("대용량 필기장"),
                "sec={}",
                sec.id
            );
        }
        let nested = secs.iter().find(|s| s.id == "sec-in-sg2").unwrap();
        assert_eq!(nested.parent_section_group_id.as_deref(), Some("sg-2"));
        assert_eq!(nested.parent_section_group_name.as_deref(), Some("하위 그룹"));

        // Direct per-notebook list_onenote_sections(Some(nb_id)) must also include grouped sections
        let nb_secs = client
            .list_onenote_sections(&OnenoteScope::Me, Some("nb-big"))
            .await
            .unwrap();
        assert_eq!(nb_secs.len(), 3);

        // is_pagelevel_bad_request must NOT retry on Error 10008 even though URL contains pagelevel=true
        let err_10008 = client
            .list_onenote_pages(&OnenoteScope::Me, "sec-10008", 20)
            .await
            .unwrap_err();
        assert!(err_10008.to_string().contains("10008"));
        assert_eq!(
            page_10008_hits.load(Ordering::SeqCst),
            1,
            "10008 error must not be retried as a pagelevel error"
        );
        assert!(!is_pagelevel_bad_request(&err_10008));
        assert!(is_pagelevel_bad_request(&anyhow::anyhow!(
            "Graph API error (GET http://x/pages?pagelevel=true): 400 Bad Request [20152] pagelevel not supported"
        )));
        assert!(!is_pagelevel_bad_request(&anyhow::anyhow!(
            "Graph API error (GET http://x/pages?pagelevel=true): 400 Bad Request [BadRequest] invalid section"
        )));

        // --- Part B: HTTP 500 across all section endpoints must propagate Err(...) ---
        let app_500 = Router::new()
            .route(
                "/v1.0/me/onenote/sections",
                get(|| async {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": { "code": "InternalServerError", "message": "boom" }
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups",
                get(|| async {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": { "code": "InternalServerError", "message": "boom" }
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/me/onenote/notebooks/{nb_id}/sections",
                get(|| async {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": { "code": "InternalServerError", "message": "boom" }
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/me/onenote/notebooks/{nb_id}/sectionGroups",
                get(|| async {
                    (
                        StatusCode::INTERNAL_SERVER_ERROR,
                        Json(serde_json::json!({
                            "error": { "code": "InternalServerError", "message": "boom" }
                        })),
                    )
                }),
            );

        let listener_500 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr_500 = listener_500.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener_500, app_500).await;
        });

        let client_500 = test_graph_client(format!("http://{addr_500}/v1.0"));
        let err = client_500
            .list_all_accessible_sections(&[nb])
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("500"),
            "expected 500 error propagation, got: {err}"
        );
    }

    #[test]
    fn test_is_not_found_error_requires_authoritative_http_404_or_410() {
        use crate::graph::client::{graph_http_status, GraphApiError};
        use reqwest::StatusCode;

        let err_404 = anyhow::Error::new(GraphApiError {
            status: StatusCode::NOT_FOUND,
            method: "GET".to_string(),
            url: "https://graph.microsoft.com/v1.0/me/onenote/pages/p-1".to_string(),
            detail: "404 Not Found [ResourceNotFound] Page not found".to_string(),
        });
        assert_eq!(graph_http_status(&err_404), Some(404));
        assert!(is_not_found_error(&err_404));

        let err_410 = anyhow::Error::new(GraphApiError {
            status: StatusCode::GONE,
            method: "GET".to_string(),
            url: "https://graph.microsoft.com/v1.0/me/onenote/pages/p-1".to_string(),
            detail: "410 Gone".to_string(),
        });
        assert_eq!(graph_http_status(&err_410), Some(410));
        assert!(is_not_found_error(&err_410));

        // Non-404/410 status whose backend message mentions "404" / "ResourceNotFound" / "not found"
        let err_503_with_404_text = anyhow::Error::new(GraphApiError {
            status: StatusCode::SERVICE_UNAVAILABLE,
            method: "GET".to_string(),
            url: "https://graph.microsoft.com/v1.0/me/onenote/pages/p-1".to_string(),
            detail: "503 Service Unavailable [ResourceNotFound] upstream backend 404 item not found"
                .to_string(),
        });
        assert_eq!(graph_http_status(&err_503_with_404_text), Some(503));
        assert!(!is_not_found_error(&err_503_with_404_text));

        // String-only formatted Graph 500 mentioning 404 must also NOT match
        let err_500_str = anyhow::anyhow!(
            "Graph API error (GET https://graph.microsoft.com/v1.0/me/onenote/pages/p-1): 500 Internal Server Error backend 404 not found"
        );
        assert_eq!(graph_http_status(&err_500_str), Some(500));
        assert!(!is_not_found_error(&err_500_str));

        // Plain non-Graph error mentioning 404 / not found must NOT match
        let plain_err = anyhow::anyhow!("proxy error: 404 not found");
        assert_eq!(graph_http_status(&plain_err), None);
        assert!(!is_not_found_error(&plain_err));
    }

    #[tokio::test]
    async fn test_msa_and_scopeless_tokens_skip_joined_teams() {
        use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};

        let joined_teams_hits = Arc::new(AtomicUsize::new(0));
        let hits_clone = joined_teams_hits.clone();
        let fail_me = Arc::new(AtomicBool::new(false));
        let fail_me_clone = fail_me.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/onenote/notebooks",
                get(move || {
                    let fail = fail_me_clone.clone();
                    async move {
                        if fail.load(Ordering::SeqCst) {
                            (
                                StatusCode::INTERNAL_SERVER_ERROR,
                                Json(serde_json::json!({
                                    "error": { "code": "InternalServerError", "message": "me failed" }
                                })),
                            )
                        } else {
                            (
                                StatusCode::OK,
                                Json(serde_json::json!({
                                    "value": [
                                        {
                                            "id": "nb-1",
                                            "displayName": "Personal",
                                            "isDefault": true
                                        }
                                    ]
                                })),
                            )
                        }
                    }
                }),
            )
            .route(
                "/v1.0/me/joinedTeams",
                get(move || {
                    let hits = hits_clone.clone();
                    async move {
                        hits.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({
                            "value": [
                                { "id": "team-empty", "displayName": "Empty Team" }
                            ]
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/groups/{gid}/onenote/notebooks",
                get(|| async { Json(serde_json::json!({ "value": [] })) }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let base_url = format!("http://{addr}/v1.0");

        // 1. Consumer MSA JWT (tid = 9188040d-6c67-4c5b-b112-36a304b66dad) skips /me/joinedTeams
        let msa_jwt = make_test_jwt(&serde_json::json!({
            "tid": "9188040d-6c67-4c5b-b112-36a304b66dad",
            "scp": "User.Read Notes.ReadWrite Team.ReadBasic.All"
        }));
        let client_msa_jwt =
            test_graph_client_with_token(base_url.clone(), msa_jwt, "common".into());
        let nbs = client_msa_jwt
            .list_all_accessible_notebooks(true)
            .await
            .unwrap();
        assert_eq!(nbs.len(), 1);
        assert_eq!(joined_teams_hits.load(Ordering::SeqCst), 0);

        // 2. Consumer tenant_id = "consumers" with opaque token skips /me/joinedTeams
        let client_consumers = test_graph_client_with_token(
            base_url.clone(),
            "EwB4A8l6BAAU-opaque".into(),
            "consumers".into(),
        );
        let nbs = client_consumers
            .list_all_accessible_notebooks(true)
            .await
            .unwrap();
        assert_eq!(nbs.len(), 1);
        assert_eq!(joined_teams_hits.load(Ordering::SeqCst), 0);

        // 3. Org JWT without group/team read scopes skips /me/joinedTeams
        let scopeless_jwt = make_test_jwt(&serde_json::json!({
            "tid": "org-tenant-123",
            "scp": "User.Read Notes.ReadWrite"
        }));
        let client_no_group_scope =
            test_graph_client_with_token(base_url.clone(), scopeless_jwt, "org-tenant-123".into());
        let nbs = client_no_group_scope
            .list_all_accessible_notebooks(true)
            .await
            .unwrap();
        assert_eq!(nbs.len(), 1);
        assert_eq!(joined_teams_hits.load(Ordering::SeqCst), 0);

        // 4. Org JWT with Team.ReadBasic.All queries /me/joinedTeams, AND if /me fails while group returns [],
        // me_err is propagated instead of returning Ok([])
        let org_with_team_jwt = make_test_jwt(&serde_json::json!({
            "tid": "org-tenant-123",
            "scp": "User.Read Notes.ReadWrite Team.ReadBasic.All"
        }));
        let client_org =
            test_graph_client_with_token(base_url, org_with_team_jwt, "org-tenant-123".into());
        let nbs = client_org
            .list_all_accessible_notebooks(true)
            .await
            .unwrap();
        assert_eq!(nbs.len(), 1);
        assert_eq!(joined_teams_hits.load(Ordering::SeqCst), 1);

        fail_me.store(true, Ordering::SeqCst);
        let err = client_org
            .list_all_accessible_notebooks(true)
            .await
            .unwrap_err();
        assert!(
            err.to_string().contains("500"),
            "empty group notebooks must not mask /me/onenote/notebooks failure: {err}"
        );
    }

    fn stored_tokens(access_token: String, tenant_id: &str) -> StoredTokens {
        StoredTokens {
            access_token,
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: tenant_id.to_string(),
            user_id: None,
            user_display_name: None,
            user_email: None,
            auth_mode: "pkce".to_string(),
        }
    }

    #[test]
    fn joined_teams_decision_separates_consumer_from_missing_org_scopes() {
        // Work tenant, only Notes.* scopes (a `/common` sign-in): skipped with a hint.
        let notes_only = stored_tokens(
            make_test_jwt(&serde_json::json!({
                "tid": "org-tenant-123",
                "scp": "User.Read Notes.Read Notes.ReadWrite Notes.Create"
            })),
            "common",
        );
        assert_eq!(
            joined_teams_decision(Some(&notes_only)),
            JoinedTeamsDecision::SkipMissingOrgScopes
        );
        assert_eq!(
            teams_skipped_warning(Some(&notes_only)),
            Some(TEAMS_SKIPPED_MISSING_ORG_SCOPES_WARNING)
        );
        assert!(TEAMS_SKIPPED_MISSING_ORG_SCOPES_WARNING.contains("onenote login --tenant-id"));

        // Personal Microsoft account: skipped silently.
        let consumer = stored_tokens(
            make_test_jwt(&serde_json::json!({
                "tid": "9188040d-6c67-4c5b-b112-36a304b66dad",
                "scp": "User.Read Notes.ReadWrite"
            })),
            "common",
        );
        assert_eq!(
            joined_teams_decision(Some(&consumer)),
            JoinedTeamsDecision::SkipConsumer
        );
        assert_eq!(teams_skipped_warning(Some(&consumer)), None);

        // Team.ReadBasic.All present, opaque token, or no token at all: query.
        let with_team = stored_tokens(
            make_test_jwt(&serde_json::json!({
                "tid": "org-tenant-123",
                "scp": "User.Read Notes.ReadWrite Team.ReadBasic.All"
            })),
            "org-tenant-123",
        );
        assert_eq!(
            joined_teams_decision(Some(&with_team)),
            JoinedTeamsDecision::Query
        );
        let opaque = stored_tokens("opaque-token".to_string(), "org-tenant-123");
        assert_eq!(joined_teams_decision(Some(&opaque)), JoinedTeamsDecision::Query);
        assert_eq!(joined_teams_decision(None), JoinedTeamsDecision::Query);
        assert_eq!(teams_skipped_warning(None), None);
    }

    #[test]
    fn max_joined_teams_env_value_is_clamped() {
        assert_eq!(max_joined_teams_from(None), 100);
        assert_eq!(max_joined_teams_from(Some("")), 100);
        assert_eq!(max_joined_teams_from(Some("abc")), 100);
        assert_eq!(max_joined_teams_from(Some("-3")), 100);
        assert_eq!(max_joined_teams_from(Some(" 40 ")), 40);
        assert_eq!(max_joined_teams_from(Some("0")), 1);
        assert_eq!(max_joined_teams_from(Some("100000")), 500);
    }

    #[tokio::test]
    async fn joined_teams_cap_truncation_and_failures_are_reported() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let group_hits: Arc<Mutex<Vec<String>>> = Arc::new(Mutex::new(Vec::new()));
        let group_hits_clone = group_hits.clone();
        let fail_teams = Arc::new(AtomicBool::new(false));
        let fail_teams_clone = fail_teams.clone();

        let app = Router::new()
            .route(
                "/v1.0/me/onenote/notebooks",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{ "id": "nb-me", "displayName": "Personal", "isDefault": true }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/joinedTeams",
                get(move || {
                    let fail = fail_teams_clone.clone();
                    async move {
                        if fail.load(Ordering::SeqCst) {
                            return (
                                StatusCode::FORBIDDEN,
                                Json(serde_json::json!({
                                    "error": { "code": "Forbidden", "message": "no teams for you" }
                                })),
                            );
                        }
                        let teams: Vec<serde_json::Value> = (1..=30)
                            .map(|i| {
                                serde_json::json!({
                                    "id": format!("team-{i:02}"),
                                    "displayName": format!("Team {i:02}")
                                })
                            })
                            .collect();
                        (StatusCode::OK, Json(serde_json::json!({ "value": teams })))
                    }
                }),
            )
            .route(
                "/v1.0/groups/{gid}/onenote/notebooks",
                get(move |AxumPath(gid): AxumPath<String>| {
                    let hits = group_hits_clone.clone();
                    async move {
                        hits.lock().unwrap().push(gid.clone());
                        Json(serde_json::json!({
                            "value": [{ "id": format!("nb-{gid}"), "displayName": format!("Notes {gid}") }]
                        }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = test_graph_client(format!("http://{addr}/v1.0"));

        // 30 joined Teams under the default cap of 100: all queried, no warning.
        // (25 was the old cap, so Teams 26..30 used to be invisible.)
        let all = client
            .list_all_accessible_notebooks_with_team_cap(true, DEFAULT_MAX_JOINED_TEAMS_FOR_ONENOTE)
            .await
            .unwrap();
        assert_eq!(all.notebooks.len(), 31);
        assert_eq!(group_hits.lock().unwrap().len(), 30);
        assert!(all.warnings.is_empty(), "{:?}", all.warnings);
        assert!(all.complete_scopes.contains("group:team-30"));

        // Exactly at the cap is not a truncation.
        let exact = client
            .list_all_accessible_notebooks_with_team_cap(true, 30)
            .await
            .unwrap();
        assert!(exact.warnings.is_empty(), "{:?}", exact.warnings);

        // Cap 2: two Teams queried and one warning naming the override.
        group_hits.lock().unwrap().clear();
        let capped = client
            .list_all_accessible_notebooks_with_team_cap(true, 2)
            .await
            .unwrap();
        assert_eq!(capped.notebooks.len(), 3);
        let mut capped_hits = group_hits.lock().unwrap().clone();
        capped_hits.sort();
        assert_eq!(capped_hits, ["team-01", "team-02"]);
        assert_eq!(capped.warnings.len(), 1);
        assert!(capped.warnings[0].contains("first 2 of your joined Teams"));
        assert!(capped.warnings[0].contains("ONENOTE_MAX_TEAMS"));
        assert!(!capped.complete_scopes.contains("group:team-03"));

        // Background sync under the automatic request budget keeps the cap of 25.
        let budgeted = client.with_automatic_budget(
            &crate::graph::AutomaticRequestBudget::default(),
            std::time::Instant::now(),
            1_000,
        );
        assert_eq!(budgeted.max_joined_teams(), 25);
        group_hits.lock().unwrap().clear();
        let background = budgeted
            .list_all_accessible_notebooks_scoped(true)
            .await
            .unwrap();
        assert_eq!(group_hits.lock().unwrap().len(), 25);
        assert_eq!(background.warnings.len(), 1);

        // /me/joinedTeams fails: personal notebooks still come back, with a warning.
        fail_teams.store(true, Ordering::SeqCst);
        group_hits.lock().unwrap().clear();
        let failed = client
            .list_all_accessible_notebooks_with_team_cap(true, DEFAULT_MAX_JOINED_TEAMS_FOR_ONENOTE)
            .await
            .unwrap();
        assert_eq!(failed.notebooks.len(), 1);
        assert_eq!(failed.notebooks[0].notebook_key, "me|nb-me");
        assert!(group_hits.lock().unwrap().is_empty());
        assert_eq!(failed.warnings.len(), 1);
        assert!(failed.warnings[0].contains("listing your joined Teams failed"));
        assert!(failed.warnings[0].contains("403"), "{}", failed.warnings[0]);

        // Without include_teams nothing Teams-related is reported.
        let personal_only = client
            .list_all_accessible_notebooks_with_team_cap(false, 2)
            .await
            .unwrap();
        assert!(personal_only.warnings.is_empty());
    }

    #[tokio::test]
    async fn section_listing_reports_partial_section_group_fetch() {
        use std::sync::atomic::{AtomicBool, Ordering};

        let fail_group = Arc::new(AtomicBool::new(true));
        let fail_group_clone = fail_group.clone();
        let fail_direct = Arc::new(AtomicBool::new(false));
        let fail_direct_clone = fail_direct.clone();
        let server_error = || {
            (
                StatusCode::INTERNAL_SERVER_ERROR,
                Json(serde_json::json!({
                    "error": { "code": "InternalServerError", "message": "backend down" }
                })),
            )
        };

        let app = Router::new()
            .route(
                "/v1.0/me/onenote/notebooks/{nb_id}/sections",
                get(move || {
                    let fail = fail_direct_clone.clone();
                    async move {
                        if fail.load(Ordering::SeqCst) {
                            return server_error();
                        }
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "value": [{ "id": "sec-direct", "displayName": "Direct" }]
                            })),
                        )
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/notebooks/{nb_id}/sectionGroups",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "sg-1",
                            "displayName": "Group",
                            "parentNotebook": { "id": "nb-1" }
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups/{sg_id}/sections",
                get(move || {
                    let fail = fail_group_clone.clone();
                    async move {
                        if fail.load(Ordering::SeqCst) {
                            return server_error();
                        }
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "value": [{
                                    "id": "sec-grouped",
                                    "displayName": "Grouped",
                                    "parentSectionGroup": { "id": "sg-1" }
                                }]
                            })),
                        )
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups/{sg_id}/sectionGroups",
                get(|| async { Json(serde_json::json!({ "value": [] })) }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = test_graph_client(format!("http://{addr}/v1.0"));

        // The section group's sections fail: what was fetched is returned, flagged partial.
        let (sections, complete) = client
            .list_onenote_sections_scoped(&OnenoteScope::Me, Some("nb-1"))
            .await
            .unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].section_key, "me|sec-direct");
        assert!(!complete);
        // The plain wrapper keeps returning just the list.
        assert_eq!(
            client
                .list_onenote_sections(&OnenoteScope::Me, Some("nb-1"))
                .await
                .unwrap()
                .len(),
            1
        );

        // Everything succeeds: complete.
        fail_group.store(false, Ordering::SeqCst);
        let (sections, complete) = client
            .list_onenote_sections_scoped(&OnenoteScope::Me, Some("nb-1"))
            .await
            .unwrap();
        assert_eq!(sections.len(), 2);
        assert!(complete);

        // Direct sections fail but grouped ones are found: partial, not an error.
        fail_direct.store(true, Ordering::SeqCst);
        let (sections, complete) = client
            .list_onenote_sections_scoped(&OnenoteScope::Me, Some("nb-1"))
            .await
            .unwrap();
        assert_eq!(sections.len(), 1);
        assert_eq!(sections[0].section_key, "me|sec-grouped");
        assert!(!complete);

        // Nothing at all could be fetched: still an error, as before.
        fail_group.store(true, Ordering::SeqCst);
        assert!(client
            .list_onenote_sections_scoped(&OnenoteScope::Me, Some("nb-1"))
            .await
            .is_err());
    }

    #[tokio::test]
    async fn pagelevel_unsupported_is_remembered_after_first_400() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let pagelevel_hits = Arc::new(AtomicUsize::new(0));
        let plain_hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/v1.0/me/onenote/sections/sec-1/pages",
                get({
                    let pl = pagelevel_hits.clone();
                    let ok = plain_hits.clone();
                    move |Query(params): Query<HashMap<String, String>>| async move {
                        if params.contains_key("pagelevel") {
                            pl.fetch_add(1, Ordering::SeqCst);
                            return (
                                StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({
                                    "error": { "code": "20102", "message": "pagelevel query option is invalid" }
                                })),
                            );
                        }
                        ok.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "value": [{ "id": "p-1", "title": "Note 1" }]
                            })),
                        )
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/pages/p-1",
                get({
                    let pl = pagelevel_hits.clone();
                    let ok = plain_hits.clone();
                    move |Query(params): Query<HashMap<String, String>>| async move {
                        if params.contains_key("pagelevel") {
                            pl.fetch_add(1, Ordering::SeqCst);
                            return (
                                StatusCode::BAD_REQUEST,
                                Json(serde_json::json!({
                                    "error": { "code": "20102", "message": "pagelevel query option is invalid" }
                                })),
                            );
                        }
                        ok.fetch_add(1, Ordering::SeqCst);
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "id": "p-1",
                                "title": "Note 1",
                                "parentSection": { "id": "sec-1", "displayName": "Section 1" }
                            })),
                        )
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/pages/p-1/content",
                get(|| async {
                    (
                        StatusCode::OK,
                        [("content-type", "text/html")],
                        "<!DOCTYPE html><html><head><title>Note 1</title></head><body><div id=\"div:{1}\"><p id=\"p:{1}\">Hello</p></div></body></html>",
                    )
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(format!("http://{addr}/v1.0"));
        let (pages, has_more) = client
            .list_onenote_pages_with_more(&OnenoteScope::Me, "sec-1", 50)
            .await
            .unwrap();
        assert_eq!(pages.len(), 1);
        assert!(!has_more);
        assert_eq!(pagelevel_hits.load(Ordering::SeqCst), 1);

        // Subsequent summary + detail calls for a page cached in that section
        // must skip `?pagelevel=true` completely and keep the cached level.
        let mut cached = pages[0].clone();
        cached.level = 1;
        let summary = client
            .get_onenote_page_summary(&OnenoteScope::Me, "p-1", Some(cached))
            .await
            .unwrap();
        assert_eq!(summary.id, "p-1");
        assert_eq!(
            summary.level, 1,
            "a level Graph omitted must not flatten the subpage"
        );
        let detail = client
            .get_onenote_page_detail(&OnenoteScope::Me, "p-1", Some(summary))
            .await
            .unwrap();
        assert!(detail.body_plain.contains("Hello"));
        assert_eq!(
            pagelevel_hits.load(Ordering::SeqCst),
            1,
            "pagelevel=true 400 fallback must be remembered across calls"
        );
        assert_eq!(plain_hits.load(Ordering::SeqCst), 3);
        srv.abort();
    }

    #[tokio::test]
    async fn list_all_accessible_hierarchy_scoped_overlaps_personal_requests() {
        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let app = Router::new()
            .route(
                "/v1.0/me/onenote/notebooks",
                get({
                    let b = barrier.clone();
                    move || async move {
                        b.wait().await;
                        Json(serde_json::json!({
                            "value": [{ "id": "nb-1", "displayName": "Personal" }]
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/sections",
                get({
                    let b = barrier.clone();
                    move || async move {
                        b.wait().await;
                        Json(serde_json::json!({
                            "value": [{
                                "id": "sec-1",
                                "displayName": "Notes",
                                "parentNotebook": { "id": "nb-1", "displayName": "Personal" }
                            }]
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups",
                get({
                    let b = barrier.clone();
                    move || async move {
                        b.wait().await;
                        Json(serde_json::json!({ "value": [] }))
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let client = test_graph_client(format!("http://{addr}/v1.0"));
        let (nb_res, sec_res) = tokio::time::timeout(
            std::time::Duration::from_secs(3),
            client.list_all_accessible_hierarchy_scoped(false),
        )
        .await
        .expect("notebooks, sections, and sectionGroups must execute concurrently")
        .unwrap();
        assert_eq!(nb_res.notebooks.len(), 1);
        assert_eq!(sec_res.sections.len(), 1);
        assert!(nb_res.complete_scopes.contains("me"));
        assert!(sec_res.complete_scopes.contains("me"));
        srv.abort();
    }

    #[tokio::test]
    async fn pagelevel_400_in_one_section_leaves_other_sections_hierarchical() {
        use std::sync::atomic::{AtomicUsize, Ordering};

        let a_pagelevel = Arc::new(AtomicUsize::new(0));
        let a_page_pagelevel = Arc::new(AtomicUsize::new(0));
        let b_pagelevel = Arc::new(AtomicUsize::new(0));
        let b_plain = Arc::new(AtomicUsize::new(0));
        let rejected = || {
            (
                StatusCode::BAD_REQUEST,
                Json(serde_json::json!({
                    "error": { "code": "20152", "message": "pagelevel is not supported here" }
                })),
            )
        };
        let app = Router::new()
            .route(
                "/v1.0/me/onenote/sections/sec-a/pages",
                get({
                    let hits = a_pagelevel.clone();
                    move |Query(params): Query<HashMap<String, String>>| async move {
                        if params.contains_key("pagelevel") {
                            hits.fetch_add(1, Ordering::SeqCst);
                            return rejected();
                        }
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({ "value": [{ "id": "pa-1", "title": "A1" }] })),
                        )
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/sections/sec-b/pages",
                get({
                    let (pl, plain) = (b_pagelevel.clone(), b_plain.clone());
                    move |Query(params): Query<HashMap<String, String>>| async move {
                        if params.contains_key("pagelevel") {
                            pl.fetch_add(1, Ordering::SeqCst);
                            return Json(serde_json::json!({ "value": [
                                { "id": "pb-1", "title": "Parent", "level": 0, "order": 0 },
                                { "id": "pb-2", "title": "Child", "level": 1, "order": 1 }
                            ] }));
                        }
                        plain.fetch_add(1, Ordering::SeqCst);
                        Json(serde_json::json!({ "value": [
                            { "id": "pb-1", "title": "Parent" },
                            { "id": "pb-2", "title": "Child" }
                        ] }))
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/pages/pa-1",
                get({
                    let hits = a_page_pagelevel.clone();
                    move |Query(params): Query<HashMap<String, String>>| async move {
                        if params.contains_key("pagelevel") {
                            hits.fetch_add(1, Ordering::SeqCst);
                            return rejected();
                        }
                        (
                            StatusCode::OK,
                            Json(serde_json::json!({
                                "id": "pa-1",
                                "title": "A1",
                                "parentSection": { "id": "sec-a", "displayName": "A" }
                            })),
                        )
                    }
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = test_graph_client(format!("http://{addr}/v1.0"));

        let a = client
            .list_onenote_page_listing(&OnenoteScope::Me, "sec-a", 50)
            .await
            .unwrap();
        assert!(
            !a.levels_known,
            "a pagelevel 400 must report levels as unknown"
        );
        assert_eq!(a_pagelevel.load(Ordering::SeqCst), 1);

        // Section B is unaffected by section A's 400: it still asks for levels.
        let b = client
            .list_onenote_page_listing(&OnenoteScope::Me, "sec-b", 50)
            .await
            .unwrap();
        assert!(b.levels_known);
        assert_eq!(b_pagelevel.load(Ordering::SeqCst), 1);
        assert_eq!(b_plain.load(Ordering::SeqCst), 0);
        assert_eq!(
            b.pages
                .iter()
                .map(|p| (p.id.as_str(), p.level))
                .collect::<Vec<_>>(),
            vec![("pb-1", 0), ("pb-2", 1)]
        );

        // Section A's 400 is remembered for A only.
        let a_again = client
            .list_onenote_page_listing(&OnenoteScope::Me, "sec-a", 50)
            .await
            .unwrap();
        assert!(!a_again.levels_known);
        assert_eq!(a_pagelevel.load(Ordering::SeqCst), 1);
        let b_again = client
            .list_onenote_page_listing(&OnenoteScope::Me, "sec-b", 50)
            .await
            .unwrap();
        assert!(b_again.levels_known);
        assert_eq!(b_pagelevel.load(Ordering::SeqCst), 2);
        assert_eq!(b_plain.load(Ordering::SeqCst), 0);

        // A page cached in section A skips the parameter and keeps its cached level.
        let mut cached = a.pages[0].clone();
        cached.level = 2;
        let summary = client
            .get_onenote_page_summary(&OnenoteScope::Me, "pa-1", Some(cached))
            .await
            .unwrap();
        assert_eq!(a_page_pagelevel.load(Ordering::SeqCst), 0);
        assert_eq!(summary.level, 2);
        srv.abort();
    }

    #[tokio::test]
    async fn failed_or_truncated_section_group_listing_keeps_scope_incomplete() {
        use std::sync::atomic::{AtomicU8, Ordering};

        // 0: notebook sectionGroups fail; 1: none; 2: more than the traversal cap.
        let nb_groups_mode = Arc::new(AtomicU8::new(0));
        let app = Router::new()
            .route(
                "/v1.0/me/onenote/notebooks",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{ "id": "nb-1", "displayName": "Personal" }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sections",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "sec-1",
                            "displayName": "Notes",
                            "parentNotebook": { "id": "nb-1", "displayName": "Personal" }
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups",
                get(|| async {
                    (
                        StatusCode::FORBIDDEN,
                        Json(serde_json::json!({ "error": { "code": "40004", "message": "denied" } })),
                    )
                }),
            )
            .route(
                "/v1.0/me/onenote/notebooks/nb-1/sectionGroups",
                get({
                    let mode = nb_groups_mode.clone();
                    move || async move {
                        match mode.load(Ordering::SeqCst) {
                            0 => (
                                StatusCode::FORBIDDEN,
                                Json(serde_json::json!({
                                    "error": { "code": "40004", "message": "denied" }
                                })),
                            ),
                            1 => (StatusCode::OK, Json(serde_json::json!({ "value": [] }))),
                            _ => {
                                let groups: Vec<serde_json::Value> = (0..=MAX_SECTION_GROUP_TRAVERSAL)
                                    .map(|i| {
                                        serde_json::json!({
                                            "id": format!("sg-{i}"),
                                            "displayName": format!("Group {i}")
                                        })
                                    })
                                    .collect();
                                (StatusCode::OK, Json(serde_json::json!({ "value": groups })))
                            }
                        }
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups/{sg_id}/sectionGroups",
                get(|| async { Json(serde_json::json!({ "value": [] })) }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let client = test_graph_client(format!("http://{addr}/v1.0"));

        // `/sections` succeeded but no section-group listing did: the sections
        // are returned, yet the scope must not be offered for pruning.
        let (_nb, failed) = client
            .list_all_accessible_hierarchy_scoped(false)
            .await
            .unwrap();
        assert_eq!(failed.sections.len(), 1);
        assert!(
            !failed.complete_scopes.contains("me"),
            "a failed sectionGroups fetch must leave scope_complete = false"
        );

        // The per-notebook fallback walk succeeds: the scope is complete again.
        nb_groups_mode.store(1, Ordering::SeqCst);
        let (_nb, walked) = client
            .list_all_accessible_hierarchy_scoped(false)
            .await
            .unwrap();
        assert!(walked.complete_scopes.contains("me"));

        // The walk stops at the traversal cap: incomplete.
        nb_groups_mode.store(2, Ordering::SeqCst);
        let (_nb, capped) = client
            .list_all_accessible_hierarchy_scoped(false)
            .await
            .unwrap();
        assert_eq!(capped.section_groups.len(), MAX_SECTION_GROUP_TRAVERSAL);
        assert!(!capped.complete_scopes.contains("me"));
        srv.abort();
    }
}
