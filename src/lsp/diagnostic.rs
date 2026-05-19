//! Diagnostic pretty-printing + report blocks for tool output.
//!
//! Mirrors opencode's `lsp/diagnostic.ts`: only ERROR severity surfaces in
//! the report, max 20 per file (overflow becomes "... and N more"), wrapped
//! in `<diagnostics file="...">` tags that the LLM recognizes as
//! out-of-band tool context.
//!
//! The full agent-facing block (current file + capped other files) is
//! built by [`build_report_block`]. That's what `write` / `edit` tools
//! append to their `Ok(String)` output in Phase 6.

use std::collections::HashMap;
use std::path::{Path, PathBuf};

use lsp_types::{Diagnostic, DiagnosticSeverity};

/// Max diagnostics rendered per file before truncating with a "... and N
/// more" footer. Bounds blast radius for a generated file with hundreds of
/// errors.
const MAX_PER_FILE: usize = 20;

/// Max additional files surfaced in the project-wide section beyond the
/// just-edited file. Stops a single edit from dumping the entire project's
/// diagnostic state into the agent's context on each turn.
const MAX_PROJECT_DIAGNOSTICS_FILES: usize = 5;

/// One-line human-readable rendering of an LSP diagnostic.
/// Converts LSP's 0-based line/character to the 1-based form editors and
/// agents typically display.
pub fn pretty(d: &Diagnostic) -> String {
    let severity = match d.severity {
        Some(DiagnosticSeverity::ERROR) => "ERROR",
        Some(DiagnosticSeverity::WARNING) => "WARN",
        Some(DiagnosticSeverity::INFORMATION) => "INFO",
        Some(DiagnosticSeverity::HINT) => "HINT",
        _ => "ERROR",
    };
    let line = d.range.start.line.saturating_add(1);
    let col = d.range.start.character.saturating_add(1);
    format!("{severity} [{line}:{col}] {}", d.message)
}

/// Render a single file's diagnostics as a `<diagnostics>` block. Only
/// ERROR severity is included — warnings and hints would be noise. Returns
/// `None` when there are zero errors (so callers can skip emitting a
/// section heading).
pub fn report(file: &str, issues: &[Diagnostic]) -> Option<String> {
    let errors: Vec<&Diagnostic> = issues
        .iter()
        .filter(|d| d.severity == Some(DiagnosticSeverity::ERROR))
        .collect();
    if errors.is_empty() {
        return None;
    }
    let total = errors.len();
    let limited = errors.iter().take(MAX_PER_FILE);
    let mut body: Vec<String> = limited.map(|d| pretty(d)).collect();
    if total > MAX_PER_FILE {
        body.push(format!("... and {} more", total - MAX_PER_FILE));
    }
    Some(format!(
        "<diagnostics file=\"{file}\">\n{}\n</diagnostics>",
        body.join("\n")
    ))
}

