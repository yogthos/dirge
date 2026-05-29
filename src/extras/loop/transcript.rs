use std::path::PathBuf;

use chrono::Utc;

fn transcript_dir(session_id: &str) -> PathBuf {
    // Route through the shared data-dir base so `DIRGE_DATA_DIR` is
    // honored here too — previously this used `dirs::data_dir()`
    // directly, so an override relocated sessions but NOT loop
    // transcripts (dirge-f8oe).
    crate::session::storage::dirs_path()
        .join("loops")
        .join(session_id)
}

pub fn save_iteration(
    session_id: &str,
    iteration: u32,
    prompt: &str,
    response: &str,
    validation_output: Option<&str>,
    summary: &str,
) -> anyhow::Result<()> {
    let dir = transcript_dir(session_id);
    std::fs::create_dir_all(&dir)?;

    let record = serde_json::json!({
        "iteration": iteration,
        "timestamp": Utc::now().to_rfc3339(),
        "prompt": prompt,
        "response": response,
        "validation_output": validation_output,
        "summary": summary,
    });

    let path = dir.join(format!("iter-{:04}.json", iteration));
    std::fs::write(&path, serde_json::to_string_pretty(&record)?)?;
    Ok(())
}
