//! Lightweight regex-based syntax highlighter for fenced code blocks
//! in chat output. No new heavy deps (no syntect / no tree-sitter);
//! per-language rule tables + a small tokenizer that walks the source
//! once classifying spans as keyword / string / comment / number /
//! type / function-call / plain.
//!
//! Coverage matches dirge's `semantic-*` feature set (TypeScript /
//! Python / Bash / Clojure / Go / Ruby / Rust / Java / C / C++) plus
//! JSON / YAML / Markdown / SQL / TOML / Shell. Unknown languages
//! fall back to a uniform `tool()` color — same as the pre-highlight
//! behavior — so adding a fenced block with `language: foo` never
//! crashes, just declines to colorize.
//!
//! Trade-offs vs syntect / tree-sitter:
//!   - Doesn't handle nested constructs (template strings, regex
//!     literals in JS, raw strings in Rust) perfectly.
//!   - Doesn't follow scope state across line breaks for block
//!     comments — line-based highlighting only.
//!   - In exchange, ~0 binary cost over what we already pay for the
//!     `regex` crate.

use compact_str::CompactString;
use crossterm::style::Color;

use crate::ui::theme;

/// One styled span on a single visual line.
#[derive(Debug, Clone)]
pub struct Span {
    pub text: CompactString,
    pub color: Color,
}

/// Highlight `code` as `lang`, returning one Vec<Span> per line.
/// Unknown / unsupported languages return a single span per line
/// painted with `theme::tool()` (the pre-highlight default).
pub fn highlight_code(code: &str, lang: &str) -> Vec<Vec<Span>> {
    let lang_norm = normalize_lang(lang);
    let rules = rules_for(lang_norm);
    let mut out: Vec<Vec<Span>> = Vec::new();
    // Block-comment state must cross lines for /* … */ in C-family,
    // """ in Python, =begin/=end in Ruby, etc. The tokenizer walks
    // each line and reports a possibly-open block-comment trailer; we
    // carry the open state forward into the next line.
    let mut in_block_comment = false;
    for raw in code.split('\n') {
        let line = raw.trim_end_matches('\r');
        if let Some(rules) = rules.as_ref() {
            let (spans, still_in_block) = tokenize_line(line, rules, in_block_comment);
            in_block_comment = still_in_block;
            out.push(spans);
        } else {
            out.push(vec![Span {
                text: CompactString::new(line),
                color: theme::tool(),
            }]);
        }
    }
    out
}

/// Whether `lang` (or one of its aliases) has dedicated highlighting
/// rules. Callers that render unknown content in their own plain color
/// (e.g. `read` boxes for `.txt`/`.md`) use this to skip highlighting
/// rather than paint everything the uniform fallback color.
pub fn supports(lang: &str) -> bool {
    rules_for(normalize_lang(lang)).is_some()
}

/// Normalize fence info strings to a stable language id.
fn normalize_lang(lang: &str) -> &str {
    // Some markdown sources include attributes like ```rust,no_run.
    let head = lang.split([',', ' ']).next().unwrap_or("");
    match head {
        "ts" | "tsx" | "typescript" | "typescriptreact" => "typescript",
        "js" | "jsx" | "javascript" | "javascriptreact" | "mjs" | "cjs" => "javascript",
        "py" | "python" | "py3" | "python3" => "python",
        "sh" | "bash" | "shell" | "zsh" => "bash",
        "clj" | "cljs" | "cljc" | "clojure" | "edn" => "clojure",
        "go" | "golang" => "go",
        "rb" | "ruby" => "ruby",
        "rs" | "rust" => "rust",
        "java" => "java",
        "c" | "h" => "c",
        "cpp" | "cc" | "cxx" | "hpp" | "hh" | "hxx" | "c++" => "cpp",
        "json" | "jsonc" => "json",
        "yaml" | "yml" => "yaml",
        "toml" => "toml",
        "sql" => "sql",
        "md" | "markdown" => "markdown",
        other => other,
    }
}