/// Build the full diagnostic block appended to a `write` / `edit` tool's
/// output. Two sections (each optional):
/// - **This file**: errors in the just-edited file. Always surfaced if any.
/// - **Other files**: errors in other files (e.g. a downstream caller that
///   now fails type-checking). Capped at `MAX_PROJECT_DIAGNOSTICS_FILES`
///   so a single edit doesn't dump the entire project's state.
///
/// Returns an empty string when there's nothing worth reporting, so callers
/// can `output.push_str(&block)` unconditionally.
pub fn build_report_block(
    current_file: &Path,
    all_diagnostics: &HashMap<PathBuf, Vec<Diagnostic>>,
) -> String {
    let current_canonical = current_file
        .canonicalize()
        .unwrap_or_else(|_| current_file.to_path_buf());

    let mut out = String::new();

    // Current-file section. Display the caller-supplied path (not the
    // canonical form) so the agent sees the same path it just wrote to —
    // e.g. `/tmp/foo.rs` rather than `/private/tmp/foo.rs` on macOS.
    if let Some(issues) = lookup_diagnostics(&current_canonical, all_diagnostics)
        && let Some(block) = report(&current_file.display().to_string(), issues)
    {
        out.push_str("\n\nLSP errors detected in this file, please fix:\n");
        out.push_str(&block);
    }

    // Other-files section. We iterate the diagnostic map deterministically
    // (sorted by path) so test assertions don't flake on hash order.
    let mut other_paths: Vec<&PathBuf> = all_diagnostics
        .keys()
        .filter(|p| {
            p.canonicalize()
                .map(|c| c != current_canonical)
                .unwrap_or(p.as_path() != current_canonical)
        })
        .collect();
    other_paths.sort();

    let mut other_count = 0;
    for path in other_paths {
        if other_count >= MAX_PROJECT_DIAGNOSTICS_FILES {
            break;
        }
        let Some(issues) = all_diagnostics.get(path) else {
            continue;
        };
        let Some(block) = report(&path.display().to_string(), issues) else {
            continue;
        };
        if other_count == 0 {
            out.push_str("\n\nLSP errors detected in other files:\n");
        } else {
            out.push('\n');
        }
        out.push_str(&block);
        other_count += 1;
    }

    out
}

/// Look up diagnostics for `target` in `map`, trying the canonical form
/// first then the literal path. Cheaper than canonicalizing every key.
fn lookup_diagnostics<'a>(
    target: &Path,
    map: &'a HashMap<PathBuf, Vec<Diagnostic>>,
) -> Option<&'a Vec<Diagnostic>> {
    if let Some(v) = map.get(target) {
        return Some(v);
    }
    for (k, v) in map.iter() {
        if k.canonicalize().map(|c| c == target).unwrap_or(k == target) {
            return Some(v);
        }
    }
    None
}

#[cfg(test)]
mod tests {
    use super::*;
    use lsp_types::{NumberOrString, Position, Range};

    fn diag(severity: DiagnosticSeverity, line: u32, col: u32, msg: &str) -> Diagnostic {
        Diagnostic {
            range: Range {
                start: Position {
                    line,
                    character: col,
                },
                end: Position {
                    line,
                    character: col,
                },
            },
            severity: Some(severity),
            code: Some(NumberOrString::String("E0001".to_string())),
            code_description: None,
            source: Some("rustc".to_string()),
            message: msg.to_string(),
            related_information: None,
            tags: None,
            data: None,
        }
    }

    // ---- pretty ----

    #[test]
    fn pretty_uses_severity_label_and_one_based_coordinates() {
        let d = diag(DiagnosticSeverity::ERROR, 4, 2, "type mismatch");
        // LSP line=4 col=2 → 1-based display "[5:3]".
        assert_eq!(pretty(&d), "ERROR [5:3] type mismatch");
    }

    #[test]
    fn pretty_handles_all_severities() {
        for (sev, label) in [
            (DiagnosticSeverity::ERROR, "ERROR"),
            (DiagnosticSeverity::WARNING, "WARN"),
            (DiagnosticSeverity::INFORMATION, "INFO"),
            (DiagnosticSeverity::HINT, "HINT"),
        ] {
            assert!(pretty(&diag(sev, 0, 0, "x")).starts_with(label));
        }
    }

    // Regression: an absent `severity` field used to crash some pretty-
    // printers. We default to ERROR — anything else would silently hide a
    // diagnostic from the agent's view.
    #[test]
    fn regression_missing_severity_defaults_to_error() {
        let mut d = diag(DiagnosticSeverity::ERROR, 0, 0, "x");
        d.severity = None;
        assert!(pretty(&d).starts_with("ERROR"));
    }

    // ---- report ----

    #[test]
    fn report_returns_none_for_no_errors() {
        assert!(report("x.rs", &[]).is_none());
        // Warnings alone do not produce a report.
        let warnings = vec![diag(DiagnosticSeverity::WARNING, 0, 0, "unused")];
        assert!(report("x.rs", &warnings).is_none());
    }

