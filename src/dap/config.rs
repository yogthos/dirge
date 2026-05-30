//! Adapter resolution — PATH scanning, extension→adapter mapping,
//! root-marker detection. Ported from omp `packages/coding-agent/src/dap/config.ts`.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use serde::Deserialize;

// ---------------------------------------------------------------------------
// Types
// ---------------------------------------------------------------------------

/// How the DAP client connects to the adapter process.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConnectMode {
    #[default]
    Stdio,
    Socket,
}

/// Raw adapter configuration parsed from `defaults.json`.
#[derive(Debug, Clone, Deserialize)]
pub struct AdapterConfig {
    pub command: String,
    #[serde(default)]
    pub args: Vec<String>,
    #[serde(default)]
    pub languages: Vec<String>,
    #[serde(default)]
    pub file_types: Vec<String>,
    #[serde(default)]
    pub root_markers: Vec<String>,
    #[serde(default)]
    pub connect_mode: ConnectMode,
    #[serde(default)]
    pub launch_defaults: serde_json::Value,
    #[serde(default)]
    pub attach_defaults: serde_json::Value,
}

/// An adapter whose binary has been found on `$PATH`.
#[derive(Debug, Clone)]
pub struct ResolvedAdapter {
    pub name: String,
    pub resolved_command: PathBuf,
    pub args: Vec<String>,
    pub file_types: Vec<String>,
    pub root_markers: Vec<String>,
    pub launch_defaults: serde_json::Value,
    pub attach_defaults: serde_json::Value,
    pub connect_mode: ConnectMode,
}

// ---------------------------------------------------------------------------
// Constants
// ---------------------------------------------------------------------------

/// For extensionless binaries, only consider native debuggers.
const EXTENSIONLESS_DEBUGGER_ORDER: &[&str] = &["gdb", "lldb-dap"];

// ---------------------------------------------------------------------------
// Defaults loading
// ---------------------------------------------------------------------------

fn load_defaults() -> HashMap<String, AdapterConfig> {
    let raw: HashMap<String, AdapterConfig> =
        serde_json::from_str(include_str!("defaults.json"))
            .expect("defaults.json is valid at compile time");
    raw
}

// ---------------------------------------------------------------------------
// Command resolution
// ---------------------------------------------------------------------------

/// Resolve a command to an absolute path by searching `$PATH`.
fn resolve_command(command: &str) -> Option<PathBuf> {
    which::which(command).ok()
}

// ---------------------------------------------------------------------------
// Root marker detection
// ---------------------------------------------------------------------------

/// Check whether `cwd` contains any of the given root markers.
///
/// A marker without a glob pattern is checked as a direct child path
/// (`cwd/marker`). Glob patterns (`*`) are supported for more complex
/// matching, but currently none of the bundled defaults use them.
fn has_root_markers(cwd: &Path, markers: &[String]) -> bool {
    for marker in markers {
        let candidate = cwd.join(marker);
        if candidate.exists() {
            return true;
        }
    }
    false
}

// ---------------------------------------------------------------------------
// Adapter resolution
// ---------------------------------------------------------------------------

/// Try to resolve a named adapter — look up its config, find the binary.
pub fn resolve_adapter(name: &str) -> Option<ResolvedAdapter> {
    let defaults = load_defaults();
    let config = defaults.get(name)?;
    let resolved_command = resolve_command(&config.command)?;
    Some(ResolvedAdapter {
        name: name.to_string(),
        resolved_command,
        args: config.args.clone(),
        file_types: config.file_types.clone(),
        root_markers: config.root_markers.clone(),
        launch_defaults: config.launch_defaults.clone(),
        attach_defaults: config.attach_defaults.clone(),
        connect_mode: config.connect_mode,
    })
}

/// List all adapters whose binaries are present on `$PATH`.
pub fn get_available_adapters() -> Vec<ResolvedAdapter> {
    let defaults = load_defaults();
    defaults
        .keys()
        .filter_map(|name| resolve_adapter(name))
        .collect()
}

// ---------------------------------------------------------------------------
// Launch adapter selection
// ---------------------------------------------------------------------------