/// Per-language tokenization rules. Static, lifetime-of-program.
struct Rules {
    keywords: &'static [&'static str],
    types: &'static [&'static str],
    /// Line-comment marker, e.g. `//` or `#` or `--`.
    line_comment: Option<&'static str>,
    /// Block-comment delimiters, e.g. `/*` / `*/`.
    block_comment: Option<(&'static str, &'static str)>,
    /// Characters that open a string literal. The same char closes it.
    /// `'` and `"` are universal; some langs add `` ` ``.
    string_delims: &'static [char],
    /// Whether `#` introduces a directive (e.g. `#include` in C) that
    /// should be colored as keyword rather than comment. Tells the
    /// tokenizer to NOT treat `#` as line-comment when this is true
    /// AND the next char is alphabetic.
    hash_directive: bool,
}

fn rules_for(lang: &str) -> Option<&'static Rules> {
    match lang {
        "typescript" | "javascript" => Some(&JS_RULES),
        "python" => Some(&PY_RULES),
        "bash" => Some(&BASH_RULES),
        "clojure" => Some(&CLJ_RULES),
        "go" => Some(&GO_RULES),
        "ruby" => Some(&RUBY_RULES),
        "rust" => Some(&RUST_RULES),
        "java" => Some(&JAVA_RULES),
        "c" => Some(&C_RULES),
        "cpp" => Some(&CPP_RULES),
        "json" => Some(&JSON_RULES),
        "yaml" => Some(&YAML_RULES),
        "toml" => Some(&TOML_RULES),
        "sql" => Some(&SQL_RULES),
        _ => None,
    }
}

