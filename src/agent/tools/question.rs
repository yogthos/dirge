use rig::completion::ToolDefinition;
use rig::tool::Tool;
use serde::Deserialize;
use tokio::sync::{mpsc, oneshot};
use uuid::Uuid;

use crate::agent::tools::ToolError;

pub type QuestionSender = mpsc::Sender<QuestionRequest>;
pub type QuestionReceiver = mpsc::Receiver<QuestionRequest>;

#[derive(Debug)]
pub struct QuestionRequest {
    #[allow(dead_code)]
    pub id: String,
    pub questions: Vec<QuestionItem>,
    pub reply: oneshot::Sender<QuestionResponse>,
}

#[derive(Debug)]
pub enum QuestionResponse {
    Answered(Vec<Vec<String>>),
    Rejected,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QuestionArgs {
    pub questions: Vec<QuestionItem>,
}

#[derive(Deserialize, Debug, Clone)]
pub struct QuestionItem {
    pub question: String,
    pub header: Option<String>,
    pub options: Vec<QuestionOption>,
    #[serde(default)]
    pub multi_select: Option<bool>,
    #[serde(default = "default_custom")]
    pub custom: bool,
}

fn default_custom() -> bool {
    true
}

#[derive(Deserialize, Debug, Clone)]
pub struct QuestionOption {
    pub label: String,
    pub description: String,
}

pub struct QuestionTool {
    pub question_tx: QuestionSender,
}

impl QuestionTool {
    pub fn new(question_tx: QuestionSender) -> Self {
        Self { question_tx }
    }
}

impl Tool for QuestionTool {
    const NAME: &'static str = "question";

    type Error = ToolError;
    type Args = QuestionArgs;
    type Output = String;

    async fn definition(&self, _prompt: String) -> ToolDefinition {
        ToolDefinition {
            name: "question".to_string(),
            description: "Ask the user one or more structured questions. Use when you need clarification, decisions, or preferences. The tool blocks until the user answers. Each question has options with labels and descriptions; set multi_select to true for multiple-choice. The custom option (default true) adds a free-text 'Type your own answer' choice."
                .to_string(),
            parameters: serde_json::json!({
                "type": "object",
                "properties": {
                    "questions": {
                        "type": "array",
                        "description": "List of questions to ask the user",
                        "items": {
                            "type": "object",
                            "properties": {
                                "question": {
                                    "type": "string",
                                    "description": "The question text"
                                },
                                "header": {
                                    "type": "string",
                                    "description": "Optional section heading displayed above the question"
                                },
                                "options": {
                                    "type": "array",
                                    "description": "Answer choices for the user",
                                    "items": {
                                        "type": "object",
                                        "properties": {
                                            "label": {"type": "string", "description": "Short display label for the option"},
                                            "description": {"type": "string", "description": "Explanation of what this choice means"}
                                        },
                                        "required": ["label", "description"]
                                    }
                                },
                                "multi_select": {
                                    "type": "boolean",
                                    "description": "Allow selecting multiple options (default: false)"
                                },
                                "custom": {
                                    "type": "boolean",
                                    "description": "Whether to show a 'Type your own answer' option (default: true)"
                                }
                            },
                            "required": ["question", "options"]
                        }
                    }
                },
                "required": ["questions"]
            }),
        }
    }

