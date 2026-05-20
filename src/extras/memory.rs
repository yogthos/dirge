use std::path::{Path, PathBuf};

fn home_dir() -> PathBuf {
    std::env::var("HOME")
        .map(PathBuf::from)
        .unwrap_or_else(|_| PathBuf::from("."))
}

pub fn memory_dir(cwd: &Path) -> PathBuf {
    let project_id = project_id(cwd);
    let base = home_dir().join(".dirge").join("memories");
    base.join(project_id)
}

fn project_id(cwd: &Path) -> String {
    let root = find_git_root(cwd).unwrap_or_else(|| cwd.to_path_buf());
    let canonical = std::fs::canonicalize(&root).unwrap_or_else(|_| root.clone());
    let basename = root
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or("unknown");

    let path_key = canonical
        .to_string_lossy()
        .replace(['/', '\\', ':'], "-")
        .trim_matches('-')
        .to_string();

    format!("{}-{}", basename, path_key)
}

fn find_git_root(cwd: &Path) -> Option<PathBuf> {
    let mut current = cwd.to_path_buf();
    loop {
        if current.join(".git").is_dir() {
            return Some(current);
        }
        if !current.pop() {
            return None;
        }
    }
}

pub fn safe_resolve(dir: &Path, path: &str) -> Result<PathBuf, String> {
    let normalized = path.replace('\\', "/");
    let mut resolved = PathBuf::from(dir);
    for component in normalized.split('/') {
        if component.is_empty() || component == "." {
            continue;
        }
        if component == ".." {
            return Err("Path traversal via '..' is not allowed".to_string());
        }
        resolved.push(component);
    }
    if normalized.starts_with('/') {
        return Err("Absolute paths are not allowed".to_string());
    }
    let dir_canon = dir.canonicalize().unwrap_or_else(|_| dir.to_path_buf());
    let resolved_canon = resolved
        .parent()
        .and_then(|p| p.canonicalize().ok())
        .map(|p| p.join(resolved.file_name().unwrap_or_default()))
        .unwrap_or_else(|| resolved.clone());
    if !resolved_canon.starts_with(&dir_canon) {
        return Err("Path escapes memory directory".to_string());
    }
    if let Ok(meta) = std::fs::symlink_metadata(&resolved) {
        if meta.file_type().is_symlink() {
            return Err("Symlinks are not allowed in memory directory".to_string());
        }
    }
    Ok(resolved)
}

pub fn list_files(dir: &Path) -> Result<Vec<String>, String> {
    std::fs::create_dir_all(dir).map_err(|e| format!("Failed to create memory dir: {e}"))?;
    let mut files = Vec::new();
    if let Ok(entries) = std::fs::read_dir(dir) {
        for entry in entries.flatten() {
            if entry.file_type().map(|t| t.is_file()).unwrap_or(false) {
                if let Some(name) = entry.file_name().to_str() {
                    files.push(name.to_string());
                }
            }
        }
    }
    files.sort();
    Ok(files)
}

pub fn read_file(dir: &Path, path: &str) -> Result<String, String> {
    let resolved = safe_resolve(dir, path)?;
    if !resolved.is_file() {
        return Err(format!("Memory '{}' not found", path));
    }
    // 1 MB cap with truncation marker — same reasoning as the skill
    // size cap. Memories are meant to be terse notes; multi-MB
    // memories almost certainly need a different storage mechanism
    // (and would explode LLM context if loaded raw).
    const MEMORY_MAX_BYTES: usize = 1024 * 1024;
    let raw =
        std::fs::read_to_string(&resolved).map_err(|e| format!("Failed to read memory: {e}"))?;
    if raw.len() > MEMORY_MAX_BYTES {
        let mut truncated: String = raw.chars().take(MEMORY_MAX_BYTES).collect();
        truncated.push_str("\n\n…[truncated: memory exceeds 1 MB cap]");
        Ok(truncated)
    } else {
        Ok(raw)
    }
}

pub fn write_file(dir: &Path, path: &str, content: &str) -> Result<(), String> {
    let resolved = safe_resolve(dir, path)?;
    if let Some(parent) = resolved.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("Failed to create directory: {e}"))?;
    }
    std::fs::write(&resolved, content).map_err(|e| format!("Failed to write memory: {e}"))
}

pub fn delete_file(dir: &Path, path: &str) -> Result<(), String> {
    let resolved = safe_resolve(dir, path)?;
    if !resolved.is_file() {
        return Err(format!("Memory '{}' not found", path));
    }
    std::fs::remove_file(&resolved).map_err(|e| format!("Failed to delete memory: {e}"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_safe_resolve_rejects_parent_traversal() {
        let dir = PathBuf::from("/tmp/test-memories");
        assert!(safe_resolve(&dir, "../escape").is_err());
    }

    #[test]
    fn test_safe_resolve_rejects_absolute() {
        let dir = PathBuf::from("/tmp/test-memories");
        assert!(safe_resolve(&dir, "/etc/passwd").is_err());
    }

    #[test]
    fn test_safe_resolve_allows_normal_path() {
        let dir = PathBuf::from("/tmp/test-memories");
        let result = safe_resolve(&dir, "my-memory.md");
        assert!(result.is_ok());
    }

    #[test]
    fn test_write_and_read_file() {
        let dir = std::env::temp_dir().join("dirge-mem-test");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write_file(&dir, "test.md", "hello world").unwrap();
        let content = read_file(&dir, "test.md").unwrap();
        assert_eq!(content, "hello world");

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_list_files() {
        let dir = std::env::temp_dir().join("dirge-mem-test-list");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write_file(&dir, "a.md", "a").unwrap();
        write_file(&dir, "b.md", "b").unwrap();

        let files = list_files(&dir).unwrap();
        assert_eq!(files, vec!["a.md", "b.md"]);

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[test]
    fn test_delete_file() {
        let dir = std::env::temp_dir().join("dirge-mem-test-del");
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        write_file(&dir, "todelete.md", "x").unwrap();
        assert!(delete_file(&dir, "todelete.md").is_ok());
        assert!(read_file(&dir, "todelete.md").is_err());

        let _ = std::fs::remove_dir_all(&dir);
    }
}