// Walk one line classifying spans. Returns (spans, in_block_comment).
fn tokenize_line(line: &str, rules: &Rules, mut in_block: bool) -> (Vec<Span>, bool) {
    let mut spans: Vec<Span> = Vec::new();
    let bytes = line.as_bytes();
    let mut i = 0usize;

    // If we entered the line inside a block comment, scan until the
    // close marker (or end of line).
    if in_block && let Some((_open, close)) = rules.block_comment {
        let close_b = close.as_bytes();
        if let Some(pos) = find_subseq(&bytes[i..], close_b) {
            let end = i + pos + close_b.len();
            spans.push(Span {
                text: CompactString::new(&line[i..end]),
                color: theme::dim(),
            });
            i = end;
            in_block = false;
            let _ = in_block; // explicitly acknowledge: continuation falls through below
        } else {
            spans.push(Span {
                text: CompactString::new(&line[i..]),
                color: theme::dim(),
            });
            return (spans, true);
        }
    }

    while i < bytes.len() {
        let ch = bytes[i] as char;

        // Line comment.
        if let Some(marker) = rules.line_comment {
            let mb = marker.as_bytes();
            if bytes[i..].starts_with(mb) {
                // Bash `#`-style: don't treat `#!` shebang or `#` mid-string
                // — `#` in `$#`, `#{...}` etc. — as comment unless preceded
                // by whitespace or start-of-line. The tokenizer already
                // consumes strings before we reach here, so the remaining
                // concern is just shebang. Shebang is the whole line:
                // include from i to end.
                spans.push(Span {
                    text: CompactString::new(&line[i..]),
                    color: theme::dim(),
                });
                return (spans, false);
            }
        }

        // Block comment open.
        if let Some((open, _close)) = rules.block_comment {
            let ob = open.as_bytes();
            if bytes[i..].starts_with(ob) {
                let close = rules.block_comment.unwrap().1;
                let close_b = close.as_bytes();
                if let Some(pos) = find_subseq(&bytes[i + ob.len()..], close_b) {
                    let end = i + ob.len() + pos + close_b.len();
                    spans.push(Span {
                        text: CompactString::new(&line[i..end]),
                        color: theme::dim(),
                    });
                    i = end;
                    continue;
                } else {
                    // Block comment runs to EOL — carry open state.
                    spans.push(Span {
                        text: CompactString::new(&line[i..]),
                        color: theme::dim(),
                    });
                    return (spans, true);
                }
            }
        }

        // String literal.
        if rules.string_delims.contains(&ch) {
            let delim = ch;
            let start = i;
            i += 1;
            while i < bytes.len() {
                let c = bytes[i] as char;
                if c == '\\' && i + 1 < bytes.len() {
                    i += 2;
                    continue;
                }
                if c == delim {
                    i += 1;
                    break;
                }
                // Bytes path so we don't desync on multi-byte chars.
                let step = utf8_char_len(bytes[i]);
                i += step.max(1);
            }
            spans.push(Span {
                text: CompactString::new(&line[start..i]),
                color: theme::accent(),
            });
            continue;
        }

        // Number literal (digits + optional decimal / hex prefix).
        if ch.is_ascii_digit() {
            let start = i;
            // 0x… / 0b… / 0o…
            if ch == '0' && i + 1 < bytes.len() {
                let next = bytes[i + 1] as char;
                if matches!(next, 'x' | 'X' | 'b' | 'B' | 'o' | 'O') {
                    i += 2;
                    while i < bytes.len() && (bytes[i] as char).is_ascii_alphanumeric() {
                        i += 1;
                    }
                    spans.push(Span {
                        text: CompactString::new(&line[start..i]),
                        color: theme::warn(),
                    });
                    continue;
                }
            }
            while i < bytes.len()
                && ((bytes[i] as char).is_ascii_digit() || bytes[i] == b'.' || bytes[i] == b'_')
            {
                i += 1;
            }
            // Optional exponent / unit suffix (rust `_i32`, `f64`).
            while i < bytes.len() && ((bytes[i] as char).is_ascii_alphanumeric()) {
                i += 1;
            }
            spans.push(Span {
                text: CompactString::new(&line[start..i]),
                color: theme::warn(),
            });
            continue;
        }

        // Identifier / keyword / type / call.
        if is_ident_start(ch, rules) {
            let start = i;
            // Use char iteration so a non-ASCII grapheme inside an
            // identifier (`naïve`, identifiers in non-English code)
            // doesn't terminate the token half-way through. The old
            // `bytes[i] as char` cast produced a Latin-1 char for
            // the lead byte of a multi-byte sequence and the
            // `is_ascii_alphanumeric` predicate rejected it,
            // splitting the identifier mid-word. Predicate is now
            // ASCII-OR-non-control: matches the spirit of "part of
            // a word" without dragging Unicode XID classification in.
            while i < bytes.len() {
                let Some(next) = line[i..].chars().next() else {
                    break;
                };
                if !is_ident_cont(next, rules) {
                    break;
                }
                i += next.len_utf8();
            }
            let word = &line[start..i];

            // Distinguish keyword / type / call / plain.
            let color = if rules.keywords.contains(&word) {
                theme::user()
            } else if rules.types.contains(&word) || looks_like_type(word, rules) {
                theme::header()
            } else if i < bytes.len() && bytes[i] == b'(' {
                theme::tool()
            } else if rules.hash_directive && start > 0 && bytes[start - 1] == b'#' {
                // `#include` etc. The `#` itself was emitted as
                // plain in the previous span; recolor would require
                // backtracking. Acceptable as-is — most viewers will
                // perceive the keyword color as covering the
                // directive name and read `#` as plain punctuation.
                theme::user()
            } else {
                theme::agent()
            };
            spans.push(Span {
                text: CompactString::new(word),
                color,
            });
            continue;
        }

        // Punctuation / whitespace / anything else: lump consecutive
        // non-special chars into one plain span to avoid producing
        // hundreds of single-char spans.
        let start = i;
        while i < bytes.len() {
            let c = bytes[i] as char;
            if is_ident_start(c, rules) || c.is_ascii_digit() || rules.string_delims.contains(&c) {
                break;
            }
            if let Some(marker) = rules.line_comment
                && bytes[i..].starts_with(marker.as_bytes())
            {
                break;
            }
            if let Some((open, _)) = rules.block_comment
                && bytes[i..].starts_with(open.as_bytes())
            {
                break;
            }
            i += utf8_char_len(bytes[i]).max(1);
        }
        if i == start {
            // Defensive: never emit empty spans / loop forever.
            i += 1;
        }
        spans.push(Span {
            text: CompactString::new(&line[start..i.min(bytes.len())]),
            color: theme::agent(),
        });
    }

    (spans, false)
}

