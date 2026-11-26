//! Adaptive two-lane background synchronization engine for Microsoft OneNote
//! (Lane 1: Fast Interactive Lane + Lane 2: Quiet Background Poll Lane).

use std::{
    collections::{HashMap, HashSet, VecDeque},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use futures::stream::{self, StreamExt};
use rand::Rng;
use serde::{Deserialize, Serialize};
use tokio::{
    sync::{mpsc, watch},
    time,
};

use crate::{
    config::AppConfig,
    graph::{AutomaticRequestBudget, AutomaticRequestBudgetExhausted, GraphClient},
    models::{
        split_onenote_entity_key, CoreEvent, NotificationCandidate, OnenotePageSummary,
        OnenoteScope, OnenoteSection, SyncCursor, POLL_RETRY_SUMMARY,
    },
    storage::Storage,
};

const IDLE_THRESHOLD_SECS: u64 = 120;
const DEEP_IDLE_THRESHOLD_SECS: u64 = 600;
const AUTH_EXPIRY_PAUSE_SECS: u64 = 30;
const MAX_ERROR_BACKOFF_MS: u64 = 300_000;
const BOOTSTRAP_MAX_SECTIONS: usize = 64;
const BACKGROUND_MAX_SECTIONS: usize = 6;
const BOOTSTRAP_SECTION_CONCURRENCY: usize = 4;
/// Page-list cap per section for explicit refreshes (20 requests at `$top=100`).
const SECTION_PAGES_LIMIT: usize = 2_000;
/// Cap for polls running under the automatic request budget (5 requests).
const BUDGETED_SECTION_PAGES_LIMIT: usize = 500;
const INACTIVE_SECTION_STALE_SECS: i64 = 300;
const MAX_REHYDRATE_PER_SYNC: usize = 16;
const BACKGROUND_MAX_REHYDRATE: usize = 4;
const DEFAULT_OPEN_PAGE_POLL_MS: u64 = 15_000;
const DEFAULT_ACTIVE_SECTION_POLL_MS: u64 = 60_000;
const AUTO_HOURLY_BUDGET: usize = 340;
const AUTO_HOURLY_HARD_CAP: usize = 350;
const STARTUP_HTTP_BUDGET: usize = 40;
const HIERARCHY_HOURLY_BUDGET: u64 = 40;
const MIN_BUDGETED_HIERARCHY_SECS: u64 = 270;
const MAX_BUDGETED_HIERARCHY_SECS: u64 = 3_600;
const OVERLAP_COALESCE_MS: u64 = 450;
const SECTION_COALESCE_MS: u64 = 1_500;
const SECTION_PAGE_PREFETCH_LIMIT: usize = 4;
const SECTION_PAGE_PREFETCH_CONCURRENCY: usize = 2;

fn section_pages_cursor_key(section_key: &str) -> String {
    format!("onenote:pages:{section_key}")
}

fn resolve_effective_scope(
    entity_key: &str,
    summary_hint: Option<&OnenotePageSummary>,
) -> (OnenoteScope, String) {
    let (parsed_scope, raw_id) = split_onenote_entity_key(entity_key);
    let scope = summary_hint
        .filter(|h| h.scope.scope_key() == parsed_scope.scope_key())
        .map(|h| h.scope.clone())
        .unwrap_or(parsed_scope);
    (scope, raw_id)
}

pub(crate) struct SyncSharedState {
    epoch: Instant,
    last_activity_ms: AtomicU64,
    activity_seq: AtomicU64,
    auth_pause_until_ms: AtomicU64,
}

impl SyncSharedState {
    pub(crate) fn new() -> Self {
        Self::with_epoch(Instant::now())
    }

    pub(crate) fn with_epoch(epoch: Instant) -> Self {
        Self {
            epoch,
            last_activity_ms: AtomicU64::new(0),
            activity_seq: AtomicU64::new(0),
            auth_pause_until_ms: AtomicU64::new(0),
        }
    }

    pub(crate) fn record_activity(&self, now: Instant) {
        let ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        self.last_activity_ms.store(ms, Ordering::Release);
        self.activity_seq.fetch_add(1, Ordering::AcqRel);
    }

    pub(crate) fn idle_snapshot(&self, now: Instant) -> (Duration, u64) {
        let seq = self.activity_seq.load(Ordering::Acquire);
        let last_ms = self.last_activity_ms.load(Ordering::Acquire);
        let now_ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        (Duration::from_millis(now_ms.saturating_sub(last_ms)), seq)
    }

    pub(crate) fn pause_auth(&self, now: Instant, duration: Duration) {
        let until_ms = (now.saturating_duration_since(self.epoch) + duration).as_millis() as u64;
        self.auth_pause_until_ms
            .store(until_ms.max(1), Ordering::Release);
    }

    pub(crate) fn clear_auth_pause(&self) {
        self.auth_pause_until_ms.store(0, Ordering::Release);
    }

    pub(crate) fn is_auth_paused(&self, now: Instant) -> bool {
        let until_ms = self.auth_pause_until_ms.load(Ordering::Acquire);
        if until_ms == 0 {
            return false;
        }
        let now_ms = now.saturating_duration_since(self.epoch).as_millis() as u64;
        now_ms < until_ms
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SyncErrorKind {
    Forbidden403,
    Unauthorized401,
    Transient,
}

fn extract_error_detail_for_status(lower: &str) -> &str {
    if let Some(idx) = lower.find("graph api error (") {
        let rest = &lower[idx..];
        if let Some((_, tail)) = rest.split_once("): ") {
            return tail;
        }
        return rest;
    }
    lower
}

pub(crate) fn classify_sync_error(err: &anyhow::Error) -> SyncErrorKind {
    let lower = format!("{err:#}").to_ascii_lowercase();
    let detail = extract_error_detail_for_status(&lower);

    if detail.contains("403")
        || lower.contains("forbidden")
        || lower.contains("authorization_requestdenied")
        || lower.contains("erroraccessdenied")
        || lower.contains("insufficient")
    {
        return SyncErrorKind::Forbidden403;
    }

    if detail.contains("401")
        || lower.contains("unauthorized")
        || lower.contains("token expired")
    {
        return SyncErrorKind::Unauthorized401;
    }

    SyncErrorKind::Transient
}

#[derive(Debug)]
pub(crate) struct ErrorBackoffState {
    base_ms: u64,
    consecutive_failures: u32,
    next_retry_at: Option<Instant>,
    degraded: bool,
}

impl ErrorBackoffState {
    pub(crate) fn new(base_ms: u64) -> Self {
        Self {
            base_ms: base_ms.max(1),
            consecutive_failures: 0,
            next_retry_at: None,
            degraded: false,
        }
    }

    pub(crate) fn compute_backoff_ms(base_ms: u64, failures: u32, jitter_factor: f64) -> u64 {
        let shift = failures.min(6);
        let raw_ms = base_ms
            .saturating_mul(1u64 << shift)
            .min(MAX_ERROR_BACKOFF_MS);
        let jittered = (raw_ms as f64 * jitter_factor).round() as u64;
        jittered.clamp(1, MAX_ERROR_BACKOFF_MS)
    }

    pub(crate) fn record_transient_failure(&mut self, now: Instant) -> bool {
        self.consecutive_failures = self.consecutive_failures.saturating_add(1);
        let jitter = rand::thread_rng().gen_range(0.8_f64..=1.2_f64);
        let delay_ms = Self::compute_backoff_ms(self.base_ms, self.consecutive_failures, jitter);
        self.next_retry_at = Some(now + Duration::from_millis(delay_ms));
        if !self.degraded {
            self.degraded = true;
            true
        } else {
            false
        }
    }

    pub(crate) fn record_success(&mut self) -> bool {
        self.consecutive_failures = 0;
        self.next_retry_at = None;
        if self.degraded {
            self.degraded = false;
            true
        } else {
            false
        }
    }

    pub(crate) fn can_attempt(&self, now: Instant) -> bool {
        self.next_retry_at.is_none_or(|t| now >= t)
    }

    pub(crate) fn is_degraded(&self) -> bool {
        self.degraded
    }

    pub(crate) fn clear_degraded(&mut self) {
        self.degraded = false;
    }
}

/// Active focus context shared from TUI/daemon clients to prioritize sync work.
#[derive(Debug, Clone, Default, PartialEq, Eq, Serialize, Deserialize)]
pub struct FocusTarget {
    pub notebook_key: Option<String>,
    pub section_key: Option<String>,
    pub page_key: Option<String>,
}

/// Interactive commands sent to the background sync engine.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "cmd", rename_all = "snake_case")]
pub enum SyncCommand {
    Bootstrap,
    RefreshAll,
    RefreshHierarchy,
    RefreshSectionPages { section_key: String },
    HydratePage { page_key: String },
    NotifyUserActivity,
    Shutdown,
}

#[derive(Clone)]
pub struct SyncController {
    focus_tx: watch::Sender<FocusTarget>,
    nudge_tx: mpsc::UnboundedSender<SyncCommand>,
}

impl SyncController {
    pub fn from_channels(
        focus_tx: watch::Sender<FocusTarget>,
        nudge_tx: mpsc::UnboundedSender<SyncCommand>,
    ) -> Self {
        Self { focus_tx, nudge_tx }
    }

    pub fn set_focus(&self, target: FocusTarget) {
        self.focus_tx.send_if_modified(|current| {
            if *current == target {
                false
            } else {
                *current = target;
                true
            }
        });
    }

    pub fn request(&self, cmd: SyncCommand) {
        let _ = self.nudge_tx.send(cmd);
    }

    pub fn notify_activity(&self) {
        self.request(SyncCommand::NotifyUserActivity);
    }
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct OnenoteBootstrapSummary {
    pub notebooks: usize,
    pub section_groups: usize,
    pub sections: usize,
    pub pages: usize,
}

/// Performs an initial OneNote sync: reconciles notebooks, section groups, sections,
/// and syncs pages across all accessible sections (bounded concurrency).
pub async fn bootstrap_onenote(
    graph: &GraphClient,
    storage: &Storage,
) -> Result<OnenoteBootstrapSummary> {
    Ok(sync_hierarchy_and_sections_inner(graph, storage, true, None)
        .await?
        .summary)
}

struct HierarchySyncOutcome {
    summary: OnenoteBootstrapSummary,
    synced_section_keys: HashSet<String>,
}

#[cfg(test)]
async fn sync_hierarchy_and_sections(
    graph: &GraphClient,
    storage: &Storage,
    force_all_sections: bool,
) -> Result<OnenoteBootstrapSummary> {
    Ok(sync_hierarchy_and_sections_inner(graph, storage, force_all_sections, None)
        .await?
        .summary)
}

/// One section and the result of listing its pages (pages, whether more exist).
type SectionPagesFetch = (
    OnenoteSection,
    Result<crate::graph::onenote::OnenotePageListing>,
);

async fn sync_hierarchy_and_sections_inner(
    graph: &GraphClient,
    storage: &Storage,
    force_all_sections: bool,
    skip_recent_section_key: Option<&str>,
) -> Result<HierarchySyncOutcome> {
    let start_epoch = storage.current_mutation_epoch();
    let (mut nb_res, mut sec_res) = graph.list_all_accessible_hierarchy_scoped(true).await?;

    // Guard against in-flight local notebook/section creations that completed after `start_epoch`.
    if storage.hierarchy_mutated_since(start_epoch) {
        let created_nbs = storage.notebooks_created_since(start_epoch);
        if !created_nbs.is_empty() {
            if let Ok(local_nbs) = storage.list_onenote_notebooks() {
                for nb in local_nbs {
                    if created_nbs.contains(&nb.notebook_key)
                        && !nb_res
                            .notebooks
                            .iter()
                            .any(|existing| existing.notebook_key == nb.notebook_key)
                    {
                        nb_res.notebooks.push(nb);
                    }
                }
            }
        }
        let created_secs = storage.sections_created_since(start_epoch);
        if !created_secs.is_empty() {
            if let Ok(local_secs) = storage.list_onenote_sections(None) {
                for sec in local_secs {
                    if created_secs.contains(&sec.section_key)
                        && !sec_res
                            .sections
                            .iter()
                            .any(|existing| existing.section_key == sec.section_key)
                    {
                        sec_res.sections.push(sec);
                    }
                }
            }
        }
    }

    let reconciled_scopes: HashSet<String> = nb_res
        .complete_scopes
        .intersection(&sec_res.complete_scopes)
        .cloned()
        .collect();
    if !reconciled_scopes.is_empty() {
        storage.reconcile_onenote_hierarchy_for_scopes(
            &nb_res.notebooks,
            &sec_res.section_groups,
            &sec_res.sections,
            &reconciled_scopes,
        )?;
    } else {
        storage.reconcile_onenote_notebooks_for_scopes(&nb_res.notebooks, &nb_res.complete_scopes)?;
        storage.reconcile_onenote_sections_for_scopes(
            &sec_res.section_groups,
            &sec_res.sections,
            &sec_res.complete_scopes,
        )?;
    }

    let notebooks = nb_res.notebooks;
    let section_groups = sec_res.section_groups;
    let sections = sec_res.sections;
    let now = chrono::Utc::now();

    let mut ordered_sections: Vec<(&OnenoteSection, Option<SyncCursor>, bool)> = sections
        .iter()
        .map(|s| {
            let cur = storage
                .get_sync_cursor(&section_pages_cursor_key(&s.section_key))
                .ok()
                .flatten();
            let modified_changed = cur.as_ref().is_none_or(|c| {
                s.last_modified_at.is_none()
                    || c.delta_link.as_deref() != s.last_modified_at.as_deref()
            });
            (s, cur, modified_changed)
        })
        .collect();
    ordered_sections.sort_by(|(a, cur_a, mod_a), (b, cur_b, mod_b)| {
        let prio = |s: &OnenoteSection| {
            if s.is_default && s.scope.is_personal() {
                0u8
            } else if s.scope.is_personal() {
                1u8
            } else {
                2u8
            }
        };
        let sync_ts_a = cur_a
            .as_ref()
            .map(|c| c.last_synced_at.as_str())
            .unwrap_or("");
        let sync_ts_b = cur_b
            .as_ref()
            .map(|c| c.last_synced_at.as_str())
            .unwrap_or("");
        mod_b
            .cmp(mod_a)
            .then_with(|| prio(a).cmp(&prio(b)))
            .then_with(|| sync_ts_a.cmp(sync_ts_b))
    });

    let iter = ordered_sections
        .into_iter()
        .filter_map(|(sec, cursor, modified_changed)| {
            if skip_recent_section_key == Some(sec.section_key.as_str()) {
                // Only the active-section path can confirm that a missing open page moved.
                return None;
            }
            if force_all_sections {
                return Some(sec.clone());
            }
            let needs_sync = match cursor {
                None => true,
                Some(cur) => {
                    let stale_by_age = chrono::DateTime::parse_from_rfc3339(&cur.last_synced_at)
                        .ok()
                        .is_some_and(|dt| {
                            now.signed_duration_since(dt.with_timezone(&chrono::Utc))
                                .num_seconds()
                                >= INACTIVE_SECTION_STALE_SECS
                        });
                    modified_changed || stale_by_age
                }
            };
            needs_sync.then(|| sec.clone())
        });

    let section_cap = if force_all_sections {
        usize::MAX
    } else if skip_recent_section_key.is_some() {
        BACKGROUND_MAX_SECTIONS
    } else {
        BOOTSTRAP_MAX_SECTIONS
    };
    let sections_to_sync: Vec<OnenoteSection> = iter.take(section_cap).collect();

    let attempted_sections = sections_to_sync.len();
    let page_limit = section_pages_limit(graph);
    let results: Vec<SectionPagesFetch> =
        stream::iter(sections_to_sync.into_iter().map(|sec| async move {
            let res = graph
                .list_onenote_page_listing(&sec.scope, &sec.id, page_limit)
                .await;
            (sec, res)
        }))
        .buffer_unordered(BOOTSTRAP_SECTION_CONCURRENCY)
        .collect()
        .await;

    let mut pages_synced = 0usize;
    let mut sections_succeeded = 0usize;
    let mut auth_err: Option<anyhow::Error> = None;
    let mut first_err: Option<anyhow::Error> = None;
    let mut invalidated_pages: Vec<String> = Vec::new();
    let mut succeeded_sections: Vec<(OnenoteSection, Vec<OnenotePageSummary>, bool)> = Vec::new();
    let mut synced_section_keys: HashSet<String> = HashSet::new();
    let rehydrate_cap = if force_all_sections {
        MAX_REHYDRATE_PER_SYNC
    } else {
        BACKGROUND_MAX_REHYDRATE
    };

    for (sec, pages_res) in results {
        match pages_res {
            Ok(mut listing) => {
                // Without page levels, keep the cached subpage hierarchy.
                if !listing.levels_known {
                    let _ = storage
                        .preserve_cached_page_hierarchy(&sec.section_key, &mut listing.pages);
                }
                synced_section_keys.insert(sec.section_key.clone());
                succeeded_sections.push((sec, listing.pages, listing.has_more));
            }
            Err(err) => {
                if classify_sync_error(&err) == SyncErrorKind::Unauthorized401 && auth_err.is_none()
                {
                    auth_err = Some(err);
                } else if first_err.is_none() {
                    first_err = Some(err);
                }
            }
        }
    }

    // Pass 1: Upsert all fetched pages across succeeded sections first so pages moved
    // between sections update their `section_key` in place without losing cached `detail_json`.
    // Guard against in-flight local page deletions/mutations started after `start_epoch`.
    let protected_pages = storage.pages_protected_since(start_epoch);
    for (sec, pages, _has_more) in &succeeded_sections {
        let filtered_pages: Vec<OnenotePageSummary> = pages
            .iter()
            .filter(|p| {
                !storage.page_deleted_since(&p.page_key, start_epoch)
                    && !storage.page_mutated_since(&p.page_key, start_epoch)
            })
            .cloned()
            .collect();
        let invalidated = storage.reconcile_onenote_section_pages_returning_invalidated(
            &sec.section_key,
            &filtered_pages,
            false,
        )?;
        for pk in invalidated {
            if invalidated_pages.len() < rehydrate_cap && !invalidated_pages.contains(&pk) {
                invalidated_pages.push(pk);
            }
        }
        pages_synced += filtered_pages.len();
        sections_succeeded += 1;
    }

    // Pass 2: Prune deleted pages only for sections whose full page list was fetched,
    // while preserving any pages created/mutated locally after `start_epoch`.
    for (sec, pages, has_more) in &succeeded_sections {
        if !has_more {
            let filtered_pages: Vec<OnenotePageSummary> = pages
                .iter()
                .filter(|p| !storage.page_deleted_since(&p.page_key, start_epoch))
                .cloned()
                .collect();
            storage.prune_onenote_section_pages_preserving_set(
                &sec.section_key,
                &filtered_pages,
                &protected_pages,
            )?;
        }
        let _ = storage.upsert_sync_cursor(&SyncCursor {
            scope_key: section_pages_cursor_key(&sec.section_key),
            delta_link: sec.last_modified_at.clone(),
            history_next_link: None,
            last_synced_at: chrono::Utc::now().to_rfc3339(),
        });
    }

    if !invalidated_pages.is_empty() {
        let rehydrate_results: Vec<Option<crate::models::OnenotePageDetail>> =
            stream::iter(invalidated_pages.into_iter().map(|pk| async move {
                let hint = storage.get_onenote_page_summary(&pk).ok().flatten();
                let (scope, raw_id) = resolve_effective_scope(&pk, hint.as_ref());
                graph
                    .get_onenote_page_detail(&scope, &raw_id, hint)
                    .await
                    .ok()
            }))
            .buffer_unordered(BOOTSTRAP_SECTION_CONCURRENCY)
            .collect()
            .await;
        for detail in rehydrate_results.into_iter().flatten() {
            if !storage.page_mutated_since(&detail.summary.page_key, start_epoch) {
                let _ = storage.upsert_onenote_page_detail(&detail);
            }
        }
    }

    if let Some(err) = auth_err {
        return Err(err);
    }
    if attempted_sections > 0 && sections_succeeded == 0 {
        if let Some(err) = first_err {
            return Err(err);
        }
    }

    let _ = storage.upsert_sync_cursor(&SyncCursor {
        scope_key: "onenote:hierarchy".to_string(),
        delta_link: None,
        history_next_link: None,
        last_synced_at: chrono::Utc::now().to_rfc3339(),
    });

    Ok(HierarchySyncOutcome {
        summary: OnenoteBootstrapSummary {
            notebooks: notebooks.len(),
            section_groups: section_groups.len(),
            sections: sections.len(),
            pages: pages_synced,
        },
        synced_section_keys,
    })
}

/// Deterministic quota-aware polling and deduplication scheduler shared by `run_sync_loop`
/// and virtual-clock tests.
#[derive(Debug)]
pub(crate) struct SyncScheduleState {
    open_page_ms: u64,
    base_hierarchy_ms: u64,
    accelerated_intervals: bool,
    pub(crate) next_page_poll_at: Instant,
    pub(crate) next_section_poll_at: Instant,
    pub(crate) next_hierarchy_poll_at: Instant,
    last_section_cost: u64,
    last_hierarchy_cost: u64,
    automatic_budget: AutomaticRequestBudget,
    last_page_hydrate_at: HashMap<String, Instant>,
    last_section_fetch_at: HashMap<String, Instant>,
}

impl SyncScheduleState {
    pub(crate) fn new(config: &AppConfig, now: Instant) -> Self {
        let open_page_ms = if config.active_poll_ms == 10_000
            && config.hierarchy_poll_ms == DEFAULT_ACTIVE_SECTION_POLL_MS
        {
            // The 0.1.1 default was serialized when a TUI preference was saved.
            DEFAULT_OPEN_PAGE_POLL_MS
        } else {
            config.active_poll_ms.max(2_000)
        };
        let base_hierarchy_ms = config.hierarchy_poll_ms.max(15_000);
        let accelerated_intervals = open_page_ms < DEFAULT_OPEN_PAGE_POLL_MS
            || config.hierarchy_poll_ms < DEFAULT_ACTIVE_SECTION_POLL_MS;
        let mut s = Self {
            open_page_ms,
            base_hierarchy_ms,
            accelerated_intervals,
            next_page_poll_at: now,
            next_section_poll_at: now,
            next_hierarchy_poll_at: now,
            last_section_cost: 1,
            last_hierarchy_cost: 3,
            automatic_budget: AutomaticRequestBudget::default(),
            last_page_hydrate_at: HashMap::new(),
            last_section_fetch_at: HashMap::new(),
        };
        s.next_page_poll_at = now + s.open_page_interval();
        s.next_section_poll_at = now + s.active_section_interval(Duration::ZERO);
        s.next_hierarchy_poll_at = now + s.hierarchy_interval(Duration::ZERO);
        s
    }

    pub(crate) fn open_page_interval(&self) -> Duration {
        Duration::from_millis(self.open_page_ms)
    }

    pub(crate) fn active_section_interval(&self, idle: Duration) -> Duration {
        if self.accelerated_intervals {
            return Duration::from_millis(self.open_page_ms);
        }
        let base_secs = (DEFAULT_ACTIVE_SECTION_POLL_MS / 1000)
            .saturating_mul(self.last_section_cost.max(1))
            .min(600);
        let secs = if idle.as_secs() >= DEEP_IDLE_THRESHOLD_SECS {
            base_secs.saturating_mul(2).min(600)
        } else {
            base_secs
        };
        Duration::from_secs(secs)
    }

    pub(crate) fn hierarchy_interval(&self, idle: Duration) -> Duration {
        if self.accelerated_intervals {
            let base = Duration::from_millis(self.base_hierarchy_ms);
            return if idle.as_secs() >= IDLE_THRESHOLD_SECS {
                base.saturating_mul(2)
            } else {
                base
            };
        }
        let cost = self.last_hierarchy_cost.max(3);
        let adaptive_secs = cost
            .saturating_mul(3_600)
            .div_ceil(HIERARCHY_HOURLY_BUDGET);
        let base_secs = adaptive_secs
            .max(self.base_hierarchy_ms / 1_000)
            .max(MIN_BUDGETED_HIERARCHY_SECS);
        let mult = if idle.as_secs() >= DEEP_IDLE_THRESHOLD_SECS {
            3
        } else if idle.as_secs() >= IDLE_THRESHOLD_SECS {
            2
        } else {
            1
        };
        Duration::from_secs(base_secs.saturating_mul(mult).min(MAX_BUDGETED_HIERARCHY_SECS))
    }

    fn can_run_page_poll(&self, now: Instant) -> bool {
        self.automatic_budget.used(now) < AUTO_HOURLY_HARD_CAP
    }

    fn can_run_section_poll(&self, now: Instant) -> bool {
        self.automatic_budget.used(now) + self.last_section_cost.max(1) as usize <= AUTO_HOURLY_BUDGET
    }

    fn can_run_hierarchy_poll(&self, now: Instant) -> bool {
        self.automatic_budget.used(now) + self.last_hierarchy_cost.max(3) as usize <= AUTO_HOURLY_BUDGET
    }

    pub(crate) fn next_wakeup(&self, focus: &FocusTarget) -> Instant {
        let mut earliest = self.next_hierarchy_poll_at;
        if focus.page_key.is_some() && self.next_page_poll_at < earliest {
            earliest = self.next_page_poll_at;
        }
        if focus.section_key.is_some() && self.next_section_poll_at < earliest {
            earliest = self.next_section_poll_at;
        }
        earliest
    }

    pub(crate) fn should_coalesce_page_hydrate(&self, page_key: &str, now: Instant) -> bool {
        self.last_page_hydrate_at
            .get(page_key)
            .is_some_and(|t| now.saturating_duration_since(*t) < Duration::from_millis(OVERLAP_COALESCE_MS))
    }

    pub(crate) fn should_coalesce_section_fetch(&self, section_key: &str, now: Instant) -> bool {
        self.last_section_fetch_at
            .get(section_key)
            .is_some_and(|t| now.saturating_duration_since(*t) < Duration::from_millis(SECTION_COALESCE_MS))
    }

    pub(crate) fn mark_page_hydrated(&mut self, page_key: &str, now: Instant, is_focused: bool) {
        self.last_page_hydrate_at.insert(page_key.to_string(), now);
        if is_focused {
            self.next_page_poll_at = now + self.open_page_interval();
        }
    }

    pub(crate) fn mark_section_fetched(
        &mut self,
        section_key: &str,
        now: Instant,
        is_focused: bool,
        idle: Duration,
        http_cost: u64,
    ) {
        self.last_section_fetch_at
            .insert(section_key.to_string(), now);
        if is_focused {
            if http_cost > 0 {
                self.last_section_cost = http_cost;
            }
            self.next_section_poll_at = now + self.active_section_interval(idle);
        }
    }

    pub(crate) fn mark_hierarchy_synced(
        &mut self,
        synced_sections: &HashSet<String>,
        now: Instant,
        focus: &FocusTarget,
        idle: Duration,
        http_cost: u64,
    ) {
        for sk in synced_sections {
            self.last_section_fetch_at.insert(sk.clone(), now);
        }
        if http_cost > 0 {
            self.last_hierarchy_cost = http_cost;
        }
        self.next_hierarchy_poll_at = now + self.hierarchy_interval(idle);
        if let Some(sec_key) = focus.section_key.as_deref() {
            if synced_sections.contains(sec_key) {
                self.next_section_poll_at = now + self.active_section_interval(idle);
            }
        }
    }
}

/// Spawns the adaptive two-lane background sync engine for OneNote.
pub fn spawn_sync_engine(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    event_tx: mpsc::UnboundedSender<CoreEvent>,
) -> SyncController {
    let (focus_tx, focus_rx) = watch::channel(FocusTarget::default());
    let (nudge_tx, nudge_rx) = mpsc::unbounded_channel::<SyncCommand>();
    let shared = Arc::new(SyncSharedState::new());

    tokio::spawn(run_sync_loop(
        config, graph, storage, event_tx, focus_rx, nudge_rx, shared,
    ));

    SyncController { focus_tx, nudge_tx }
}

fn drain_matching_pending_commands(
    nudge_rx: &mut mpsc::UnboundedReceiver<SyncCommand>,
    pending: &mut VecDeque<SyncCommand>,
    drop_page_key: Option<&str>,
    drop_section_key: Option<&str>,
) {
    while let Ok(cmd) = nudge_rx.try_recv() {
        let should_drop = match &cmd {
            SyncCommand::HydratePage { page_key } => {
                drop_page_key.is_some_and(|k| k == page_key.as_str())
            }
            SyncCommand::RefreshSectionPages { section_key } => {
                drop_section_key.is_some_and(|k| k == section_key.as_str())
            }
            SyncCommand::NotifyUserActivity => true,
            _ => false,
        };
        if !should_drop {
            pending.push_back(cmd);
        }
    }
}

#[allow(clippy::too_many_arguments)]
pub(crate) async fn run_scheduled_due_work_at(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    focus: &FocusTarget,
    backoff: &mut ErrorBackoffState,
    shared: &SyncSharedState,
    schedule: &mut SyncScheduleState,
    now: Instant,
) {
    if shared.is_auth_paused(now) || !backoff.can_attempt(now) {
        let retry_at = backoff
            .next_retry_at
            .unwrap_or_else(|| now + Duration::from_secs(1))
            .max(now + Duration::from_secs(1));
        if focus.page_key.is_some() && schedule.next_page_poll_at <= now {
            schedule.next_page_poll_at = retry_at;
        }
        if focus.section_key.is_some() && schedule.next_section_poll_at <= now {
            schedule.next_section_poll_at = retry_at;
        }
        if schedule.next_hierarchy_poll_at <= now {
            schedule.next_hierarchy_poll_at = retry_at;
        }
        return;
    }

    let (idle, _) = shared.idle_snapshot(now);

    // Priority 1: Open page content poll (GET /content?includeIDs=true every 15s, even across 2m/10m idle).
    if let Some(page_key) = focus.page_key.as_deref() {
        if now >= schedule.next_page_poll_at {
            schedule.next_page_poll_at = now + schedule.open_page_interval();
            if schedule.can_run_page_poll(now) {
                let page_graph = graph.with_automatic_budget(&schedule.automatic_budget, now, AUTO_HOURLY_HARD_CAP);
                let _ = run_open_page_content_poll(
                    &page_graph,
                    storage,
                    event_tx,
                    page_key,
                    backoff,
                    shared,
                    now,
                )
                .await;
            }
        }
    }

    if shared.is_auth_paused(now) || !backoff.can_attempt(now) {
        return;
    }

    // Priority 2: Active section page-list poll (60s default; probes missing open page once).
    if let Some(sec_key) = focus.section_key.as_deref() {
        if now >= schedule.next_section_poll_at {
            schedule.next_section_poll_at = now + schedule.active_section_interval(idle);
            if schedule.can_run_section_poll(now) {
                let before = graph.http_request_count();
                let section_graph = graph.with_automatic_budget(&schedule.automatic_budget, now, AUTO_HOURLY_BUDGET);
                let _ = run_section_pages_refresh(
                    &section_graph,
                    storage,
                    event_tx,
                    sec_key,
                    focus.page_key.as_deref(),
                    true,
                    backoff,
                    shared,
                    now,
                )
                .await;
                let cost = graph.http_request_count().saturating_sub(before);
                schedule.mark_section_fetched(sec_key, now, true, idle, cost);
            } else {
                schedule.next_section_poll_at = now + Duration::from_secs(60);
            }
        }
    }

    if shared.is_auth_paused(now) || !backoff.can_attempt(now) {
        return;
    }

    // Priority 3: Background hierarchy poll (slowed automatically for heavy/paginated notebooks).
    if now >= schedule.next_hierarchy_poll_at {
        schedule.next_hierarchy_poll_at = now + schedule.hierarchy_interval(idle);
        if schedule.can_run_hierarchy_poll(now) {
            let before = graph.http_request_count();
            let hierarchy_graph = graph.with_automatic_budget(&schedule.automatic_budget, now, AUTO_HOURLY_BUDGET);
            let (_ok, synced_secs) = run_hierarchy_refresh(
                &hierarchy_graph,
                storage,
                event_tx,
                true,
                focus.section_key.as_deref(),
                backoff,
                shared,
                now,
            )
            .await;
            let cost = graph.http_request_count().saturating_sub(before);
            schedule.mark_hierarchy_synced(&synced_secs, now, focus, idle, cost);
        } else {
            schedule.next_hierarchy_poll_at = now + Duration::from_secs(60);
        }
    }
}

async fn run_sync_loop(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    event_tx: mpsc::UnboundedSender<CoreEvent>,
    mut focus_rx: watch::Receiver<FocusTarget>,
    mut nudge_rx: mpsc::UnboundedReceiver<SyncCommand>,
    shared: Arc<SyncSharedState>,
) {
    let start_now = Instant::now();
    let mut schedule = SyncScheduleState::new(&config, start_now);
    let mut backoff = ErrorBackoffState::new(schedule.open_page_ms);
    let mut last_focus = focus_rx.borrow().clone();
    let mut pending_cmds: VecDeque<SyncCommand> = VecDeque::new();

    loop {
        if let Some(cmd) = pending_cmds.pop_front() {
            if !handle_sync_command(
                cmd,
                &graph,
                &storage,
                &event_tx,
                &mut focus_rx,
                &mut nudge_rx,
                &mut pending_cmds,
                &mut last_focus,
                &mut backoff,
                &shared,
                &mut schedule,
            )
            .await
            {
                break;
            }
            continue;
        }

        let focus_snapshot = focus_rx.borrow().clone();
        let next_wakeup = schedule.next_wakeup(&focus_snapshot);
        let sleep_dur = next_wakeup.saturating_duration_since(Instant::now());

        tokio::select! {
            maybe_cmd = nudge_rx.recv() => {
                match maybe_cmd {
                    None | Some(SyncCommand::Shutdown) => break,
                    Some(cmd) => {
                        if !handle_sync_command(
                            cmd,
                            &graph,
                            &storage,
                            &event_tx,
                            &mut focus_rx,
                            &mut nudge_rx,
                            &mut pending_cmds,
                            &mut last_focus,
                            &mut backoff,
                            &shared,
                            &mut schedule,
                        )
                        .await
                        {
                            break;
                        }
                    }
                }
            }
            changed = focus_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                apply_focus_change(
                    &graph, &storage, &event_tx, &mut focus_rx, &mut nudge_rx,
                    &mut pending_cmds, &mut last_focus, &mut backoff, &shared, &mut schedule,
                ).await;
            }
            _ = time::sleep(sleep_dur) => {
                let now = Instant::now();
                let focus = focus_rx.borrow().clone();
                tokio::select! {
                    changed = focus_rx.changed() => {
                        if changed.is_err() {
                            break;
                        }
                        apply_focus_change(
                            &graph, &storage, &event_tx, &mut focus_rx, &mut nudge_rx,
                            &mut pending_cmds, &mut last_focus, &mut backoff, &shared, &mut schedule,
                        ).await;
                    }
                    urgent = next_page_command(&mut nudge_rx, &mut pending_cmds, &shared) => {
                        match urgent {
                            Some(SyncCommand::Shutdown) | None => break,
                            Some(cmd) => pending_cmds.push_front(cmd),
                        }
                    }
                    () = run_scheduled_due_work_at(
                        &graph, &storage, &event_tx, &focus, &mut backoff, &shared,
                        &mut schedule, now,
                    ) => {}
                }
            }
        }
    }
}

async fn next_page_command(
    nudge_rx: &mut mpsc::UnboundedReceiver<SyncCommand>,
    pending: &mut VecDeque<SyncCommand>,
    shared: &SyncSharedState,
) -> Option<SyncCommand> {
    while let Some(cmd) = nudge_rx.recv().await {
        match cmd {
            SyncCommand::HydratePage { .. } | SyncCommand::Shutdown => return Some(cmd),
            SyncCommand::NotifyUserActivity => shared.record_activity(Instant::now()),
            other => pending.push_back(other),
        }
    }
    None
}

#[allow(clippy::too_many_arguments)]
async fn apply_focus_change(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    focus_rx: &mut watch::Receiver<FocusTarget>,
    nudge_rx: &mut mpsc::UnboundedReceiver<SyncCommand>,
    pending: &mut VecDeque<SyncCommand>,
    last_focus: &mut FocusTarget,
    backoff: &mut ErrorBackoffState,
    shared: &SyncSharedState,
    schedule: &mut SyncScheduleState,
) {
    let now = Instant::now();
    shared.record_activity(now);
    let focus = focus_rx.borrow_and_update().clone();
    let section_changed = focus.section_key != last_focus.section_key;
    let page_changed = focus.page_key != last_focus.page_key;
    *last_focus = focus.clone();

    if let Some(page_key) = focus.page_key.as_deref().filter(|_| page_changed) {
        if !schedule.should_coalesce_page_hydrate(page_key, now) {
            if run_hydrate_page(graph, storage, event_tx, page_key).await {
                schedule.mark_page_hydrated(page_key, Instant::now(), true);
            }
        } else if let Ok(Some(detail)) = storage.get_onenote_page_detail(page_key) {
            let _ = event_tx.send(CoreEvent::OnenotePageDetailUpdated(Box::new(detail)));
        }
        drain_matching_pending_commands(nudge_rx, pending, Some(page_key), None);
    }
    if let Some(sec_key) = focus.section_key.as_deref().filter(|_| section_changed) {
        if !schedule.should_coalesce_section_fetch(sec_key, now) {
            let before = graph.http_request_count();
            let ok = run_section_pages_refresh(
                graph, storage, event_tx, sec_key, focus.page_key.as_deref(),
                false, backoff, shared, now,
            ).await;
            let cost = graph.http_request_count().saturating_sub(before);
            schedule.mark_section_fetched(sec_key, Instant::now(), true, Duration::ZERO, cost);
            if ok && !shared.is_auth_paused(Instant::now()) {
                spawn_prefetch_section_pages(
                    graph,
                    storage,
                    &schedule.automatic_budget,
                    AUTO_HOURLY_BUDGET,
                    sec_key,
                    focus.page_key.as_deref(),
                );
            }
        } else {
            let persisted = storage.list_onenote_pages(sec_key).unwrap_or_default();
            let _ = event_tx.send(CoreEvent::OnenotePagesUpdated {
                section_key: sec_key.to_string(),
                pages: persisted,
                notify_candidate: None,
            });
        }
        drain_matching_pending_commands(nudge_rx, pending, None, Some(sec_key));
    }
}

#[allow(clippy::too_many_arguments)]
async fn handle_sync_command(
    cmd: SyncCommand,
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    focus_rx: &mut watch::Receiver<FocusTarget>,
    nudge_rx: &mut mpsc::UnboundedReceiver<SyncCommand>,
    pending_cmds: &mut VecDeque<SyncCommand>,
    last_focus: &mut FocusTarget,
    backoff: &mut ErrorBackoffState,
    shared: &SyncSharedState,
    schedule: &mut SyncScheduleState,
) -> bool {
    let now = Instant::now();
    match cmd {
        SyncCommand::Shutdown => return false,
        SyncCommand::NotifyUserActivity => {
            shared.record_activity(now);
        }
        cmd @ (SyncCommand::Bootstrap | SyncCommand::RefreshAll) => {
            shared.record_activity(now);
            shared.clear_auth_pause();
            let focus = focus_rx.borrow_and_update().clone();
            *last_focus = focus.clone();
            let automatic_graph;
            let bulk_graph = if matches!(cmd, SyncCommand::Bootstrap) {
                automatic_graph = graph.with_automatic_budget(&schedule.automatic_budget, now, STARTUP_HTTP_BUDGET);
                &automatic_graph
            } else {
                graph
            };
            run_full_refresh(
                graph,
                bulk_graph,
                storage,
                event_tx,
                &focus,
                backoff,
                shared,
                schedule,
            )
            .await;
            drain_matching_pending_commands(
                nudge_rx,
                pending_cmds,
                focus.page_key.as_deref(),
                focus.section_key.as_deref(),
            );
        }
        SyncCommand::RefreshHierarchy => {
            shared.record_activity(now);
            let focus = focus_rx.borrow().clone();
            let before = graph.http_request_count();
            let (_ok, synced_secs) = run_hierarchy_refresh(
                graph,
                storage,
                event_tx,
                true,
                None,
                backoff,
                shared,
                now,
            )
            .await;
            let cost = graph.http_request_count().saturating_sub(before);
            schedule.mark_hierarchy_synced(&synced_secs, Instant::now(), &focus, Duration::ZERO, cost);
        }
        SyncCommand::RefreshSectionPages { section_key } => {
            shared.record_activity(now);
            let focus = focus_rx.borrow().clone();
            if !schedule.should_coalesce_section_fetch(&section_key, now) {
                let before = graph.http_request_count();
                let ok = run_section_pages_refresh(
                    graph,
                    storage,
                    event_tx,
                    &section_key,
                    focus.page_key.as_deref(),
                    false,
                    backoff,
                    shared,
                    now,
                )
                .await;
                let cost = graph.http_request_count().saturating_sub(before);
                let is_focused = focus.section_key.as_deref() == Some(section_key.as_str());
                schedule.mark_section_fetched(
                    &section_key,
                    Instant::now(),
                    is_focused,
                    Duration::ZERO,
                    cost,
                );
                if ok && is_focused && !shared.is_auth_paused(Instant::now()) {
                    spawn_prefetch_section_pages(
                        graph,
                        storage,
                        &schedule.automatic_budget,
                        AUTO_HOURLY_BUDGET,
                        &section_key,
                        focus.page_key.as_deref(),
                    );
                }
            } else {
                let persisted = storage.list_onenote_pages(&section_key).unwrap_or_default();
                let _ = event_tx.send(CoreEvent::OnenotePagesUpdated {
                    section_key: section_key.clone(),
                    pages: persisted,
                    notify_candidate: None,
                });
            }
            drain_matching_pending_commands(nudge_rx, pending_cmds, None, Some(&section_key));
        }
        SyncCommand::HydratePage { page_key } => {
            shared.record_activity(now);
            let focus = focus_rx.borrow().clone();
            if !schedule.should_coalesce_page_hydrate(&page_key, now) {
                let ok = run_hydrate_page(graph, storage, event_tx, &page_key).await;
                if ok {
                    let is_focused = focus.page_key.as_deref() == Some(page_key.as_str());
                    schedule.mark_page_hydrated(&page_key, Instant::now(), is_focused);
                }
            } else if let Ok(Some(detail)) = storage.get_onenote_page_detail(&page_key) {
                let _ = event_tx.send(CoreEvent::OnenotePageDetailUpdated(Box::new(detail)));
            }
            drain_matching_pending_commands(nudge_rx, pending_cmds, Some(&page_key), None);
        }
    }
    true
}

#[allow(clippy::too_many_arguments)]
async fn run_full_refresh(
    focused_graph: &GraphClient,
    bulk_graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    focus: &FocusTarget,
    backoff: &mut ErrorBackoffState,
    shared: &SyncSharedState,
    schedule: &mut SyncScheduleState,
) {
    let now = Instant::now();
    let _ = event_tx.send(CoreEvent::SyncStateChanged {
        busy: true,
        summary: "Syncing OneNote...".to_string(),
    });

    // Prioritize profile, focused section & open page concurrently BEFORE bulk hierarchy
    // so large accounts cannot exhaust `STARTUP_HTTP_BUDGET` before the user's active view loads.
    let auth_paused = shared.is_auth_paused(Instant::now());
    let me_fut = async {
        if let Ok(me) = focused_graph.get_me().await {
            let _ = event_tx.send(CoreEvent::ProfileLoaded(me));
        }
    };
    let focused_sec_fut = async {
        if let Some(sec_key) = focus.section_key.as_deref() {
            if !auth_paused {
                let before_s = focused_graph.http_request_count();
                let ok = run_section_pages_refresh(
                    focused_graph,
                    storage,
                    event_tx,
                    sec_key,
                    focus.page_key.as_deref(),
                    false,
                    backoff,
                    shared,
                    Instant::now(),
                )
                .await;
                let cost_s = focused_graph.http_request_count().saturating_sub(before_s);
                (ok, Some((sec_key.to_string(), cost_s)))
            } else {
                (false, None)
            }
        } else {
            (true, None)
        }
    };
    let focused_page_fut = async {
        if let Some(page_key) = focus.page_key.as_deref() {
            if auth_paused {
                (false, None)
            } else {
                let ok = run_hydrate_page(focused_graph, storage, event_tx, page_key).await;
                (ok, ok.then(|| page_key.to_string()))
            }
        } else {
            (true, None)
        }
    };

    let ((), (focused_pages_ok, synced_sec_info), (hydrate_ok, hydrated_page_key)) =
        tokio::join!(me_fut, focused_sec_fut, focused_page_fut);

    let synced_focused_sec = if let Some((sec_key, cost_s)) = synced_sec_info {
        schedule.mark_section_fetched(&sec_key, Instant::now(), true, Duration::ZERO, cost_s);
        Some(sec_key)
    } else {
        None
    };
    if let Some(page_key) = hydrated_page_key {
        schedule.mark_page_hydrated(&page_key, Instant::now(), true);
    }

    let (hierarchy_ok, synced_secs) = if shared.is_auth_paused(Instant::now()) {
        (false, HashSet::new())
    } else {
        let before_h = bulk_graph.http_request_count();
        let res = run_hierarchy_refresh(
            bulk_graph,
            storage,
            event_tx,
            false,
            synced_focused_sec.as_deref(),
            backoff,
            shared,
            now,
        )
        .await;
        let cost_h = bulk_graph.http_request_count().saturating_sub(before_h);
        schedule.mark_hierarchy_synced(&res.1, Instant::now(), focus, Duration::ZERO, cost_h);
        res
    };

    // If no section was focused at start (e.g. cold start with empty cache), sync the first discovered section.
    let pages_ok = if synced_focused_sec.is_some() {
        focused_pages_ok
    } else {
        let target_sec = storage
            .list_onenote_sections(None)
            .ok()
            .and_then(|secs| secs.into_iter().next().map(|s| s.section_key));
        if let Some(sec_key) = target_sec {
            if !hierarchy_ok && shared.is_auth_paused(Instant::now()) {
                false
            } else if synced_secs.contains(&sec_key) {
                let persisted = storage.list_onenote_pages(&sec_key).unwrap_or_default();
                let _ = event_tx.send(CoreEvent::OnenotePagesUpdated {
                    section_key: sec_key,
                    pages: persisted,
                    notify_candidate: None,
                });
                true
            } else {
                let before_s = focused_graph.http_request_count();
                let ok = run_section_pages_refresh(
                    focused_graph,
                    storage,
                    event_tx,
                    &sec_key,
                    focus.page_key.as_deref(),
                    false,
                    backoff,
                    shared,
                    Instant::now(),
                )
                .await;
                let cost_s = focused_graph.http_request_count().saturating_sub(before_s);
                schedule.mark_section_fetched(
                    &sec_key,
                    Instant::now(),
                    false,
                    Duration::ZERO,
                    cost_s,
                );
                ok
            }
        } else {
            true
        }
    };

    if pages_ok && !shared.is_auth_paused(Instant::now()) {
        if let Some(sec_key) = synced_focused_sec.as_deref().or(focus.section_key.as_deref()) {
            let budget_limit = if bulk_graph.has_automatic_budget() {
                STARTUP_HTTP_BUDGET
            } else {
                AUTO_HOURLY_BUDGET
            };
            spawn_prefetch_section_pages(
                focused_graph,
                storage,
                &schedule.automatic_budget,
                budget_limit,
                sec_key,
                focus.page_key.as_deref(),
            );
        }
    }

    if hierarchy_ok && pages_ok && hydrate_ok {
        let _ = event_tx.send(CoreEvent::SyncStateChanged {
            busy: false,
            summary: "Synced".to_string(),
        });
    } else if backoff.is_degraded() {
        let _ = event_tx.send(CoreEvent::SyncStateChanged {
            busy: false,
            summary: POLL_RETRY_SUMMARY.to_string(),
        });
    } else if bulk_graph.has_automatic_budget()
        && pages_ok
        && hydrate_ok
        && !shared.is_auth_paused(Instant::now())
    {
        // Bulk hierarchy hit startup cap, while focused section & page succeeded cleanly.
        let _ = event_tx.send(CoreEvent::SyncStateChanged {
            busy: false,
            summary: "Synced".to_string(),
        });
    } else {
        let _ = event_tx.send(CoreEvent::SyncStateChanged {
            busy: false,
            summary: "Sync failed".to_string(),
        });
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_hierarchy_refresh(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    incremental: bool,
    skip_recent_section_key: Option<&str>,
    backoff: &mut ErrorBackoffState,
    shared: &SyncSharedState,
    now: Instant,
) -> (bool, HashSet<String>) {
    match sync_hierarchy_and_sections_inner(graph, storage, !incremental, skip_recent_section_key)
        .await
    {
        Ok(outcome) => {
            if backoff.record_success() {
                let _ = event_tx.send(CoreEvent::SyncStateChanged {
                    busy: false,
                    summary: "Synced".to_string(),
                });
            }
            let notebooks = storage.list_onenote_notebooks().unwrap_or_default();
            let section_groups = storage.list_onenote_section_groups().unwrap_or_default();
            let sections = storage.list_onenote_sections(None).unwrap_or_default();
            let _ = event_tx.send(CoreEvent::OnenoteHierarchyUpdated {
                notebooks,
                section_groups,
                sections,
            });
            (true, outcome.synced_section_keys)
        }
        Err(err) => {
            handle_sync_error(err, graph, event_tx, backoff, shared, now).await;
            (false, HashSet::new())
        }
    }
}

/// Steady-state open-page poll: fetches ONLY `GET /{scope}/pages/{id}/content?includeIDs=true`.
/// If XHTML is unchanged vs cached storage, exits immediately (1 HTTP call).
/// Reuses a known summary for edits. The section lane checks the page's location.
async fn run_open_page_content_poll(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    page_key: &str,
    backoff: &mut ErrorBackoffState,
    shared: &SyncSharedState,
    now: Instant,
) -> bool {
    let start_epoch = storage.current_mutation_epoch();
    let cached_detail = storage.get_onenote_page_detail(page_key).ok().flatten();
    let cached_summary = cached_detail
        .as_ref()
        .map(|d| d.summary.clone())
        .or_else(|| storage.get_onenote_page_summary(page_key).ok().flatten());
    let prev_section_key = cached_summary
        .as_ref()
        .filter(|s| !s.section_id.trim().is_empty())
        .map(|s| s.scope.make_entity_key(&s.section_id));
    let (scope, raw_id) = resolve_effective_scope(page_key, cached_summary.as_ref());

    match graph.get_onenote_page_content_xhtml(&scope, &raw_id).await {
        Ok(raw_xhtml) => {
            if backoff.record_success() {
                let _ = event_tx.send(CoreEvent::SyncStateChanged {
                    busy: false,
                    summary: "Synced".to_string(),
                });
            }
            if storage.page_mutated_since(page_key, start_epoch) {
                return true;
            }
            if let Some(ref existing) = cached_detail {
                if existing.raw_xhtml == raw_xhtml {
                    return true;
                }
            }

            let summary = match cached_summary {
                Some(summary) if !summary.section_id.trim().is_empty() => summary,
                hint => match graph.get_onenote_page_summary(&scope, &raw_id, hint.clone()).await {
                    Ok(fresh) => fresh,
                    Err(err) if crate::graph::onenote::is_not_found_error(&err) => {
                        if !storage.page_mutated_since(page_key, start_epoch) {
                            let _ = storage.delete_onenote_page(page_key);
                        }
                        return false;
                    }
                    Err(_) => hint.unwrap_or_else(|| OnenotePageSummary {
                        page_key: scope.make_entity_key(&raw_id),
                        id: raw_id.clone(),
                        scope: scope.clone(),
                        section_id: String::new(),
                        section_name: None,
                        notebook_id: None,
                        notebook_name: None,
                        title: "(Untitled)".to_string(),
                        level: 0,
                        order: 0,
                        created_at: None,
                        last_modified_at: None,
                        content_url: None,
                        web_url: None,
                        preview_text: None,
                        has_todo: false,
                    }),
                },
            };

            if storage.page_mutated_since(page_key, start_epoch) {
                return true;
            }
            let detail = crate::parser::onenote::parse_onenote_page_xhtml(summary, &raw_xhtml);
            if storage.upsert_onenote_page_detail(&detail).is_err() {
                return false;
            }
            let _ = event_tx.send(CoreEvent::OnenotePageDetailUpdated(Box::new(detail)));
            true
        }
        Err(err) => {
            if crate::graph::onenote::is_not_found_error(&err) {
                if !storage.page_mutated_since(page_key, start_epoch) {
                    let _ = storage.delete_onenote_page(page_key);
                    if let Some(sec_key) = prev_section_key {
                        let persisted = storage.list_onenote_pages(&sec_key).unwrap_or_default();
                        let _ = event_tx.send(CoreEvent::OnenotePagesUpdated {
                            section_key: sec_key,
                            pages: persisted,
                            notify_candidate: None,
                        });
                    }
                }
            } else {
                // Transient failure: preserve last read result and back off normally.
                handle_sync_error(err, graph, event_tx, backoff, shared, now).await;
            }
            false
        }
    }
}

/// Explicit refreshes read up to `SECTION_PAGES_LIMIT` pages; polls charged to
/// the automatic request budget keep the smaller cap so one large section
/// cannot spend the hourly budget.
fn section_pages_limit(graph: &GraphClient) -> usize {
    if graph.has_automatic_budget() {
        BUDGETED_SECTION_PAGES_LIMIT
    } else {
        SECTION_PAGES_LIMIT
    }
}

#[allow(clippy::too_many_arguments)]
async fn run_section_pages_refresh(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    section_key: &str,
    focused_page_key: Option<&str>,
    background: bool,
    backoff: &mut ErrorBackoffState,
    shared: &SyncSharedState,
    now: Instant,
) -> bool {
    let start_epoch = storage.current_mutation_epoch();
    let (scope, section_id) = split_onenote_entity_key(section_key);
    let prev_ids: HashSet<String> = storage
        .list_onenote_page_keys(section_key)
        .unwrap_or_default()
        .into_iter()
        .collect();

    match graph
        .list_onenote_page_listing(&scope, &section_id, section_pages_limit(graph))
        .await
    {
        Ok(listing) => {
            let (mut pages, has_more) = (listing.pages, listing.has_more);
            // Without page levels, keep the cached subpage hierarchy.
            if !listing.levels_known {
                let _ = storage.preserve_cached_page_hierarchy(section_key, &mut pages);
            }
            // Prune only when Graph confirmed there is nothing past the limit.
            let is_complete = !has_more;
            let mut preserve_page_key: Option<String> = None;
            let mut moved_target: Option<(String, String)> = None;

            // If the currently open page vanished from this section's page list, probe it ONCE
            // with its original scope & page ID to distinguish move vs deletion vs transient error.
            if is_complete {
                if let Some(open_pk) = focused_page_key {
                    let was_in_section = prev_ids.contains(open_pk);
                    let still_in_list = pages.iter().any(|p| p.page_key == open_pk);
                    let locally_touched = storage.page_created_since(open_pk, start_epoch)
                        || storage.page_deleted_since(open_pk, start_epoch);
                    if was_in_section && !still_in_list && !locally_touched {
                        let cached_summary = storage.get_onenote_page_summary(open_pk).ok().flatten();
                        let (page_scope, raw_page_id) =
                            resolve_effective_scope(open_pk, cached_summary.as_ref());
                        match graph
                            .get_onenote_page_summary(&page_scope, &raw_page_id, cached_summary)
                            .await
                        {
                            Ok(mut fresh_summary) => {
                                let new_sec_key = fresh_summary
                                    .scope
                                    .make_entity_key(&fresh_summary.section_id);
                                if !fresh_summary.section_id.trim().is_empty()
                                    && new_sec_key != section_key
                                {
                                    // Relocate the last good XHTML and index atomically with the
                                    // summary. A transient replacement failure cannot blank them.
                                    let stored = match storage
                                        .get_onenote_page_detail(open_pk)
                                        .ok()
                                        .flatten()
                                    {
                                        Some(mut detail) => {
                                            fresh_summary.preview_text = detail.summary.preview_text.clone();
                                            fresh_summary.has_todo = detail.summary.has_todo;
                                            detail.summary = fresh_summary;
                                            storage.upsert_onenote_page_detail(&detail)
                                        }
                                        None => storage.upsert_onenote_pages(&[fresh_summary]),
                                    };
                                    if stored.is_ok() {
                                        moved_target = Some((open_pk.to_string(), new_sec_key));
                                    } else {
                                        preserve_page_key = Some(open_pk.to_string());
                                    }
                                } else {
                                    // Still belongs to this section (eventual consistency lag).
                                    preserve_page_key = Some(open_pk.to_string());
                                }
                            }
                            Err(err) if crate::graph::onenote::is_not_found_error(&err) => {
                                // Confirmed deleted (404/410): use existing cache + search cleanup.
                                if !storage.page_mutated_since(open_pk, start_epoch) {
                                    let _ = storage.delete_onenote_page(open_pk);
                                }
                            }
                            Err(err) => {
                                // Transient failure: preserve last read result without pruning.
                                preserve_page_key = Some(open_pk.to_string());
                                handle_sync_error(err, graph, event_tx, backoff, shared, now).await;
                            }
                        }
                    }
                }
            }

            // Guard against in-flight local page creates/edits/deletes after `start_epoch`.
            let mut preserve_keys = storage.pages_protected_since(start_epoch);
            if let Some(ref pk) = preserve_page_key {
                preserve_keys.insert(pk.clone());
            }
            let filtered_pages: Vec<OnenotePageSummary> = pages
                .iter()
                .filter(|p| {
                    !storage.page_deleted_since(&p.page_key, start_epoch)
                        && !storage.page_mutated_since(&p.page_key, start_epoch)
                })
                .cloned()
                .collect();

            let _ = storage.reconcile_onenote_section_pages_preserving_set(
                section_key,
                &filtered_pages,
                is_complete,
                &preserve_keys,
                focused_page_key,
            );
            let prev_delta = storage
                .get_sync_cursor(&section_pages_cursor_key(section_key))
                .ok()
                .flatten()
                .and_then(|c| c.delta_link);
            let _ = storage.upsert_sync_cursor(&SyncCursor {
                scope_key: section_pages_cursor_key(section_key),
                delta_link: prev_delta,
                history_next_link: None,
                last_synced_at: chrono::Utc::now().to_rfc3339(),
            });
            if preserve_page_key.is_none() && backoff.record_success() {
                let _ = event_tx.send(CoreEvent::SyncStateChanged {
                    busy: false,
                    summary: "Synced".to_string(),
                });
            }
            let notify_candidate = if background && !prev_ids.is_empty() {
                find_new_page_notification(&prev_ids, &filtered_pages)
            } else {
                None
            };
            let persisted = storage
                .list_onenote_pages(section_key)
                .unwrap_or(filtered_pages);
            let _ = event_tx.send(CoreEvent::OnenotePagesUpdated {
                section_key: section_key.to_string(),
                pages: persisted,
                notify_candidate,
            });

            if let Some((moved_pk, new_sec_key)) = moved_target {
                if let Ok(Some(detail)) = storage.get_onenote_page_detail(&moved_pk) {
                    let _ = event_tx.send(CoreEvent::OnenotePageDetailUpdated(Box::new(detail)));
                }
                let new_pages = storage.list_onenote_pages(&new_sec_key).unwrap_or_default();
                let _ = event_tx.send(CoreEvent::OnenotePagesUpdated {
                    section_key: new_sec_key,
                    pages: new_pages,
                    notify_candidate: None,
                });
            }
            true
        }
        Err(err) => {
            handle_sync_error(err, graph, event_tx, backoff, shared, now).await;
            false
        }
    }
}

async fn run_hydrate_page(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    page_key: &str,
) -> bool {
    let start_epoch = storage.current_mutation_epoch();
    let summary_hint = storage.get_onenote_page_summary(page_key).ok().flatten();
    let prev_section_key = summary_hint
        .as_ref()
        .filter(|s| !s.section_id.trim().is_empty())
        .map(|s| s.scope.make_entity_key(&s.section_id));
    let (scope, raw_id) = resolve_effective_scope(page_key, summary_hint.as_ref());
    match graph
        .get_onenote_page_detail(&scope, &raw_id, summary_hint)
        .await
    {
        Ok(detail) => {
            if storage.page_mutated_since(page_key, start_epoch) {
                return true;
            }
            let _ = storage.upsert_onenote_page_detail(&detail);
            let _ = event_tx.send(CoreEvent::OnenotePageDetailUpdated(Box::new(detail)));
            true
        }
        Err(err) => {
            if err.is::<AutomaticRequestBudgetExhausted>() {
                return false;
            }
            if crate::graph::onenote::is_not_found_error(&err) {
                if !storage.page_mutated_since(page_key, start_epoch) {
                    let _ = storage.delete_onenote_page(page_key);
                    if let Some(sec_key) = prev_section_key {
                        let persisted = storage.list_onenote_pages(&sec_key).unwrap_or_default();
                        let _ = event_tx.send(CoreEvent::OnenotePagesUpdated {
                            section_key: sec_key,
                            pages: persisted,
                            notify_candidate: None,
                        });
                    }
                }
            }
            let _ = event_tx.send(CoreEvent::SyncError(format!(
                "Failed to load OneNote page: {err}"
            )));
            false
        }
    }
}

/// Quietly warms SQLite with up to `SECTION_PAGE_PREFETCH_LIMIT` uncached pages in the
/// active section so switching pages with `j`/`k` renders immediately from cache.
fn spawn_prefetch_section_pages(
    graph: &GraphClient,
    storage: &Storage,
    budget: &AutomaticRequestBudget,
    budget_limit: usize,
    section_key: &str,
    skip_page_key: Option<&str>,
) {
    let Ok(pages) = storage.list_onenote_pages(section_key) else {
        return;
    };
    let candidates: Vec<OnenotePageSummary> = pages
        .into_iter()
        .filter(|p| Some(p.page_key.as_str()) != skip_page_key)
        .filter(|p| !p.section_id.trim().is_empty())
        .filter(|p| {
            storage
                .get_onenote_page_detail(&p.page_key)
                .ok()
                .flatten()
                .is_none()
        })
        .take(SECTION_PAGE_PREFETCH_LIMIT)
        .collect();
    if candidates.is_empty() {
        return;
    }

    let budgeted_graph = graph.with_automatic_budget(budget, Instant::now(), budget_limit);
    let storage = storage.clone();
    tokio::spawn(async move {
        stream::iter(candidates)
            .for_each_concurrent(SECTION_PAGE_PREFETCH_CONCURRENCY, |summary| {
                let graph = budgeted_graph.clone();
                let storage = storage.clone();
                async move {
                    let pk = summary.page_key.clone();
                    let start_epoch = storage.current_mutation_epoch();
                    let (scope, raw_id) = resolve_effective_scope(&pk, Some(&summary));
                    if let Ok(detail) = graph
                        .get_onenote_page_detail_content_only(&scope, &raw_id, summary)
                        .await
                    {
                        if !storage.page_mutated_since(&pk, start_epoch)
                            && !storage.page_deleted_since(&pk, start_epoch)
                        {
                            let _ = storage.upsert_onenote_page_detail(&detail);
                        }
                    }
                }
            })
            .await;
    });
}

fn find_new_page_notification(
    prev_keys: &HashSet<String>,
    pages: &[OnenotePageSummary],
) -> Option<NotificationCandidate> {
    pages
        .iter()
        .find(|p| !prev_keys.contains(&p.page_key))
        .map(|p| NotificationCandidate {
            item_id: p.page_key.clone(),
            section_name: p
                .section_name
                .clone()
                .unwrap_or_else(|| "OneNote".to_string()),
            title: p.title.clone(),
            preview: p.preview_text.clone().unwrap_or_default(),
        })
}

async fn handle_sync_error(
    err: anyhow::Error,
    graph: &GraphClient,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    backoff: &mut ErrorBackoffState,
    shared: &SyncSharedState,
    now: Instant,
) {
    if err.is::<AutomaticRequestBudgetExhausted>() {
        return;
    }
    match classify_sync_error(&err) {
        SyncErrorKind::Unauthorized401 => {
            backoff.clear_degraded();
            shared.pause_auth(now, Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS));
            let _ = event_tx.send(CoreEvent::SyncError(format!(
                "Authentication required: {err}"
            )));
        }
        SyncErrorKind::Forbidden403 => {
            backoff.clear_degraded();
            let tok = graph
                .auth()
                .current_tokens()
                .await
                .map(|t| t.access_token);
            let msg = crate::auth::explain_graph_permission_error(&err.to_string(), tok.as_deref())
                .unwrap_or_else(|| err.to_string());
            let _ = event_tx.send(CoreEvent::SyncError(msg));
        }
        SyncErrorKind::Transient => {
            let entered_degraded = backoff.record_transient_failure(now);
            if entered_degraded {
                let _ = event_tx.send(CoreEvent::SyncStateChanged {
                    busy: false,
                    summary: POLL_RETRY_SUMMARY.to_string(),
                });
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn saved_legacy_defaults_use_the_default_realtime_intervals() {
        let config = AppConfig {
            active_poll_ms: 10_000,
            hierarchy_poll_ms: 60_000,
            ..AppConfig::default()
        };
        let schedule = SyncScheduleState::new(&config, Instant::now());
        assert_eq!(schedule.open_page_interval(), Duration::from_secs(15));
        assert_eq!(schedule.active_section_interval(Duration::ZERO), Duration::from_secs(60));
    }

    #[test]
    fn test_error_classification_and_backoff_progression() {
        assert_eq!(
            classify_sync_error(&anyhow::anyhow!(
                "Graph API error (GET /me/onenote/notebooks): 403 Forbidden"
            )),
            SyncErrorKind::Forbidden403
        );
        assert_eq!(
            classify_sync_error(&anyhow::anyhow!(
                "Graph API error (GET /me/onenote/notebooks): 401 Unauthorized"
            )),
            SyncErrorKind::Unauthorized401
        );
        assert_eq!(
            classify_sync_error(&anyhow::anyhow!("connection reset by peer")),
            SyncErrorKind::Transient
        );

        let b1 = ErrorBackoffState::compute_backoff_ms(5_000, 1, 1.0);
        let b2 = ErrorBackoffState::compute_backoff_ms(5_000, 2, 1.0);
        assert_eq!(b1, 10_000);
        assert_eq!(b2, 20_000);

        let now = Instant::now();
        let mut state = ErrorBackoffState::new(1_000);
        assert!(state.can_attempt(now));
        assert!(state.record_transient_failure(now));
        assert!(!state.record_transient_failure(now));
        assert!(state.record_success());
        assert!(!state.record_success());
    }

    #[tokio::test]
    async fn test_bootstrap_onenote_syncs_all_sections_and_run_full_refresh_preserves_error_state()
    {
        use std::collections::HashMap;
        use std::sync::{
            atomic::{AtomicBool, AtomicUsize, Ordering as AtomicOrdering},
            Mutex,
        };
        use axum::{
            extract::{Path as AxumPath, State},
            http::StatusCode,
            routing::get,
            Json, Router,
        };
        use serde_json::{json, Value};

        use crate::auth::{AuthManager, StoredTokens};

        #[derive(Clone)]
        struct MockState {
            pages_by_section: Arc<Mutex<HashMap<String, Vec<Value>>>>,
            section_fetch_hits: Arc<AtomicUsize>,
            fail_with_503: Arc<AtomicBool>,
        }

        let mut initial_pages: HashMap<String, Vec<Value>> = HashMap::new();
        initial_pages.insert(
            "sec-1".to_string(),
            vec![
                json!({
                    "id": "p-1a",
                    "title": "Sec1 Page A",
                    "level": 0,
                    "order": 0,
                    "lastModifiedDateTime": "2026-11-18T10:00:00Z",
                    "parentSection": { "id": "sec-1", "displayName": "Section 1" }
                }),
                json!({
                    "id": "p-1b",
                    "title": "Sec1 Page B To Delete",
                    "level": 0,
                    "order": 1,
                    "lastModifiedDateTime": "2026-11-18T10:01:00Z",
                    "parentSection": { "id": "sec-1", "displayName": "Section 1" }
                }),
            ],
        );
        initial_pages.insert(
            "sec-2".to_string(),
            vec![json!({
                "id": "p-2a",
                "title": "Sec2 Page A",
                "level": 0,
                "order": 0,
                "lastModifiedDateTime": "2026-11-18T10:02:00Z",
                "parentSection": { "id": "sec-2", "displayName": "Section 2" }
            })],
        );
        initial_pages.insert(
            "sec-3".to_string(),
            vec![json!({
                "id": "p-3a",
                "title": "Sec3 Page A",
                "level": 0,
                "order": 0,
                "lastModifiedDateTime": "2026-11-18T10:03:00Z",
                "parentSection": { "id": "sec-3", "displayName": "Section 3" }
            })],
        );

        let state = MockState {
            pages_by_section: Arc::new(Mutex::new(initial_pages)),
            section_fetch_hits: Arc::new(AtomicUsize::new(0)),
            fail_with_503: Arc::new(AtomicBool::new(false)),
        };

        let app = Router::new()
            .route(
                "/v1.0/me",
                get(|State(st): State<MockState>| async move {
                    if st.fail_with_503.load(AtomicOrdering::SeqCst) {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("Retry-After", "0")],
                            Json(json!({ "error": { "code": "ServiceUnavailable" } })),
                        );
                    }
                    (
                        StatusCode::OK,
                        [("Retry-After", "0")],
                        Json(json!({ "id": "user-1", "displayName": "Tester" })),
                    )
                }),
            )
            .route(
                "/v1.0/me/joinedTeams",
                get(|| async { Json(json!({ "value": [] })) }),
            )
            .route(
                "/v1.0/me/onenote/notebooks",
                get(|State(st): State<MockState>| async move {
                    if st.fail_with_503.load(AtomicOrdering::SeqCst) {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("Retry-After", "0")],
                            Json(json!({ "error": { "code": "ServiceUnavailable" } })),
                        );
                    }
                    (
                        StatusCode::OK,
                        [("Retry-After", "0")],
                        Json(json!({
                            "value": [
                                { "id": "nb-1", "displayName": "Notebook 1", "isDefault": true },
                                { "id": "nb-2", "displayName": "Notebook 2", "isDefault": false }
                            ]
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups",
                get(|State(st): State<MockState>| async move {
                    if st.fail_with_503.load(AtomicOrdering::SeqCst) {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("Retry-After", "0")],
                            Json(json!({ "error": { "code": "ServiceUnavailable" } })),
                        );
                    }
                    (
                        StatusCode::OK,
                        [("Retry-After", "0")],
                        Json(json!({ "value": [] })),
                    )
                }),
            )
            .route(
                "/v1.0/me/onenote/sections",
                get(|State(st): State<MockState>| async move {
                    if st.fail_with_503.load(AtomicOrdering::SeqCst) {
                        return (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("Retry-After", "0")],
                            Json(json!({ "error": { "code": "ServiceUnavailable" } })),
                        );
                    }
                    (
                        StatusCode::OK,
                        [("Retry-After", "0")],
                        Json(json!({
                            "value": [
                                {
                                    "id": "sec-1",
                                    "displayName": "Section 1",
                                    "isDefault": true,
                                    "lastModifiedDateTime": "2026-11-18T10:01:00Z",
                                    "parentNotebook": { "id": "nb-1", "displayName": "Notebook 1" }
                                },
                                {
                                    "id": "sec-2",
                                    "displayName": "Section 2",
                                    "isDefault": false,
                                    "lastModifiedDateTime": "2026-11-18T10:02:00Z",
                                    "parentNotebook": { "id": "nb-1", "displayName": "Notebook 1" }
                                },
                                {
                                    "id": "sec-3",
                                    "displayName": "Section 3",
                                    "isDefault": false,
                                    "lastModifiedDateTime": "2026-11-18T10:03:00Z",
                                    "parentNotebook": { "id": "nb-2", "displayName": "Notebook 2" }
                                }
                            ]
                        })),
                    )
                }),
            )
            .route(
                "/v1.0/me/onenote/sections/{sec_id}/pages",
                get(
                    |State(st): State<MockState>, AxumPath(sec_id): AxumPath<String>| async move {
                        if st.fail_with_503.load(AtomicOrdering::SeqCst) {
                            return (
                                StatusCode::SERVICE_UNAVAILABLE,
                                [("Retry-After", "0")],
                                Json(json!({ "error": { "code": "ServiceUnavailable" } })),
                            );
                        }
                        st.section_fetch_hits.fetch_add(1, AtomicOrdering::SeqCst);
                        let items = st
                            .pages_by_section
                            .lock()
                            .unwrap()
                            .get(&sec_id)
                            .cloned()
                            .unwrap_or_default();
                        (
                            StatusCode::OK,
                            [("Retry-After", "0")],
                            Json(json!({ "value": items })),
                        )
                    },
                ),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let server_handle = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tokens = StoredTokens {
            access_token: "sync-test-token".to_string(),
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
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        // First bootstrap syncs pages across ALL 3 sections (not just 1).
        let summary = bootstrap_onenote(&graph, &storage).await.unwrap();
        assert_eq!(summary.notebooks, 2);
        assert_eq!(summary.sections, 3);
        assert_eq!(summary.pages, 4);
        assert_eq!(storage.list_onenote_pages("me|sec-1").unwrap().len(), 2);
        assert_eq!(storage.list_onenote_pages("me|sec-2").unwrap().len(), 1);
        assert_eq!(storage.list_onenote_pages("me|sec-3").unwrap().len(), 1);
        assert_eq!(
            storage
                .get_sync_cursor("onenote:pages:me|sec-2")
                .unwrap()
                .and_then(|c| c.delta_link)
                .as_deref(),
            Some("2026-11-18T10:02:00Z")
        );

        // Incremental hierarchy refresh skips sections whose last_modified_at is unchanged.
        let hits_before_inc = state.section_fetch_hits.load(AtomicOrdering::SeqCst);
        let inc_summary = sync_hierarchy_and_sections(&graph, &storage, false)
            .await
            .unwrap();
        assert_eq!(inc_summary.pages, 0);
        assert_eq!(
            state.section_fetch_hits.load(AtomicOrdering::SeqCst),
            hits_before_inc
        );

        // Delete p-1b remotely; second bootstrap prunes it from SQLite.
        state
            .pages_by_section
            .lock()
            .unwrap()
            .get_mut("sec-1")
            .unwrap()
            .retain(|p| p["id"].as_str() != Some("p-1b"));
        let summary2 = bootstrap_onenote(&graph, &storage).await.unwrap();
        assert_eq!(summary2.pages, 3);
        let sec1_pages = storage.list_onenote_pages("me|sec-1").unwrap();
        assert_eq!(sec1_pages.len(), 1);
        assert_eq!(sec1_pages[0].id, "p-1a");

        // Switch MockGraph to 503 Service Unavailable and run SyncCommand::RefreshAll.
        state.fail_with_503.store(true, AtomicOrdering::SeqCst);
        let (event_tx, mut event_rx) = mpsc::unbounded_channel();
        let ctrl = spawn_sync_engine(cfg, graph, storage, event_tx);
        ctrl.request(SyncCommand::RefreshAll);

        let mut saw_busy = false;
        let mut final_summary: Option<String> = None;
        let deadline = Instant::now() + Duration::from_secs(10);
        while Instant::now() < deadline {
            let remaining = deadline.saturating_duration_since(Instant::now());
            match tokio::time::timeout(remaining, event_rx.recv()).await {
                Ok(Some(CoreEvent::SyncStateChanged { busy, summary })) => {
                    if busy {
                        saw_busy = true;
                    } else if saw_busy {
                        assert_ne!(
                            summary, "Synced",
                            "failed RefreshAll must never emit 'Synced'"
                        );
                        final_summary = Some(summary);
                        // Drain any immediate follow-up events briefly to capture the true final state.
                        while let Ok(Some(ev)) =
                            tokio::time::timeout(Duration::from_millis(50), event_rx.recv()).await
                        {
                            if let CoreEvent::SyncStateChanged {
                                busy: false,
                                summary: s,
                            } = ev
                            {
                                assert_ne!(s, "Synced");
                                final_summary = Some(s);
                            }
                        }
                        break;
                    }
                }
                Ok(Some(_)) => {}
                _ => break,
            }
        }

        ctrl.request(SyncCommand::Shutdown);
        server_handle.abort();
        assert_eq!(
            final_summary.as_deref(),
            Some(POLL_RETRY_SUMMARY),
            "503 failure during RefreshAll must end in POLL_RETRY_SUMMARY"
        );
    }

    #[test]
    fn test_set_focus_deduplicates_identical_targets_and_propagates_changes() {
        let (focus_tx, mut focus_rx) = watch::channel(FocusTarget::default());
        let (nudge_tx, _nudge_rx) = mpsc::unbounded_channel();
        let ctrl = SyncController::from_channels(focus_tx, nudge_tx);

        assert!(!focus_rx.has_changed().unwrap());

        // Sending the initial default value again must not mark the channel changed.
        ctrl.set_focus(FocusTarget::default());
        assert!(!focus_rx.has_changed().unwrap());

        let target_a = FocusTarget {
            notebook_key: Some("me|nb-1".to_string()),
            section_key: Some("me|sec-1".to_string()),
            page_key: Some("me|p-1a".to_string()),
        };
        ctrl.set_focus(target_a.clone());
        assert!(focus_rx.has_changed().unwrap());
        assert_eq!(*focus_rx.borrow_and_update(), target_a);

        // Identical target must not re-notify receivers.
        ctrl.set_focus(target_a.clone());
        assert!(!focus_rx.has_changed().unwrap());

        // Changing any field must propagate.
        let target_b = FocusTarget {
            notebook_key: Some("me|nb-1".to_string()),
            section_key: Some("me|sec-2".to_string()),
            page_key: Some("me|p-2a".to_string()),
        };
        ctrl.set_focus(target_b.clone());
        assert!(focus_rx.has_changed().unwrap());
        assert_eq!(*focus_rx.borrow_and_update(), target_b);

        // Returning to a previous target (A -> B -> A) must propagate.
        ctrl.set_focus(target_a.clone());
        assert!(focus_rx.has_changed().unwrap());
        assert_eq!(*focus_rx.borrow_and_update(), target_a);
    }

    #[tokio::test]
    async fn test_incremental_sync_refreshes_stale_inactive_section_even_when_section_mtime_unchanged() {
        use axum::{
            extract::{Path as AxumPath, State},
            routing::get,
            Json, Router,
        };
        use serde_json::json;
        use std::sync::{
            atomic::{AtomicUsize, Ordering as AtomicOrdering},
            Mutex,
        };

        use crate::auth::{AuthManager, StoredTokens};

        #[derive(Clone)]
        struct StaleMockState {
            sec2_hits: Arc<AtomicUsize>,
            sec2_pages: Arc<Mutex<Vec<serde_json::Value>>>,
        }

        let state = StaleMockState {
            sec2_hits: Arc::new(AtomicUsize::new(0)),
            sec2_pages: Arc::new(Mutex::new(vec![json!({
                "id": "p-2a",
                "title": "Inactive v1",
                "level": 0,
                "order": 0,
                "lastModifiedDateTime": "2026-11-18T10:00:00Z",
                "parentSection": { "id": "sec-2", "displayName": "Inactive" }
            })])),
        };

        let app = Router::new()
            .route(
                "/v1.0/me/joinedTeams",
                get(|| async { Json(json!({ "value": [] })) }),
            )
            .route(
                "/v1.0/me/onenote/notebooks",
                get(|| async {
                    Json(json!({
                        "value": [{
                            "id": "nb-1",
                            "displayName": "NB",
                            "isDefault": true,
                            "isShared": false
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups",
                get(|| async { Json(json!({ "value": [] })) }),
            )
            .route(
                "/v1.0/me/onenote/sections",
                get(|| async {
                    Json(json!({
                        "value": [{
                            "id": "sec-2",
                            "displayName": "Inactive",
                            "isDefault": false,
                            "lastModifiedDateTime": "2026-11-18T10:02:00Z",
                            "parentNotebook": { "id": "nb-1", "displayName": "NB" }
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sections/{sec_id}/pages",
                get(
                    |State(st): State<StaleMockState>, AxumPath(_sec_id): AxumPath<String>| async move {
                        st.sec2_hits.fetch_add(1, AtomicOrdering::SeqCst);
                        let items = st.sec2_pages.lock().unwrap().clone();
                        Json(json!({ "value": items }))
                    },
                ),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tokens = StoredTokens {
            access_token: "sync-test-token".to_string(),
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
        let graph = GraphClient::new(cfg, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        bootstrap_onenote(&graph, &storage).await.unwrap();
        assert_eq!(state.sec2_hits.load(AtomicOrdering::SeqCst), 1);

        // Remote adds a page in sec-2 WITHOUT changing sec-2.lastModifiedDateTime
        state.sec2_pages.lock().unwrap().push(json!({
            "id": "p-2b",
            "title": "New Remote Page In Inactive Section",
            "level": 0,
            "order": 1,
            "lastModifiedDateTime": "2026-11-18T11:00:00Z",
            "parentSection": { "id": "sec-2", "displayName": "Inactive" }
        }));

        // Immediately after bootstrap, incremental sync skips (cursor age < INACTIVE_SECTION_STALE_SECS)
        sync_hierarchy_and_sections(&graph, &storage, false)
            .await
            .unwrap();
        assert_eq!(state.sec2_hits.load(AtomicOrdering::SeqCst), 1);

        // Age the cursor past INACTIVE_SECTION_STALE_SECS -> incremental sync refreshes sec-2!
        let old_ts = (chrono::Utc::now() - chrono::Duration::seconds(INACTIVE_SECTION_STALE_SECS + 30))
            .to_rfc3339();
        storage
            .upsert_sync_cursor(&SyncCursor {
                scope_key: section_pages_cursor_key("me|sec-2"),
                delta_link: Some("2026-11-18T10:02:00Z".to_string()),
                history_next_link: None,
                last_synced_at: old_ts,
            })
            .unwrap();

        sync_hierarchy_and_sections(&graph, &storage, false)
            .await
            .unwrap();
        assert_eq!(state.sec2_hits.load(AtomicOrdering::SeqCst), 2);
        let pages = storage.list_onenote_pages("me|sec-2").unwrap();
        assert_eq!(pages.len(), 2);
        assert!(pages.iter().any(|p| p.id == "p-2b"));

        srv.abort();
    }

    #[tokio::test]
    async fn test_open_page_content_poll_same_timestamp_diff_and_group_scope() {
        use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
        use serde_json::json;
        use std::sync::{
            atomic::{AtomicUsize, Ordering as AtomicOrdering},
            Mutex,
        };

        use crate::auth::{AuthManager, StoredTokens};

        #[derive(Clone)]
        struct GroupPageMock {
            xhtml: Arc<Mutex<String>>,
            content_hits: Arc<AtomicUsize>,
            meta_hits: Arc<AtomicUsize>,
        }

        let state = GroupPageMock {
            xhtml: Arc::new(Mutex::new(
                r#"<!DOCTYPE html><html><head><title>Team Spec</title></head><body><div id="div:{g1}"><p id="p:{gp1}">Initial Group Body</p></div></body></html>"#
                    .to_string(),
            )),
            content_hits: Arc::new(AtomicUsize::new(0)),
            meta_hits: Arc::new(AtomicUsize::new(0)),
        };

        let app = Router::new()
            .route(
                "/v1.0/groups/grp-42/onenote/pages/p-g1",
                get(|State(st): State<GroupPageMock>| async move {
                    st.meta_hits.fetch_add(1, AtomicOrdering::SeqCst);
                    Json(json!({
                        "id": "p-g1",
                        "title": "Team Spec",
                        "level": 0,
                        "order": 0,
                        "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                        "parentSection": { "id": "sec-g1", "displayName": "Team Sec" }
                    }))
                }),
            )
            .route(
                "/v1.0/groups/grp-42/onenote/pages/p-g1/content",
                get(|State(st): State<GroupPageMock>| async move {
                    st.content_hits.fetch_add(1, AtomicOrdering::SeqCst);
                    let body = st.xhtml.lock().unwrap().clone();
                    (StatusCode::OK, [("content-type", "text/html")], body)
                }),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tokens = StoredTokens {
            access_token: "sync-test-token".to_string(),
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
        let graph = GraphClient::new(cfg, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        let group_scope = OnenoteScope::Group {
            group_id: "grp-42".to_string(),
            group_name: Some("Platform".to_string()),
        };
        let initial_summary = OnenotePageSummary {
            page_key: group_scope.make_entity_key("p-g1"),
            id: "p-g1".to_string(),
            scope: group_scope.clone(),
            section_id: "sec-g1".to_string(),
            section_name: Some("Team Sec".to_string()),
            notebook_id: Some("nb-g1".to_string()),
            notebook_name: Some("Team NB".to_string()),
            title: "Team Spec".to_string(),
            level: 0,
            order: 0,
            created_at: None,
            last_modified_at: Some("2026-11-20T09:00:00Z".to_string()),
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: false,
        };
        let initial_detail = crate::parser::onenote::parse_onenote_page_xhtml(
            initial_summary,
            &state.xhtml.lock().unwrap().clone(),
        );
        storage.upsert_onenote_page_detail(&initial_detail).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut backoff = ErrorBackoffState::new(15_000);
        let shared = SyncSharedState::new();

        // 1. Unchanged XHTML -> 1 /content call, 0 metadata calls, 0 events emitted.
        assert!(
            run_open_page_content_poll(
                &graph,
                &storage,
                &tx,
                "group:grp-42|p-g1",
                &mut backoff,
                &shared,
                Instant::now(),
            )
            .await
        );
        assert_eq!(state.content_hits.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(state.meta_hits.load(AtomicOrdering::SeqCst), 0);
        assert!(rx.try_recv().is_err());

        // 2. Same-timestamp body edit -> 1 /content call + 1 metadata call (NO second /content),
        //    preserves Group scope & updates SQLite + FTS5.
        *state.xhtml.lock().unwrap() = r#"<!DOCTYPE html><html><head><title>Team Spec</title></head><body><div id="div:{g1}"><p id="p:{gp1}">SameTimestampEditedBody</p></div></body></html>"#.to_string();
        assert!(
            run_open_page_content_poll(
                &graph,
                &storage,
                &tx,
                "group:grp-42|p-g1",
                &mut backoff,
                &shared,
                Instant::now(),
            )
            .await
        );
        assert_eq!(state.content_hits.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(state.meta_hits.load(AtomicOrdering::SeqCst), 0);

        let ev = rx.try_recv().expect("expected OnenotePageDetailUpdated");
        match ev {
            CoreEvent::OnenotePageDetailUpdated(detail) => {
                assert_eq!(detail.summary.scope, group_scope);
                assert!(detail.body_plain.contains("SameTimestampEditedBody"));
            }
            other => panic!("unexpected event: {other:?}"),
        }
        let fts_hits = storage
            .search_onenote_pages("SameTimestampEditedBody", None, 10)
            .unwrap();
        assert_eq!(fts_hits.len(), 1);
        assert_eq!(fts_hits[0].page_key, "group:grp-42|p-g1");

        srv.abort();
    }

    #[tokio::test]
    async fn test_deterministic_11min_idle_updates_same_timestamp_open_page() {
        use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
        use serde_json::json;
        use std::sync::{
            atomic::{AtomicUsize, Ordering as AtomicOrdering},
            Mutex,
        };

        use crate::auth::{AuthManager, StoredTokens};

        #[derive(Clone)]
        struct IdleMock {
            xhtml: Arc<Mutex<String>>,
            content_hits: Arc<AtomicUsize>,
        }

        let state = IdleMock {
            xhtml: Arc::new(Mutex::new(
                r#"<!DOCTYPE html><html><head><title>Spec</title></head><body><div id="div:{1}"><p id="p:{1}">Before11Min</p></div></body></html>"#
                    .to_string(),
            )),
            content_hits: Arc::new(AtomicUsize::new(0)),
        };

        let app = Router::new()
            .route("/v1.0/me/joinedTeams", get(|| async { Json(json!({ "value": [] })) }))
            .route(
                "/v1.0/me/onenote/notebooks",
                get(|| async {
                    Json(json!({ "value": [{ "id": "nb-1", "displayName": "NB", "isDefault": true }] }))
                }),
            )
            .route("/v1.0/me/onenote/sectionGroups", get(|| async { Json(json!({ "value": [] })) }))
            .route(
                "/v1.0/me/onenote/sections",
                get(|| async {
                    Json(json!({
                        "value": [{
                            "id": "sec-1",
                            "displayName": "Specs",
                            "isDefault": true,
                            "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                            "parentNotebook": { "id": "nb-1", "displayName": "NB" }
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sections/sec-1/pages",
                get(|| async {
                    Json(json!({
                        "value": [{
                            "id": "p-1",
                            "title": "Spec",
                            "level": 0,
                            "order": 0,
                            "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                            "parentSection": { "id": "sec-1", "displayName": "Specs" }
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/pages/p-1",
                get(|| async {
                    Json(json!({
                        "id": "p-1",
                        "title": "Spec",
                        "level": 0,
                        "order": 0,
                        "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                        "parentSection": { "id": "sec-1", "displayName": "Specs" }
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/pages/p-1/content",
                get(|State(st): State<IdleMock>| async move {
                    st.content_hits.fetch_add(1, AtomicOrdering::SeqCst);
                    let body = st.xhtml.lock().unwrap().clone();
                    (StatusCode::OK, [("content-type", "text/html")], body)
                }),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = AppConfig::default();
        assert_eq!(cfg.active_poll_ms, 15_000);
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tokens = StoredTokens {
            access_token: "sync-test-token".to_string(),
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
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        bootstrap_onenote(&graph, &storage).await.unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        assert!(run_hydrate_page(&graph, &storage, &tx, "me|p-1").await);
        while rx.try_recv().is_ok() {}

        let epoch = Instant::now();
        let shared = SyncSharedState::with_epoch(epoch);
        let mut schedule = SyncScheduleState::new(&cfg, epoch);
        let mut backoff = ErrorBackoffState::new(cfg.active_poll_ms);
        let focus = FocusTarget {
            notebook_key: Some("me|nb-1".to_string()),
            section_key: Some("me|sec-1".to_string()),
            page_key: Some("me|p-1".to_string()),
        };

        // Simulate 11 minutes (660s) with ZERO user activity (crossing both 2m and 10m idle thresholds).
        let mut t = epoch;
        let eleven_min = epoch + Duration::from_secs(660);
        while t < eleven_min {
            let next = schedule.next_wakeup(&focus).min(eleven_min);
            t = next;
            if t < eleven_min {
                run_scheduled_due_work_at(&graph, &storage, &tx, &focus, &mut backoff, &shared, &mut schedule, t).await;
            }
        }
        assert!(
            shared.idle_snapshot(eleven_min).0 >= Duration::from_secs(660),
            "must remain completely idle for 11 minutes"
        );
        while rx.try_recv().is_ok() {}

        // Mutate open page body at 11 min with UNCHANGED lastModifiedDateTime.
        *state.xhtml.lock().unwrap() = r#"<!DOCTYPE html><html><head><title>Spec</title></head><body><div id="div:{1}"><p id="p:{1}">UpdatedAfter11MinIdle</p></div></body></html>"#.to_string();

        // Advance to next scheduled open-page poll (<= 15s away!).
        let next_due = schedule.next_page_poll_at;
        assert!(
            next_due.saturating_duration_since(eleven_min) <= Duration::from_secs(15),
            "open page must keep a next poll scheduled within 15s even after 11m idle"
        );
        run_scheduled_due_work_at(&graph, &storage, &tx, &focus, &mut backoff, &shared, &mut schedule, next_due).await;

        let mut saw_updated = false;
        while let Ok(ev) = rx.try_recv() {
            if let CoreEvent::OnenotePageDetailUpdated(d) = ev {
                if d.body_plain.contains("UpdatedAfter11MinIdle") {
                    saw_updated = true;
                }
            }
        }
        assert!(saw_updated, "open page must refresh on next cycle after 11 minutes idle");
        assert!(storage
            .get_onenote_page_detail("me|p-1")
            .unwrap()
            .unwrap()
            .body_plain
            .contains("UpdatedAfter11MinIdle"));

        srv.abort();
    }

    #[tokio::test]
    async fn test_deterministic_one_hour_quota_small_and_multi_section_paginated_and_concurrency() {
        use axum::{
            extract::{Path as AxumPath, Query, State},
            http::StatusCode,
            routing::get,
            Json, Router,
        };
        use serde_json::json;
        use std::sync::{
            atomic::{AtomicUsize, Ordering as AtomicOrdering},
            Mutex,
        };

        use crate::auth::{AuthManager, StoredTokens};

        #[derive(Clone)]
        struct QuotaMock {
            base_url: String,
            section_count: usize,
            paginate_sections: bool,
            paginate_active_pages: bool,
            counts: Arc<Mutex<HashMap<String, usize>>>,
            total_hits: Arc<AtomicUsize>,
            artificial_delay_ms: u64,
        }

        impl QuotaMock {
            fn record(&self, key: &str) {
                self.total_hits.fetch_add(1, AtomicOrdering::SeqCst);
                *self.counts.lock().unwrap().entry(key.to_string()).or_insert(0) += 1;
            }
        }

        async fn run_one_hour_sim(
            section_count: usize,
            paginate_sections: bool,
            paginate_active_pages: bool,
        ) -> (usize, HashMap<String, usize>, usize, Duration) {
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            let base_url = format!("http://{addr}/v1.0");
            let state = QuotaMock {
                base_url: base_url.clone(),
                section_count,
                paginate_sections,
                paginate_active_pages,
                counts: Arc::new(Mutex::new(HashMap::new())),
                total_hits: Arc::new(AtomicUsize::new(0)),
                artificial_delay_ms: 0,
            };

            let app = Router::new()
                .route("/v1.0/me/joinedTeams", get(|| async { Json(json!({ "value": [] })) }))
                .route(
                    "/v1.0/me/onenote/notebooks",
                    get(|State(st): State<QuotaMock>| async move {
                        st.record("GET /notebooks");
                        Json(json!({ "value": [{ "id": "nb-1", "displayName": "NB", "isDefault": true }] }))
                    }),
                )
                .route(
                    "/v1.0/me/onenote/sectionGroups",
                    get(|State(st): State<QuotaMock>| async move {
                        st.record("GET /sectionGroups");
                        Json(json!({ "value": [] }))
                    }),
                )
                .route(
                    "/v1.0/me/onenote/sections",
                    get(
                        |State(st): State<QuotaMock>, Query(q): Query<HashMap<String, String>>| async move {
                            let page_idx = q.get("page").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
                            st.record("GET /sections");
                            let all: Vec<serde_json::Value> = (1..=st.section_count)
                                .map(|i| {
                                    json!({
                                        "id": format!("sec-{i}"),
                                        "displayName": format!("Section {i}"),
                                        "isDefault": i == 1,
                                        "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                                        "parentNotebook": { "id": "nb-1", "displayName": "NB" }
                                    })
                                })
                                .collect();
                            if st.paginate_sections && page_idx == 1 && all.len() > 6 {
                                Json(json!({
                                    "value": all[..6],
                                    "@odata.nextLink": format!("{}/me/onenote/sections?page=2", st.base_url)
                                }))
                            } else if st.paginate_sections && page_idx == 2 {
                                Json(json!({ "value": all[6..] }))
                            } else {
                                Json(json!({ "value": all }))
                            }
                        },
                    ),
                )
                .route(
                    "/v1.0/me/onenote/sections/{sec_id}/pages",
                    get(
                        |State(st): State<QuotaMock>,
                         AxumPath(sec_id): AxumPath<String>,
                         Query(q): Query<HashMap<String, String>>| async move {
                            let page_idx = q.get("page").and_then(|v| v.parse::<usize>().ok()).unwrap_or(1);
                            st.record(&format!("GET /sections/{sec_id}/pages"));
                            if st.paginate_active_pages && sec_id == "sec-1" && page_idx == 1 {
                                Json(json!({
                                    "value": [{
                                        "id": "p-1",
                                        "title": "Open Page",
                                        "level": 0,
                                        "order": 0,
                                        "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                                        "parentSection": { "id": "sec-1", "displayName": "Section 1" }
                                    }],
                                    "@odata.nextLink": format!("{}/me/onenote/sections/sec-1/pages?page=2", st.base_url)
                                }))
                            } else if st.paginate_active_pages && sec_id == "sec-1" && page_idx == 2 {
                                Json(json!({
                                    "value": [{
                                        "id": "p-2",
                                        "title": "Second Page",
                                        "level": 0,
                                        "order": 1,
                                        "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                                        "parentSection": { "id": "sec-1", "displayName": "Section 1" }
                                    }]
                                }))
                            } else {
                                Json(json!({
                                    "value": [{
                                        "id": format!("p-{sec_id}"),
                                        "title": format!("Page {sec_id}"),
                                        "level": 0,
                                        "order": 0,
                                        "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                                        "parentSection": { "id": sec_id, "displayName": "Sec" }
                                    }]
                                }))
                            }
                        },
                    ),
                )
                .route(
                    "/v1.0/me/onenote/pages/{pid}",
                    get(|State(st): State<QuotaMock>, AxumPath(pid): AxumPath<String>| async move {
                        st.record("GET /pages/{id}");
                        Json(json!({
                            "id": pid,
                            "title": "Open Page",
                            "level": 0,
                            "order": 0,
                            "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                            "parentSection": { "id": "sec-1", "displayName": "Section 1" }
                        }))
                    }),
                )
                .route(
                    "/v1.0/me/onenote/pages/{pid}/content",
                    get(|State(st): State<QuotaMock>| async move {
                        if st.artificial_delay_ms > 0 {
                            tokio::time::sleep(Duration::from_millis(st.artificial_delay_ms)).await;
                        }
                        st.record("GET /pages/{id}/content");
                        (
                            StatusCode::OK,
                            [("content-type", "text/html")],
                            r#"<!DOCTYPE html><html><head><title>Open Page</title></head><body><div id="div:{1}"><p id="p:{1}">Steady Body</p></div></body></html>"#,
                        )
                    }),
                )
                .with_state(state.clone());

            let srv = tokio::spawn(async move {
                let _ = axum::serve(listener, app).await;
            });

            let mut cfg = AppConfig::default();
            cfg.graph_base_url = base_url;
            let tokens = StoredTokens {
                access_token: "sync-test-token".to_string(),
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
            let graph = GraphClient::new(cfg.clone(), auth).unwrap();
            let storage = Storage::open_in_memory().unwrap();

            // Bootstrap & hydrate initial open page, then reset counters to measure 1 full hour of automated polling.
            bootstrap_onenote(&graph, &storage).await.unwrap();
            let (tx, _rx) = mpsc::unbounded_channel();
            let focus_page_id = if paginate_active_pages { "me|p-1" } else { "me|p-sec-1" };
            assert!(run_hydrate_page(&graph, &storage, &tx, focus_page_id).await);
            state.counts.lock().unwrap().clear();
            state.total_hits.store(0, AtomicOrdering::SeqCst);

            let epoch = Instant::now();
            let shared = SyncSharedState::with_epoch(epoch);
            let mut schedule = SyncScheduleState::new(&cfg, epoch);
            let mut backoff = ErrorBackoffState::new(cfg.active_poll_ms);
            let focus = FocusTarget {
                notebook_key: Some("me|nb-1".to_string()),
                section_key: Some("me|sec-1".to_string()),
                page_key: Some(focus_page_id.to_string()),
            };

            // Simulate 3600s of active usage (worst-case active cadence, keeping user active every 60s).
            let end = epoch + Duration::from_secs(3_600);
            let mut t = epoch;
            let mut last_act = epoch;
            while t < end {
                let next = schedule.next_wakeup(&focus).min(end);
                t = next;
                if t.saturating_duration_since(last_act) >= Duration::from_secs(60) {
                    shared.record_activity(t);
                    last_act = t;
                }
                if t <= end {
                    run_scheduled_due_work_at(&graph, &storage, &tx, &focus, &mut backoff, &shared, &mut schedule, t).await;
                }
                if t == end {
                    break;
                }
            }

            let total = state.total_hits.load(AtomicOrdering::SeqCst);
            let counts = state.counts.lock().unwrap().clone();
            let max_in_flight = graph.max_in_flight_requests();
            let hier_interval = schedule.hierarchy_interval(Duration::ZERO);
            srv.abort();
            (total, counts, max_in_flight, hier_interval)
        }

        // Case A: Small notebook (1 NB, 3 sections)
        let (small_total, small_counts, small_conc, _small_hier) =
            run_one_hour_sim(3, false, false).await;
        println!("ONE_HOUR_SMALL_TOTAL={small_total} COUNTS={small_counts:?} MAX_CONC={small_conc}");
        assert!(
            small_total <= 350,
            "small notebook 1hr automated traffic must be <= 350, got {small_total} ({small_counts:?})"
        );
        assert!(
            *small_counts.get("GET /pages/{id}/content").unwrap_or(&0) >= 235,
            "open page content should poll ~240x/hr, got {small_counts:?}"
        );
        assert!(small_conc <= 5);

        // Case B: Multi-section paginated notebook (12 sections + paginated /sections + paginated /sec-1/pages
        // + aged cursors so inactive section page fetches also occur during the hour)
        let (multi_total, multi_counts, multi_conc, multi_hier) =
            run_one_hour_sim(12, true, true).await;
        println!(
            "ONE_HOUR_MULTI_TOTAL={multi_total} COUNTS={multi_counts:?} MAX_CONC={multi_conc} HIER_INTERVAL_SECS={}",
            multi_hier.as_secs()
        );
        assert!(
            multi_total <= 350,
            "multi-section paginated 1hr automated traffic must be <= 350, got {multi_total} ({multi_counts:?})"
        );
        assert!(
            multi_hier > Duration::from_secs(270),
            "heavy paginated notebook must slow background hierarchy interval, got {multi_hier:?}"
        );
        assert!(multi_conc <= 5);

        // Case B2: Verify adaptive pacing slows background hierarchy further when inactive sections
        // incur heavy per-pass cost (e.g. 12 requests per pass -> >= 1080s interval).
        let mut heavy_sched = SyncScheduleState::new(&AppConfig::default(), Instant::now());
        let mut fake_secs = HashSet::new();
        fake_secs.insert("me|sec-2".to_string());
        heavy_sched.mark_hierarchy_synced(
            &fake_secs,
            Instant::now(),
            &FocusTarget::default(),
            Duration::ZERO,
            12,
        );
        assert!(
            heavy_sched.hierarchy_interval(Duration::ZERO) >= Duration::from_secs(1080),
            "12-call hierarchy pass must slow interval to >= 1080s, got {:?}",
            heavy_sched.hierarchy_interval(Duration::ZERO)
        );
        assert!(
            heavy_sched.hierarchy_interval(Duration::from_secs(180)) >= Duration::from_secs(2160),
            "idle tier must slow heavy hierarchy further"
        );

        // Case C: Overlapping burst of 12 concurrent hierarchy + content requests stays <= 5 in-flight.
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let app = Router::new().route(
            "/v1.0/me/onenote/pages/{pid}/content",
            get(|| async {
                tokio::time::sleep(Duration::from_millis(35)).await;
                (StatusCode::OK, [("content-type", "text/html")], "<html><body><p>ok</p></body></html>")
            }),
        );
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });
        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tokens = StoredTokens {
            access_token: "sync-test-token".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("user-1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap();
        let futs = (0..12).map(|i| {
            let g = graph.clone();
            async move {
                g.get_onenote_page_content_xhtml(&OnenoteScope::Me, &format!("p-{i}"))
                    .await
                    .unwrap()
            }
        });
        let _ = futures::future::join_all(futs).await;
        assert!(
            graph.max_in_flight_requests() <= 5,
            "peak concurrent Graph requests must never exceed 5, got {}",
            graph.max_in_flight_requests()
        );
        assert!(graph.max_in_flight_requests() >= 2);
        srv.abort();
    }

    #[tokio::test]
    async fn test_open_page_disappeared_distinguishes_moved_deleted_404_and_transient_503() {
        use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
        use serde_json::json;
        use std::sync::{
            atomic::{AtomicU8, AtomicUsize, Ordering as AtomicOrdering},
            Mutex,
        };

        use crate::auth::{AuthManager, StoredTokens};

        // mode: 0 = moved to sec-2, 1 = transient 503, 2 = deleted 404
        #[derive(Clone)]
        struct ProbeMock {
            mode: Arc<AtomicU8>,
            probe_hits: Arc<AtomicUsize>,
            sec1_pages: Arc<Mutex<Vec<serde_json::Value>>>,
        }

        let state = ProbeMock {
            mode: Arc::new(AtomicU8::new(0)),
            probe_hits: Arc::new(AtomicUsize::new(0)),
            sec1_pages: Arc::new(Mutex::new(vec![json!({
                "id": "p-other",
                "title": "Other Page",
                "level": 0,
                "order": 0,
                "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                "parentSection": { "id": "sec-1", "displayName": "Sec 1" }
            })])),
        };

        let app = Router::new()
            .route(
                "/v1.0/me/onenote/sections/sec-1/pages",
                get(|State(st): State<ProbeMock>| async move {
                    Json(json!({ "value": st.sec1_pages.lock().unwrap().clone() }))
                }),
            )
            .route(
                "/v1.0/me/onenote/pages/p-open",
                get(|State(st): State<ProbeMock>| async move {
                    st.probe_hits.fetch_add(1, AtomicOrdering::SeqCst);
                    match st.mode.load(AtomicOrdering::SeqCst) {
                        0 => (
                            StatusCode::OK,
                            [("Retry-After", "0")],
                            Json(json!({
                                "id": "p-open",
                                "title": "Moved Open Page",
                                "level": 0,
                                "order": 0,
                                "lastModifiedDateTime": "2026-11-20T09:01:00Z",
                                "parentSection": { "id": "sec-2", "displayName": "Sec 2" }
                            })),
                        ),
                        1 => (
                            StatusCode::SERVICE_UNAVAILABLE,
                            [("Retry-After", "0")],
                            Json(json!({ "error": { "code": "ServiceUnavailable" } })),
                        ),
                        _ => (
                            StatusCode::NOT_FOUND,
                            [("Retry-After", "0")],
                            Json(json!({ "error": { "code": "20102", "message": "Page not found" } })),
                        ),
                    }
                }),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tokens = StoredTokens {
            access_token: "sync-test-token".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("user-1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        let seed_open_page = |st: &Storage| {
            let summary = OnenotePageSummary {
                page_key: "me|p-open".to_string(),
                id: "p-open".to_string(),
                scope: OnenoteScope::Me,
                section_id: "sec-1".to_string(),
                section_name: Some("Sec 1".to_string()),
                notebook_id: Some("nb-1".to_string()),
                notebook_name: Some("NB".to_string()),
                title: "Open Page".to_string(),
                level: 0,
                order: 0,
                created_at: None,
                last_modified_at: Some("2026-11-20T09:00:00Z".to_string()),
                content_url: None,
                web_url: None,
                preview_text: Some("Important searchable body".to_string()),
                has_todo: false,
            };
            let detail = crate::parser::onenote::parse_onenote_page_xhtml(
                summary,
                r#"<html><head><title>Open Page</title></head><body><div id="div:{1}"><p id="p:{1}">Important searchable body</p></div></body></html>"#,
            );
            st.upsert_onenote_page_detail(&detail).unwrap();
        };

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut backoff = ErrorBackoffState::new(15_000);
        let shared = SyncSharedState::new();

        // 1. Moved to sec-2: probed once, re-linked in place to sec-2, stays searchable in FTS5.
        seed_open_page(&storage);
        state.mode.store(0, AtomicOrdering::SeqCst);
        assert!(
            run_section_pages_refresh(
                &graph,
                &storage,
                &tx,
                "me|sec-1",
                Some("me|p-open"),
                true,
                &mut backoff,
                &shared,
                Instant::now(),
            )
            .await
        );
        assert_eq!(state.probe_hits.load(AtomicOrdering::SeqCst), 1);
        let moved = storage.get_onenote_page_summary("me|p-open").unwrap().unwrap();
        assert_eq!(moved.section_id, "sec-2");
        assert_eq!(moved.last_modified_at.as_deref(), Some("2026-11-20T09:01:00Z"));
        assert!(storage.get_onenote_page_detail("me|p-open").unwrap().is_some());
        assert_eq!(
            storage.search_onenote_pages("Important searchable body", None, 10).unwrap().len(),
            1
        );
        while rx.try_recv().is_ok() {}

        // 2. Transient 503: last read result preserved in SQLite + FTS5.
        seed_open_page(&storage);
        state.mode.store(1, AtomicOrdering::SeqCst);
        let _ = run_section_pages_refresh(
            &graph,
            &storage,
            &tx,
            "me|sec-1",
            Some("me|p-open"),
            true,
            &mut backoff,
            &shared,
            Instant::now(),
        )
        .await;
        assert!(
            storage.get_onenote_page_detail("me|p-open").unwrap().is_some(),
            "transient 503 must preserve cached open page detail"
        );
        assert_eq!(
            storage.search_onenote_pages("Important searchable body", None, 10).unwrap().len(),
            1
        );
        assert!(backoff.is_degraded());
        backoff.record_success();

        // 3. Confirmed 404: deleted from SQLite + FTS5 via existing cleanup path.
        state.mode.store(2, AtomicOrdering::SeqCst);
        assert!(
            run_section_pages_refresh(
                &graph,
                &storage,
                &tx,
                "me|sec-1",
                Some("me|p-open"),
                true,
                &mut backoff,
                &shared,
                Instant::now(),
            )
            .await
        );
        assert!(storage.get_onenote_page_summary("me|p-open").unwrap().is_none());
        assert!(storage
            .search_onenote_pages("Important searchable body", None, 10)
            .unwrap()
            .is_empty());

        srv.abort();
    }

    #[tokio::test]
    async fn test_429_without_retry_after_backs_off_and_preserves_cached_open_page() {
        use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
        use serde_json::json;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        use crate::auth::{AuthManager, StoredTokens};

        let hits = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/v1.0/me/onenote/pages/p-429/content",
                get(|State(c): State<Arc<AtomicUsize>>| async move {
                    c.fetch_add(1, AtomicOrdering::SeqCst);
                    // OneNote Graph 429 returns NO Retry-After header; return Retry-After=0 here
                    // only after verifying parse_retry_after_secs default fallback, wait: return 429
                    // with Retry-After=0 on attempts so test finishes quickly, while GraphClient
                    // still exhausts MAX_THROTTLE_RETRIES and feeds SyncErrorKind::Transient to ErrorBackoffState.
                    (
                        StatusCode::TOO_MANY_REQUESTS,
                        [("Retry-After", "0")],
                        Json(json!({ "error": { "code": "20166", "message": "Too many requests" } })),
                    )
                }),
            )
            .with_state(hits.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        // Verify parse_retry_after_secs fallback when Retry-After header is absent (Graph OneNote behavior).
        let empty_headers = reqwest::header::HeaderMap::new();
        assert_eq!(crate::graph::client::parse_retry_after_secs(&empty_headers, 0), 1);
        assert_eq!(crate::graph::client::parse_retry_after_secs(&empty_headers, 1), 2);
        assert_eq!(crate::graph::client::parse_retry_after_secs(&empty_headers, 2), 4);

        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tokens = StoredTokens {
            access_token: "sync-test-token".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("user-1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        let summary = OnenotePageSummary {
            page_key: "me|p-429".to_string(),
            id: "p-429".to_string(),
            scope: OnenoteScope::Me,
            section_id: "sec-1".to_string(),
            section_name: Some("Specs".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("NB".to_string()),
            title: "Throttled Page".to_string(),
            level: 0,
            order: 0,
            created_at: None,
            last_modified_at: Some("2026-11-20T09:00:00Z".to_string()),
            content_url: None,
            web_url: None,
            preview_text: Some("Preserved body".to_string()),
            has_todo: false,
        };
        let detail = crate::parser::onenote::parse_onenote_page_xhtml(
            summary,
            r#"<html><head><title>Throttled Page</title></head><body><div id="div:{1}"><p id="p:{1}">Preserved body</p></div></body></html>"#,
        );
        storage.upsert_onenote_page_detail(&detail).unwrap();

        let (tx, mut rx) = mpsc::unbounded_channel();
        let mut backoff = ErrorBackoffState::new(15_000);
        let shared = SyncSharedState::new();
        let now = Instant::now();

        let ok = run_open_page_content_poll(
            &graph,
            &storage,
            &tx,
            "me|p-429",
            &mut backoff,
            &shared,
            now,
        )
        .await;
        assert!(!ok);
        assert_eq!(hits.load(AtomicOrdering::SeqCst), 4, "1 initial + 3 throttle retries");
        assert!(backoff.is_degraded(), "429 exhaustion must trigger ErrorBackoffState");
        assert!(!backoff.can_attempt(now), "must back off before next attempt");
        assert!(
            storage.get_onenote_page_detail("me|p-429").unwrap().is_some(),
            "429 must preserve cached page detail"
        );
        let mut saw_retry_state = false;
        while let Ok(ev) = rx.try_recv() {
            if let CoreEvent::SyncStateChanged { summary, .. } = ev {
                if summary == POLL_RETRY_SUMMARY {
                    saw_retry_state = true;
                }
            }
        }
        assert!(saw_retry_state);
        let budget = AutomaticRequestBudget::default();
        let graph = graph.with_automatic_budget(&budget, Instant::now(), 1);
        let denied = graph
            .get_onenote_page_content_xhtml(&OnenoteScope::Me, "p-429")
            .await
            .unwrap_err();
        assert!(denied.is::<AutomaticRequestBudgetExhausted>());
        assert_eq!(hits.load(AtomicOrdering::SeqCst), 5, "the budget charged the first attempt and denied the retry");
        srv.abort();
    }

    #[tokio::test]
    async fn test_startup_and_focus_return_deduplicates_overlapping_page_fetches() {
        use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
        use serde_json::json;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        use crate::auth::{AuthManager, StoredTokens};

        #[derive(Clone, Default)]
        struct DedupeMock {
            sec1_hits: Arc<AtomicUsize>,
            meta_hits: Arc<AtomicUsize>,
            content_hits: Arc<AtomicUsize>,
            slow_section: Arc<std::sync::atomic::AtomicBool>,
            section_started: Arc<tokio::sync::Notify>,
        }

        let state = DedupeMock::default();
        let app = Router::new()
            .route("/v1.0/me", get(|| async { Json(json!({ "id": "u-1", "displayName": "T" })) }))
            .route("/v1.0/me/joinedTeams", get(|| async { Json(json!({ "value": [] })) }))
            .route(
                "/v1.0/me/onenote/notebooks",
                get(|| async {
                    Json(json!({ "value": [{ "id": "nb-1", "displayName": "NB", "isDefault": true }] }))
                }),
            )
            .route("/v1.0/me/onenote/sectionGroups", get(|| async { Json(json!({ "value": [] })) }))
            .route(
                "/v1.0/me/onenote/sections",
                get(|| async {
                    Json(json!({
                        "value": [{
                            "id": "sec-1",
                            "displayName": "Specs",
                            "isDefault": true,
                            "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                            "parentNotebook": { "id": "nb-1", "displayName": "NB" }
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sections/sec-1/pages",
                get(|State(st): State<DedupeMock>| async move {
                    st.sec1_hits.fetch_add(1, AtomicOrdering::SeqCst);
                    if st.slow_section.load(AtomicOrdering::SeqCst) {
                        st.section_started.notify_one();
                        tokio::time::sleep(Duration::from_secs(2)).await;
                    }
                    Json(json!({
                        "value": [{
                            "id": "p-1",
                            "title": "Spec",
                            "level": 0,
                            "order": 0,
                            "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                            "parentSection": { "id": "sec-1", "displayName": "Specs" }
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/pages/p-1",
                get(|State(st): State<DedupeMock>| async move {
                    st.meta_hits.fetch_add(1, AtomicOrdering::SeqCst);
                    Json(json!({
                        "id": "p-1",
                        "title": "Spec",
                        "level": 0,
                        "order": 0,
                        "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                        "parentSection": { "id": "sec-1", "displayName": "Specs" }
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/pages/p-1/content",
                get(|State(st): State<DedupeMock>| async move {
                    st.content_hits.fetch_add(1, AtomicOrdering::SeqCst);
                    (
                        StatusCode::OK,
                        [("content-type", "text/html")],
                        r#"<html><head><title>Spec</title></head><body><div id="div:{1}"><p id="p:{1}">Body</p></div></body></html>"#,
                    )
                }),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tokens = StoredTokens {
            access_token: "sync-test-token".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("user-1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".to_string(),
        };
        cfg.active_poll_ms = 2_000;
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ctrl = spawn_sync_engine(cfg, graph, storage, tx);

        // Overlapping cold start: Bootstrap + immediate set_focus + 3 duplicate HydratePage + RefreshSectionPages
        ctrl.request(SyncCommand::Bootstrap);
        ctrl.set_focus(FocusTarget {
            notebook_key: Some("me|nb-1".to_string()),
            section_key: Some("me|sec-1".to_string()),
            page_key: Some("me|p-1".to_string()),
        });
        ctrl.request(SyncCommand::RefreshSectionPages {
            section_key: "me|sec-1".to_string(),
        });
        ctrl.request(SyncCommand::HydratePage {
            page_key: "me|p-1".to_string(),
        });
        ctrl.request(SyncCommand::HydratePage {
            page_key: "me|p-1".to_string(),
        });

        let deadline = Instant::now() + Duration::from_secs(3);
        let mut saw_detail = false;
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                Ok(Some(CoreEvent::OnenotePageDetailUpdated(_))) => {
                    saw_detail = true;
                }
                Ok(Some(_)) => {}
                _ => {
                    if saw_detail {
                        break;
                    }
                }
            }
        }
        assert!(saw_detail);
        assert_eq!(
            state.sec1_hits.load(AtomicOrdering::SeqCst),
            1,
            "overlapping startup must fetch sec-1/pages only once"
        );
        assert_eq!(
            state.meta_hits.load(AtomicOrdering::SeqCst),
            1,
            "overlapping startup/select/focus must fetch page metadata only once"
        );
        assert_eq!(
            state.content_hits.load(AtomicOrdering::SeqCst),
            1,
            "overlapping startup/select/focus must fetch page content only once"
        );

        // After coalescing window, focus-return HydratePage triggers immediately (< 500ms).
        tokio::time::sleep(Duration::from_millis(OVERLAP_COALESCE_MS + 60)).await;
        let t_focus = Instant::now();
        ctrl.request(SyncCommand::HydratePage {
            page_key: "me|p-1".to_string(),
        });
        let mut saw_return_detail = false;
        while t_focus.elapsed() < Duration::from_millis(500) {
            if let Ok(Some(CoreEvent::OnenotePageDetailUpdated(_))) =
                tokio::time::timeout(Duration::from_millis(100), rx.recv()).await
            {
                saw_return_detail = true;
                break;
            }
        }
        assert!(saw_return_detail, "focus return hydrate must complete quickly");
        assert_eq!(state.meta_hits.load(AtomicOrdering::SeqCst), 2);
        assert_eq!(state.content_hits.load(AtomicOrdering::SeqCst), 2);

        state.slow_section.store(true, AtomicOrdering::SeqCst);
        tokio::time::timeout(Duration::from_secs(3), state.section_started.notified())
            .await
            .expect("the active section poll starts");
        ctrl.request(SyncCommand::HydratePage { page_key: "me|p-1".to_string() });
        let detail = tokio::time::timeout(Duration::from_millis(500), async {
            while let Some(event) = rx.recv().await {
                if matches!(event, CoreEvent::OnenotePageDetailUpdated(_)) {
                    return;
                }
            }
            panic!("sync engine event stream closed");
        })
        .await;
        assert!(detail.is_ok(), "focus must preempt the slower section poll");
        assert_eq!(state.meta_hits.load(AtomicOrdering::SeqCst), 3);

        ctrl.request(SyncCommand::Shutdown);
        srv.abort();
    }

    #[tokio::test]
    async fn test_section_page_list_prunes_on_has_more_flag_not_on_row_count() {
        use axum::{
            extract::{Query, State},
            routing::get,
            Json, Router,
        };
        use serde_json::json;
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        use crate::auth::{AuthManager, StoredTokens};

        #[derive(Clone)]
        struct PagedMock {
            base: String,
            total: Arc<AtomicUsize>,
        }

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let state = PagedMock {
            base: format!("http://{addr}/v1.0"),
            total: Arc::new(AtomicUsize::new(500)),
        };
        let app = Router::new()
            .route(
                "/v1.0/me/onenote/sections/sec-1/pages",
                get(
                    |State(st): State<PagedMock>, Query(q): Query<HashMap<String, String>>| async move {
                        let total = st.total.load(AtomicOrdering::SeqCst);
                        let skip: usize = q.get("$skip").and_then(|s| s.parse().ok()).unwrap_or(0);
                        let top: usize = q.get("$top").and_then(|s| s.parse().ok()).unwrap_or(100);
                        let end = skip.saturating_add(top).min(total);
                        let value: Vec<serde_json::Value> = (skip..end)
                            .map(|i| {
                                json!({
                                    "id": format!("p-{i:04}"),
                                    "title": format!("Page {i}"),
                                    "level": 0,
                                    "order": i,
                                    "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                                    "parentSection": { "id": "sec-1", "displayName": "Sec 1" }
                                })
                            })
                            .collect();
                        let mut body = json!({ "value": value });
                        if end < total {
                            body["@odata.nextLink"] = json!(format!(
                                "{}/me/onenote/sections/sec-1/pages?$top={top}&$skip={end}",
                                st.base
                            ));
                        }
                        Json(body)
                    },
                ),
            )
            .with_state(state.clone());
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "sync-test-token".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("user-1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap();
        let budget = AutomaticRequestBudget::default();
        let budgeted = graph.with_automatic_budget(&budget, Instant::now(), 10_000);
        assert_eq!(section_pages_limit(&budgeted), 500);
        assert_eq!(section_pages_limit(&graph), 2_000);

        let storage = Storage::open_in_memory().unwrap();
        let seed_stale = |st: &Storage| {
            st.upsert_onenote_pages(&[OnenotePageSummary {
                page_key: "me|p-stale".to_string(),
                id: "p-stale".to_string(),
                scope: OnenoteScope::Me,
                section_id: "sec-1".to_string(),
                section_name: Some("Sec 1".to_string()),
                notebook_id: None,
                notebook_name: None,
                title: "Deleted on the server".to_string(),
                level: 0,
                order: 0,
                created_at: None,
                last_modified_at: Some("2026-11-19T09:00:00Z".to_string()),
                content_url: None,
                web_url: None,
                preview_text: None,
                has_todo: false,
            }])
            .unwrap();
        };
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut backoff = ErrorBackoffState::new(15_000);
        let shared = SyncSharedState::new();

        // 1. Exactly 500 pages at the 500-page cap and no nextLink: the list is complete,
        //    so the page deleted on the server is pruned (row count alone used to block it).
        seed_stale(&storage);
        assert!(
            run_section_pages_refresh(
                &budgeted, &storage, &tx, "me|sec-1", None, true, &mut backoff, &shared,
                Instant::now(),
            )
            .await
        );
        assert!(storage.get_onenote_page_summary("me|p-stale").unwrap().is_none());
        assert_eq!(storage.list_onenote_pages("me|sec-1").unwrap().len(), 500);

        // 2. 501 pages at the 500-page cap: truncated, so nothing is pruned.
        state.total.store(501, AtomicOrdering::SeqCst);
        seed_stale(&storage);
        assert!(
            run_section_pages_refresh(
                &budgeted, &storage, &tx, "me|sec-1", None, true, &mut backoff, &shared,
                Instant::now(),
            )
            .await
        );
        assert!(storage.get_onenote_page_summary("me|p-stale").unwrap().is_some());
        assert!(storage.get_onenote_page_summary("me|p-0500").unwrap().is_none());

        // 3. An explicit refresh reads up to 2,000 pages: page 501 is cached and the
        //    stale row is pruned.
        assert!(
            run_section_pages_refresh(
                &graph, &storage, &tx, "me|sec-1", None, false, &mut backoff, &shared,
                Instant::now(),
            )
            .await
        );
        assert!(storage.get_onenote_page_summary("me|p-0500").unwrap().is_some());
        assert!(storage.get_onenote_page_summary("me|p-stale").unwrap().is_none());
        assert_eq!(storage.list_onenote_pages("me|sec-1").unwrap().len(), 501);

        srv.abort();
    }

    #[tokio::test]
    async fn test_pagelevel_rejection_keeps_cached_subpage_levels_per_section() {
        use axum::{extract::Query, http::StatusCode, routing::get, Json, Router};
        use serde_json::json;
        use std::collections::HashMap;
        use std::sync::atomic::{AtomicUsize, Ordering as AtomicOrdering};

        use crate::auth::{AuthManager, StoredTokens};

        let b_pagelevel = Arc::new(AtomicUsize::new(0));
        let b_plain = Arc::new(AtomicUsize::new(0));
        let app = Router::new()
            .route(
                "/v1.0/me/onenote/sections/sec-a/pages",
                get(|Query(q): Query<HashMap<String, String>>| async move {
                    if q.contains_key("pagelevel") {
                        return (
                            StatusCode::BAD_REQUEST,
                            Json(json!({
                                "error": { "code": "20152", "message": "pagelevel is not supported here" }
                            })),
                        );
                    }
                    // Without `pagelevel`: no level/order, newest first.
                    let page = |id: &str| {
                        json!({
                            "id": id,
                            "title": id,
                            "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                            "parentSection": { "id": "sec-a", "displayName": "A" }
                        })
                    };
                    (
                        StatusCode::OK,
                        Json(json!({ "value": [page("pa-new"), page("pa-child"), page("pa-parent")] })),
                    )
                }),
            )
            .route(
                "/v1.0/me/onenote/sections/sec-b/pages",
                get({
                    let (pl, plain) = (b_pagelevel.clone(), b_plain.clone());
                    move |Query(q): Query<HashMap<String, String>>| async move {
                        let leveled = q.contains_key("pagelevel");
                        if leveled {
                            pl.fetch_add(1, AtomicOrdering::SeqCst);
                        } else {
                            plain.fetch_add(1, AtomicOrdering::SeqCst);
                        }
                        let page = |id: &str, level: i32, order: i32| {
                            let mut p = json!({
                                "id": id,
                                "title": id,
                                "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                                "parentSection": { "id": "sec-b", "displayName": "B" }
                            });
                            if leveled {
                                p["level"] = json!(level);
                                p["order"] = json!(order);
                            }
                            p
                        };
                        Json(json!({ "value": [page("pb-parent", 0, 0), page("pb-child", 1, 1)] }))
                    }
                }),
            );
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "sync-test-token".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("user-1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let cached = |id: &str, level: i32, order: i32| OnenotePageSummary {
            page_key: format!("me|{id}"),
            id: id.to_string(),
            scope: OnenoteScope::Me,
            section_id: "sec-a".to_string(),
            section_name: Some("A".to_string()),
            notebook_id: None,
            notebook_name: None,
            title: id.to_string(),
            level,
            order,
            created_at: None,
            last_modified_at: Some("2026-11-20T09:00:00Z".to_string()),
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: false,
        };
        storage
            .upsert_onenote_pages(&[cached("pa-parent", 0, 0), cached("pa-child", 1, 1)])
            .unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let mut backoff = ErrorBackoffState::new(15_000);
        let shared = SyncSharedState::new();

        // Section A rejects `pagelevel`: the cached subpage keeps its level and
        // position under its parent; the new page goes after them as a root page.
        assert!(
            run_section_pages_refresh(
                &graph,
                &storage,
                &tx,
                "me|sec-a",
                None,
                false,
                &mut backoff,
                &shared,
                Instant::now(),
            )
            .await
        );
        let a_pages: Vec<(String, i32, i32)> = storage
            .list_onenote_pages("me|sec-a")
            .unwrap()
            .into_iter()
            .map(|p| (p.id, p.level, p.order))
            .collect();
        assert_eq!(
            a_pages,
            vec![
                ("pa-parent".to_string(), 0, 0),
                ("pa-child".to_string(), 1, 1),
                ("pa-new".to_string(), 0, 2),
            ]
        );

        // Section B still requests and stores page levels.
        assert!(
            run_section_pages_refresh(
                &graph,
                &storage,
                &tx,
                "me|sec-b",
                None,
                false,
                &mut backoff,
                &shared,
                Instant::now(),
            )
            .await
        );
        assert_eq!(b_pagelevel.load(AtomicOrdering::SeqCst), 1);
        assert_eq!(b_plain.load(AtomicOrdering::SeqCst), 0);
        let b_levels: Vec<(String, i32)> = storage
            .list_onenote_pages("me|sec-b")
            .unwrap()
            .into_iter()
            .map(|p| (p.id, p.level))
            .collect();
        assert_eq!(
            b_levels,
            vec![("pb-parent".to_string(), 0), ("pb-child".to_string(), 1)]
        );

        srv.abort();
    }

    #[tokio::test]
    async fn test_mutation_epoch_blocks_stale_poll_overwrite_resurrection_and_prune() {
        use axum::{extract::State, http::StatusCode, routing::get, Json, Router};
        use serde_json::json;

        use crate::auth::{AuthManager, StoredTokens};

        #[derive(Clone)]
        struct RaceMock {
            content_started: Arc<tokio::sync::Notify>,
            content_release: Arc<tokio::sync::Notify>,
            section_started: Arc<tokio::sync::Notify>,
            section_release: Arc<tokio::sync::Notify>,
        }

        let state = RaceMock {
            content_started: Arc::new(tokio::sync::Notify::new()),
            content_release: Arc::new(tokio::sync::Notify::new()),
            section_started: Arc::new(tokio::sync::Notify::new()),
            section_release: Arc::new(tokio::sync::Notify::new()),
        };

        let app = Router::new()
            .route(
                "/v1.0/me/onenote/pages/p-1/content",
                get(|State(st): State<RaceMock>| async move {
                    st.content_started.notify_one();
                    st.content_release.notified().await;
                    (
                        StatusCode::OK,
                        [("content-type", "text/html")],
                        r#"<html><head><title>Spec</title></head><body><div id="div:1"><p id="p:1">STALE REMOTE BODY</p></div></body></html>"#,
                    )
                }),
            )
            .route(
                "/v1.0/me/onenote/sections/sec-1/pages",
                get(|State(st): State<RaceMock>| async move {
                    st.section_started.notify_one();
                    st.section_release.notified().await;
                    Json(json!({
                        "value": [
                            {
                                "id": "p-1",
                                "title": "Spec",
                                "level": 0,
                                "order": 0,
                                "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                                "parentSection": { "id": "sec-1", "displayName": "Sec 1" }
                            },
                            {
                                "id": "p-deleted",
                                "title": "Deleted Locally",
                                "level": 0,
                                "order": 1,
                                "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                                "parentSection": { "id": "sec-1", "displayName": "Sec 1" }
                            }
                        ]
                    }))
                }),
            )
            .with_state(state.clone());

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("u1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg, auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();

        let mk_summary = |id: &str, title: &str| OnenotePageSummary {
            page_key: format!("me|{id}"),
            id: id.to_string(),
            scope: OnenoteScope::Me,
            section_id: "sec-1".to_string(),
            section_name: Some("Sec 1".to_string()),
            notebook_id: Some("nb-1".to_string()),
            notebook_name: Some("NB".to_string()),
            title: title.to_string(),
            level: 0,
            order: 0,
            created_at: None,
            last_modified_at: Some("2026-11-20T09:00:00Z".to_string()),
            content_url: None,
            web_url: None,
            preview_text: None,
            has_todo: false,
        };
        let initial_p1 = crate::parser::onenote::parse_onenote_page_xhtml(
            mk_summary("p-1", "Spec"),
            r#"<html><head><title>Spec</title></head><body><div id="div:1"><p id="p:1">Initial</p></div></body></html>"#,
        );
        storage.upsert_onenote_page_detail(&initial_p1).unwrap();
        storage
            .upsert_onenote_pages(&[mk_summary("p-deleted", "Deleted Locally")])
            .unwrap();

        // 1. Start in-flight open-page poll, perform a local edit while paused, then release poll.
        let (tx, mut rx) = mpsc::unbounded_channel();
        let g1 = graph.clone();
        let s1 = storage.clone();
        let tx1 = tx.clone();
        let poll_task = tokio::spawn(async move {
            let mut backoff = ErrorBackoffState::new(15_000);
            let shared = SyncSharedState::new();
            run_open_page_content_poll(&g1, &s1, &tx1, "me|p-1", &mut backoff, &shared, Instant::now()).await
        });
        state.content_started.notified().await;

        let local_edit = crate::parser::onenote::parse_onenote_page_xhtml(
            mk_summary("p-1", "Spec"),
            r#"<html><head><title>Spec</title></head><body><div id="div:1"><p id="p:1">LOCAL EDIT WINS</p></div></body></html>"#,
        );
        storage.upsert_onenote_page_detail(&local_edit).unwrap();
        storage.record_page_mutated("me|p-1");
        state.content_release.notify_one();
        assert!(poll_task.await.unwrap());

        assert!(
            storage
                .get_onenote_page_detail("me|p-1")
                .unwrap()
                .unwrap()
                .body_plain
                .contains("LOCAL EDIT WINS"),
            "stale poll must not overwrite newer local edit"
        );
        assert!(
            rx.try_recv().is_err(),
            "stale poll must not emit OnenotePageDetailUpdated"
        );

        // 2. Start in-flight section refresh, locally create `p-new` and delete `p-deleted`, then release.
        let g2 = graph.clone();
        let s2 = storage.clone();
        let tx2 = tx.clone();
        let sec_task = tokio::spawn(async move {
            let mut backoff = ErrorBackoffState::new(15_000);
            let shared = SyncSharedState::new();
            run_section_pages_refresh(
                &g2,
                &s2,
                &tx2,
                "me|sec-1",
                Some("me|p-1"),
                false,
                &mut backoff,
                &shared,
                Instant::now(),
            )
            .await
        });
        state.section_started.notified().await;

        storage
            .upsert_onenote_pages(&[mk_summary("p-new", "Brand New Local Page")])
            .unwrap();
        storage.record_page_created("me|p-new");
        storage.delete_onenote_page("me|p-deleted").unwrap();
        storage.record_page_deleted("me|p-deleted");
        state.section_release.notify_one();
        assert!(sec_task.await.unwrap());

        assert!(
            storage.get_onenote_page_summary("me|p-new").unwrap().is_some(),
            "stale section refresh must not prune locally created page"
        );
        assert!(
            storage.get_onenote_page_summary("me|p-deleted").unwrap().is_none(),
            "stale section refresh must not resurrect locally deleted page"
        );

        srv.abort();
    }

    #[tokio::test]
    async fn test_bootstrap_prioritizes_focused_section_and_page_before_startup_budget_cap() {
        use axum::{extract::Path as AxumPath, http::StatusCode, routing::get, Json, Router};
        use serde_json::json;

        use crate::auth::{AuthManager, StoredTokens};

        let app = Router::new()
            .route("/v1.0/me", get(|| async { Json(json!({ "id": "u1", "displayName": "T" })) }))
            .route("/v1.0/me/joinedTeams", get(|| async { Json(json!({ "value": [] })) }))
            .route(
                "/v1.0/me/onenote/notebooks",
                get(|| async {
                    Json(json!({ "value": [{ "id": "nb-1", "displayName": "Big NB", "isDefault": true }] }))
                }),
            )
            .route("/v1.0/me/onenote/sectionGroups", get(|| async { Json(json!({ "value": [] })) }))
            .route(
                "/v1.0/me/onenote/sections",
                get(|| async {
                    let secs: Vec<serde_json::Value> = (1..=50)
                        .map(|i| {
                            json!({
                                "id": format!("sec-{i}"),
                                "displayName": format!("Section {i}"),
                                "isDefault": i == 1,
                                "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                                "parentNotebook": { "id": "nb-1", "displayName": "Big NB" }
                            })
                        })
                        .collect();
                    Json(json!({ "value": secs }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sections/{sec_id}/pages",
                get(|AxumPath(sec_id): AxumPath<String>| async move {
                    Json(json!({
                        "value": [{
                            "id": format!("p-{sec_id}"),
                            "title": format!("Page in {sec_id}"),
                            "level": 0,
                            "order": 0,
                            "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                            "parentSection": { "id": sec_id, "displayName": "Sec" }
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/pages/{pid}",
                get(|AxumPath(pid): AxumPath<String>| async move {
                    Json(json!({
                        "id": pid,
                        "title": "Focused Page 50",
                        "level": 0,
                        "order": 0,
                        "lastModifiedDateTime": "2026-11-20T09:00:00Z",
                        "parentSection": { "id": "sec-50", "displayName": "Section 50" }
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/pages/{pid}/content",
                get(|| async {
                    (
                        StatusCode::OK,
                        [("content-type", "text/html")],
                        r#"<html><head><title>Focused Page 50</title></head><body><p>Focused Body 50</p></body></html>"#,
                    )
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let cfg = AppConfig {
            graph_base_url: format!("http://{addr}/v1.0"),
            ..AppConfig::default()
        };
        let tokens = StoredTokens {
            access_token: "tok".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "tid".to_string(),
            user_id: Some("u1".to_string()),
            user_display_name: None,
            user_email: None,
            auth_mode: "access_token".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (tx, mut rx) = mpsc::unbounded_channel();
        let ctrl = spawn_sync_engine(cfg, graph, storage.clone(), tx);

        ctrl.set_focus(FocusTarget {
            notebook_key: Some("me|nb-1".to_string()),
            section_key: Some("me|sec-50".to_string()),
            page_key: Some("me|p-sec-50".to_string()),
        });
        ctrl.request(SyncCommand::Bootstrap);

        let mut saw_focused_detail = false;
        let mut saw_sync_error = false;
        let deadline = Instant::now() + Duration::from_secs(5);
        while Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(250), rx.recv()).await {
                Ok(Some(CoreEvent::OnenotePageDetailUpdated(d)))
                    if d.summary.page_key == "me|p-sec-50" =>
                {
                    saw_focused_detail = true;
                }
                Ok(Some(CoreEvent::SyncError(_))) => {
                    saw_sync_error = true;
                }
                Ok(Some(CoreEvent::SyncStateChanged { busy: false, .. }))
                    if saw_focused_detail =>
                {
                    break;
                }
                Ok(Some(_)) => {}
                _ => {
                    if saw_focused_detail {
                        break;
                    }
                }
            }
        }
        ctrl.request(SyncCommand::Shutdown);
        srv.abort();

        assert!(
            saw_focused_detail,
            "bootstrap must hydrate focused sec-50 page even with 50 sections > STARTUP_HTTP_BUDGET"
        );
        assert!(
            !saw_sync_error,
            "startup budget cap must not emit SyncError for focused page"
        );
        assert!(storage
            .get_onenote_page_detail("me|p-sec-50")
            .unwrap()
            .is_some());
    }

    #[tokio::test]
    async fn run_full_refresh_overlaps_active_requests_and_prefetches_siblings() {
        use axum::{http::StatusCode, routing::get, Json, Router};
        use crate::auth::{AuthManager, StoredTokens};

        let barrier = Arc::new(tokio::sync::Barrier::new(3));
        let app = Router::new()
            .route(
                "/v1.0/me",
                get({
                    let b = barrier.clone();
                    move || async move {
                        b.wait().await;
                        Json(serde_json::json!({
                            "id": "user-1",
                            "displayName": "Tester",
                            "mail": "tester@example.com"
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/notebooks",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{ "id": "nb-1", "displayName": "NB" }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sections",
                get(|| async {
                    Json(serde_json::json!({
                        "value": [{
                            "id": "sec-1",
                            "displayName": "Sec",
                            "parentNotebook": { "id": "nb-1", "displayName": "NB" }
                        }]
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/sectionGroups",
                get(|| async { Json(serde_json::json!({ "value": [] })) }),
            )
            .route(
                "/v1.0/me/onenote/sections/sec-1/pages",
                get({
                    let b = barrier.clone();
                    move || async move {
                        b.wait().await;
                        Json(serde_json::json!({
                            "value": [
                                { "id": "p-1", "title": "Active Page", "level": 0, "order": 0, "parentSection": { "id": "sec-1", "displayName": "Sec" } },
                                { "id": "p-2", "title": "Sibling Page", "level": 0, "order": 1, "parentSection": { "id": "sec-1", "displayName": "Sec" } }
                            ]
                        }))
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/pages/p-1",
                get(|| async {
                    Json(serde_json::json!({
                        "id": "p-1",
                        "title": "Active Page",
                        "level": 0,
                        "order": 0,
                        "parentSection": { "id": "sec-1", "displayName": "Sec" }
                    }))
                }),
            )
            .route(
                "/v1.0/me/onenote/pages/p-1/content",
                get({
                    let b = barrier.clone();
                    move || async move {
                        b.wait().await;
                        (
                            StatusCode::OK,
                            [("content-type", "text/html")],
                            "<!DOCTYPE html><html><head><title>Active Page</title></head><body><div id=\"div:{1}\"><p id=\"p:{1}\">Active body</p></div></body></html>",
                        )
                    }
                }),
            )
            .route(
                "/v1.0/me/onenote/pages/p-2/content",
                get(|| async {
                    (
                        StatusCode::OK,
                        [("content-type", "text/html")],
                        "<!DOCTYPE html><html><head><title>Sibling Page</title></head><body><div id=\"div:{2}\"><p id=\"p:{2}\">Prefetched sibling body</p></div></body></html>",
                    )
                }),
            );

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let srv = tokio::spawn(async move {
            let _ = axum::serve(listener, app).await;
        });

        let mut cfg = AppConfig::default();
        cfg.graph_base_url = format!("http://{addr}/v1.0");
        let tokens = StoredTokens {
            access_token: "sync-test-token".to_string(),
            refresh_token: None,
            expires_at: chrono::Utc::now().timestamp() + 3600,
            client_id: "cid".to_string(),
            tenant_id: "consumers".to_string(),
            user_id: Some("user-1".to_string()),
            user_display_name: Some("Tester".to_string()),
            user_email: Some("tester@example.com".to_string()),
            auth_mode: "pkce".to_string(),
        };
        let auth = AuthManager::from_tokens(cfg.clone(), tokens);
        let graph = GraphClient::new(cfg.clone(), auth).unwrap();
        let storage = Storage::open_in_memory().unwrap();
        let (tx, _rx) = mpsc::unbounded_channel();
        let epoch = Instant::now();
        let shared = SyncSharedState::with_epoch(epoch);
        let mut schedule = SyncScheduleState::new(&cfg, epoch);
        let mut backoff = ErrorBackoffState::new(cfg.active_poll_ms);
        let focus = FocusTarget {
            notebook_key: Some("me|nb-1".to_string()),
            section_key: Some("me|sec-1".to_string()),
            page_key: Some("me|p-1".to_string()),
        };

        tokio::time::timeout(
            Duration::from_secs(3),
            run_full_refresh(
                &graph,
                &graph,
                &storage,
                &tx,
                &focus,
                &mut backoff,
                &shared,
                &mut schedule,
            ),
        )
        .await
        .expect("get_me, focused section pages, and focused page hydrate must overlap concurrently");

        // Wait briefly for the spawned sibling prefetch to warm SQLite.
        let mut prefetched = None;
        for _ in 0..40 {
            if let Some(d) = storage.get_onenote_page_detail("me|p-2").unwrap() {
                prefetched = Some(d);
                break;
            }
            tokio::time::sleep(Duration::from_millis(25)).await;
        }
        let prefetched = prefetched.expect("active section sibling p-2 must be prefetched into SQLite");
        assert!(prefetched.body_plain.contains("Prefetched sibling body"));
        srv.abort();
    }
}
