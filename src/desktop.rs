//! Existing hierarchy mutations through OneNote for Windows.

use std::{path::PathBuf, process::Stdio, time::Duration};

use anyhow::{bail, Context, Result};
use serde_json::{json, Value};
use tokio::{io::AsyncWriteExt, process::Command, time::timeout};

pub async fn set_page_level(page_id: &str, level: i64) -> Result<Value> {
    if !(0..=2).contains(&level) {
        bail!("Desktop page level must be 0, 1, or 2.");
    }
    run_edit(page_id, json!({ "operation": "level", "level": level })).await
}

pub async fn rename_section(section_id: &str, name: &str) -> Result<Value> {
    let name = name.trim();
    if name.is_empty() {
        bail!("The new section name is required.");
    }
    run_edit(section_id, json!({ "operation": "rename", "name": name })).await
}

async fn run_edit(id: &str, mut request: Value) -> Result<Value> {
    use std::os::windows::process::CommandExt;

    let id = id.trim();
    let Some((guid, suffix)) = id.strip_prefix('{').and_then(|s| s.split_once('}')) else {
        bail!("Provide the complete OneNote desktop ID from GetHierarchy.");
    };
    if uuid::Uuid::parse_str(guid).is_err() || !suffix.starts_with('{') || !suffix.ends_with('}') {
        bail!("Provide the complete OneNote desktop ID from GetHierarchy.");
    }
    request["id"] = json!(id);
    let shell = PathBuf::from(
        std::env::var_os("SystemRoot").context("Windows system path is unavailable")?,
    )
    .join("System32/WindowsPowerShell/v1.0/powershell.exe");
    let mut cmd = std::process::Command::new(shell);
    cmd.creation_flags(0x08000000); // CREATE_NO_WINDOW
    cmd.args(["-NoLogo", "-NoProfile", "-NonInteractive", "-Sta", "-Command"])
        .arg(include_str!("desktop.ps1"))
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    let mut child = Command::from(cmd)
        .kill_on_drop(true)
        .spawn()
        .context("Could not start Windows PowerShell for OneNote desktop")?;
    child
        .stdin
        .take()
        .context("OneNote desktop input pipe was unavailable")?
        .write_all(&serde_json::to_vec(&request)?)
        .await?;
    let output = timeout(Duration::from_secs(45), child.wait_with_output())
        .await
        .context("OneNote desktop timed out. Read the target before retrying.")??;
    if !output.status.success() {
        let message = String::from_utf8_lossy(&output.stderr);
        bail!("OneNote desktop: {}", message.trim());
    }
    serde_json::from_slice(&output.stdout).context("OneNote desktop returned invalid JSON")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn rejects_invalid_level_and_graph_ids_before_running_desktop_app() {
        assert!(set_page_level("me|page", 3)
            .await
            .unwrap_err()
            .to_string()
            .contains("must be 0, 1, or 2"));
        assert!(set_page_level("me|page", 1)
            .await
            .unwrap_err()
            .to_string()
            .contains("complete OneNote desktop ID"));
    }
}
