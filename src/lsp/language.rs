//! Extension → LSP language identifier mapping.
//!
//! Returned values are the LSP `languageId` strings (see the LSP spec §3.18.1)
//! used in `textDocument/didOpen`. Unknown extensions return `"plaintext"` so
//! `notify.open` always has a well-formed payload.

use std::path::Path;

/// Returns the LSP `languageId` for the given file path.
///
/// Looks at the lowercased file extension. Files with no extension match the
/// filename (e.g. `Makefile` → `makefile`). Returns `"plaintext"` for any
/// unrecognised extension/filename.
pub fn language_for_path(path: &Path) -> &'static str {
    let name = path
        .file_name()
        .and_then(|s| s.to_str())
        .unwrap_or("")
        .to_lowercase();

    if let Some(ext) = path.extension().and_then(|s| s.to_str()) {
        if let Some(lang) = LANGUAGES.iter().find(|(e, _)| *e == ext.to_lowercase()) {
            return lang.1;
        }
    }

    // Some filenames are themselves the marker (Makefile, Dockerfile, etc).
    if let Some(lang) = FILENAMES.iter().find(|(n, _)| *n == name) {
        return lang.1;
    }
    "plaintext"
}

/// Extension → languageId. Lowercase keys; lookups lowercase the input.
const LANGUAGES: &[(&str, &str)] = &[
    ("rs", "rust"),
    ("ts", "typescript"),
    ("tsx", "typescriptreact"),
    ("mts", "typescript"),
    ("cts", "typescript"),
    ("js", "javascript"),
    ("jsx", "javascriptreact"),
    ("mjs", "javascript"),
    ("cjs", "javascript"),
    ("py", "python"),
    ("pyi", "python"),
    ("clj", "clojure"),
    ("cljs", "clojure"),
    ("cljc", "clojure"),
    ("edn", "clojure"),
    ("bb", "clojure"),
    ("go", "go"),
    ("c", "c"),
    ("h", "c"),
    ("cpp", "cpp"),
    ("cxx", "cpp"),
    ("cc", "cpp"),
    ("hpp", "cpp"),
    ("hxx", "cpp"),
    ("hh", "cpp"),
    ("java", "java"),
    ("rb", "ruby"),
    ("sh", "shellscript"),
    ("bash", "shellscript"),
    ("zsh", "shellscript"),
    ("json", "json"),
    ("yaml", "yaml"),
    ("yml", "yaml"),
    ("toml", "toml"),
    ("md", "markdown"),
    ("html", "html"),
    ("css", "css"),
    ("scss", "scss"),
    ("xml", "xml"),
    ("nix", "nix"),
    ("zig", "zig"),
];

const FILENAMES: &[(&str, &str)] = &[("makefile", "makefile"), ("dockerfile", "dockerfile")];

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    fn lang(p: &str) -> &'static str {
        language_for_path(&PathBuf::from(p))
    }

    #[test]
    fn rs_is_rust() {
        assert_eq!(lang("src/main.rs"), "rust");
        assert_eq!(lang("main.rs"), "rust");
    }

    #[test]
    fn ts_and_tsx_are_distinct() {
        assert_eq!(lang("a.ts"), "typescript");
        assert_eq!(lang("a.tsx"), "typescriptreact");
        assert_eq!(lang("a.mts"), "typescript");
    }

    #[test]
    fn jsx_is_javascriptreact_not_javascript() {
        assert_eq!(lang("a.jsx"), "javascriptreact");
        assert_eq!(lang("a.js"), "javascript");
    }

    #[test]
    fn clojure_dialects_all_clojure() {
        for ext in &["clj", "cljs", "cljc", "edn", "bb"] {
            assert_eq!(lang(&format!("foo.{ext}")), "clojure", "ext={ext}");
        }
    }

    #[test]
    fn python_extensions() {
        assert_eq!(lang("a.py"), "python");
        assert_eq!(lang("a.pyi"), "python");
    }

    #[test]
    fn extension_lookup_is_case_insensitive() {
        // LSP language IDs are stable identifiers; pathological capitalisation
        // in filenames must not break the mapping.
        assert_eq!(lang("README.MD"), "markdown");
        assert_eq!(lang("Main.RS"), "rust");
    }

    #[test]
    fn unknown_extension_returns_plaintext() {
        assert_eq!(lang("a.unknown_ext_42"), "plaintext");
    }

    #[test]
    fn missing_extension_returns_plaintext() {
        assert_eq!(lang("just_a_filename"), "plaintext");
    }

    #[test]
    fn filenames_without_extension_match_by_name() {
        assert_eq!(lang("Makefile"), "makefile");
        assert_eq!(lang("Dockerfile"), "dockerfile");
        // Case insensitive.
        assert_eq!(lang("makefile"), "makefile");
        assert_eq!(lang("path/to/Makefile"), "makefile");
    }

    #[test]
    fn empty_path_returns_plaintext() {
        assert_eq!(lang(""), "plaintext");
    }
}
