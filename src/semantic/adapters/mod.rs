pub mod bash;
#[cfg(feature = "semantic-c")]
mod c;
#[cfg(feature = "semantic-clojure")]
mod clojure;
#[cfg(feature = "semantic-cpp")]
mod cpp;
#[cfg(feature = "semantic-go")]
mod go;
#[cfg(feature = "semantic-java")]
mod java;
#[cfg(feature = "semantic-python")]
mod python;
#[cfg(feature = "semantic-ruby")]
mod ruby;
#[cfg(feature = "semantic-rust")]
mod rust;
#[cfg(feature = "semantic-ts")]
mod typescript;

#[cfg(feature = "semantic-c")]
pub use c::CAdapter;
#[cfg(feature = "semantic-clojure")]
pub use clojure::ClojureAdapter;
#[cfg(feature = "semantic-cpp")]
pub use cpp::CppAdapter;
#[cfg(feature = "semantic-go")]
pub use go::GoAdapter;
#[cfg(feature = "semantic-java")]
pub use java::JavaAdapter;
#[cfg(feature = "semantic-python")]
pub use python::PythonAdapter;
#[cfg(feature = "semantic-ruby")]
pub use ruby::RubyAdapter;
#[cfg(feature = "semantic-rust")]
pub use rust::RustAdapter;
#[cfg(feature = "semantic-ts")]
pub use typescript::TypescriptAdapter;

use std::path::Path;

use crate::semantic::adapter::LanguageAdapter;

pub struct AdapterRegistry {
    adapters: Vec<Box<dyn LanguageAdapter>>,
}

impl AdapterRegistry {
    pub fn new(adapters: Vec<Box<dyn LanguageAdapter>>) -> Self {
        Self { adapters }
    }

    pub fn find_for_file(&self, file_path: &Path) -> Option<&dyn LanguageAdapter> {
        self.find_for_file_with_content(file_path, None)
    }

    /// Same as `find_for_file` but takes optional file content for
    /// extension tie-breaks. Used by audit L3: `.h` headers can be C
    /// or C++. With the C adapter listed first, every C++ project's
    /// public headers parsed as C and classes/namespaces silently
    /// vanished from `list_symbols`. When content is provided AND
    /// the extension is `.h`, sniff for C++-only constructs (class,
    /// namespace, template, ::) and prefer a C++ adapter if found.
    /// Pure-path callers (no content yet) fall back to the prior
    /// first-match behavior.
    pub fn find_for_file_with_content(
        &self,
        file_path: &Path,
        content: Option<&str>,
    ) -> Option<&dyn LanguageAdapter> {
        let ext = file_path.extension()?.to_str()?.to_lowercase();
        if ext == "h"
            && let Some(src) = content
            && self.looks_like_cpp_header(src)
        {
            // Prefer a C++ adapter for `.h` files whose content shows
            // C++-only tokens. Falls through to the regular search if
            // no C++ adapter is registered.
            if let Some(cpp) = self.adapters.iter().find(|a| {
                a.extensions()
                    .iter()
                    .any(|e| e.trim_start_matches('.') == "cpp" || e.trim_start_matches('.') == "hpp")
            }) {
                return Some(cpp.as_ref());
            }
        }
        self.adapters
            .iter()
            .find(|a| {
                a.extensions()
                    .iter()
                    .any(|e| e.trim_start_matches('.') == ext)
            })
            .map(|a| a.as_ref())
    }

    /// Cheap sniff: scan a prefix of the source for C++-only tokens.
    /// Whole-token match against `class `, `namespace `, `template`,
    /// and `::` (scope resolution) — none of these appear in valid C
    /// outside of comments/strings, so a single hit is a strong
    /// signal. Caps the scan at 32 KiB so a huge header doesn't slow
    /// the registry call.
    fn looks_like_cpp_header(&self, src: &str) -> bool {
        const SNIFF_BYTES: usize = 32 * 1024;
        let head = if src.len() > SNIFF_BYTES {
            // Cut on a UTF-8 boundary; ASCII-only is the common case.
            let mut cut = SNIFF_BYTES;
            while cut > 0 && !src.is_char_boundary(cut) {
                cut -= 1;
            }
            &src[..cut]
        } else {
            src
        };
        head.contains("class ")
            || head.contains("namespace ")
            || head.contains("template<")
            || head.contains("template <")
            || head.contains("::")
    }

    pub fn all_extensions(&self) -> Vec<String> {
        self.adapters
            .iter()
            .flat_map(|a| {
                a.extensions()
                    .iter()
                    .map(|e| e.trim_start_matches('.').to_string())
            })
            .collect()
    }
}
