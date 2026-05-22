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
        if !self.enabled {
            let mut cmd = Command::new("bash");
            cmd.arg("-c").arg(command);
            return cmd;
        }

        let mut cmd = Command::new("bwrap");
        cmd.args(["--ro-bind", "/", "/", "--bind"]);
        cmd.arg(cwd.as_os_str());
        cmd.arg(cwd.as_os_str());
        cmd.args([
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
        cmd
    }
}
