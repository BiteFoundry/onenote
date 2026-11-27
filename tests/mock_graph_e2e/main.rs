//! End-to-end integration tests for standalone `@bitefoundry/onenote` (`onenote-cli`)
//! against an in-process mock Microsoft Graph v1.0 server.

mod cli_feedback_regression;
mod confirmed_bug_regressions;
mod core_e2e;
mod discovery_completeness;
mod native_copy;
mod search_and_retry;
mod server;
mod sync_e2e;