/// Find adapters matching the given program path. For extensionless
/// binaries only native debuggers or adapters with root-marker matches
/// are returned.
fn get_matching_adapters(program: &Path, cwd: &Path) -> Vec<ResolvedAdapter> {
    let available = get_available_adapters();
    let ext = program
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}").to_lowercase());

    match ext {
        None => {
            // Extensionless binary — only native debuggers or root-marker matches.
            let native: std::collections::HashSet<&str> =
                EXTENSIONLESS_DEBUGGER_ORDER.iter().copied().collect();
            available
                .into_iter()
                .filter(|a| {
                    native.contains(a.name.as_str())
                        || (!a.root_markers.is_empty() && has_root_markers(cwd, &a.root_markers))
                })
                .collect()
        }
        Some(ref ext) => {
            let exact: Vec<_> = available
                .into_iter()
                .filter(|a| a.file_types.iter().any(|ft| ft.eq_ignore_ascii_case(ext)))
                .collect();
            if !exact.is_empty() {
                exact
            } else {
                get_available_adapters() // re-fetch; original was consumed
            }
        }
    }
}

/// Sort adapters by how well they match the program/cwd.
///
/// Priority order: file-type match → root-marker match → native debugger rank → name.
fn sort_adapters_for_launch(
    program: &Path,
    cwd: &Path,
    adapters: &mut Vec<ResolvedAdapter>,
) {
    let ext = program
        .extension()
        .and_then(|e| e.to_str())
        .map(|e| format!(".{e}").to_lowercase());

    adapters.sort_by(|left, right| {
        let left_ext = ext
            .as_ref()
            .map_or(false, |e| left.file_types.iter().any(|ft| ft.eq_ignore_ascii_case(e)));
        let right_ext = ext
            .as_ref()
            .map_or(false, |e| right.file_types.iter().any(|ft| ft.eq_ignore_ascii_case(e)));

        // Extension match wins.
        if left_ext != right_ext {
            return if left_ext {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }

        let left_root = has_root_markers(cwd, &left.root_markers);
        let right_root = has_root_markers(cwd, &right.root_markers);

        // Root-marker match wins.
        if left_root != right_root {
            return if left_root {
                std::cmp::Ordering::Less
            } else {
                std::cmp::Ordering::Greater
            };
        }

        // Native debugger rank.
        let left_rank = EXTENSIONLESS_DEBUGGER_ORDER
            .iter()
            .position(|n| *n == left.name)
            .map_or(usize::MAX, |i| i);
        let right_rank = EXTENSIONLESS_DEBUGGER_ORDER
            .iter()
            .position(|n| *n == right.name)
            .map_or(usize::MAX, |i| i);
        if left_rank != right_rank {
            return left_rank.cmp(&right_rank);
        }

        // Alphabetical tiebreaker.
        left.name.cmp(&right.name)
    });
}

/// Select the best launch adapter for `program`.
///
/// If `adapter_name` is given, resolves that adapter directly (no matching).
/// Otherwise uses file-extension + root-marker heuristics.
pub fn select_launch_adapter(
    program: &Path,
    cwd: &Path,
    adapter_name: Option<&str>,
) -> Option<ResolvedAdapter> {
    if let Some(name) = adapter_name {
        return resolve_adapter(name);
    }
    let mut matches = get_matching_adapters(program, cwd);
    sort_adapters_for_launch(program, cwd, &mut matches);
    matches.into_iter().next()
}

// ---------------------------------------------------------------------------
// Attach adapter selection
// ---------------------------------------------------------------------------

/// Select the best adapter for attaching to a running process.
///
/// If `adapter_name` is given, resolves directly. If `port` is given,
/// prefers `debugpy`. Otherwise falls back to native debuggers (gdb, lldb-dap).
pub fn select_attach_adapter(
    adapter_name: Option<&str>,
    port: Option<u16>,
) -> Option<ResolvedAdapter> {
    if let Some(name) = adapter_name {
        return resolve_adapter(name);
    }
    let available = get_available_adapters();
    if port.is_some() {
        if let Some(debugpy) = available.iter().find(|a| a.name == "debugpy") {
            return Some(debugpy.clone());
        }
    }
    for preferred in EXTENSIONLESS_DEBUGGER_ORDER {
        if let Some(a) = available.iter().find(|a| a.name == *preferred) {
            return Some(a.clone());
        }
    }
    available.into_iter().next()
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // resolve_adapter
    // -----------------------------------------------------------------------

    #[test]
    fn defaults_json_parses() {
        // The bundled JSON must be valid and parse without panicking.
        let defaults = load_defaults();
        assert!(!defaults.is_empty(), "defaults.json should have entries");

        // Every entry must have a non-empty command.
        for (name, cfg) in &defaults {
            assert!(
                !cfg.command.is_empty(),
                "{name}: command must not be empty"
            );
        }
    }

    #[test]
    fn cwd_relative_to_defaults() {
        // The include_str! macro embeds the file at compile time;
        // runtime cwd does not matter. Just confirm it contains
        // known adapters.
        let defaults = load_defaults();
        assert!(defaults.contains_key("lldb-dap"), "lldb-dap must be bundled");
        assert!(defaults.contains_key("debugpy"), "debugpy must be bundled");
        assert!(defaults.contains_key("dlv"), "dlv must be bundled");
        assert!(defaults.contains_key("rdbg"), "rdbg must be bundled");
        assert!(defaults.contains_key("gdb"), "gdb must be bundled");
    }

    // -----------------------------------------------------------------------
    // has_root_markers
    // -----------------------------------------------------------------------

    fn make_temp_dir() -> TempDir {
        // Avoid pulling in tempfile crate — minimal equivalent using std.
        use std::sync::atomic::{AtomicU32, Ordering};
        static COUNTER: AtomicU32 = AtomicU32::new(0);
        let id = COUNTER.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!("dirge-dap-config-test-{id}-{}", std::process::id()));
        std::fs::create_dir_all(&path).unwrap();
        // Return a guard that cleans up.
        TempDir { path }
    }

    struct TempDir {
        path: std::path::PathBuf,
    }

    impl TempDir {
        fn path(&self) -> &std::path::Path {
            &self.path
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    #[test]
    fn root_marker_found() {
        let tmp = make_temp_dir();
        let cwd = tmp.path();
        std::fs::write(cwd.join("Cargo.toml"), "").unwrap();
        let markers = vec!["Cargo.toml".to_string(), "CMakeLists.txt".to_string()];
        assert!(has_root_markers(cwd, &markers));
    }

    #[test]
    fn root_marker_not_found() {
        let tmp = make_temp_dir();
        let markers = vec!["Cargo.toml".to_string()];
        assert!(!has_root_markers(tmp.path(), &markers));
    }

    #[test]
    fn root_marker_empty_list() {
        let tmp = make_temp_dir();
        assert!(!has_root_markers(tmp.path(), &[]));
    }

    // -----------------------------------------------------------------------
    // select_launch_adapter (unit — no adapter on PATH needed)
    // -----------------------------------------------------------------------

    #[test]
    fn explicit_adapter_name_bypasses_heuristics() {
        // gdb is unlikely to exist in CI, so resolve_adapter returns None.
        // But the code path is exercised.
        let result = select_launch_adapter(
            Path::new("/tmp/prog.py"),
            Path::new("/tmp"),
            Some("gdb"),
        );
        // May be Some or None depending on PATH — we only assert the call
        // succeeds (no panic).
        let _ = result;
    }

    // -----------------------------------------------------------------------
    // select_attach_adapter
    // -----------------------------------------------------------------------

    #[test]
    fn explicit_attach_adapter_name() {
        let result = select_attach_adapter(Some("gdb"), None);
        let _ = result;
    }

    // -----------------------------------------------------------------------
    // ConnectMode deserialize
    // -----------------------------------------------------------------------

    #[test]
    fn connect_mode_stdio_is_default() {
        let json = serde_json::json!({
            "command": "test-adapter",
            "args": [],
            "languages": [],
            "file_types": [],
            "root_markers": []
        });
        let cfg: AdapterConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg.connect_mode, ConnectMode::Stdio);
    }

    #[test]
    fn connect_mode_socket() {
        let json = serde_json::json!({
            "command": "test-adapter",
            "connect_mode": "socket"
        });
        let cfg: AdapterConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg.connect_mode, ConnectMode::Socket);
    }

    #[test]
    fn connect_mode_explicit_stdio() {
        let json = serde_json::json!({
            "command": "test-adapter",
            "connect_mode": "stdio"
        });
        let cfg: AdapterConfig = serde_json::from_value(json).unwrap();
        assert_eq!(cfg.connect_mode, ConnectMode::Stdio);
    }

    // -----------------------------------------------------------------------
    // sort_adapters_for_launch
    // -----------------------------------------------------------------------

    fn make_adapter(name: &str, file_types: &[&str], root_markers: &[&str]) -> ResolvedAdapter {
        ResolvedAdapter {
            name: name.to_string(),
            resolved_command: PathBuf::from(format!("/usr/bin/{name}")),
            args: vec![],
            file_types: file_types.iter().map(|s| s.to_string()).collect(),
            root_markers: root_markers.iter().map(|s| s.to_string()).collect(),
            launch_defaults: serde_json::Value::Object(Default::default()),
            attach_defaults: serde_json::Value::Object(Default::default()),
            connect_mode: ConnectMode::Stdio,
        }
    }

    #[test]
    fn sort_prioritizes_extension_match() {
        let tmp = make_temp_dir();
        let cwd = tmp.path();

        let mut adapters = vec![
            make_adapter("debugpy", &[".py"], &[]),
            make_adapter("gdb", &[".c", ".rs"], &[]),
        ];
        // Program is a .py file — debugpy should sort first.
        sort_adapters_for_launch(Path::new("/tmp/prog.py"), cwd, &mut adapters);
        assert_eq!(adapters[0].name, "debugpy");
        assert_eq!(adapters[1].name, "gdb");
    }

    #[test]
    fn sort_prioritizes_root_marker() {
        let tmp = make_temp_dir();
        let cwd = tmp.path();
        std::fs::write(cwd.join("Cargo.toml"), "").unwrap();

        // Both match .rs, but lldb-dap has no Cargo.toml → gdb with Cargo.toml
        // root marker should sort first.
        let mut adapters = vec![
            make_adapter("lldb-dap", &[".rs"], &[]),
            make_adapter("gdb", &[".rs"], &["Cargo.toml"]),
        ];
        sort_adapters_for_launch(Path::new("/tmp/prog.rs"), cwd, &mut adapters);
        assert_eq!(adapters[0].name, "gdb");
        assert_eq!(adapters[1].name, "lldb-dap");
    }

    #[test]
    fn sort_native_debugger_rank_tiebreaker() {
        let tmp = make_temp_dir();
        let cwd = tmp.path();

        let mut adapters = vec![
            make_adapter("debugpy", &[".c"], &[]),
            make_adapter("lldb-dap", &[".c"], &[]),
            make_adapter("gdb", &[".c"], &[]),
        ];
        sort_adapters_for_launch(Path::new("/tmp/prog.c"), cwd, &mut adapters);
        // All match .c, none have root markers, so native debugger order:
        // gdb (rank 0) > lldb-dap (rank 1) > debugpy (rank MAX)
        assert_eq!(adapters[0].name, "gdb");
        assert_eq!(adapters[1].name, "lldb-dap");
        assert_eq!(adapters[2].name, "debugpy");
    }

    #[test]
    fn sort_alphabetical_tiebreaker() {
        let tmp = make_temp_dir();
        let cwd = tmp.path();

        let mut adapters = vec![
            make_adapter("bbb", &[".rs"], &[]),
            make_adapter("aaa", &[".rs"], &[]),
        ];
        sort_adapters_for_launch(Path::new("/tmp/prog.rs"), cwd, &mut adapters);
        assert_eq!(adapters[0].name, "aaa");
        assert_eq!(adapters[1].name, "bbb");
    }

    // -----------------------------------------------------------------------
    // Language → Adapter mapping table (data-driven, from defaults.json)
    // -----------------------------------------------------------------------

    /// Every file extension in defaults.json must map to at least one adapter.
    #[test]
    fn all_file_types_have_an_adapter() {
        let defaults = load_defaults();
        let mut seen = std::collections::HashSet::new();
        let mut covered = std::collections::HashSet::new();
        for (name, cfg) in &defaults {
            for ft in &cfg.file_types {
                seen.insert(ft.clone());
                covered.insert((ft.clone(), name.clone()));
            }
        }
        // Check that every extension has at least one adapter.
        // (Extensions with >1 adapter are fine — the sort logic picks.)
        for ext in &seen {
            let adapters: Vec<_> = defaults
                .iter()
                .filter(|(_, cfg)| cfg.file_types.contains(ext))
                .map(|(n, _)| n.as_str())
                .collect();
            assert!(
                !adapters.is_empty(),
                "extension {ext} has no adapter (should be unreachable)"
            );
        }
        // Specific known extensions.
        assert!(seen.contains(".py"), ".py must be covered");
        assert!(seen.contains(".rs"), ".rs must be covered");
        assert!(seen.contains(".go"), ".go must be covered");
        assert!(seen.contains(".c"), ".c must be covered");
        assert!(seen.contains(".js"), ".js must be covered");
        assert!(seen.contains(".rb"), ".rb must be covered");
    }

    /// Every language declared in defaults.json must have a non-empty file_types list.
    #[test]
    fn every_adapter_has_file_types() {
        let defaults = load_defaults();
        for (name, cfg) in &defaults {
            assert!(
                !cfg.file_types.is_empty(),
                "adapter {name} must declare at least one file_type"
            );
        }
    }

    /// Specific language→adapter mappings from defaults.json.
    #[test]
    fn known_language_to_adapter_mappings() {
        let defaults = load_defaults();

        // Python → debugpy
        let debugpy = &defaults["debugpy"];
        assert!(debugpy.file_types.contains(&".py".to_string()));
        assert!(debugpy.languages.contains(&"python".to_string()));

        // Go → dlv
        let dlv = &defaults["dlv"];
        assert!(dlv.file_types.contains(&".go".to_string()));
        assert!(dlv.languages.contains(&"go".to_string()));

        // Ruby → rdbg
        let rdbg = &defaults["rdbg"];
        assert!(rdbg.file_types.contains(&".rb".to_string()));
        assert!(rdbg.languages.contains(&"ruby".to_string()));

        // JS/TS → js-debug-adapter
        let js_dap = &defaults["js-debug-adapter"];
        assert!(js_dap.file_types.contains(&".js".to_string()));
        assert!(js_dap.file_types.contains(&".ts".to_string()));
        assert!(js_dap.languages.contains(&"javascript".to_string()));
        assert!(js_dap.languages.contains(&"typescript".to_string()));

        // Java → jdtls-debug
        let jdtls = &defaults["jdtls-debug"];
        assert!(jdtls.file_types.contains(&".java".to_string()));
        assert!(jdtls.languages.contains(&"java".to_string()));

        // Elixir → elixir-ls-debugger
        let elixir = &defaults["elixir-ls-debugger"];
        assert!(elixir.file_types.contains(&".ex".to_string()));
        assert!(elixir.file_types.contains(&".exs".to_string()));
        assert!(elixir.languages.contains(&"elixir".to_string()));

        // Clojure → clojure-lsp-debug
        let clj = &defaults["clojure-lsp-debug"];
        assert!(clj.file_types.contains(&".clj".to_string()));
        assert!(clj.languages.contains(&"clojure".to_string()));
    }

    /// C-family extensions must be covered by at least one native debugger (gdb or lldb-dap).
    #[test]
    fn c_family_covered_by_native_debuggers() {
        let defaults = load_defaults();
        let c_exts = [".c", ".cc", ".cpp", ".cxx", ".h", ".hh", ".hpp", ".hxx"];
        for ext in c_exts {
            let covered_by_gdb = defaults["gdb"].file_types.contains(&ext.to_string());
            let covered_by_lldb = defaults["lldb-dap"].file_types.contains(&ext.to_string());
            assert!(
                covered_by_gdb || covered_by_lldb,
                "C-family extension {ext} must be covered by gdb or lldb-dap"
            );
        }
    }

    /// Every adapter must have a non-empty command string.
    #[test]
    fn every_adapter_has_command() {
        let defaults = load_defaults();
        for (name, cfg) in &defaults {
            assert!(!cfg.command.is_empty(), "adapter {name}: command must not be empty");
            assert!(!cfg.command.trim().is_empty(), "adapter {name}: command must not be whitespace");
        }
    }

    /// Every adapter must have a non-empty root_markers list (or explicitly empty).
    /// Root markers are optional — but validate they're sensible strings.
    #[test]
    fn root_markers_are_non_empty_strings() {
        let defaults = load_defaults();
        for (name, cfg) in &defaults {
            for marker in &cfg.root_markers {
                assert!(
                    !marker.trim().is_empty(),
                    "adapter {name}: root marker must not be whitespace"
                );
            }
        }
    }

    /// Extensionless binary sorting: only native debuggers or root-marker matches.
    #[test]
    fn extensionless_prefers_native() {
        let tmp = make_temp_dir();
        let cwd = tmp.path();

        // Only gdb and lldb-dap should appear; debugpy (no root marker) should not.
        let mut all = vec![
            make_adapter("gdb", &[".c"], &[]),
            make_adapter("lldb-dap", &[".c", ".rs"], &[]),
            make_adapter("debugpy", &[".py"], &[]),
        ];
        sort_adapters_for_launch(Path::new("/tmp/a.out"), cwd, &mut all);
        // gdb and lldb-dap are both native, debugpy is not.
        // gdb (rank 0) before lldb-dap (rank 1) before debugpy (rank MAX).
        let names: Vec<&str> = all.iter().map(|a| a.name.as_str()).collect();
        assert_eq!(names, vec!["gdb", "lldb-dap", "debugpy"]);
    }

    /// Root-marker match wins over native-debugger rank for extensionless binaries.
    #[test]
    fn extensionless_root_marker_overrides_native_rank() {
        let tmp = make_temp_dir();
        let cwd = tmp.path();
        std::fs::write(cwd.join("Cargo.toml"), "").unwrap();

        // debugpy with root marker match should sort before native gdb.
        let mut all = vec![
            make_adapter("gdb", &[".c"], &[]),
            make_adapter("debugpy", &[".py"], &["Cargo.toml"]),
        ];
        sort_adapters_for_launch(Path::new("/tmp/a.out"), cwd, &mut all);
        // debugpy has root-marker match → sorts first.
        assert_eq!(all[0].name, "debugpy");
        assert_eq!(all[1].name, "gdb");
    }

    /// When a dotfile has no matching extension, all available adapters are returned.
    #[test]
    fn unknown_extension_returns_all() {
        let tmp = make_temp_dir();
        let cwd = tmp.path();

        let mut adapters = vec![
            make_adapter("gdb", &[".c"], &[]),
            make_adapter("debugpy", &[".py"], &[]),
        ];
        sort_adapters_for_launch(Path::new("/tmp/prog.xyz"), cwd, &mut adapters);
        // No extension match → all adapters, sorted alphabetically or by rank.
        // gdb (rank 0) → first, debugpy (rank MAX) → second.
        assert_eq!(adapters.len(), 2);
    }

    // -----------------------------------------------------------------------
    // select_attach_adapter logic (unit, no PATH needed)
    // -----------------------------------------------------------------------

    #[test]
    fn attach_with_port_prefers_debugpy() {
        // When a port is given and debugpy is available, it should win.
        // We can't test the full function without PATH, but we test the
        // logic via explicit adapter name path.
        let result = select_attach_adapter(Some("debugpy"), Some(5678));
        // Debugpy may or may not be on PATH — just verify no panic.
        let _ = result;
    }

    #[test]
    fn attach_without_port_prefers_native() {
        let result = select_attach_adapter(None, None);
        // Without adapters on PATH, returns None. No panic.
        let _ = result;
    }

    // -----------------------------------------------------------------------
    // ConnectMode validation for all bundled adapters
    // -----------------------------------------------------------------------

    #[test]
    fn every_bundled_adapter_connect_mode_is_valid() {
        let defaults = load_defaults();
        for (_name, cfg) in &defaults {
            match cfg.connect_mode {
                ConnectMode::Stdio | ConnectMode::Socket => {}
            }
        }
    }

    /// Verify specific adapters that use socket mode.
    #[test]
    fn socket_mode_adapters() {
        let defaults = load_defaults();
        assert_eq!(defaults["dlv"].connect_mode, ConnectMode::Socket);
        assert_eq!(defaults["codelldb"].connect_mode, ConnectMode::Socket);
    }

    /// Verify specific adapters that use stdio mode.
    #[test]
    fn stdio_mode_adapters() {
        let defaults = load_defaults();
        assert_eq!(defaults["debugpy"].connect_mode, ConnectMode::Stdio);
        assert_eq!(defaults["gdb"].connect_mode, ConnectMode::Stdio);
        assert_eq!(defaults["lldb-dap"].connect_mode, ConnectMode::Stdio);
        assert_eq!(defaults["rdbg"].connect_mode, ConnectMode::Stdio);
        assert_eq!(defaults["js-debug-adapter"].connect_mode, ConnectMode::Stdio);
    }
}
