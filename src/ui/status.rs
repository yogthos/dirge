use std::path::Path;

use crate::session::Session;

pub struct StatusLine;

fn fmt_tokens(n: u64) -> String {
    if n >= 1_000_000 {
        format!("{:.1}M", n as f64 / 1_000_000.0)
    } else if n >= 1_000 {
        format!("{}k", n / 1000)
    } else {
        n.to_string()
    }
}

impl StatusLine {
    #[allow(clippy::too_many_arguments)]
    pub fn render(
        session: &Session,
        is_running: bool,
        _spinner_tick: u64,
        loop_label: Option<&str>,
        prompt_name: Option<&str>,
        perm_mode: Option<&str>,
    ) -> String {
        let state = if is_running { "running" } else { "ready" };
        let dir = Path::new(&session.working_dir)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or(&session.working_dir);

        let ctx = session.context_window;
        let used = session.total_estimated_tokens;
        let pct = if ctx > 0 { (used * 100) / ctx } else { 0 };

        // TODO(cost-tracking): `session.total_cost` is always 0.0
        // because dirge doesn't yet have a per-provider pricing
        // table — `AgentEvent::Done` emits `cost: 0.0` unconditionally
        // (see `src/agent/runner.rs::run_stream`). Until that's wired,
        // the cost segment is suppressed entirely to avoid showing a
        // misleading "$0.0000". When pricing lands, restore the
        // conditional formatter that was here previously.
        let cost_str = String::new();

        let compact_badge = if session.compactions.is_empty() {
            String::new()
        } else {
            format!(" cmp:{}", session.compactions.len())
        };

        let loop_badge = match loop_label {
            Some(label) => format!(" [{}]", label),
            None => String::new(),
        };

        let prompt_badge = match prompt_name {
            Some(name) => format!(" [{}]", name),
            None => String::new(),
        };

        let perm_badge = match perm_mode {
            Some(m) if m != "standard" => format!(" | mode:{}", m),
            _ => String::new(),
        };

        format!(
            "{}{} | {}{} | {}/{} ({}%) | {}msgs | {}{}{}{}",
            dir,
            cost_str,
            session.model,
            loop_badge,
            fmt_tokens(used),
            fmt_tokens(ctx),
            pct,
            session.messages.len(),
            state,
            compact_badge,
            prompt_badge,
            perm_badge,
        )
    }
}
