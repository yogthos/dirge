use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;

use ignore::WalkBuilder;

use crate::agent::tools::MAX_GREP_RESULTS;

const MAX_INDEX_FILES: usize = 2000;
use crate::semantic::adapters::AdapterRegistry;
use crate::semantic::types::{ExtractedFile, Symbol, SymbolKind};

type FileCache = HashMap<PathBuf, ExtractedFile>;

pub struct SymbolIndex {
    registry: Arc<AdapterRegistry>,
    cache: FileCache,
}

impl SymbolIndex {
    pub fn new(registry: Arc<AdapterRegistry>) -> Self {
        Self {
            registry,
            cache: HashMap::new(),
        }
    }

    pub fn ensure_file(&mut self, path: &Path) -> Result<&ExtractedFile, String> {
        let canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());

        let adapter = self
            .registry
            .find_for_file(&canonical)
            .ok_or_else(|| format!("No language adapter for file: {}", canonical.display()))?;

        let mtime = std::fs::metadata(&canonical)
            .ok()
            .and_then(|m| m.modified().ok());

        let needs_refresh = match self.cache.get(&canonical) {
            Some(entry) => mtime.map_or(true, |mt| mt != entry.mtime),
            None => true,
        };

        if needs_refresh {
            let source =
                std::fs::read_to_string(&canonical).map_err(|e| format!("Read error: {e}"))?;
            let mut extracted = adapter.extract(&canonical, &source)?;
            if let Some(mt) = mtime {
                extracted.mtime = mt;
            }
            self.cache.insert(canonical.clone(), extracted);
        }

