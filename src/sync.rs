//! Adaptive two-lane background synchronization engine for Microsoft OneNote
//! (Lane 1: Fast Interactive Lane + Lane 2: Quiet Background Poll Lane).

use std::{
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
    time::{self, MissedTickBehavior},
};

use crate::{
    config::AppConfig,
    graph::GraphClient,
    models::{
        split_onenote_entity_key, CoreEvent, NotificationCandidate, OnenotePageSummary,
        OnenoteSection, SyncCursor, POLL_RETRY_SUMMARY,
    },
    storage::Storage,
};

const IDLE_THRESHOLD_SECS: u64 = 120;
const DEEP_IDLE_THRESHOLD_SECS: u64 = 600;
const AUTH_EXPIRY_PAUSE_SECS: u64 = 30;
const MAX_ERROR_BACKOFF_MS: u64 = 300_000;
const BOOTSTRAP_MAX_SECTIONS: usize = 64;
const BOOTSTRAP_SECTION_CONCURRENCY: usize = 6;
const SECTION_PAGES_LIMIT: usize = 500;

fn section_pages_cursor_key(section_key: &str) -> String {
    format!("onenote:pages:{section_key}")
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
    sync_hierarchy_and_sections(graph, storage, true).await
}

async fn sync_hierarchy_and_sections(
    graph: &GraphClient,
    storage: &Storage,
    force_all_sections: bool,
) -> Result<OnenoteBootstrapSummary> {
    let notebooks = graph.list_all_accessible_notebooks(true).await?;
    let (section_groups, sections) = graph.list_all_accessible_sections(&notebooks).await?;
    storage.reconcile_onenote_hierarchy(&notebooks, &section_groups, &sections, true)?;

    let mut ordered_sections: Vec<&OnenoteSection> = sections.iter().collect();
    ordered_sections.sort_by_key(|s| {
        if s.is_default && s.scope.is_personal() {
            0u8
        } else if s.scope.is_personal() {
            1u8
        } else {
            2u8
        }
    });

    let sections_to_sync: Vec<OnenoteSection> = ordered_sections
        .into_iter()
        .filter(|sec| {
            if force_all_sections {
                return true;
            }
            let cursor = storage
                .get_sync_cursor(&section_pages_cursor_key(&sec.section_key))
                .ok()
                .flatten();
            match cursor {
                None => true,
                Some(cur) => {
                    sec.last_modified_at.is_none()
                        || cur.delta_link.as_deref() != sec.last_modified_at.as_deref()
                }
            }
        })
        .take(BOOTSTRAP_MAX_SECTIONS)
        .cloned()
        .collect();

    let attempted_sections = sections_to_sync.len();
    let results: Vec<(OnenoteSection, Result<Vec<OnenotePageSummary>>)> =
        stream::iter(sections_to_sync.into_iter().map(|sec| async move {
            let res = graph
                .list_onenote_pages(&sec.scope, &sec.id, SECTION_PAGES_LIMIT)
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

    for (sec, pages_res) in results {
        match pages_res {
            Ok(pages) => {
                let is_complete = pages.len() < SECTION_PAGES_LIMIT;
                storage.reconcile_onenote_section_pages(&sec.section_key, &pages, is_complete)?;
                pages_synced += pages.len();
                sections_succeeded += 1;
                let _ = storage.upsert_sync_cursor(&SyncCursor {
                    scope_key: section_pages_cursor_key(&sec.section_key),
                    delta_link: sec.last_modified_at.clone(),
                    history_next_link: None,
                    last_synced_at: chrono::Utc::now().to_rfc3339(),
                });
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

    Ok(OnenoteBootstrapSummary {
        notebooks: notebooks.len(),
        section_groups: section_groups.len(),
        sections: sections.len(),
        pages: pages_synced,
    })
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

async fn run_sync_loop(
    config: AppConfig,
    graph: GraphClient,
    storage: Storage,
    event_tx: mpsc::UnboundedSender<CoreEvent>,
    mut focus_rx: watch::Receiver<FocusTarget>,
    mut nudge_rx: mpsc::UnboundedReceiver<SyncCommand>,
    shared: Arc<SyncSharedState>,
) {
    let active_ms = config.active_poll_ms.max(2_000);
    let hierarchy_ms = config.hierarchy_poll_ms.max(15_000);

    let mut active_tick = time::interval(Duration::from_millis(active_ms));
    active_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    active_tick.tick().await;

    let mut hierarchy_tick = time::interval(Duration::from_millis(hierarchy_ms));
    hierarchy_tick.set_missed_tick_behavior(MissedTickBehavior::Skip);
    hierarchy_tick.tick().await;

    let mut backoff = ErrorBackoffState::new(active_ms);

    loop {
        tokio::select! {
            maybe_cmd = nudge_rx.recv() => {
                match maybe_cmd {
                    None | Some(SyncCommand::Shutdown) => break,
                    Some(SyncCommand::NotifyUserActivity) => {
                        shared.record_activity(Instant::now());
                    }
                    Some(SyncCommand::Bootstrap) | Some(SyncCommand::RefreshAll) => {
                        shared.record_activity(Instant::now());
                        shared.clear_auth_pause();
                        let focus = focus_rx.borrow().clone();
                        run_full_refresh(&graph, &storage, &event_tx, &focus, &mut backoff, &shared).await;
                    }
                    Some(SyncCommand::RefreshHierarchy) => {
                        shared.record_activity(Instant::now());
                        let _ = run_hierarchy_refresh(&graph, &storage, &event_tx, true, &mut backoff, &shared).await;
                    }
                    Some(SyncCommand::RefreshSectionPages { section_key }) => {
                        shared.record_activity(Instant::now());
                        let _ = run_section_pages_refresh(&graph, &storage, &event_tx, &section_key, false, &mut backoff, &shared).await;
                    }
                    Some(SyncCommand::HydratePage { page_key }) => {
                        shared.record_activity(Instant::now());
                        let _ = run_hydrate_page(&graph, &storage, &event_tx, &page_key).await;
                    }
                }
            }
            changed = focus_rx.changed() => {
                if changed.is_err() {
                    break;
                }
                shared.record_activity(Instant::now());
                let focus = focus_rx.borrow_and_update().clone();
                if let Some(sec_key) = focus.section_key.as_deref() {
                    let _ = run_section_pages_refresh(&graph, &storage, &event_tx, sec_key, false, &mut backoff, &shared).await;
                }
                if let Some(page_key) = focus.page_key.as_deref() {
                    let _ = run_hydrate_page(&graph, &storage, &event_tx, page_key).await;
                }
            }
            _ = active_tick.tick() => {
                let now = Instant::now();
                if shared.is_auth_paused(now) || !backoff.can_attempt(now) {
                    continue;
                }
                let (idle, _) = shared.idle_snapshot(now);
                if idle.as_secs() >= DEEP_IDLE_THRESHOLD_SECS {
                    continue;
                }
                let focus = focus_rx.borrow().clone();
                if let Some(sec_key) = focus.section_key.as_deref() {
                    let _ = run_section_pages_refresh(&graph, &storage, &event_tx, sec_key, true, &mut backoff, &shared).await;
                }
            }
            _ = hierarchy_tick.tick() => {
                let now = Instant::now();
                if shared.is_auth_paused(now) || !backoff.can_attempt(now) {
                    continue;
                }
                let (idle, _) = shared.idle_snapshot(now);
                if idle.as_secs() >= IDLE_THRESHOLD_SECS {
                    continue;
                }
                let _ = run_hierarchy_refresh(&graph, &storage, &event_tx, true, &mut backoff, &shared).await;
            }
        }
    }
}

async fn run_full_refresh(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    focus: &FocusTarget,
    backoff: &mut ErrorBackoffState,
    shared: &SyncSharedState,
) {
    let _ = event_tx.send(CoreEvent::SyncStateChanged {
        busy: true,
        summary: "Syncing OneNote...".to_string(),
    });
    if let Ok(me) = graph.get_me().await {
        let _ = event_tx.send(CoreEvent::ProfileLoaded(me));
    }
    let hierarchy_ok =
        run_hierarchy_refresh(graph, storage, event_tx, false, backoff, shared).await;
    let target_sec = focus.section_key.clone().or_else(|| {
        storage
            .list_onenote_sections(None)
            .ok()
            .and_then(|secs| secs.into_iter().next().map(|s| s.section_key))
    });
    let pages_ok = if let Some(sec_key) = target_sec {
        if !hierarchy_ok && shared.is_auth_paused(Instant::now()) {
            false
        } else {
            run_section_pages_refresh(graph, storage, event_tx, &sec_key, false, backoff, shared)
                .await
        }
    } else {
        true
    };
    let hydrate_ok = if let Some(page_key) = focus.page_key.as_deref() {
        if !hierarchy_ok && shared.is_auth_paused(Instant::now()) {
            false
        } else {
            run_hydrate_page(graph, storage, event_tx, page_key).await
        }
    } else {
        true
    };

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
    } else {
        let _ = event_tx.send(CoreEvent::SyncStateChanged {
            busy: false,
            summary: "Sync failed".to_string(),
        });
    }
}

async fn run_hierarchy_refresh(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    incremental: bool,
    backoff: &mut ErrorBackoffState,
    shared: &SyncSharedState,
) -> bool {
    match sync_hierarchy_and_sections(graph, storage, !incremental).await {
        Ok(_) => {
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
            true
        }
        Err(err) => {
            handle_sync_error(err, graph, event_tx, backoff, shared).await;
            false
        }
    }
}

async fn run_section_pages_refresh(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    section_key: &str,
    background: bool,
    backoff: &mut ErrorBackoffState,
    shared: &SyncSharedState,
) -> bool {
    let (scope, section_id) = split_onenote_entity_key(section_key);
    let prev_ids: std::collections::HashSet<String> = storage
        .list_onenote_pages(section_key)
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.page_key)
        .collect();

    match graph
        .list_onenote_pages(&scope, &section_id, SECTION_PAGES_LIMIT)
        .await
    {
        Ok(pages) => {
            let is_complete = pages.len() < SECTION_PAGES_LIMIT;
            let _ = storage.reconcile_onenote_section_pages(section_key, &pages, is_complete);
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
            if backoff.record_success() {
                let _ = event_tx.send(CoreEvent::SyncStateChanged {
                    busy: false,
                    summary: "Synced".to_string(),
                });
            }
            let notify_candidate = if background && !prev_ids.is_empty() {
                find_new_page_notification(&prev_ids, &pages)
            } else {
                None
            };
            let persisted = storage.list_onenote_pages(section_key).unwrap_or(pages);
            let _ = event_tx.send(CoreEvent::OnenotePagesUpdated {
                section_key: section_key.to_string(),
                pages: persisted,
                notify_candidate,
            });
            true
        }
        Err(err) => {
            handle_sync_error(err, graph, event_tx, backoff, shared).await;
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
    let summary_hint = storage.get_onenote_page_summary(page_key).ok().flatten();
    let (scope, raw_id) = split_onenote_entity_key(page_key);
    match graph
        .get_onenote_page_detail(&scope, &raw_id, summary_hint)
        .await
    {
        Ok(detail) => {
            let _ = storage.upsert_onenote_page_detail(&detail);
            let _ = event_tx.send(CoreEvent::OnenotePageDetailUpdated(Box::new(detail)));
            true
        }
        Err(err) => {
            let _ = event_tx.send(CoreEvent::SyncError(format!(
                "Failed to load OneNote page: {err}"
            )));
            false
        }
    }
}

fn find_new_page_notification(
    prev_keys: &std::collections::HashSet<String>,
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
) {
    let now = Instant::now();
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
}