fn is_ident_start(c: char, _rules: &Rules) -> bool {
    c.is_ascii_alphabetic() || c == '_' || c == '$'
}

fn is_ident_cont(c: char, rules: &Rules) -> bool {
    // Accept non-ASCII letters as identifier continuation so a
    // unicode-named symbol (`naïve`, `日本語`) stays one token
    // instead of splitting at the first non-ASCII byte. ASCII path
    // unchanged. Review #4.
    c.is_ascii_alphanumeric()
        || c == '_'
        || c == '$'
        || (!c.is_ascii() && !c.is_control() && !c.is_whitespace())
        || (rules.string_delims.is_empty() && c == '-')
}

fn looks_like_type(word: &str, _rules: &Rules) -> bool {
    // Heuristic: identifier starting with uppercase ASCII letter,
    // ≥3 chars. Tightened from ≥2 to skip false-positives like
    // `Ok`/`No`/`Hi`/`Id` — short capitalised words are usually
    // identifiers / variables, not type names. Real 2-char Rust
    // types (`Ok`) are in the `Result`/`Option` keyword family and
    // get colored via the types table when listed there. Review #5.
    let bytes = word.as_bytes();
    bytes.len() >= 3
        && bytes[0].is_ascii_uppercase()
        && bytes[1..].iter().any(|b| b.is_ascii_lowercase())
}

fn utf8_char_len(first_byte: u8) -> usize {
    if first_byte < 0xC0 {
        // ASCII (<0x80) or invalid continuation byte (0x80..0xC0):
        // advance by 1 in both cases.
        1
    } else if first_byte < 0xE0 {
        2
    } else if first_byte < 0xF0 {
        3
    } else {
        4
    }
}

fn find_subseq(haystack: &[u8], needle: &[u8]) -> Option<usize> {
    if needle.is_empty() || haystack.len() < needle.len() {
        return None;
    }
    haystack.windows(needle.len()).position(|w| w == needle)
}

// --- Language rule tables ---

static JS_RULES: Rules = Rules {
    keywords: &[
        "abstract",
        "as",
        "async",
        "await",
        "break",
        "case",
        "catch",
        "class",
        "const",
        "continue",
        "debugger",
        "default",
        "delete",
        "do",
        "else",
        "enum",
        "export",
        "extends",
        "false",
        "finally",
        "for",
        "from",
        "function",
        "get",
        "if",
        "implements",
        "import",
        "in",
        "instanceof",
        "interface",
        "is",
        "let",
        "new",
        "null",
        "of",
        "package",
        "private",
        "protected",
        "public",
        "readonly",
        "return",
        "satisfies",
        "set",
        "static",
        "super",
        "switch",
        "this",
        "throw",
        "true",
        "try",
        "type",
        "typeof",
        "undefined",
        "var",
        "void",
        "while",
        "with",
        "yield",
    ],
    types: &[
        "string", "number", "boolean", "object", "any", "unknown", "never", "bigint", "symbol",
        "Promise", "Array", "Map", "Set", "Date", "RegExp", "Error",
    ],
    line_comment: Some("//"),
    block_comment: Some(("/*", "*/")),
    string_delims: &['"', '\'', '`'],
    hash_directive: false,
};

static PY_RULES: Rules = Rules {
    keywords: &[
        "False", "None", "True", "and", "as", "assert", "async", "await", "break", "class",
        "continue", "def", "del", "elif", "else", "except", "finally", "for", "from", "global",
        "if", "import", "in", "is", "lambda", "match", "nonlocal", "not", "or", "pass", "raise",
        "return", "try", "while", "with", "yield",
    ],
    types: &[
        "int", "float", "str", "bool", "list", "tuple", "dict", "set", "bytes", "None",
    ],
    line_comment: Some("#"),
    block_comment: None,
    string_delims: &['"', '\''],
    hash_directive: false,
};

