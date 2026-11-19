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
        SyncCursor, POLL_RETRY_SUMMARY,
    },
    storage::Storage,
};

const IDLE_THRESHOLD_SECS: u64 = 120;
const DEEP_IDLE_THRESHOLD_SECS: u64 = 600;
const AUTH_EXPIRY_PAUSE_SECS: u64 = 30;
const MAX_ERROR_BACKOFF_MS: u64 = 300_000;

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
        let _ = self.focus_tx.send(target);
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

/// Performs an initial OneNote sync: notebooks, section groups, sections, and pages for the first/default section.
pub async fn bootstrap_onenote(
    graph: &GraphClient,
    storage: &Storage,
) -> Result<OnenoteBootstrapSummary> {
    let notebooks = graph.list_all_accessible_notebooks(true).await?;
    storage.upsert_onenote_notebooks(&notebooks)?;

    let (section_groups, sections) = graph.list_all_accessible_sections(&notebooks).await?;
    if !section_groups.is_empty() {
        storage.upsert_onenote_section_groups(&section_groups)?;
    }
    storage.upsert_onenote_sections(&sections)?;

    let mut pages_synced = 0usize;
    if let Some(first_sec) = sections
        .iter()
        .find(|s| s.is_default && s.scope.is_personal())
        .or_else(|| sections.first())
    {
        if let Ok(pages) = graph
            .list_onenote_pages(&first_sec.scope, &first_sec.id, 100)
            .await
        {
            pages_synced = pages.len();
            let _ = storage.upsert_onenote_pages(&pages);
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
                        run_hierarchy_refresh(&graph, &storage, &event_tx, &mut backoff, &shared).await;
                    }
                    Some(SyncCommand::RefreshSectionPages { section_key }) => {
                        shared.record_activity(Instant::now());
                        run_section_pages_refresh(&graph, &storage, &event_tx, &section_key, false, &mut backoff, &shared).await;
                    }
                    Some(SyncCommand::HydratePage { page_key }) => {
                        shared.record_activity(Instant::now());
                        run_hydrate_page(&graph, &storage, &event_tx, &page_key).await;
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
                    run_section_pages_refresh(&graph, &storage, &event_tx, sec_key, false, &mut backoff, &shared).await;
                }
                if let Some(page_key) = focus.page_key.as_deref() {
                    run_hydrate_page(&graph, &storage, &event_tx, page_key).await;
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
                    run_section_pages_refresh(&graph, &storage, &event_tx, sec_key, true, &mut backoff, &shared).await;
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
                run_hierarchy_refresh(&graph, &storage, &event_tx, &mut backoff, &shared).await;
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
    run_hierarchy_refresh(graph, storage, event_tx, backoff, shared).await;
    let target_sec = focus.section_key.clone().or_else(|| {
        storage
            .list_onenote_sections(None)
            .ok()
            .and_then(|secs| secs.into_iter().next().map(|s| s.section_key))
    });
    if let Some(sec_key) = target_sec {
        run_section_pages_refresh(graph, storage, event_tx, &sec_key, false, backoff, shared).await;
    }
    if let Some(page_key) = focus.page_key.as_deref() {
        run_hydrate_page(graph, storage, event_tx, page_key).await;
    }
    let _ = event_tx.send(CoreEvent::SyncStateChanged {
        busy: false,
        summary: "Synced".to_string(),
    });
}

async fn run_hierarchy_refresh(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    backoff: &mut ErrorBackoffState,
    shared: &SyncSharedState,
) {
    match bootstrap_onenote(graph, storage).await {
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
        }
        Err(err) => handle_sync_error(err, graph, event_tx, backoff, shared).await,
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
) {
    let (scope, section_id) = split_onenote_entity_key(section_key);
    let prev_ids: std::collections::HashSet<String> = storage
        .list_onenote_pages(section_key)
        .unwrap_or_default()
        .into_iter()
        .map(|p| p.page_key)
        .collect();

    match graph.list_onenote_pages(&scope, &section_id, 100).await {
        Ok(pages) => {
            let _ = storage.upsert_onenote_pages(&pages);
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
        }
        Err(err) => handle_sync_error(err, graph, event_tx, backoff, shared).await,
    }
}

async fn run_hydrate_page(
    graph: &GraphClient,
    storage: &Storage,
    event_tx: &mpsc::UnboundedSender<CoreEvent>,
    page_key: &str,
) {
    let summary_hint = storage.get_onenote_page_summary(page_key).ok().flatten();
    let (scope, raw_id) = split_onenote_entity_key(page_key);
    match graph
        .get_onenote_page_detail(&scope, &raw_id, summary_hint)
        .await
    {
        Ok(detail) => {
            let _ = storage.upsert_onenote_page_detail(&detail);
            let _ = event_tx.send(CoreEvent::OnenotePageDetailUpdated(Box::new(detail)));
        }
        Err(err) => {
            let _ = event_tx.send(CoreEvent::SyncError(format!(
                "Failed to load OneNote page: {err}"
            )));
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
            shared.pause_auth(now, Duration::from_secs(AUTH_EXPIRY_PAUSE_SECS));
            let _ = event_tx.send(CoreEvent::SyncError(format!(
                "Authentication required: {err}"
            )));
        }
        SyncErrorKind::Forbidden403 => {
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
}
