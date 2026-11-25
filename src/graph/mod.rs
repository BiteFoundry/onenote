//! Microsoft Graph v1.0 client for OneNote notebooks, section groups, sections, pages, and resources.

pub mod client;
pub mod copy;
pub mod media;
pub mod onenote;

use std::{
    collections::VecDeque,
    sync::{
        atomic::{AtomicBool, AtomicU64, AtomicUsize, Ordering},
        Arc, Mutex, OnceLock,
    },
    time::{Duration, Instant},
};

use anyhow::Result;
use tokio::sync::Semaphore;

use crate::{auth::AuthManager, config::AppConfig};

/// Canonical prefix emitted by `GraphClient` HTTP errors; kept as a shared constant for safe parsing.
pub const GRAPH_API_ERROR_PREFIX: &str = "Graph API error (";
/// Maximum concurrent outbound Microsoft Graph requests per client (strictly below OneNote's 5-request limit).
pub const MAX_CONCURRENT_GRAPH_REQUESTS: usize = 4;

#[derive(Clone, Debug, Default)]
pub(crate) struct AutomaticRequestBudget(Arc<Mutex<VecDeque<Instant>>>);

impl AutomaticRequestBudget {
    fn requests(&self, now: Instant) -> std::sync::MutexGuard<'_, VecDeque<Instant>> {
        let mut requests = self.0.lock().unwrap_or_else(|err| err.into_inner());
        while requests.front().is_some_and(|sent| {
            now.saturating_duration_since(*sent) >= Duration::from_secs(3600)
        }) {
            requests.pop_front();
        }
        requests
    }

    pub(crate) fn used(&self, now: Instant) -> usize {
        self.requests(now).len()
    }

    fn charge(&self, now: Instant, limit: usize) -> bool {
        let mut requests = self.requests(now);
        if requests.len() >= limit {
            return false;
        }
        requests.push_back(now);
        true
    }
}

#[derive(Clone)]
struct RequestAllowance {
    budget: AutomaticRequestBudget,
    minimum_time: Instant,
    limit: usize,
}

#[derive(Debug)]
pub(crate) struct AutomaticRequestBudgetExhausted;

impl std::fmt::Display for AutomaticRequestBudgetExhausted {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("Automatic OneNote request allowance exhausted")
    }
}

impl std::error::Error for AutomaticRequestBudgetExhausted {}

#[derive(Clone)]
pub struct GraphClient {
    auth: AuthManager,
    config: AppConfig,
    http: reqwest::Client,
    operation_http: reqwest::Client,
    my_user_id: Option<String>,
    cached_my_user_id: Arc<OnceLock<String>>,
    resolve_user_mutex: Arc<tokio::sync::Mutex<()>>,
    request_semaphore: Arc<Semaphore>,
    http_requests_total: Arc<AtomicU64>,
    in_flight_requests: Arc<AtomicUsize>,
    max_in_flight_requests: Arc<AtomicUsize>,
    pagelevel_unsupported: Arc<AtomicBool>,
    request_allowance: Option<RequestAllowance>,
}

impl GraphClient {
    pub fn new(config: AppConfig, auth: AuthManager) -> Result<Self> {
        let http_builder = || -> Result<reqwest::ClientBuilder> {
            Ok(crate::http::client_builder()?
                .connect_timeout(Duration::from_secs(10))
                .timeout(Duration::from_secs(60))
                .tcp_nodelay(true)
                .pool_max_idle_per_host(16)
                .pool_idle_timeout(Duration::from_secs(90))
                .tcp_keepalive(Duration::from_secs(30)))
        };
        let http = http_builder()?.build()?;
        let operation_http = http_builder()?
            .redirect(reqwest::redirect::Policy::none())
            .retry(reqwest::retry::never())
            .build()?;
        Ok(Self {
            auth,
            config,
            http,
            operation_http,
            my_user_id: None,
            cached_my_user_id: Arc::new(OnceLock::new()),
            resolve_user_mutex: Arc::new(tokio::sync::Mutex::new(())),
            request_semaphore: Arc::new(Semaphore::new(MAX_CONCURRENT_GRAPH_REQUESTS)),
            http_requests_total: Arc::new(AtomicU64::new(0)),
            in_flight_requests: Arc::new(AtomicUsize::new(0)),
            max_in_flight_requests: Arc::new(AtomicUsize::new(0)),
            pagelevel_unsupported: Arc::new(AtomicBool::new(false)),
            request_allowance: None,
        })
    }

    pub(crate) fn is_pagelevel_unsupported(&self) -> bool {
        self.pagelevel_unsupported.load(Ordering::Relaxed)
    }

    pub(crate) fn mark_pagelevel_unsupported(&self) {
        self.pagelevel_unsupported.store(true, Ordering::Relaxed);
    }

    pub(crate) fn with_automatic_budget(
        &self,
        budget: &AutomaticRequestBudget,
        now: Instant,
        limit: usize,
    ) -> Self {
        let mut graph = self.clone();
        graph.request_allowance = Some(RequestAllowance {
            budget: budget.clone(),
            minimum_time: now,
            limit,
        });
        graph
    }

    /// True for clones created by `with_automatic_budget` (background sync).
    pub(crate) fn has_automatic_budget(&self) -> bool {
        self.request_allowance.is_some()
    }

    pub fn http_request_count(&self) -> u64 {
        self.http_requests_total.load(Ordering::Relaxed)
    }

    pub fn max_in_flight_requests(&self) -> usize {
        self.max_in_flight_requests.load(Ordering::Relaxed)
    }

    pub fn with_my_user_id(mut self, user_id: impl Into<String>) -> Self {
        let uid = user_id.into();
        let lock = OnceLock::new();
        let _ = lock.set(uid.clone());
        self.cached_my_user_id = Arc::new(lock);
        self.my_user_id = Some(uid);
        self
    }

    pub fn my_user_id(&self) -> Option<&str> {
        self.my_user_id
            .as_deref()
            .or_else(|| self.cached_my_user_id.get().map(String::as_str))
    }

    pub fn auth(&self) -> &AuthManager {
        &self.auth
    }

    pub fn config(&self) -> &AppConfig {
        &self.config
    }

    pub fn http(&self) -> &reqwest::Client {
        &self.http
    }
}
