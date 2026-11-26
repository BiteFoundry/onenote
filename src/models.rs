//! Shared domain models and wire-independent contracts across Graph, Storage, Parser, Sync, TUI, CLI, HTTP, and MCP.
//!
//! IMPORTANT: Do NOT break existing field names or signatures in this file when working in a module,
//! as all sibling modules compile against these exact types.

use std::path::PathBuf;

use serde::{Deserialize, Serialize};

/// Sentinel summary emitted via `CoreEvent::SyncStateChanged { busy: false, summary }`
/// while background polling is silently retrying transient errors.
pub const POLL_RETRY_SUMMARY: &str = "__poll_retrying__";

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct UserProfile {
    pub id: String,
    pub display_name: String,
    pub email: Option<String>,
    pub job_title: Option<String>,
    pub department: Option<String>,
}

/// Outgoing inline image upload (e.g. clipboard PNG or local image attached to a OneNote page).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct InlineImageUpload {
    pub temp_id: String,
    pub mime_type: String,
    pub bytes: Vec<u8>,
    pub file_name: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadedFile {
    pub path: PathBuf,
    pub file_name: String,
    pub bytes_written: u64,
}

/// Canonical resource download result returned by CLI (`onenote resource --json`),
/// HTTP (`POST /v1/onenote/resources/download`), and MCP (`download_resource`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct DownloadedResource {
    pub ok: bool,
    pub path: String,
    pub file_name: String,
    pub mime_type: String,
    pub size_bytes: u64,
    pub is_image: bool,
}

// ---------------------------------------------------------------------------
// Microsoft OneNote Domain & XHTML/PATCH Models (Personal + Group/Site Shared)
// ---------------------------------------------------------------------------

/// Identifies which OneNote container owns a notebook/section/page (`/me`, `/groups/{id}`, or `/sites/{id}`).
#[derive(Debug, Clone, PartialEq, Eq, Hash, Serialize, Deserialize, Default)]
#[serde(tag = "scope_type", rename_all = "snake_case")]
pub enum OnenoteScope {
    #[default]
    Me,
    Group {
        group_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        group_name: Option<String>,
    },
    Site {
        site_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        site_name: Option<String>,
    },
}

impl OnenoteScope {
    /// Canonical stable key prefix (`"me"`, `"group:{id}"`, `"site:{id}"`).
    pub fn scope_key(&self) -> String {
        match self {
            Self::Me => "me".to_string(),
            Self::Group { group_id, .. } => format!("group:{group_id}"),
            Self::Site { site_id, .. } => format!("site:{site_id}"),
        }
    }

    pub fn from_scope_key(key: &str, name_hint: Option<String>) -> Option<Self> {
        let trimmed = key.trim();
        if trimmed.eq_ignore_ascii_case("me") || trimmed.is_empty() {
            return Some(Self::Me);
        }
        if let Some(gid) = trimmed.strip_prefix("group:") {
            if !gid.is_empty() {
                return Some(Self::Group {
                    group_id: gid.to_string(),
                    group_name: name_hint,
                });
            }
        }
        if let Some(sid) = trimmed.strip_prefix("site:") {
            if !sid.is_empty() {
                return Some(Self::Site {
                    site_id: sid.to_string(),
                    site_name: name_hint,
                });
            }
        }
        None
    }

    /// Microsoft Graph v1.0 relative base path (`"/me/onenote"`, `"/groups/{id}/onenote"`, `"/sites/{id}/onenote"`).
    pub fn graph_base_path(&self) -> String {
        match self {
            Self::Me => "/me/onenote".to_string(),
            Self::Group { group_id, .. } => format!("/groups/{group_id}/onenote"),
            Self::Site { site_id, .. } => format!("/sites/{site_id}/onenote"),
        }
    }

    pub fn owner_label(&self) -> String {
        match self {
            Self::Me => "My Notebooks".to_string(),
            Self::Group {
                group_id,
                group_name,
            } => group_name
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(group_id)
                .to_string(),
            Self::Site { site_id, site_name } => site_name
                .as_deref()
                .filter(|s| !s.trim().is_empty())
                .unwrap_or(site_id)
                .to_string(),
        }
    }