static BASH_RULES: Rules = Rules {
    keywords: &[
        "if", "then", "else", "elif", "fi", "case", "esac", "for", "select", "while", "until",
        "do", "done", "in", "function", "return", "break", "continue", "exit", "export", "local",
        "readonly", "declare", "typeset", "unset", "alias", "trap", "source", "eval", "exec",
    ],
    types: &[],
    line_comment: Some("#"),
    block_comment: None,
    string_delims: &['"', '\''],
    hash_directive: false,
};

static CLJ_RULES: Rules = Rules {
    keywords: &[
        "def",
        "defn",
        "defn-",
        "defmacro",
        "defmulti",
        "defmethod",
        "defprotocol",
        "defrecord",
        "deftype",
        "defstruct",
        "deflinked-type",
        "definterface",
        "defonce",
        "defproject",
        "fn",
        "let",
        "letfn",
        "do",
        "quote",
        "var",
        "if",
        "if-not",
        "if-let",
        "if-some",
        "when",
        "when-not",
        "when-let",
        "when-some",
        "cond",
        "condp",
        "case",
        "loop",
        "recur",
        "try",
        "catch",
        "finally",
        "throw",
        "and",
        "or",
        "not",
        "nil",
        "true",
        "false",
        "ns",
        "require",
        "import",
        "use",
        "in-ns",
    ],
    types: &[],
    line_comment: Some(";"),
    block_comment: None,
    string_delims: &['"'],
    hash_directive: false,
};

static GO_RULES: Rules = Rules {
    keywords: &[
        "break",
        "case",
        "chan",
        "const",
        "continue",
        "default",
        "defer",
        "else",
        "fallthrough",
        "for",
        "func",
        "go",
        "goto",
        "if",
        "import",
        "interface",
        "map",
        "package",
        "range",
        "return",
        "select",
        "struct",
        "switch",
        "type",
        "var",
        "nil",
        "true",
        "false",
        "iota",
    ],
    types: &[
        "bool",
        "byte",
        "complex64",
        "complex128",
        "error",
        "float32",
        "float64",
        "int",
        "int8",
        "int16",
        "int32",
        "int64",
        "rune",
        "string",
        "uint",
        "uint8",
        "uint16",
        "uint32",
        "uint64",
        "uintptr",
    ],
    line_comment: Some("//"),
    block_comment: Some(("/*", "*/")),
    string_delims: &['"', '\'', '`'],
    hash_directive: false,
};

static RUBY_RULES: Rules = Rules {
    keywords: &[
        "BEGIN",
        "END",
        "alias",
        "and",
        "begin",
        "break",
        "case",
        "class",
        "def",
        "defined?",
        "do",
        "else",
        "elsif",
        "end",
        "ensure",
        "false",
        "for",
        "if",
        "in",
        "module",
        "next",
        "nil",
        "not",
        "or",
        "redo",
        "rescue",
        "retry",
        "return",
        "self",
        "super",
        "then",
        "true",
        "undef",
        "unless",
        "until",
        "when",
        "while",
        "yield",
        "require",
        "require_relative",
        "include",
        "extend",
        "attr_accessor",
        "attr_reader",
        "attr_writer",
    ],
    types: &[],
    line_comment: Some("#"),
    block_comment: None,
    string_delims: &['"', '\''],
    hash_directive: false,
};

static RUST_RULES: Rules = Rules {
    keywords: &[
        "as", "async", "await", "break", "const", "continue", "crate", "dyn", "else", "enum",
        "extern", "false", "fn", "for", "if", "impl", "in", "let", "loop", "match", "mod", "move",
        "mut", "pub", "ref", "return", "self", "Self", "static", "struct", "super", "trait",
        "true", "type", "union", "unsafe", "use", "where", "while", "yield",
    ],
    types: &[
        "bool", "char", "f32", "f64", "i8", "i16", "i32", "i64", "i128", "isize", "u8", "u16",
        "u32", "u64", "u128", "usize", "str", "String", "Vec", "Option", "Result", "Box", "Rc",
        // Common variant constructors (idiomatically used like
        // type/variant names; review #5 tightened the substring
        // heuristic so 2-char `Ok`/`Err` no longer hit by accident).
        "Ok", "Err", "Some", "None", "Arc", "RefCell", "Cell", "HashMap", "HashSet", "BTreeMap",
        "BTreeSet",
    ],
    line_comment: Some("//"),
    block_comment: Some(("/*", "*/")),
    string_delims: &['"', '\''],
    hash_directive: false,
};

