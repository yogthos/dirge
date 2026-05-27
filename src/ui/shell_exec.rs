//! Sandboxed shell execution used by `/sh` and the equivalent
//! event-driven shell command path. Extracted from `ui/mod.rs`.

use crate::sandbox::Sandbox;
use crate::ui::ansi;

pub(crate) async fn run_shell_command(cmd: &str, sandbox: &Sandbox) -> anyhow::Result<String> {
    let output = tokio::time::timeout(
        std::time::Duration::from_secs(120),
        sandbox.wrap_command(cmd).output(),
    )
    .await
    .map_err(|_| anyhow::anyhow!("Command timed out after 120s"))??;
    let stdout = String::from_utf8_lossy(&output.stdout).to_string();
    let stderr = String::from_utf8_lossy(&output.stderr).to_string();
    let mut result = stdout;
    if !stderr.is_empty() {
        if !result.is_empty() {
            result.push('\n');
        }
        result.push_str(&stderr);
    }
    let exit_code = output.status.code().unwrap_or(-1);
    if exit_code != 0 {
        result.push_str(&format!("\nExit code: {}", exit_code));
    }
    // Strip control characters before the output reaches the
    // chat buffer. Shell commands can emit ANSI escapes, BEL,
    // and other terminal controls that `write_line` would pass
    // straight to ratatui's buffer — and from there to the
    // terminal emulator.
    Ok(ansi::strip_escapes(&result, ansi::StripPolicy::KEEP_NEWLINE))
}
