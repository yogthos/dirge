use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};

use crate::agent::tools::ToolError;

pub type PlanSwitchSender = mpsc::Sender<PlanSwitchRequest>;
pub type PlanSwitchReceiver = mpsc::Receiver<PlanSwitchRequest>;

#[derive(Debug)]
pub struct PlanSwitchRequest {
    pub action: PlanAction,
    pub reply: oneshot::Sender<PlanSwitchResponse>,
}

#[derive(Debug, Clone, Copy)]
pub enum PlanAction {
    Enter,
    Exit,
}

#[derive(Debug)]
pub enum PlanSwitchResponse {
    Accepted,
    Rejected,
}

// --- plan_enter ---

pub struct PlanEnterTool {
    plan_tx: PlanSwitchSender,
}

impl PlanEnterTool {
    pub fn new(plan_tx: PlanSwitchSender) -> Self {
        Self { plan_tx }
    }
}

#[derive(Deserialize)]
pub struct PlanEnterArgs {}

impl Tool for PlanEnterTool {
    const NAME: &'static str = "plan_enter";

    type Error = ToolError;
    type Args = PlanEnterArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "plan_enter".to_string(),
            description: "Suggest switching to plan mode for complex tasks. The user will be asked to confirm. In plan mode, the agent uses a planning prompt that focuses on analysis and creating a detailed implementation plan rather than writing code."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    async fn call(&self, _args: PlanEnterArgs) -> Result<String, ToolError> {
        let (reply_tx, reply_rx) = oneshot::channel();

        self.plan_tx
            .send(PlanSwitchRequest {
                action: PlanAction::Enter,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ToolError::Msg("plan system unavailable".to_string()))?;

        match reply_rx.await {
            Ok(PlanSwitchResponse::Accepted) => Ok("plan mode activated".to_string()),
            Ok(PlanSwitchResponse::Rejected) => {
                Err(ToolError::Msg("user declined plan mode".to_string()))
            }
            Err(_) => Err(ToolError::Msg(
                "plan channel closed unexpectedly".to_string(),
            )),
        }
    }
}

// --- plan_exit ---

pub struct PlanExitTool {
    plan_tx: PlanSwitchSender,
}

impl PlanExitTool {
    pub fn new(plan_tx: PlanSwitchSender) -> Self {
        Self { plan_tx }
    }
}

#[derive(Deserialize)]
pub struct PlanExitArgs {}

impl Tool for PlanExitTool {
    const NAME: &'static str = "plan_exit";

    type Error = ToolError;
    type Args = PlanExitArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "plan_exit".to_string(),
            description: "Suggest switching from plan mode to implementation mode. The user will be asked to confirm. The agent will switch to the code prompt for writing and executing code."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {},
                "required": []
            }),
        }
    }

    async fn call(&self, _args: PlanExitArgs) -> Result<String, ToolError> {
        let (reply_tx, reply_rx) = oneshot::channel();

        self.plan_tx
            .send(PlanSwitchRequest {
                action: PlanAction::Exit,
                reply: reply_tx,
            })
            .await
            .map_err(|_| ToolError::Msg("plan system unavailable".to_string()))?;

        match reply_rx.await {
            Ok(PlanSwitchResponse::Accepted) => Ok("switched to implementation mode".to_string()),
            Ok(PlanSwitchResponse::Rejected) => {
                Err(ToolError::Msg("user declined mode switch".to_string()))
            }
            Err(_) => Err(ToolError::Msg(
                "plan channel closed unexpectedly".to_string(),
            )),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_plan_enter_accepted() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = PlanEnterTool::new(tx);

        let handle = tokio::spawn(async move { tool.call(PlanEnterArgs {}).await });

        let req = rx.recv().await.unwrap();
        assert!(matches!(req.action, PlanAction::Enter));
        let _ = req.reply.send(PlanSwitchResponse::Accepted);

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result, "plan mode activated");
    }

    #[tokio::test]
    async fn test_plan_enter_rejected() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = PlanEnterTool::new(tx);

        let handle = tokio::spawn(async move { tool.call(PlanEnterArgs {}).await });

        let req = rx.recv().await.unwrap();
        let _ = req.reply.send(PlanSwitchResponse::Rejected);

        let result = handle.await.unwrap();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("declined"));
    }

    #[tokio::test]
    async fn test_plan_exit_accepted() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = PlanExitTool::new(tx);

        let handle = tokio::spawn(async move { tool.call(PlanExitArgs {}).await });

        let req = rx.recv().await.unwrap();
        assert!(matches!(req.action, PlanAction::Exit));
        let _ = req.reply.send(PlanSwitchResponse::Accepted);

        let result = handle.await.unwrap().unwrap();
        assert_eq!(result, "switched to implementation mode");
    }

    #[tokio::test]
    async fn test_plan_exit_rejected() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = PlanExitTool::new(tx);

        let handle = tokio::spawn(async move { tool.call(PlanExitArgs {}).await });

        let req = rx.recv().await.unwrap();
        let _ = req.reply.send(PlanSwitchResponse::Rejected);

        let result = handle.await.unwrap();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("declined"));
    }

    #[tokio::test]
    async fn test_both_definitions() {
        let (tx1, _rx) = mpsc::channel(1);
        let (tx2, _rx) = mpsc::channel(1);

        let enter = PlanEnterTool::new(tx1).definition(String::new()).await;
        assert_eq!(enter.name, "plan_enter");

        let exit = PlanExitTool::new(tx2).definition(String::new()).await;
        assert_eq!(exit.name, "plan_exit");
    }

    // Regression: a prior version of plan_exit wrote a "Implementation Plan"
    // placeholder to PLAN.md in CWD whenever the user accepted the mode
    // switch. That side-effect bypassed the file-write permission system and
    // surprised users whose CWD already contained an unrelated PLAN.md. The
    // fix removed the write entirely — this test guards against
    // re-introducing it by inspecting the source.
    #[test]
    fn regression_plan_exit_has_no_filesystem_side_effects() {
        let src = include_str!("plan.rs");
        // The impl block for PlanExitTool. We don't want fs::write or PLAN.md
        // string literals anywhere in the call() path.
        let impl_start = src
            .find("impl Tool for PlanExitTool")
            .expect("PlanExitTool impl present");
        let impl_end = src[impl_start..]
            .find("\n}\n")
            .map(|i| impl_start + i)
            .unwrap_or(src.len());
        let body = &src[impl_start..impl_end];
        assert!(
            !body.contains("PLAN.md"),
            "plan_exit must not reference PLAN.md (side-effect regression)"
        );
        assert!(
            !body.contains("fs::write"),
            "plan_exit must not write files (side-effect regression)"
        );
    }

    // Regression: dropping the receiver (UI not subscribed) must surface a
    // clean error rather than panic or hang.
    #[tokio::test]
    async fn plan_enter_channel_unavailable() {
        let (tx, rx) = mpsc::channel(1);
        drop(rx);
        let tool = PlanEnterTool::new(tx);
        let result = tool.call(PlanEnterArgs {}).await;
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("unavailable"),
            "expected 'unavailable' error",
        );
    }

    // Regression: if the UI accepts the request handle but drops the oneshot
    // before replying, the tool must error cleanly (channel closed) rather
    // than block forever.
    #[tokio::test]
    async fn plan_enter_reply_dropped() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = PlanEnterTool::new(tx);
        let handle = tokio::spawn(async move { tool.call(PlanEnterArgs {}).await });

        let req = rx.recv().await.unwrap();
        drop(req.reply);

        let result = handle.await.unwrap();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("channel closed"));
    }
}
