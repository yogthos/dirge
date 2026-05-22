#[cfg(test)]
mod semantic_tests {
    use std::path::PathBuf;
    use std::sync::Arc;

    use crate::semantic::adapters::AdapterRegistry;
    use crate::semantic::{LanguageAdapter, SymbolIndex};

    fn fixtures_dir() -> PathBuf {
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .join("src")
            .join("tests")
            .join("fixtures")
    }

    fn mk_registry() -> Arc<AdapterRegistry> {
        #[allow(unused_mut)]
        let mut adapters: Vec<Box<dyn LanguageAdapter>> = Vec::new();

        #[cfg(feature = "semantic-ts")]
        adapters.push(Box::new(crate::semantic::adapters::TypescriptAdapter));

        #[cfg(feature = "semantic-python")]
        adapters.push(Box::new(crate::semantic::adapters::PythonAdapter));

        #[cfg(feature = "semantic-elixir")]
        adapters.push(Box::new(crate::semantic::adapters::ElixirAdapter));

        Arc::new(AdapterRegistry::new(adapters))
    }

    // ── CRITICAL #1: find_callers word boundary ──────────────────────────

    #[test]
    #[cfg(feature = "semantic-ts")]
    fn find_callers_respects_word_boundaries() {
        let registry = mk_registry();
        let mut index = SymbolIndex::new(registry);

        let _file = fixtures_dir().join("callers_test.ts");

        let callers = index.find_callers("run", &fixtures_dir()).unwrap();

        // "run" should match only the call `run()` on line 6 (inside runner),
        // NOT "runner" (line 2), NOT "running" (line 7), NOT "prune" (line 9)
        let has_runner_def = callers.iter().any(|c| c.contains("fn runner"));
        let has_running_def = callers.iter().any(|c| c.contains("fn running"));
        let has_prune = callers.iter().any(|c| c.contains("prune"));
        let has_run_call = callers.iter().any(|c| c.contains("run()"));

        assert!(
            !has_runner_def,
            "should NOT match 'runner' (substring match bug)"
        );
        assert!(
            !has_running_def,
            "should NOT match 'running' (substring match bug)"
        );
        assert!(!has_prune, "should NOT match 'prune' (substring match bug)");
        assert!(has_run_call, "should match 'run()' call inside runner()");
    }

    // ── HIGH #2: find_callees on TSX files ─────────────────────────────

    #[test]
    #[cfg(feature = "semantic-ts")]
    fn find_callees_works_on_tsx() {
        let registry = mk_registry();
        let mut index = SymbolIndex::new(registry);

        let file = fixtures_dir().join("component.tsx");

        let callees = index.find_callees(&file, "Component").unwrap();

        assert!(
            callees.iter().any(|c| c == "helper"),
            "should find 'helper' callee in TSX Component"
        );
    }

    // ── HIGH #3: extract does not panic on imports ─────────────────────

    #[test]
    #[cfg(feature = "semantic-ts")]
    fn ts_extract_does_not_panic_on_imports() {
        let registry = mk_registry();
        let mut index = SymbolIndex::new(registry);

        let file = fixtures_dir().join("component.tsx");

        let entry = index.ensure_file(&file).unwrap();
        let functions: Vec<_> = entry
            .symbols
            .iter()
            .filter(|s| s.kind == SymbolKind::Function)
            .collect();

        assert!(!functions.is_empty(), "should find at least one function");
        assert!(
            functions.iter().any(|s| s.name == "helper"),
            "should find helper function"
        );
    }

    // ── MEDIUM #9: Python decorated functions ──────────────────────────

    #[test]
    #[cfg(feature = "semantic-python")]
    fn python_extracts_decorated_functions() {
        let registry = mk_registry();
        let mut index = SymbolIndex::new(registry);

        let file = fixtures_dir().join("decorated.py");

        let entry = index.ensure_file(&file).unwrap();
        let names: Vec<&str> = entry.symbols.iter().map(|s| s.name.as_str()).collect();

        assert!(
            names.contains(&"cached_func"),
            "should find @lru_cache decorated function 'cached_func'. Found: {:?}",
            names
        );
        assert!(
            names.contains(&"my_prop"),
            "should find @property decorated method 'my_prop'. Found: {:?}",
            names
        );
        assert!(
            names.contains(&"regular_method"),
            "should find undecorated method 'regular_method'. Found: {:?}",
            names
        );
    }

    // ── MEDIUM #5: cold cache auto-initializes ────────────────────────

    #[test]
    #[cfg(feature = "semantic-ts")]
    fn cold_cache_auto_initializes() {
        let registry = mk_registry();
        let mut index = SymbolIndex::new(registry);

        // Cache is cold but find_definition auto-initializes from cwd.
        // Verify it doesn't error — it may find symbols or return empty.
        let _ = index.find_definition("nonexistent_surely").unwrap();
    }

    // ── MEDIUM #8: ensure_file with binary file doesn't poison cache ──

    #[test]
    #[cfg(feature = "semantic-ts")]
    fn ensure_file_skips_binary_gracefully() {
        let registry = mk_registry();
        let mut index = SymbolIndex::new(registry);

        let bin_path = fixtures_dir().join("not_a_ts_file.bin");
        std::fs::write(&bin_path, b"\x00\x01\x02").unwrap();

        let result = index.ensure_file(&bin_path);
        // Should either error gracefully or the tool layer filters it
        // The key behavior: it must not panic
        let _ = result;
        std::fs::remove_file(&bin_path).unwrap();
    }
}