    pub fn is_personal(&self) -> bool {
        matches!(self, Self::Me)
    }

    pub fn make_entity_key(&self, entity_id: &str) -> String {
        format!("{}|{}", self.scope_key(), entity_id)
    }
}

pub fn split_onenote_entity_key(composite_or_raw: &str) -> (OnenoteScope, String) {
    if let Some((scope_part, raw_id)) = composite_or_raw.split_once('|') {
        if let Some(scope) = OnenoteScope::from_scope_key(scope_part, None) {
            if !raw_id.is_empty() {
                return (scope, raw_id.to_string());
            }
        }
    }
    (OnenoteScope::Me, composite_or_raw.to_string())
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenoteNotebook {
    /// Composite key `"{scope_key}|{id}"`.
    pub notebook_key: String,
    pub id: String,
    pub scope: OnenoteScope,
    pub display_name: String,
    pub color: Option<String>,
    pub is_default: bool,
    pub is_shared: bool,
    pub user_role: Option<String>,
    pub created_at: Option<String>,
    pub last_modified_at: Option<String>,
    pub created_by: Option<String>,
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenoteSectionGroup {
    /// Composite key `"{scope_key}|{id}"`.
    pub section_group_key: String,
    pub id: String,
    pub scope: OnenoteScope,
    pub notebook_id: Option<String>,
    pub parent_section_group_id: Option<String>,
    pub display_name: String,
    pub created_at: Option<String>,
    pub last_modified_at: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenoteSection {
    /// Composite key `"{scope_key}|{id}"`.
    pub section_key: String,
    pub id: String,
    pub scope: OnenoteScope,
    pub notebook_id: Option<String>,
    pub notebook_name: Option<String>,
    pub parent_section_group_id: Option<String>,
    pub parent_section_group_name: Option<String>,
    pub display_name: String,
    pub color: Option<String>,
    pub is_default: bool,
    pub pages_url: Option<String>,
    pub created_at: Option<String>,
    pub last_modified_at: Option<String>,
    pub web_url: Option<String>,
}

impl OnenoteSection {
    /// The section's parent section group name, when it sits in one.
    pub fn section_group_name(&self) -> Option<&str> {
        self.parent_section_group_name
            .as_deref()
            .map(str::trim)
            .filter(|name| !name.is_empty())
    }

    /// `Group / Section` for a section inside a section group, else the plain
    /// section name, so same-named sections in different groups stay distinct.
    pub fn group_qualified_name(&self) -> String {
        match self.section_group_name() {
            Some(group) => format!("{group} / {}", self.display_name),
            None => self.display_name.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenotePageSummary {
    /// Composite key `"{scope_key}|{id}"`.
    pub page_key: String,
    pub id: String,
    pub scope: OnenoteScope,
    pub section_id: String,
    pub section_name: Option<String>,
    pub notebook_id: Option<String>,
    pub notebook_name: Option<String>,
    pub title: String,
    /// Indentation level in OneNote page list (`0` = root page, `1` = subpage, `2` = sub-subpage).
    pub level: i32,
    pub order: i32,
    pub created_at: Option<String>,
    pub last_modified_at: Option<String>,
    pub content_url: Option<String>,
    pub web_url: Option<String>,
    pub preview_text: Option<String>,
    pub has_todo: bool,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum OnenoteTagKind {
    Todo { completed: bool },
    Important,
    Question,
    Critical,
    Highlight,
    Idea,
    Custom(String),
}

impl OnenoteTagKind {
    pub fn is_todo(&self) -> bool {
        matches!(self, Self::Todo { .. })
    }

    pub fn data_tag_value(&self) -> String {
        match self {
            Self::Todo { completed: false } => "to-do".to_string(),
            Self::Todo { completed: true } => "to-do:completed".to_string(),
            Self::Important => "important".to_string(),
            Self::Question => "question".to_string(),
            Self::Critical => "critical".to_string(),
            Self::Highlight => "highlight".to_string(),
            Self::Idea => "idea".to_string(),
            Self::Custom(raw) => raw.clone(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenoteResource {
    pub element_id: Option<String>,
    pub resource_url: String,
    pub fullres_url: Option<String>,
    pub mime_type: String,
    pub file_name: Option<String>,
    pub is_image: bool,
    pub alt_text: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenoteBlock {
    /// System-generated Graph element ID (`p:{guid}{40}`, `div:{guid}{32}`, `table:{...}`, etc.) from `?includeIDs=true`.
    pub element_id: Option<String>,
    /// Optional OneNote `data-tag` attached to this element.
    pub tag: Option<OnenoteTagKind>,
    /// Original outer HTML snippet of this element so surgical tag toggling preserves inline styles/links.
    pub raw_html: Option<String>,
    pub kind: OnenoteBlockKind,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "block_type", rename_all = "snake_case")]
pub enum OnenoteBlockKind {
    Heading {
        level: u8,
        spans: Vec<RichSpan>,
    },
    Paragraph {
        spans: Vec<RichSpan>,
    },
    ListItem {
        ordered: bool,
        index: Option<usize>,
        depth: u8,
        spans: Vec<RichSpan>,
    },
    Quote {
        spans: Vec<RichSpan>,
    },
    CodeBlock {
        lang: Option<String>,
        code: String,
    },
    Table {
        headers: Vec<Vec<RichSpan>>,
        rows: Vec<Vec<Vec<RichSpan>>>,
    },
    Image {
        resource_index: usize,
        src: String,
        alt: Option<String>,
        width: Option<u32>,
        height: Option<u32>,
    },
    Attachment {
        resource_index: usize,
        name: String,
        src: String,
        mime_type: String,
    },
    Divider,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenotePageDetail {
    pub summary: OnenotePageSummary,
    pub raw_xhtml: String,
    pub body_markdown: String,
    pub body_plain: String,
    pub blocks: Vec<OnenoteBlock>,
    pub resources: Vec<OnenoteResource>,
    /// Primary `#div:{...}` content container ID on the page (if present) for clean `PATCH` append/replace.
    pub primary_div_id: Option<String>,
    pub synced_at: Option<String>,
}

/// Wire payload item for `PATCH /{scope}/pages/{id}/content` (`application/json`).
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenotePatchCommand {
    pub target: String,
    pub action: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub position: Option<String>,
    pub content: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OnenoteSearchHit {
    pub page_key: String,
    pub page_id: String,
    pub scope: OnenoteScope,
    pub notebook_name: Option<String>,
    pub section_id: String,
    pub section_name: Option<String>,
    pub title: String,
    pub last_modified_at: Option<String>,
    pub snippet: String,
    pub web_url: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct OutgoingOnenotePage {
    pub title: String,
    pub markdown_body: String,
    pub inline_images: Vec<InlineImageUpload>,
    pub level: Option<i32>,
}

// ---------------------------------------------------------------------------
// Sync & Notification Events
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct SyncCursor {
    pub scope_key: String,
    pub delta_link: Option<String>,
    pub history_next_link: Option<String>,
    pub last_synced_at: String,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct NotificationCandidate {
    pub item_id: String,
    pub section_name: String,
    pub title: String,
    pub preview: String,
}

/// Asynchronous events pushed from `SyncEngine` to TUI or live subscribers.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "event", content = "data", rename_all = "snake_case")]
pub enum CoreEvent {
    ProfileLoaded(UserProfile),
    OnenoteHierarchyUpdated {
        notebooks: Vec<OnenoteNotebook>,
        section_groups: Vec<OnenoteSectionGroup>,
        sections: Vec<OnenoteSection>,
    },
    OnenotePagesUpdated {
        section_key: String,
        pages: Vec<OnenotePageSummary>,
        notify_candidate: Option<NotificationCandidate>,
    },
    OnenotePageDetailUpdated(Box<OnenotePageDetail>),
    SyncStateChanged {
        busy: bool,
        summary: String,
    },
    SyncError(String),
}

// ---------------------------------------------------------------------------
// Parser Rich AST (shared between `parser` and `ui`)
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct TextStyleFlags {
    pub bold: bool,
    pub italic: bool,
    pub strike: bool,
    pub underline: bool,
    pub code: bool,
    pub mention: bool,
    pub link_url: Option<String>,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct RichSpan {
    pub text: String,
    pub style: TextStyleFlags,
}

impl RichSpan {
    pub fn plain(text: impl Into<String>) -> Self {
        Self {
            text: text.into(),
            style: TextStyleFlags::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub enum RichBlock {
    Paragraph(Vec<RichSpan>),
    Heading {
        level: u8,
        spans: Vec<RichSpan>,
    },
    Quote(Vec<RichSpan>),
    CodeBlock {
        lang: Option<String>,
        code: String,
    },
    BulletList(Vec<Vec<RichSpan>>),
    Table {
        headers: Vec<Vec<RichSpan>>,
        rows: Vec<Vec<Vec<RichSpan>>>,
    },
    Divider,
    AttachmentChip {
        id: String,
        name: Option<String>,
    },
    InlineImageChip {
        src: String,
        content_id: Option<String>,
        index: usize,
    },
    QuotedHistory {
        line_count: usize,
        blocks: Vec<RichBlock>,
    },
}

// ---------------------------------------------------------------------------
// Self-Updater Model
// ---------------------------------------------------------------------------

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct UpdateCheckResult {
    pub current_version: String,
    pub latest_version: String,
    pub update_available: bool,
    pub release_url: String,
    pub asset_name: Option<String>,
    pub asset_size: Option<u64>,
    pub updated: bool,
    pub binary_path: PathBuf,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_onenote_scope_keys_and_paths() {
        let me = OnenoteScope::Me;
        assert_eq!(me.scope_key(), "me");
        assert_eq!(me.graph_base_path(), "/me/onenote");
        assert_eq!(me.owner_label(), "My Notebooks");
        assert!(me.is_personal());
        assert_eq!(me.make_entity_key("nb-1"), "me|nb-1");

        let grp = OnenoteScope::Group {
            group_id: "g-42".to_string(),
            group_name: Some("Engineering".to_string()),
        };
        assert_eq!(grp.scope_key(), "group:g-42");
        assert_eq!(grp.graph_base_path(), "/groups/g-42/onenote");
        assert_eq!(grp.owner_label(), "Engineering");
        assert!(!grp.is_personal());

        let (parsed_scope, raw_id) = split_onenote_entity_key("group:g-42|sec-9");
        assert_eq!(parsed_scope.scope_key(), "group:g-42");
        assert_eq!(raw_id, "sec-9");

        let (fallback_scope, raw_only) = split_onenote_entity_key("raw-page-id");
        assert_eq!(fallback_scope, OnenoteScope::Me);
        assert_eq!(raw_only, "raw-page-id");
    }

    #[test]
    fn test_onenote_tag_kind_data_tag_values() {
        assert!(OnenoteTagKind::Todo { completed: false }.is_todo());
        assert_eq!(
            OnenoteTagKind::Todo { completed: false }.data_tag_value(),
            "to-do"
        );
        assert_eq!(
            OnenoteTagKind::Todo { completed: true }.data_tag_value(),
            "to-do:completed"
        );
        assert_eq!(OnenoteTagKind::Important.data_tag_value(), "important");
        assert_eq!(
            OnenoteTagKind::Custom("project-a".to_string()).data_tag_value(),
            "project-a"
        );
    }

    #[test]
    fn test_core_event_tagged_serialization() {
        let evt = CoreEvent::SyncStateChanged {
            busy: false,
            summary: POLL_RETRY_SUMMARY.to_string(),
        };
        let json = serde_json::to_value(&evt).unwrap();
        assert_eq!(json["event"], "sync_state_changed");
        assert_eq!(json["data"]["busy"], false);
        assert_eq!(json["data"]["summary"], POLL_RETRY_SUMMARY);
    }
}
