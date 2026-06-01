//! Headless run path for [`AnyAgent`]. Split out of `provider/mod.rs`
//! (dirge-4y4l stage 8): the `--print` / `--loop` entry point that drives
//! the agent loop and collects output for the non-interactive CLI modes.
//!
//! Child module of `provider`, so it reaches `AnyAgent`'s private fields and
//! `spawn_runner` directly (privacy = defining module + descendants).

use super::AnyAgent;
use crate::agent::runner;
use crate::event::AgentEvent;

impl AnyAgent {
    pub async fn run_print(
        &self,
        prompt: &str,
        max_turns: usize,
        output_format: crate::cli::OutputFormat,
    ) -> anyhow::Result<String> {
        // dirge-nqr: honor the cap explicitly even if the agent was
        // built with a different one. `run_print` is the headless
        // entry point — callers explicitly pass the cap they want.
        let agent = self.clone().with_max_turns(Some(max_turns));
        let start_instant = std::time::Instant::now();
        let session_id = runner::uuid_v4_simple();
        let mut num_turns: u32 = 0;
        let suppress_inline = !matches!(output_format, crate::cli::OutputFormat::Text);

        // Plugin `on-prompt` dispatch. Headless modes (--print, --loop)
        // previously skipped this — plugins that mutate the user prompt
        // or block it never fired in CI/script contexts.
        let effective_prompt: String = {
            #[cfg(feature = "plugin")]
            {
                if let Some(pm_arc) = crate::plugin::hook::global() {
                    let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
                    runner::resolve_prompt_with_hooks(prompt, &mut mgr)
                } else {
                    prompt.to_string()
                }
            }
            #[cfg(not(feature = "plugin"))]
            {
                prompt.to_string()
            }
        };

        // StreamJson init event — fires once at startup so downstream
        // tools can pick up cwd/session/model before any turns stream.
        if matches!(output_format, crate::cli::OutputFormat::StreamJson) {
            let cwd = std::env::current_dir()
                .map(|p| p.to_string_lossy().to_string())
                .unwrap_or_default();
            runner::emit_stream_json_event(serde_json::json!({
                "type": "system",
                "subtype": "init",
                "cwd": cwd,
                "session_id": session_id,
                "tools": Vec::<String>::new(),
                "model": "",
            }));
        }

        // Wire through the new agent_loop path: clone the agent (cheap
        // — Arc internals + refcounts), spawn a runner, and drain the
        // event channel collecting text. Use the max_turns-stamped
        // `agent` from above so the cap is honored.
        let runner = agent.spawn_runner(effective_prompt.clone(), Vec::new(), None);
        let task = runner.task;
        let mut event_rx = runner.event_rx;

        let mut full_response = String::new();
        let mut had_output = false;

        while let Some(event) = event_rx.recv().await {
            match event {
                AgentEvent::Token(text) => {
                    full_response.push_str(&text);
                    if !suppress_inline {
                        let safe = crate::ui::ansi::strip_controls(
                            &text,
                            crate::ui::ansi::StripPolicy::KEEP_NEWLINE,
                        );
                        print!("{safe}");
                        let _ = std::io::Write::flush(&mut std::io::stdout());
                    }
                    had_output = true;
                }
                AgentEvent::Done { response, .. } => {
                    // `Done.response` is the authoritative full text.
                    full_response = response.to_string();
                    break;
                }
                AgentEvent::Error(err) => {
                    if had_output {
                        println!();
                    }
                    eprintln!("Error: {}", err);
                    let _ = task.await;
                    return Err(anyhow::anyhow!("{}", err));
                }
                AgentEvent::TurnEnd { .. } => {
                    num_turns += 1;
                }
                AgentEvent::SystemNotice { content } => {
                    // dirge-originated runtime notice (e.g. the
                    // max-agent-turns cap). Headless drives output from
                    // events, so surface it to stderr — otherwise a
                    // truncated run looks like a clean success to a
                    // `--print` consumer.
                    if had_output {
                        println!();
                    }
                    eprintln!("{}", content);
                }
                // Plugin-driven model swap after last run puts the
                // request in the mgr; caller drains via
                // take_pending_next_model().
                _ => {}
            }
        }

        // Await the spawned task to catch any panics.
        let _ = task.await;

        // Plugin `on-response` + `on-complete` + `prepare-next-run`
        // dispatch. Headless modes previously skipped these.
        #[cfg(feature = "plugin")]
        if let Some(pm_arc) = crate::plugin::hook::global() {
            let mut mgr = pm_arc.lock().unwrap_or_else(|e| e.into_inner());
            let result = runner::apply_response_hooks(&full_response, &mut mgr);
            if let Some(replacement) = result.replacement {
                if suppress_inline {
                    full_response = replacement;
                } else {
                    println!();
                    println!("[plugin replace-result]");
                    let safe = crate::ui::ansi::strip_controls(
                        &replacement,
                        crate::ui::ansi::StripPolicy::KEEP_NEWLINE,
                    );
                    println!("{safe}");
                    full_response = replacement;
                }
            }
        }

        match output_format {
            crate::cli::OutputFormat::Text => {
                println!();
            }
            crate::cli::OutputFormat::Json => {
                let result = serde_json::json!({
                    "type": "result",
                    "subtype": "success",
                    "is_error": false,
                    "duration_ms": start_instant.elapsed().as_millis() as u64,
                    "num_turns": num_turns,
                    "result": full_response.clone(),
                    "session_id": session_id,
                    "total_cost_usd": 0.0,
                });
                if let Ok(s) = serde_json::to_string(&result) {
                    println!("{}", s);
                }
            }
            crate::cli::OutputFormat::StreamJson => {
                runner::emit_stream_json_event(serde_json::json!({
                    "type": "assistant",
                    "message": {
                        "role": "assistant",
                        "content": [{"type": "text", "text": full_response.clone()}],
                    },
                    "session_id": session_id,
                }));
                runner::emit_stream_json_event(serde_json::json!({
                    "type": "result",
                    "subtype": "success",
                    "is_error": false,
                    "duration_ms": start_instant.elapsed().as_millis() as u64,
                    "num_turns": num_turns,
                    "result": full_response.clone(),
                    "session_id": session_id,
                    "total_cost_usd": 0.0,
                }));
            }
        }
        Ok(full_response)
    }
}