    // Regression: only ERROR severity is surfaced. Warnings/hints would be
    // noise — agents only need to fix the things blocking compilation.
    #[test]
    fn regression_report_filters_to_errors_only() {
        let issues = vec![
            diag(DiagnosticSeverity::ERROR, 0, 0, "real error"),
            diag(DiagnosticSeverity::WARNING, 1, 0, "unused"),
            diag(DiagnosticSeverity::INFORMATION, 2, 0, "fyi"),
            diag(DiagnosticSeverity::HINT, 3, 0, "consider"),
        ];
        let block = report("x.rs", &issues).unwrap();
        assert!(block.contains("real error"));
        assert!(!block.contains("unused"));
        assert!(!block.contains("fyi"));
        assert!(!block.contains("consider"));
    }

    #[test]
    fn report_wraps_in_diagnostics_tags() {
        let block = report("/tmp/x.rs", &[diag(DiagnosticSeverity::ERROR, 0, 0, "msg")]).unwrap();
        assert!(block.starts_with("<diagnostics file=\"/tmp/x.rs\">\n"));
        assert!(block.ends_with("</diagnostics>"));
    }

    // Regression: capping at MAX_PER_FILE prevents a generated file with
    // 500 errors from blowing the agent's context. The footer tells the
    // agent there's more, so they know to look further if needed.
    #[test]
    fn regression_report_caps_at_max_per_file_with_overflow_footer() {
        let issues: Vec<Diagnostic> = (0..MAX_PER_FILE + 7)
            .map(|i| diag(DiagnosticSeverity::ERROR, i as u32, 0, &format!("err {i}")))
            .collect();
        let block = report("x.rs", &issues).unwrap();
        let line_count = block.lines().count();
        // 1 header line + MAX_PER_FILE error lines + 1 overflow footer + 1 closing tag.
        assert_eq!(line_count, MAX_PER_FILE + 3);
        assert!(block.contains("... and 7 more"));
        // First 20 are listed; #20 onward are in the overflow.
        assert!(block.contains("err 0"));
        assert!(block.contains(&format!("err {}", MAX_PER_FILE - 1)));
        assert!(!block.contains("err 25"));
    }

    #[test]
    fn report_below_cap_has_no_overflow_footer() {
        let issues = vec![diag(DiagnosticSeverity::ERROR, 0, 0, "one"); 3];
        let block = report("x.rs", &issues).unwrap();
        assert!(!block.contains("and") && !block.contains("more"));
    }

    // ---- build_report_block ----

    #[test]
    fn build_report_block_returns_empty_when_no_diagnostics() {
        let block = build_report_block(Path::new("/tmp/x.rs"), &HashMap::new());
        assert_eq!(block, "");
    }

    #[test]
    fn build_report_block_emits_current_file_section() {
        let path = PathBuf::from("/tmp/edited.rs");
        let mut map = HashMap::new();
        map.insert(
            path.clone(),
            vec![diag(DiagnosticSeverity::ERROR, 0, 0, "bad type")],
        );
        let block = build_report_block(&path, &map);
        assert!(block.contains("errors detected in this file"));
        assert!(block.contains("bad type"));
        assert!(!block.contains("errors detected in other files"));
    }

    #[test]
    fn build_report_block_emits_other_files_section_when_relevant() {
        let current = PathBuf::from("/tmp/a.rs");
        let other = PathBuf::from("/tmp/b.rs");
        let mut map = HashMap::new();
        map.insert(
            other.clone(),
            vec![diag(DiagnosticSeverity::ERROR, 0, 0, "downstream break")],
        );

        let block = build_report_block(&current, &map);
        assert!(!block.contains("errors detected in this file"));
        assert!(block.contains("errors detected in other files"));
        assert!(block.contains("downstream break"));
    }

