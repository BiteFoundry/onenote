use std::ffi::OsStr;

use anyhow::Result;

fn needs_multi_thread_runtime<I, S>(args: I) -> bool
where
    I: IntoIterator<Item = S>,
    S: AsRef<OsStr>,
{
    let mut iter = args.into_iter();
    let _bin = iter.next();
    match iter.next() {
        None => true,
        Some(arg) => match arg.as_ref().to_str() {
            Some("tui" | "serve" | "api" | "mcp" | "mcp-server") => true,
            Some("daemon") => matches!(
                iter.next().as_ref().and_then(|s| s.as_ref().to_str()),
                Some("run")
            ),
            _ => false,
        },
    }
}

fn main() -> Result<()> {
    // Windows debug binaries default to a 1 MiB main-thread stack, which clap's
    // unoptimized Subcommand derive builder and deep async futures can exceed.
    let handle = std::thread::Builder::new()
        .name("onenote-main".to_string())
        .stack_size(8 * 1024 * 1024)
        .spawn(run_main)?;
    match handle.join() {
        Ok(res) => res,
        Err(panic_payload) => std::panic::resume_unwind(panic_payload),
    }
}

fn run_main() -> Result<()> {
    if let Some(res) = onenote_cli::cli::try_fast_help_or_version() {
        return res;
    }

    if needs_multi_thread_runtime(std::env::args_os()) {
        let rt = tokio::runtime::Builder::new_multi_thread()
            .enable_all()
            .build()?;
        rt.block_on(async {
            let _ = rustls::crypto::ring::default_provider().install_default();
            tokio::task::spawn_blocking(onenote_cli::updater::cleanup_stale_old_binary);
            onenote_cli::cli::run_from_env().await
        })
    } else {
        let _ = rustls::crypto::ring::default_provider().install_default();
        onenote_cli::updater::cleanup_stale_old_binary();
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;
        rt.block_on(onenote_cli::cli::run_from_env())
    }
}

#[cfg(test)]
mod tests {
    use super::needs_multi_thread_runtime;

    #[test]
    fn detects_multi_thread_commands_only_for_tui_serve_mcp_and_daemon_run() {
        assert!(needs_multi_thread_runtime(["onenote"]));
        assert!(needs_multi_thread_runtime(["onenote", "tui"]));
        assert!(needs_multi_thread_runtime(["onenote", "serve"]));
        assert!(needs_multi_thread_runtime(["onenote", "api"]));
        assert!(needs_multi_thread_runtime(["onenote", "mcp"]));
        assert!(needs_multi_thread_runtime(["onenote", "mcp-server"]));
        assert!(needs_multi_thread_runtime(["onenote", "daemon", "run"]));

        for sub in ["status", "stop", "start", "restart"] {
            assert!(
                !needs_multi_thread_runtime(["onenote", "daemon", sub]),
                "daemon {sub}"
            );
        }

        for cmd in [
            "login",
            "logout",
            "whoami",
            "doctor",
            "notebooks",
            "sections",
            "pages",
            "read",
            "create-notebook",
            "create-section",
            "create",
            "append",
            "update",
            "check",
            "delete",
            "search",
            "resource",
            "sync",
            "preview-file",
            "self-update",
            "--help",
            "-h",
            "--version",
            "-V",
        ] {
            assert!(!needs_multi_thread_runtime(["onenote", cmd]), "{cmd}");
        }
    }
}
