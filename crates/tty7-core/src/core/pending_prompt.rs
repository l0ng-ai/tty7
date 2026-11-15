//! What a waiting agent is asking the user, as structured data.
//!
//! The status alone says only that an agent is waiting, and its message is
//! a sentence for a status line. The hooks that report the wait already carry
//! the question itself, though: the tool the agent wants to run and with what,
//! or the questions and choices it put on screen. An [`AgentPrompt`] keeps
//! that, so a UI can show the choices without scraping the terminal.
//!
//! The hook process builds it (see `agent_hooks::build_hook_body`), since the
//! payload is only readable there, and sends it in the event body; the daemon
//! keeps it on the pane's [`AgentSessionState`] for as long as the pane is
//! waiting.
//!
//! # Where the fields come from
//!
//! Both agents send the same hook shape: `tool_name`, `tool_input`, and, on
//! `PreToolUse`, a `tool_use_id`.
//!
//! | Field                  | Claude Code                                  | Codex                                          |
//! |------------------------|----------------------------------------------|------------------------------------------------|
//! | question tool          | `AskUserQuestion` on `PreToolUse`, and on `PermissionRequest` where that fires for it | `request_user_input` on `PreToolUse` |
//! | question `text`        | `questions[].question`                       | `questions[].question`                         |
//! | question `header`      | `questions[].header`                         | `questions[].header`                           |
//! | question `options`     | `questions[].options[].{label,description}`  | `questions[].options[].{label,description}`    |
//! | question `multi_select`| `questions[].multiSelect`                    | always `false`: Codex has no multi-select      |
//! | question `allow_other` | always `true`: Claude always offers "Other"  | `questions[].isOther`; `true` with no options  |
//! | permission `tool`      | `tool_name` (`Bash`, `Edit`, `WebFetch`, `mcp__…`) | `tool_name` (`Bash`, `apply_patch`, `mcp__…`) |
//! | permission `summary`   | `tool_input.command`, else `file_path`, `notebook_path`, `path`, `url`, `pattern`, `query`, else the input as JSON | `tool_input.command`; for `apply_patch`, the files the patch touches |
//! | permission `detail`    | `tool_input.description` (`Bash`), `prompt` (`WebFetch`) | `tool_input.description`; for `apply_patch`, the patch |
//! | prompt `id`            | `tool_use_id` (`PreToolUse` only), else made up by the hook | `tool_use_id` (`PreToolUse` only), else made up |
//!
//! Codex hands its tool's arguments over as parsed JSON, or as the raw string
//! when they do not parse; a string is tried as JSON before giving up. Its
//! `isSecret` and `isBlocking` are not carried.
//!
//! Anything missing or of the wrong type yields no prompt rather than a
//! partial one, and every string is clipped: the prompt rides the hook event,
//! which on its fallback path is an OSC sequence on the agent's own tty.
//!
//! [`AgentSessionState`]: crate::core::cli_agent::AgentSessionState

use serde::{Deserialize, Deserializer, Serialize};
use serde_json::Value;