static JAVA_RULES: Rules = Rules {
    keywords: &[
        "abstract",
        "assert",
        "boolean",
        "break",
        "byte",
        "case",
        "catch",
        "char",
        "class",
        "const",
        "continue",
        "default",
        "do",
        "double",
        "else",
        "enum",
        "extends",
        "final",
        "finally",
        "float",
        "for",
        "goto",
        "if",
        "implements",
        "import",
        "instanceof",
        "int",
        "interface",
        "long",
        "native",
        "new",
        "null",
        "package",
        "private",
        "protected",
        "public",
        "return",
        "short",
        "static",
        "strictfp",
        "super",
        "switch",
        "synchronized",
        "this",
        "throw",
        "throws",
        "transient",
        "true",
        "false",
        "try",
        "void",
        "volatile",
        "while",
        "yield",
        "record",
        "sealed",
        "permits",
        "non-sealed",
    ],
    types: &[
        "String", "Object", "Integer", "Long", "Double", "Boolean", "List", "Map", "Set",
    ],
    line_comment: Some("//"),
    block_comment: Some(("/*", "*/")),
    string_delims: &['"', '\''],
    hash_directive: false,
};

static C_RULES: Rules = Rules {
    keywords: &[
        "auto",
        "break",
        "case",
        "char",
        "const",
        "continue",
        "default",
        "do",
        "double",
        "else",
        "enum",
        "extern",
        "float",
        "for",
        "goto",
        "if",
        "int",
        "long",
        "register",
        "return",
        "short",
        "signed",
        "sizeof",
        "static",
        "struct",
        "switch",
        "typedef",
        "union",
        "unsigned",
        "void",
        "volatile",
        "while",
        "inline",
        "restrict",
        "_Bool",
        "_Complex",
        "_Imaginary",
        "include",
        "define",
        "ifdef",
        "ifndef",
        "endif",
        "pragma",
        "error",
        "undef",
        "elif",
    ],
    types: &[
        "size_t",
        "ssize_t",
        "ptrdiff_t",
        "intptr_t",
        "uintptr_t",
        "int8_t",
        "int16_t",
        "int32_t",
        "int64_t",
        "uint8_t",
        "uint16_t",
        "uint32_t",
        "uint64_t",
        "FILE",
        "NULL",
    ],
    line_comment: Some("//"),
    block_comment: Some(("/*", "*/")),
    string_delims: &['"', '\''],
    hash_directive: true,
};

static CPP_RULES: Rules = Rules {
    keywords: &[
        "alignas",
        "alignof",
        "and",
        "and_eq",
        "asm",
        "auto",
        "bitand",
        "bitor",
        "bool",
        "break",
        "case",
        "catch",
        "char",
        "char16_t",
        "char32_t",
        "class",
        "compl",
        "const",
        "constexpr",
        "const_cast",
        "continue",
        "decltype",
        "default",
        "delete",
        "do",
        "double",
        "dynamic_cast",
        "else",
        "enum",
        "explicit",
        "export",
        "extern",
        "false",
        "float",
        "for",
        "friend",
        "goto",
        "if",
        "inline",
        "int",
        "long",
        "mutable",
        "namespace",
        "new",
        "noexcept",
        "not",
        "not_eq",
        "nullptr",
        "operator",
        "or",
        "or_eq",
        "private",
        "protected",
        "public",
        "register",
        "reinterpret_cast",
        "return",
        "short",
        "signed",
        "sizeof",
        "static",
        "static_assert",
        "static_cast",
        "struct",
        "switch",
        "template",
        "this",
        "thread_local",
        "throw",
        "true",
        "try",
        "typedef",
        "typeid",
        "typename",
        "union",
        "unsigned",
        "using",
        "virtual",
        "void",
        "volatile",
        "wchar_t",
        "while",
        "xor",
        "xor_eq",
        "concept",
        "requires",
        "co_await",
        "co_return",
        "co_yield",
    ],
    types: &[
        "size_t",
        "ssize_t",
        "ptrdiff_t",
        "string",
        "vector",
        "map",
        "set",
        "unordered_map",
        "unordered_set",
        "shared_ptr",
        "unique_ptr",
        "weak_ptr",
        "optional",
        "variant",
    ],
    line_comment: Some("//"),
    block_comment: Some(("/*", "*/")),
    string_delims: &['"', '\''],
    hash_directive: true,
};