    // Regression: cap on other-files section keeps tool output bounded.
    // Without this, a refactor that broke 100 dependents would dump 100
    // diagnostic blocks at the agent on each subsequent edit.
    #[test]
    fn regression_build_report_block_caps_other_files() {
        let current = PathBuf::from("/tmp/current.rs");
        let mut map = HashMap::new();
        for i in 0..MAX_PROJECT_DIAGNOSTICS_FILES + 5 {
            let p = PathBuf::from(format!("/tmp/other{i:02}.rs"));
            map.insert(
                p,
                vec![diag(
                    DiagnosticSeverity::ERROR,
                    0,
                    0,
                    &format!("err in {i}"),
                )],
            );
        }
        let block = build_report_block(&current, &map);
        let other_blocks = block.matches("<diagnostics file=").count();
        assert_eq!(
            other_blocks, MAX_PROJECT_DIAGNOSTICS_FILES,
            "must cap at MAX_PROJECT_DIAGNOSTICS_FILES, got {other_blocks}"
        );
    }

    // Regression: a file with ONLY warnings must not produce a diagnostic
    // block, even when other files have errors. (The "other files" section
    // header still fires from the genuinely-broken neighbours.)
    #[test]
    fn regression_warning_only_files_do_not_appear() {
        let current = PathBuf::from("/tmp/main.rs");
        let warn_only = PathBuf::from("/tmp/warn.rs");
        let bad = PathBuf::from("/tmp/bad.rs");
        let mut map = HashMap::new();
        map.insert(
            warn_only.clone(),
            vec![diag(DiagnosticSeverity::WARNING, 0, 0, "unused")],
        );
        map.insert(
            bad.clone(),
            vec![diag(DiagnosticSeverity::ERROR, 0, 0, "real")],
        );
        let block = build_report_block(&current, &map);
        assert!(!block.contains("unused"));
        assert!(block.contains("real"));
    }

    // Regression: the rendered current-file section must echo the caller's
    // path, not the canonical form. On macOS /tmp ↔ /private/tmp would
    // otherwise show the agent a path it didn't type — confusing when the
    // agent then tries to refer back to the file in a follow-up edit.
    #[test]
    fn regression_current_file_section_preserves_caller_path() {
        let tmp = std::env::temp_dir().join(format!(
            "dirge-diag-path-test-{}-{}.rs",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&tmp, "// test\n").unwrap();
        let canonical = tmp.canonicalize().unwrap();

        // Caller passes tmp; map keyed by canonical.
        let mut map = HashMap::new();
        map.insert(canonical, vec![diag(DiagnosticSeverity::ERROR, 0, 0, "x")]);
        let block = build_report_block(&tmp, &map);

        // The block should reference the caller's `tmp` path.
        assert!(
            block.contains(&tmp.display().to_string()),
            "expected caller path {} in: {block}",
            tmp.display()
        );

        std::fs::remove_file(&tmp).ok();
    }

    // Regression: current-file matching must handle the path that comes in
    // having a different on-disk identity from the map key (e.g. when the
    // map was populated using the LSP URI's decoded path while the caller
    // passed a relative path that was later joined to cwd).
    #[test]
    fn current_file_matches_through_canonicalize() {
        // Use a tempfile so the paths actually exist for canonicalize().
        let tmp = std::env::temp_dir().join(format!(
            "dirge-diagnostic-test-{}-{}.rs",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::write(&tmp, "// test\n").unwrap();
        let canonical = tmp.canonicalize().unwrap();
        let mut map = HashMap::new();
        // Map key is the canonical form; caller passes the non-canonical
        // tmp path (e.g., on macOS /tmp ↔ /private/tmp).
        map.insert(
            canonical.clone(),
            vec![diag(DiagnosticSeverity::ERROR, 0, 0, "x")],
        );
        let block = build_report_block(&tmp, &map);
        assert!(
            block.contains("errors detected in this file"),
            "expected current-file section; got: {block}"
        );
        std::fs::remove_file(&tmp).ok();
    }
}