        self.cache
            .get(&canonical)
            .ok_or_else(|| "Cache miss after insert".to_string())
    }

    pub fn ensure_all(&mut self, root: &Path, include: Option<&str>) -> Result<usize, String> {
        let mut count = 0;
        let extensions = self.registry.all_extensions();

        let mut walker = WalkBuilder::new(root);
        walker
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .hidden(false)
            .filter_entry(move |entry| {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    let name = entry.file_name().to_str().unwrap_or("");
                    !matches!(name, "node_modules" | "target" | ".git" | "__pycache__")
                } else {
                    true
                }
            });

        for entry in walker
            .build()
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        {
            if count >= MAX_INDEX_FILES {
                break;
            }

            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            if !extensions.iter().any(|e| e == &ext) {
                continue;
            }

            if let Some(pattern) = include {
                let fname = path.file_name().and_then(|n| n.to_str()).unwrap_or("");
                if let Ok(re) = regex::Regex::new(pattern) {
                    if !re.is_match(fname) {
                        continue;
                    }
                }
            }

            if let Ok(meta) = path.metadata()
                && meta.len() > 10 * 1024 * 1024
            {
                continue;
            }

            if self.ensure_file(path).is_ok() {
                count += 1;
            }
        }

        Ok(count)
    }

    pub fn find_definition(&mut self, name: &str) -> Result<Vec<(PathBuf, Symbol)>, String> {
        let mut results = Vec::new();

        if self.cache.is_empty() {
            self.ensure_all(
                &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                None,
            )?;
        }

        let file_paths: Vec<PathBuf> = self.cache.keys().cloned().collect();
        for path in file_paths {
            let Ok(entry) = self.ensure_file(&path) else {
                continue;
            };
            for sym in &entry.symbols {
                if sym.name == name {
                    results.push((entry.file_path.clone(), sym.clone()));
                }
            }
        }

        Ok(results)
    }

    pub fn find_callers(&mut self, name: &str, root: &Path) -> Result<Vec<String>, String> {
        let mut results = Vec::new();

        let extensions = self.registry.all_extensions();
        let re = regex::Regex::new(&format!(r"\b{}\b", regex::escape(name)))
            .map_err(|e| format!("Regex error: {e}"))?;

        let mut walker = WalkBuilder::new(root);
        walker
            .git_ignore(true)
            .git_global(true)
            .git_exclude(true)
            .require_git(false)
            .hidden(false)
            .filter_entry(|entry| {
                if entry.file_type().map(|t| t.is_dir()).unwrap_or(false) {
                    // Share the central skip list with find_files /
                    // glob / list_dir / grep — the previous inline
                    // `matches!` only listed 4 dirs and diverged
                    // silently from the canonical set in
                    // `agent::tools::is_skip_dir`.
                    let name = entry.file_name().to_str().unwrap_or("");
                    !crate::agent::tools::is_skip_dir(name)
                } else {
                    true
                }
            });

        for entry in walker
            .build()
            .flatten()
            .filter(|e| e.file_type().map(|t| t.is_file()).unwrap_or(false))
        {
            if results.len() >= MAX_GREP_RESULTS {
                break;
            }

            let path = entry.path();
            let ext = path
                .extension()
                .and_then(|e| e.to_str())
                .unwrap_or("")
                .to_lowercase();

            if !extensions.iter().any(|e| e == &ext) {
                continue;
            }

            if let Ok(meta) = path.metadata()
                && meta.len() > 10 * 1024 * 1024
            {
                continue;
            }

            let Ok(data) = std::fs::read(path) else {
                continue;
            };

            if data.iter().take(8192).any(|&b| b == 0) {
                continue;
            }

            let content = String::from_utf8_lossy(&data);
            let path_str = path.to_string_lossy();

            let path_canonical = path.canonicalize().unwrap_or_else(|_| path.to_path_buf());
            let entry = self.ensure_file(&path_canonical).ok();

            for (line_num, line) in content.lines().enumerate() {
                if re.is_match(line) {
                    if let Some(ref entry) = entry {
                        let is_definition = entry.symbols.iter().any(|s| {
                            s.name == name
                                && s.range.start_line <= line_num + 1
                                && s.range.end_line >= line_num + 1
                        });
                        if is_definition {
                            continue;
                        }
                    }
                    results.push(format!("{}:{}: {}", path_str, line_num + 1, line.trim()));
                    if results.len() >= MAX_GREP_RESULTS {
                        break;
                    }
                }
            }
        }

        Ok(results)
    }

    pub fn find_callees(&mut self, file_path: &Path, name: &str) -> Result<Vec<String>, String> {
        let entry = self.ensure_file(file_path)?;

        let matches: Vec<&Symbol> = entry.symbols.iter().filter(|s| s.name == name).collect();

        if matches.is_empty() {
            return Err(format!(
                "Symbol '{}' not found in {}",
                name,
                file_path.display()
            ));
        }

        if matches.len() > 1 {
            let hints: Vec<String> = matches
                .iter()
                .map(|s| {
                    format!(
                        "  {} [{}] at line {}",
                        s.name,
                        match &s.parent_class {
                            Some(c) => format!("method of {}", c),
                            None => s.kind.to_string(),
                        },
                        s.range.start_line
                    )
                })
                .collect();
            return Err(format!(
                "Multiple symbols named '{}' found in {}:\n{}\n\nUse a more specific identifier or try get_symbol_body to inspect candidates.",
                name,
                file_path.display(),
                hints.join("\n")
            ));
        }

        let symbol = matches[0];
        let range = symbol.range;
        let source = std::fs::read_to_string(file_path).map_err(|e| format!("Read error: {e}"))?;

        if range.start_byte >= source.len() || range.end_byte > source.len() {
            return Err(
                "File modified since last parse — byte range is stale. Re-run the query."
                    .to_string(),
            );
        }

        let adapter = self
            .registry
            .find_for_file(file_path)
            .ok_or_else(|| format!("No language adapter for file: {}", file_path.display()))?;

        adapter.find_callees_in_range(&source, file_path, range)
    }

    pub fn get_symbol_body(&mut self, file_path: &Path, name: &str) -> Result<String, String> {
        let entry = self.ensure_file(file_path)?;

        let symbol = entry
            .symbols
            .iter()
            .find(|s| s.name == name)
            .ok_or_else(|| format!("Symbol '{}' not found in {}", name, file_path.display()))?;

        let source = std::fs::read_to_string(file_path).map_err(|e| format!("Read error: {e}"))?;

        if symbol.range.start_byte >= source.len() || symbol.range.end_byte > source.len() {
            return Err(
                "File modified since last parse — byte range is stale. Re-run the query."
                    .to_string(),
            );
        }

        let bytes = source.as_bytes();
        let body_slice = &bytes[symbol.range.start_byte..symbol.range.end_byte];
        Ok(String::from_utf8_lossy(body_slice).to_string())
    }

    pub fn list_symbols(
        &mut self,
        file_path: Option<&Path>,
        kind_filter: Option<SymbolKind>,
    ) -> Result<Vec<(PathBuf, Vec<Symbol>)>, String> {
        let mut result: Vec<(PathBuf, Vec<Symbol>)> = Vec::new();

        if let Some(path) = file_path {
            let entry = self.ensure_file(path)?;
            let symbols: Vec<Symbol> = entry
                .symbols
                .iter()
                .filter(|s| kind_filter.map_or(true, |k| s.kind == k))
                .cloned()
                .collect();
            result.push((entry.file_path.clone(), symbols));
        } else {
            if self.cache.is_empty() {
                self.ensure_all(
                    &std::env::current_dir().unwrap_or_else(|_| PathBuf::from(".")),
                    None,
                )?;
            }
            let file_paths: Vec<PathBuf> = self.cache.keys().cloned().collect();
            for path in file_paths {
                let Ok(entry) = self.ensure_file(&path) else {
                    continue;
                };
                let symbols: Vec<Symbol> = entry
                    .symbols
                    .iter()
                    .filter(|s| kind_filter.map_or(true, |k| s.kind == k))
                    .cloned()
                    .collect();
                if !symbols.is_empty() {
                    result.push((entry.file_path.clone(), symbols));
                }
            }
        }

        Ok(result)
    }
}