static JSON_RULES: Rules = Rules {
    keywords: &["true", "false", "null"],
    types: &[],
    line_comment: None,
    block_comment: None,
    string_delims: &['"'],
    hash_directive: false,
};

static YAML_RULES: Rules = Rules {
    keywords: &["true", "false", "null", "yes", "no", "on", "off"],
    types: &[],
    line_comment: Some("#"),
    block_comment: None,
    string_delims: &['"', '\''],
    hash_directive: false,
};

static TOML_RULES: Rules = Rules {
    keywords: &["true", "false"],
    types: &[],
    line_comment: Some("#"),
    block_comment: None,
    string_delims: &['"', '\''],
    hash_directive: false,
};

static SQL_RULES: Rules = Rules {
    keywords: &[
        "SELECT",
        "FROM",
        "WHERE",
        "GROUP",
        "BY",
        "ORDER",
        "HAVING",
        "JOIN",
        "LEFT",
        "RIGHT",
        "INNER",
        "OUTER",
        "FULL",
        "ON",
        "AS",
        "AND",
        "OR",
        "NOT",
        "NULL",
        "IS",
        "IN",
        "LIKE",
        "BETWEEN",
        "INSERT",
        "UPDATE",
        "DELETE",
        "INTO",
        "VALUES",
        "SET",
        "CREATE",
        "TABLE",
        "DROP",
        "ALTER",
        "INDEX",
        "VIEW",
        "PRIMARY",
        "KEY",
        "FOREIGN",
        "REFERENCES",
        "DEFAULT",
        "UNIQUE",
        "CHECK",
        "CONSTRAINT",
        "CASCADE",
        "TRUE",
        "FALSE",
        "LIMIT",
        "OFFSET",
        "WITH",
        "UNION",
        "ALL",
        "DISTINCT",
        "CASE",
        "WHEN",
        "THEN",
        "ELSE",
        "END",
        "BEGIN",
        "COMMIT",
        "ROLLBACK",
        "TRANSACTION",
        // also lowercase variants
        "select",
        "from",
        "where",
        "group",
        "by",
        "order",
        "having",
        "join",
        "left",
        "right",
        "inner",
        "outer",
        "full",
        "on",
        "as",
        "and",
        "or",
        "not",
        "null",
        "is",
        "in",
        "like",
        "between",
        "insert",
        "update",
        "delete",
        "into",
        "values",
        "set",
        "create",
        "table",
        "drop",
        "alter",
        "index",
        "view",
        "primary",
        "key",
        "foreign",
        "references",
        "default",
        "unique",
        "check",
        "constraint",
        "cascade",
        "true",
        "false",
        "limit",
        "offset",
        "with",
        "union",
        "all",
        "distinct",
        "case",
        "when",
        "then",
        "else",
        "end",
        "begin",
        "commit",
        "rollback",
        "transaction",
    ],
    types: &[
        "INT",
        "INTEGER",
        "VARCHAR",
        "TEXT",
        "BOOLEAN",
        "DATE",
        "TIMESTAMP",
        "FLOAT",
        "DOUBLE",
    ],
    line_comment: Some("--"),
    block_comment: Some(("/*", "*/")),
    string_delims: &['\''],
    hash_directive: false,
};

#[cfg(test)]
mod tests {
    use super::*;

