use std::collections::HashMap;
use std::path::PathBuf;

use include_dir::{Dir, include_dir};

static EMBEDDED: Dir = include_dir!("$CARGO_MANIFEST_DIR/prompts");

pub fn global_prompts_dir() -> PathBuf {
    crate::session::storage::config_path().join("prompts")
}

/// Load all prompts available to the session, with merge order:
///
///   embedded  (lowest precedence — only fills gaps)
///     ↓
///   global    (`~/.config/dirge/prompts/`)
///     ↓
///   local     (`./prompts/`, highest precedence)
///
/// Implementation contract (audit H14): embedded uses `or_insert_with`
/// (soft) so a global / local prompt of the same name overrides it;
/// global and local use `insert` (hard, last-write-wins). The three
/// blocks below MUST stay in this order — swapping them would
/// silently invert precedence. New tiers (e.g. workspace-scoped)
/// should slot in by precedence with the same soft-then-hard pattern.
pub fn load() -> HashMap<String, String> {
    let mut prompts: HashMap<String, String> = HashMap::new();

    for file in EMBEDDED.files() {
        if file.path().extension().is_some_and(|e| e == "md")
            && let Some(name) = file.path().file_stem().and_then(|s| s.to_str())
            && let Some(content) = file.contents_utf8()
        {
            prompts
                .entry(name.to_string())
                .or_insert_with(|| content.to_string());
        }
    }

    let global = global_prompts_dir();
    if global.exists()
        && let Ok(entries) = std::fs::read_dir(&global)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md")
                && let Some(name) = path.file_stem().and_then(|s| s.to_str())
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                prompts.insert(name.to_string(), content);
            }
        }
    }

    let local = PathBuf::from("prompts");
    if local.exists()
        && let Ok(entries) = std::fs::read_dir(&local)
    {
        for entry in entries.flatten() {
            let path = entry.path();
            if path.extension().is_some_and(|e| e == "md")
                && let Some(name) = path.file_stem().and_then(|s| s.to_str())
                && let Ok(content) = std::fs::read_to_string(&path)
            {
                prompts.insert(name.to_string(), content);
            }
        }
    }

    prompts
}

pub fn ensure_global() -> anyhow::Result<()> {
    let dir = global_prompts_dir();
    if !dir.exists() {
        std::fs::create_dir_all(&dir)?;
        copy_embedded(&dir)?;
    }
    Ok(())
}

pub fn regen() -> anyhow::Result<()> {
    let dir = global_prompts_dir();
    std::fs::create_dir_all(&dir)?;
    copy_embedded(&dir)
}

fn copy_embedded(dest: &PathBuf) -> anyhow::Result<()> {
    for file in EMBEDDED.files() {
        if let Some(name) = file.path().file_name().and_then(|s| s.to_str()) {
            let dest_path = dest.join(name);
            if let Some(content) = file.contents_utf8() {
                std::fs::write(&dest_path, content)?;
            }
        }
    }
    Ok(())
}