/// What a waiting agent asks, and the id it is known by while it waits.
#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentPrompt {
    /// Stable for as long as this one prompt is open, and different for the
    /// next one, even with the same content: a UI tells a new prompt from the
    /// one it already showed, and refuses an answer meant for one that is
    /// gone, by comparing it. Opaque; unique per pane, not per machine.
    pub id: String,
    #[serde(flatten)]
    pub kind: AgentPromptKind,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AgentPromptKind {
    /// The agent put one or more questions to the user, each answered on its
    /// own.
    Question { questions: Vec<AgentQuestion> },
    /// The agent wants to run a tool and needs the user's go-ahead.
    Permission {
        /// The tool, as the agent names it: `Bash`, `Edit`, `apply_patch`,
        /// `mcp__server__tool`, …
        tool: String,
        /// What it would do, in one line where it can: the command, the
        /// file, the URL. Empty when the tool's input says nothing usable.
        summary: String,
        /// More about it, when the agent gave more: the command's stated
        /// purpose, the patch.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        detail: Option<String>,
    },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentQuestion {
    /// A short tag for the question (Claude caps it at 12 characters).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub header: Option<String>,
    pub text: String,
    /// The choices, in the agent's order. Empty for a free-text question.
    #[serde(default)]
    pub options: Vec<AgentQuestionOption>,
    /// More than one option may be picked.
    #[serde(default)]
    pub multi_select: bool,
    /// An answer of the user's own words is accepted besides the options.
    #[serde(default)]
    pub allow_other: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct AgentQuestionOption {
    pub label: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
}

const ID_MAX: usize = 128;
const TOOL_MAX: usize = 100;
const SUMMARY_MAX: usize = 500;
const DETAIL_MAX: usize = 1000;
const HEADER_MAX: usize = 60;
const TEXT_MAX: usize = 500;
const LABEL_MAX: usize = 120;
const DESCRIPTION_MAX: usize = 300;
/// Claude allows four questions of four options each; Codex asks for fewer.
/// Room to spare, but a bound on what a hostile payload can make us carry.
const QUESTIONS_MAX: usize = 8;
const OPTIONS_MAX: usize = 10;

/// The tools that stop a turn to ask the user a question: Claude's
/// `AskUserQuestion` and Codex's `request_user_input`.
pub const QUESTION_TOOLS: &[&str] = &["AskUserQuestion", "request_user_input"];

impl AgentPrompt {
    /// Whether `other` asks the same thing, whatever its id. One prompt can
    /// be reported twice — Claude's `AskUserQuestion` fires `PreToolUse` and
    /// then `PermissionRequest` — and must keep the id it was first given.
    pub fn asks_the_same_as(&self, other: &AgentPrompt) -> bool {
        self.kind == other.kind
    }

    /// The prompt within the size caps, or `None` when nothing worth showing
    /// is left: no id, no tool, no question with any text.
    pub fn sanitized(self) -> Option<AgentPrompt> {
        let id = clip(&self.id, ID_MAX)?;
        let kind = match self.kind {
            AgentPromptKind::Question { questions } => {
                let questions: Vec<_> = questions
                    .into_iter()
                    .filter_map(AgentQuestion::sanitized)
                    .take(QUESTIONS_MAX)
                    .collect();
                if questions.is_empty() {
                    return None;
                }
                AgentPromptKind::Question { questions }
            }
            AgentPromptKind::Permission {
                tool,
                summary,
                detail,
            } => AgentPromptKind::Permission {
                tool: clip(&tool, TOOL_MAX)?,
                summary: clip(&summary, SUMMARY_MAX).unwrap_or_default(),
                detail: detail.and_then(|d| clip(&d, DETAIL_MAX)),
            },
        };
        Some(AgentPrompt { id, kind })
    }
}

impl AgentQuestion {
    fn sanitized(self) -> Option<AgentQuestion> {
        Some(AgentQuestion {
            header: self.header.and_then(|h| clip(&h, HEADER_MAX)),
            text: clip(&self.text, TEXT_MAX)?,
            options: self
                .options
                .into_iter()
                .filter_map(|o| {
                    Some(AgentQuestionOption {
                        label: clip(&o.label, LABEL_MAX)?,
                        description: o.description.and_then(|d| clip(&d, DESCRIPTION_MAX)),
                    })
                })
                .take(OPTIONS_MAX)
                .collect(),
            multi_select: self.multi_select,
            allow_other: self.allow_other,
        })
    }
}

/// The prompt a hook payload describes, for the tty7 events that open one
/// (`question-asked`, `permission-request`). `fresh_id` names it when the
/// payload carries no `tool_use_id`.
///
/// `None` for any other event, a payload without a tool, or a tool input of
/// the wrong shape.
pub fn from_hook_payload(
    event: &str,
    payload: &Value,
    fresh_id: impl FnOnce() -> String,
) -> Option<AgentPrompt> {
    if !matches!(event, "question-asked" | "permission-request") {
        return None;
    }
    let tool = payload.get("tool_name")?.as_str()?.trim();
    if tool.is_empty() {
        return None;
    }
    let input = tool_input(payload);
    let kind = if QUESTION_TOOLS.contains(&tool) {
        questions(tool, input.as_ref()?)?
    } else if event == "permission-request" {
        permission(tool, input.as_ref())
    } else {
        return None;
    };
    let id = payload
        .get("tool_use_id")
        .and_then(Value::as_str)
        .filter(|id| !id.trim().is_empty())
        .map_or_else(fresh_id, str::to_string);
    AgentPrompt { id, kind }.sanitized()
}

/// The tool's input as an object. Codex passes arguments that fail to parse
/// as the raw string; one that does parse is taken.
fn tool_input(payload: &Value) -> Option<Value> {
    match payload.get("tool_input")? {
        Value::String(raw) => serde_json::from_str::<Value>(raw)
            .ok()
            .filter(Value::is_object),
        v @ Value::Object(_) => Some(v.clone()),
        _ => None,
    }
}

fn questions(tool: &str, input: &Value) -> Option<AgentPromptKind> {
    let claude = tool == "AskUserQuestion";
    let questions = input
        .get("questions")?
        .as_array()?
        .iter()
        .map(|q| {
            let text = str_field(q, &["question", "text"])?;
            let options = match q.get("options") {
                None | Some(Value::Null) => Vec::new(),
                Some(options) => options
                    .as_array()?
                    .iter()
                    .map(|o| {
                        Some(AgentQuestionOption {
                            label: o.get("label")?.as_str()?.to_string(),
                            description: str_field(o, &["description"]),
                        })
                    })
                    .collect::<Option<Vec<_>>>()?,
            };
            let flag = |names: &[&str]| {
                names
                    .iter()
                    .find_map(|n| q.get(*n).and_then(Value::as_bool))
                    .unwrap_or(false)
            };
            Some(AgentQuestion {
                header: str_field(q, &["header"]),
                text,
                multi_select: flag(&["multiSelect", "multi_select"]),
                allow_other: claude || options.is_empty() || flag(&["isOther", "is_other"]),
                options,
            })
        })
        .collect::<Option<Vec<_>>>()?;
    Some(AgentPromptKind::Question { questions })
}

fn permission(tool: &str, input: Option<&Value>) -> AgentPromptKind {
    let (summary, detail) = input.map(permission_summary).unwrap_or_default();
    AgentPromptKind::Permission {
        tool: tool.to_string(),
        summary,
        detail,
    }
}

/// One line for what a tool would do, and the longer word on it if any.
///
/// The first of `command`, `file_path`, `notebook_path`, `path`, `url`,
/// `pattern`, `query` the input has; else the input itself as compact JSON.
/// A `command` given as an argv is joined with spaces. A patch (Codex's
/// `apply_patch`) is summed up by the files it touches, with the patch as
/// the detail. Otherwise the detail is `description`, or `prompt`.
fn permission_summary(input: &Value) -> (String, Option<String>) {
    let detail = str_field(input, &["description", "prompt"]);
    if let Some(command) = input.get("command") {
        let command = match command {
            Value::String(s) => Some(s.clone()),
            Value::Array(argv) => argv
                .iter()
                .map(|a| a.as_str().map(str::to_string))
                .collect::<Option<Vec<_>>>()
                .map(|argv| argv.join(" ")),
            _ => None,
        };
        if let Some(command) = command.filter(|c| !c.trim().is_empty()) {
            if let Some(files) = patch_files(&command) {
                return (files, Some(command));
            }
            return (command, detail);
        }
    }
    let summary = [
        "file_path",
        "notebook_path",
        "path",
        "url",
        "pattern",
        "query",
    ]
    .iter()
    .find_map(|k| str_field(input, &[k]))
    .or_else(|| {
        input
            .as_object()
            .filter(|o| !o.is_empty())
            .map(|_| input.to_string())
    })
    .unwrap_or_default();
    (summary, detail)
}

/// The files an `apply_patch` envelope touches, comma-separated; `None` for
/// anything that is not one.
fn patch_files(command: &str) -> Option<String> {
    if !command.trim_start().starts_with("*** Begin Patch") {
        return None;
    }
    let files: Vec<_> = command
        .lines()
        .filter_map(|l| {
            ["*** Add File: ", "*** Update File: ", "*** Delete File: "]
                .iter()
                .find_map(|p| l.strip_prefix(p))
        })
        .map(str::trim)
        .filter(|f| !f.is_empty())
        .collect();
    (!files.is_empty()).then(|| files.join(", "))
}

/// The first of `names` that holds a non-blank string.
fn str_field(v: &Value, names: &[&str]) -> Option<String> {
    names.iter().find_map(|n| {
        v.get(*n)
            .and_then(Value::as_str)
            .filter(|s| !s.trim().is_empty())
            .map(str::to_string)
    })
}

/// `text` trimmed and cut to `max` characters, or `None` when blank.
fn clip(text: &str, max: usize) -> Option<String> {
    let text = text.trim();
    if text.is_empty() {
        return None;
    }
    Some(match text.char_indices().nth(max) {
        Some((end, _)) => text[..end].to_string(),
        None => text.to_string(),
    })
}

/// Deserialize an optional prompt, taking one that does not parse — a newer
/// peer's kind, a malformed hook body — as none, so it cannot cost the rest
/// of the message it rides in.
pub fn lenient<'de, D: Deserializer<'de>>(d: D) -> Result<Option<AgentPrompt>, D::Error> {
    let value = Option::<Value>::deserialize(d)?;
    Ok(value
        .and_then(|v| serde_json::from_value::<AgentPrompt>(v).ok())
        .and_then(AgentPrompt::sanitized))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(event: &str, payload: &str) -> Option<AgentPrompt> {
        let payload: Value = serde_json::from_str(payload).expect("fixture is JSON");
        from_hook_payload(event, &payload, || "fresh".to_string())
    }

    /// Claude Code's `PreToolUse` stdin for a single-select `AskUserQuestion`.
    const CLAUDE_ASK_SINGLE: &str = r#"{
        "session_id": "4f1c2a9e-7a51-4c55-9b0e-0d5a3f7f1b2c",
        "transcript_path": "/Users/me/.claude/projects/-src-api/4f1c2a9e.jsonl",
        "cwd": "/src/api",
        "permission_mode": "default",
        "hook_event_name": "PreToolUse",
        "tool_name": "AskUserQuestion",
        "tool_input": {
            "questions": [{
                "question": "Which database should the cache use?",
                "header": "Cache",
                "multiSelect": false,
                "options": [
                    {"label": "Redis", "description": "Shared across instances"},
                    {"label": "In-memory", "description": "Per process, no new dependency"}
                ]
            }]
        },
        "tool_use_id": "toolu_01AbCdEfGhIjKlMnOpQrStUv"
    }"#;

    #[test]
    fn claude_single_select_question() {
        let prompt = parse("question-asked", CLAUDE_ASK_SINGLE).unwrap();
        assert_eq!(prompt.id, "toolu_01AbCdEfGhIjKlMnOpQrStUv");
        assert_eq!(
            prompt.kind,
            AgentPromptKind::Question {
                questions: vec![AgentQuestion {
                    header: Some("Cache".into()),
                    text: "Which database should the cache use?".into(),
                    options: vec![
                        AgentQuestionOption {
                            label: "Redis".into(),
                            description: Some("Shared across instances".into()),
                        },
                        AgentQuestionOption {
                            label: "In-memory".into(),
                            description: Some("Per process, no new dependency".into()),
                        },
                    ],
                    multi_select: false,
                    allow_other: true,
                }],
            }
        );
    }

    #[test]
    fn claude_multi_select_and_several_questions() {
        let payload = r#"{
            "session_id": "s", "hook_event_name": "PreToolUse",
            "tool_name": "AskUserQuestion",
            "tool_input": {"questions": [
                {"question": "Which checks should run?", "header": "Checks", "multiSelect": true,
                 "options": [{"label": "clippy", "description": "Lints"}, {"label": "tests"}, {"label": "fmt"}]},
                {"question": "Ship it after?", "header": "Ship", "multiSelect": false,
                 "options": [{"label": "Yes", "description": "Push"}, {"label": "No", "description": "Stop"}]}
            ]},
            "tool_use_id": "toolu_02"
        }"#;
        let Some(AgentPrompt {
            kind: AgentPromptKind::Question { questions },
            ..
        }) = parse("question-asked", payload)
        else {
            panic!("a question prompt");
        };
        assert_eq!(questions.len(), 2);
        assert!(questions[0].multi_select);
        assert!(!questions[1].multi_select);
        assert_eq!(questions[0].options[1].description, None);
        assert_eq!(
            questions[0]
                .options
                .iter()
                .map(|o| o.label.as_str())
                .collect::<Vec<_>>(),
            ["clippy", "tests", "fmt"]
        );
    }

    /// A `PermissionRequest` for `AskUserQuestion` is the same question, and
    /// has no `tool_use_id` to name it by.
    #[test]
    fn a_permission_request_for_a_question_is_the_question() {
        let mut payload: Value = serde_json::from_str(CLAUDE_ASK_SINGLE).unwrap();
        payload["hook_event_name"] = "PermissionRequest".into();
        payload.as_object_mut().unwrap().remove("tool_use_id");
        let asked = parse("question-asked", CLAUDE_ASK_SINGLE).unwrap();
        let requested = from_hook_payload("permission-request", &payload, || "x".into()).unwrap();
        assert_eq!(requested.id, "x");
        assert!(requested.asks_the_same_as(&asked));
    }

    #[test]
    fn claude_bash_permission() {
        let payload = r#"{
            "session_id": "4f1c2a9e-7a51-4c55-9b0e-0d5a3f7f1b2c",
            "transcript_path": "/Users/me/.claude/projects/-src-api/4f1c2a9e.jsonl",
            "cwd": "/src/api",
            "permission_mode": "default",
            "hook_event_name": "PermissionRequest",
            "tool_name": "Bash",
            "tool_input": {"command": "rm -rf target && cargo build", "description": "Clean rebuild"},
            "permission_suggestions": [{"type": "addRules", "rules": [{"toolName": "Bash", "ruleContent": "cargo build:*"}], "behavior": "allow", "destination": "localSettings"}]
        }"#;
        let prompt = parse("permission-request", payload).unwrap();
        assert_eq!(prompt.id, "fresh");
        assert_eq!(
            prompt.kind,
            AgentPromptKind::Permission {
                tool: "Bash".into(),
                summary: "rm -rf target && cargo build".into(),
                detail: Some("Clean rebuild".into()),
            }
        );
    }

    #[test]
    fn claude_file_and_web_permissions_name_their_target() {
        let edit = r#"{"tool_name": "Edit", "tool_input": {"file_path": "/src/api/main.rs", "old_string": "a", "new_string": "b"}}"#;
        let fetch = r#"{"tool_name": "WebFetch", "tool_input": {"url": "https://example.com", "prompt": "Summarise"}}"#;
        let mcp = r#"{"tool_name": "mcp__db__query", "tool_input": {"sql": "select 1"}}"#;
        let summary = |p: &str| match parse("permission-request", p).unwrap().kind {
            AgentPromptKind::Permission {
                summary, detail, ..
            } => (summary, detail),
            other => panic!("{other:?}"),
        };
        assert_eq!(summary(edit), ("/src/api/main.rs".into(), None));
        assert_eq!(
            summary(fetch),
            ("https://example.com".into(), Some("Summarise".into()))
        );
        assert_eq!(summary(mcp), (r#"{"sql":"select 1"}"#.into(), None));
    }

    /// Codex's `PreToolUse` stdin for `request_user_input`: its arguments as
    /// `codex_protocol::request_user_input::RequestUserInputArgs` has them.
    const CODEX_REQUEST_USER_INPUT: &str = r#"{
        "session_id": "0199a1b2-c3d4-7e5f-8a9b-0c1d2e3f4a5b",
        "turn_id": "0199a1b2-0000-7000-8000-000000000001",
        "transcript_path": "/Users/me/.codex/sessions/2026/11/13/rollout.jsonl",
        "cwd": "/src/api",
        "hook_event_name": "PreToolUse",
        "model": "gpt-5.5-codex",
        "permission_mode": "default",
        "tool_name": "request_user_input",
        "tool_use_id": "call_Q1w2E3r4T5y6",
        "tool_input": {
            "questions": [
                {"id": "target", "header": "Target", "question": "Deploy to which environment?",
                 "isOther": false,
                 "options": [{"label": "staging", "description": "Safe to break"},
                             {"label": "production", "description": "Real users"}]},
                {"id": "note", "header": "Note", "question": "Anything for the changelog?",
                 "isOther": false, "isSecret": false}
            ],
            "isBlocking": true
        }
    }"#;

    #[test]
    fn codex_request_user_input() {
        let prompt = parse("question-asked", CODEX_REQUEST_USER_INPUT).unwrap();
        assert_eq!(prompt.id, "call_Q1w2E3r4T5y6");
        let AgentPromptKind::Question { questions } = prompt.kind else {
            panic!("a question prompt");
        };
        assert_eq!(questions[0].text, "Deploy to which environment?");
        assert_eq!(questions[0].header.as_deref(), Some("Target"));
        assert_eq!(questions[0].options.len(), 2);
        assert!(
            !questions[0].allow_other,
            "Codex says when Other is offered"
        );
        assert!(!questions[0].multi_select);
        assert!(questions[1].options.is_empty());
        assert!(questions[1].allow_other, "free text is the only answer");
    }

    /// Codex passes arguments that do not parse as the raw string; ones that
    /// do parse are taken from it.
    #[test]
    fn codex_tool_input_given_as_a_string() {
        let mut payload: Value = serde_json::from_str(CODEX_REQUEST_USER_INPUT).unwrap();
        payload["tool_input"] = payload["tool_input"].to_string().into();
        let prompt = from_hook_payload("question-asked", &payload, String::new).unwrap();
        assert!(matches!(prompt.kind, AgentPromptKind::Question { .. }));
        payload["tool_input"] = "{not json".into();
        assert_eq!(
            from_hook_payload("question-asked", &payload, String::new),
            None
        );
    }

    #[test]
    fn codex_permissions_for_a_command_and_a_patch() {
        let bash = r#"{"session_id": "s", "turn_id": "t", "hook_event_name": "PermissionRequest",
            "model": "gpt-5.5-codex", "permission_mode": "default", "cwd": "/src", "transcript_path": null,
            "tool_name": "Bash", "tool_input": {"command": "git push --force", "description": "Publish the branch"}}"#;
        let argv = r#"{"tool_name": "Bash", "tool_input": {"command": ["bash", "-lc", "ls -la"]}}"#;
        let patch = r#"{"tool_name": "apply_patch", "tool_input": {"command": "*** Begin Patch\n*** Update File: src/a.rs\n@@\n-a\n+b\n*** Add File: src/b.rs\n+x\n*** End Patch"}}"#;
        let kind = |p: &str| parse("permission-request", p).unwrap().kind;
        assert_eq!(
            kind(bash),
            AgentPromptKind::Permission {
                tool: "Bash".into(),
                summary: "git push --force".into(),
                detail: Some("Publish the branch".into()),
            }
        );
        assert!(
            matches!(kind(argv), AgentPromptKind::Permission { summary, .. } if summary == "bash -lc ls -la")
        );
        let AgentPromptKind::Permission {
            tool,
            summary,
            detail,
        } = kind(patch)
        else {
            panic!("a permission prompt");
        };
        assert_eq!(tool, "apply_patch");
        assert_eq!(summary, "src/a.rs, src/b.rs");
        assert!(detail.unwrap().contains("+b"));
    }

    #[test]
    fn malformed_payloads_make_no_prompt() {
        for (event, payload) in [
            // Not an event that opens a prompt.
            ("tool-complete", CLAUDE_ASK_SINGLE),
            // No tool at all: Qoder's `Elicitation`, Gemini's notification.
            ("question-asked", r#"{"message": "pick one"}"#),
            (
                "permission-request",
                r#"{"notification_type": "ToolPermission"}"#,
            ),
            (
                "permission-request",
                r#"{"tool_name": 7, "tool_input": {}}"#,
            ),
            (
                "permission-request",
                r#"{"tool_name": "  ", "tool_input": {}}"#,
            ),
            // A tool other than a question, on the question event.
            (
                "question-asked",
                r#"{"tool_name": "Bash", "tool_input": {"command": "ls"}}"#,
            ),
            // Questions missing, of the wrong type, or empty.
            ("question-asked", r#"{"tool_name": "AskUserQuestion"}"#),
            (
                "question-asked",
                r#"{"tool_name": "AskUserQuestion", "tool_input": []}"#,
            ),
            (
                "question-asked",
                r#"{"tool_name": "AskUserQuestion", "tool_input": {"questions": "which?"}}"#,
            ),
            (
                "question-asked",
                r#"{"tool_name": "AskUserQuestion", "tool_input": {"questions": []}}"#,
            ),
            (
                "question-asked",
                r#"{"tool_name": "AskUserQuestion", "tool_input": {"questions": [{"header": "no text"}]}}"#,
            ),
            (
                "question-asked",
                r#"{"tool_name": "AskUserQuestion", "tool_input": {"questions": [{"question": "  "}]}}"#,
            ),
            // An option of the wrong shape spoils its question.
            (
                "question-asked",
                r#"{"tool_name": "AskUserQuestion", "tool_input": {"questions": [{"question": "q", "options": [{"label": 1}]}]}}"#,
            ),
            (
                "question-asked",
                r#"{"tool_name": "AskUserQuestion", "tool_input": {"questions": [{"question": "q", "options": "a,b"}]}}"#,
            ),
        ] {
            assert_eq!(parse(event, payload), None, "{event} {payload}");
        }
    }

    #[test]
    fn a_permission_with_an_unreadable_input_still_names_the_tool() {
        let prompt = parse(
            "permission-request",
            r#"{"tool_name": "Bash", "tool_input": 42}"#,
        )
        .unwrap();
        assert_eq!(
            prompt.kind,
            AgentPromptKind::Permission {
                tool: "Bash".into(),
                summary: String::new(),
                detail: None,
            }
        );
    }

    #[test]
    fn everything_is_clipped() {
        let long = "x".repeat(10_000);
        let options: Vec<_> = (0..50)
            .map(|i| serde_json::json!({"label": format!("{i}{long}"), "description": long}))
            .collect();
        let questions: Vec<_> = (0..50)
            .map(|_| serde_json::json!({"question": long, "header": long, "options": options}))
            .collect();
        let payload = serde_json::json!({
            "tool_name": "AskUserQuestion",
            "tool_use_id": long,
            "tool_input": {"questions": questions},
        });
        let prompt = from_hook_payload("question-asked", &payload, String::new).unwrap();
        assert_eq!(prompt.id.chars().count(), ID_MAX);
        let AgentPromptKind::Question { questions } = &prompt.kind else {
            panic!("a question prompt");
        };
        assert_eq!(questions.len(), QUESTIONS_MAX);
        let q = &questions[0];
        assert_eq!(q.text.chars().count(), TEXT_MAX);
        assert_eq!(q.header.as_ref().unwrap().chars().count(), HEADER_MAX);
        assert_eq!(q.options.len(), OPTIONS_MAX);
        assert_eq!(q.options[0].label.chars().count(), LABEL_MAX);
        assert_eq!(
            q.options[0].description.as_ref().unwrap().chars().count(),
            DESCRIPTION_MAX
        );
        assert!(serde_json::to_string(&prompt).unwrap().len() < 64 * 1024);

        let bash = serde_json::json!({"tool_name": long, "tool_input": {"command": long, "description": long}});
        let AgentPromptKind::Permission {
            tool,
            summary,
            detail,
        } = from_hook_payload("permission-request", &bash, || "id".into())
            .unwrap()
            .kind
        else {
            panic!("a permission prompt");
        };
        assert_eq!(tool.chars().count(), TOOL_MAX);
        assert_eq!(summary.chars().count(), SUMMARY_MAX);
        assert_eq!(detail.unwrap().chars().count(), DETAIL_MAX);
    }

    #[test]
    fn the_wire_shape_is_flat_and_tagged() {
        let prompt = parse(
            "permission-request",
            r#"{"tool_name": "Bash", "tool_input": {"command": "ls"}}"#,
        )
        .unwrap();
        assert_eq!(
            serde_json::to_value(&prompt).unwrap(),
            serde_json::json!({"id": "fresh", "kind": "permission", "tool": "Bash", "summary": "ls"})
        );
        let back: AgentPrompt =
            serde_json::from_value(serde_json::to_value(&prompt).unwrap()).unwrap();
        assert_eq!(back, prompt);
    }

    #[test]
    fn a_prompt_that_does_not_parse_is_read_as_none() {
        #[derive(Deserialize)]
        struct Holder {
            #[serde(default, deserialize_with = "lenient")]
            prompt: Option<AgentPrompt>,
            n: u32,
        }
        for json in [
            r#"{"n": 1, "prompt": {"id": "a", "kind": "brand_new_kind", "x": 1}}"#,
            r#"{"n": 1, "prompt": {"kind": "permission", "tool": "Bash", "summary": ""}}"#,
            r#"{"n": 1, "prompt": {"id": "", "kind": "permission", "tool": "Bash", "summary": ""}}"#,
            r#"{"n": 1, "prompt": "question"}"#,
            r#"{"n": 1, "prompt": null}"#,
            r#"{"n": 1}"#,
        ] {
            let held: Holder = serde_json::from_str(json).expect(json);
            assert_eq!((held.n, held.prompt), (1, None), "{json}");
        }
    }
}