    fn render(code: &str, lang: &str) -> Vec<Vec<Span>> {
        highlight_code(code, lang)
    }

    /// Smoke: every supported language returns the right number of
    /// lines and at least one keyword-coloured span on a recognized
    /// keyword.
    #[test]
    fn rust_basic_keywords_colored() {
        let lines = render("fn main() {}", "rust");
        assert_eq!(lines.len(), 1);
        let row = &lines[0];
        let fn_span = row.iter().find(|s| s.text == "fn").expect("fn span");
        assert_eq!(fn_span.color, theme::user());
    }

    #[test]
    fn strings_get_accent_color() {
        let lines = render(r#"let s = "hello";"#, "rust");
        let row = &lines[0];
        let str_span = row.iter().find(|s| s.text == "\"hello\"").expect("string");
        assert_eq!(str_span.color, theme::accent());
    }

    #[test]
    fn line_comments_dim() {
        let lines = render("let x = 1; // comment", "rust");
        let row = &lines[0];
        let com = row
            .iter()
            .find(|s| s.text.contains("comment"))
            .expect("comment");
        assert_eq!(com.color, theme::dim());
    }

    #[test]
    fn block_comment_spans_lines() {
        let lines = render("a\n/* multi\nline */\nb", "rust");
        // Lines 2 and 3 should be entirely dim.
        let line2 = &lines[1];
        let line3 = &lines[2];
        assert!(line2.iter().all(|s| s.color == theme::dim()));
        assert!(line3.iter().any(|s| s.color == theme::dim()));
    }

    #[test]
    fn unknown_lang_falls_back_to_uniform_color() {
        let lines = render("nonsense ::: gibberish", "fortran");
        assert_eq!(lines.len(), 1);
        assert_eq!(lines[0].len(), 1);
        assert_eq!(lines[0][0].color, theme::tool());
    }

    /// `0xDEADBEEF`, `42.0`, `0b1010`, `1_000_000` all get number color.
    #[test]
    fn number_literals_colored() {
        for n in &["42", "3.14", "0xDEADBEEF", "0b1010", "1_000_000"] {
            let lines = render(n, "rust");
            assert_eq!(lines[0][0].color, theme::warn(), "for {n}");
        }
    }

    /// Capitalized identifiers in Rust look like types.
    #[test]
    fn capitalized_words_get_type_color() {
        let lines = render("let v: Vec<String> = Vec::new();", "rust");
        let row = &lines[0];
        assert!(
            row.iter()
                .any(|s| s.text == "Vec" && s.color == theme::header())
        );
    }

    /// Python triple-quoted strings are NOT block comments — they're
    /// strings — but our line-based highlighter treats `"""..."""` as
    /// a string spanning one line only. Verify single-line case.
    #[test]
    fn python_def_keyword_colored() {
        let lines = render("def hello():\n    pass", "python");
        let def = lines[0].iter().find(|s| s.text == "def").expect("def");
        assert_eq!(def.color, theme::user());
    }

    /// SQL is case-insensitive but our keyword list includes both
    /// variants — verify both color.
    #[test]
    fn sql_keywords_case_variants_both_match() {
        let lines = render("SELECT * FROM t WHERE x = 1", "sql");
        let row = &lines[0];
        assert!(
            row.iter()
                .any(|s| s.text == "SELECT" && s.color == theme::user())
        );
        let lines = render("select * from t where x = 1", "sql");
        let row = &lines[0];
        assert!(
            row.iter()
                .any(|s| s.text == "select" && s.color == theme::user())
        );
    }

    /// Language-id aliases collapse to the same Rules.
    #[test]
    fn lang_aliases_normalize() {
        assert_eq!(normalize_lang("ts"), "typescript");
        assert_eq!(normalize_lang("tsx"), "typescript");
        assert_eq!(normalize_lang("py"), "python");
        assert_eq!(normalize_lang("c++"), "cpp");
        assert_eq!(normalize_lang("rust,no_run"), "rust");
        assert_eq!(normalize_lang("rust ignore"), "rust");
    }
}