    async fn call(&self, args: QuestionArgs) -> Result<String, ToolError> {
        let id = Uuid::new_v4().to_string();
        let (reply_tx, reply_rx) = oneshot::channel();

        self.question_tx
            .send(QuestionRequest {
                id,
                questions: args.questions.clone(),
                reply: reply_tx,
            })
            .await
            .map_err(|_| ToolError::Msg("question system unavailable".to_string()))?;

        match reply_rx.await {
            Ok(QuestionResponse::Answered(answers)) => {
                let mut output = String::new();
                for (i, (item, answer)) in args.questions.iter().zip(answers.iter()).enumerate() {
                    if i > 0 {
                        output.push_str("\n\n");
                    }
                    if let Some(header) = &item.header {
                        output.push_str(&format!("## {}\n", header));
                    }
                    output.push_str(&format!(
                        "**Q{}:** {}\n**A:** {}",
                        i + 1,
                        item.question,
                        answer.join(", ")
                    ));
                }
                if output.is_empty() {
                    output = "(no questions answered)".to_string();
                }
                Ok(output)
            }
            Ok(QuestionResponse::Rejected) => {
                Err(ToolError::Msg("question rejected by user".to_string()))
            }
            Err(_) => Err(ToolError::Msg("question channel closed unexpectedly".to_string())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn test_definition_has_correct_name() {
        let (tx, _rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);
        let def = tool.definition(String::new()).await;
        assert_eq!(def.name, "question");
    }

    #[tokio::test]
    async fn test_single_select_question() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);

        let args = QuestionArgs {
            questions: vec![QuestionItem {
                question: "What color?".to_string(),
                header: None,
                options: vec![
                    QuestionOption {
                        label: "Red".to_string(),
                        description: "Red color".to_string(),
                    },
                    QuestionOption {
                        label: "Blue".to_string(),
                        description: "Blue color".to_string(),
                    },
                ],
                multi_select: None,
                custom: false,
            }],
        };

        let handle = tokio::spawn(async move { tool.call(args).await });

        let req = rx.recv().await.unwrap();
        assert_eq!(req.questions.len(), 1);
        assert_eq!(req.questions[0].question, "What color?");

        let _ = req
            .reply
            .send(QuestionResponse::Answered(vec![vec!["Red".to_string()]]));

        let result = handle.await.unwrap().unwrap();
        assert!(result.contains("**Q1:** What color?"));
        assert!(result.contains("**A:** Red"));
    }

    #[tokio::test]
    async fn test_multi_select_question() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);

        let args = QuestionArgs {
            questions: vec![QuestionItem {
                question: "Pick colors".to_string(),
                header: Some("Colors".to_string()),
                options: vec![
                    QuestionOption {
                        label: "Red".to_string(),
                        description: "".to_string(),
                    },
                    QuestionOption {
                        label: "Blue".to_string(),
                        description: "".to_string(),
                    },
                ],
                multi_select: Some(true),
                custom: false,
            }],
        };

        let handle = tokio::spawn(async move { tool.call(args).await });

        let req = rx.recv().await.unwrap();
        let _ = req.reply.send(QuestionResponse::Answered(vec![vec![
            "Red".to_string(),
            "Blue".to_string(),
        ]]));

        let result = handle.await.unwrap().unwrap();
        assert!(result.contains("## Colors"));
        assert!(result.contains("**A:** Red, Blue"));
    }

    #[tokio::test]
    async fn test_reject_returns_error() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);

        let args = QuestionArgs {
            questions: vec![QuestionItem {
                question: "What?".to_string(),
                header: None,
                options: vec![QuestionOption {
                    label: "A".to_string(),
                    description: "".to_string(),
                }],
                multi_select: None,
                custom: false,
            }],
        };

        let handle = tokio::spawn(async move { tool.call(args).await });

        let req = rx.recv().await.unwrap();
        let _ = req.reply.send(QuestionResponse::Rejected);

        let result = handle.await.unwrap();
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("rejected"));
    }

    #[tokio::test]
    async fn test_multiple_questions() {
        let (tx, mut rx) = mpsc::channel(1);
        let tool = QuestionTool::new(tx);

        let args = QuestionArgs {
            questions: vec![
                QuestionItem {
                    question: "Q1".to_string(),
                    header: None,
                    options: vec![QuestionOption {
                        label: "A1".to_string(),
                        description: "".to_string(),
                    }],
                    multi_select: None,
                    custom: false,
                },
                QuestionItem {
                    question: "Q2".to_string(),
                    header: None,
                    options: vec![QuestionOption {
                        label: "A2".to_string(),
                        description: "".to_string(),
                    }],
                    multi_select: None,
                    custom: false,
                },
            ],
        };

        let handle = tokio::spawn(async move { tool.call(args).await });

        let req = rx.recv().await.unwrap();
        assert_eq!(req.questions.len(), 2);
        let _ = req.reply.send(QuestionResponse::Answered(vec![
            vec!["A1".to_string()],
            vec!["A2".to_string()],
        ]));

        let result = handle.await.unwrap().unwrap();
        assert!(result.contains("Q1"));
        assert!(result.contains("Q2"));
        assert!(result.contains("A1"));
        assert!(result.contains("A2"));
    }
}
