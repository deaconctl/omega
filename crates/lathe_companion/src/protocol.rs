//! Messages exchanged with the companion app inside the encrypted channel.
//!
//! The canonical definition is `src/protocol/messages.ts` and
//! `src/protocol/README.md` in the `lathe-companion-app` repo. These types mirror
//! it, so a change on either side has to be carried to the other and
//! `PROTOCOL_VERSION` bumped when the change is not backward compatible.

use serde::{Deserialize, Serialize};

pub const PROTOCOL_VERSION: u32 = 1;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DesktopMessage {
    Hello {
        protocol_version: u32,
        lathe_version: String,
        channel: String,
        desktop_name: String,
    },
    Workspaces {
        list: Vec<WorkspaceSummary>,
    },
    Threads {
        list: Vec<ThreadSummary>,
    },
    ThreadSnapshot {
        thread_id: String,
        seq: u64,
        start_index: usize,
        entries: Vec<Entry>,
        has_more_before: bool,
        /// The model and effort choices the thread's agent offers. Absent
        /// when it offers none the phone may change.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        config_options: Option<Vec<ConfigOption>>,
    },
    ThreadOlder {
        thread_id: String,
        start_index: usize,
        entries: Vec<Entry>,
        has_more_before: bool,
    },
    ThreadDelta {
        thread_id: String,
        seq: u64,
        op: DeltaOp,
    },
    ToolAuthorization {
        thread_id: String,
        request_id: String,
        tool_title: String,
        tool_kind: String,
        options: Vec<PermissionOption>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff_preview: Option<Vec<FileDiff>>,
    },
    /// The thread's model and effort choices changed, from the phone, the desk
    /// or the agent itself. Outside the numbered deltas, so an app that
    /// doesn't know it skips it without seeing a gap.
    ThreadConfig {
        thread_id: String,
        options: Vec<ConfigOption>,
    },
    ToolAuthorizationResolved {
        thread_id: String,
        request_id: String,
    },
    Elicitation {
        thread_id: String,
        elicitation_id: String,
        message: String,
        schema: serde_json::Value,
    },
    ElicitationResolved {
        thread_id: String,
        elicitation_id: String,
    },
    Ack {
        client_msg_id: String,
        ok: bool,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        error: Option<ErrorBody>,
        /// Only on a successful `new_thread`: the thread it started.
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
    },
    Error {
        code: ErrorCode,
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        thread_id: Option<String>,
    },
}

/// Every phone message carries a `client_msg_id`, which the desktop answers
/// with an `ack`.
#[derive(Clone, Debug, PartialEq, Deserialize)]
pub struct PhoneEnvelope {
    pub client_msg_id: String,
    #[serde(flatten)]
    pub message: PhoneMessage,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum PhoneMessage {
    Subscribe {
        thread_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        since_seq: Option<u64>,
    },
    Unsubscribe {
        thread_id: String,
    },
    Refresh {},
    LoadOlder {
        thread_id: String,
        before_index: usize,
    },
    SendMessage {
        thread_id: String,
        text: String,
    },
    Authorize {
        thread_id: String,
        request_id: String,
        option_id: String,
    },
    RespondElicitation {
        thread_id: String,
        elicitation_id: String,
        value: serde_json::Map<String, serde_json::Value>,
    },
    Cancel {
        thread_id: String,
    },
    SetConfigOption {
        thread_id: String,
        config_id: String,
        /// A select option's `value`, or a boolean for a boolean option.
        value: serde_json::Value,
    },
    RenameThread {
        thread_id: String,
        title: String,
    },
    NewThread {
        workspace_id: String,
        agent_id: String,
        text: String,
    },
    Unpair {},
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ErrorBody {
    pub code: ErrorCode,
    pub message: String,
}

impl ErrorBody {
    pub fn new(code: ErrorCode, message: impl Into<String>) -> Self {
        Self {
            code,
            message: message.into(),
        }
    }
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ErrorCode {
    WorkspaceNotOpen,
    ThreadNotFound,
    RequestNotFound,
    BadMessage,
    UnsupportedVersion,
    AgentNotFound,
    Unsupported,
    Internal,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct WorkspaceSummary {
    pub workspace_id: String,
    pub name: String,
    pub paths: Vec<String>,
    pub is_open: bool,
    /// Agents a new thread can use here. Only sent for open workspaces.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub agents: Option<Vec<AgentSummary>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub default_agent_id: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct AgentSummary {
    pub agent_id: String,
    pub name: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ThreadSummary {
    pub thread_id: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub session_id: Option<String>,
    pub agent_id: String,
    pub title: String,
    pub workspace_id: String,
    /// RFC 3339.
    pub updated_at: String,
    pub status: ThreadStatus,
    pub needs_attention: bool,
    pub is_loaded: bool,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ThreadStatus {
    Idle,
    Running,
    WaitingAuthorization,
    WaitingInput,
    Error,
}

impl ThreadStatus {
    pub fn needs_attention(self) -> bool {
        matches!(
            self,
            ThreadStatus::WaitingAuthorization | ThreadStatus::WaitingInput | ThreadStatus::Error
        )
    }
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum DeltaOp {
    NewEntry { entry: Entry },
    UpdateEntry { index: usize, entry: Entry },
    RemoveEntries { start: usize, end: usize },
    Status { status: ThreadStatus },
    Title { title: String },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfigOption {
    pub config_id: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    pub category: ConfigCategory,
    #[serde(flatten)]
    pub kind: ConfigKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ConfigCategory {
    Model,
    ThoughtLevel,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum ConfigKind {
    Select {
        current_value: String,
        options: Vec<ConfigChoice>,
    },
    Boolean {
        current_value: bool,
    },
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct ConfigChoice {
    pub value: String,
    pub name: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub description: Option<String>,
    /// The group heading this choice is listed under, when the agent groups
    /// them.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub group: Option<String>,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum Entry {
    UserMessage {
        text: String,
    },
    AssistantMessage {
        markdown: String,
    },
    Thought {
        markdown: String,
    },
    ToolCall {
        tool_call_id: String,
        title: String,
        tool_kind: String,
        status: ToolCallStatus,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        diff: Option<Vec<FileDiff>>,
    },
    Plan {
        items: Vec<PlanItem>,
    },
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ToolCallStatus {
    Pending,
    Running,
    Completed,
    Failed,
    Rejected,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct FileDiff {
    pub path: String,
    pub unified_diff: String,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PlanItem {
    pub text: String,
    pub status: PlanItemStatus,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PlanItemStatus {
    Pending,
    InProgress,
    Completed,
}

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
pub struct PermissionOption {
    pub option_id: String,
    pub label: String,
    pub kind: PermissionOptionKind,
}

#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum PermissionOptionKind {
    AllowOnce,
    AllowAlways,
    RejectOnce,
    RejectAlways,
}

#[cfg(test)]
mod tests {
    use super::*;
    use pretty_assertions::assert_eq;
    use serde_json::json;

    #[test]
    fn desktop_messages_use_the_app_wire_format() {
        let message = DesktopMessage::ThreadDelta {
            thread_id: "t1".into(),
            seq: 7,
            op: DeltaOp::UpdateEntry {
                index: 2,
                entry: Entry::AssistantMessage {
                    markdown: "Done.".into(),
                },
            },
        };
        assert_eq!(
            serde_json::to_value(&message).unwrap(),
            json!({
                "type": "thread_delta",
                "thread_id": "t1",
                "seq": 7,
                "op": {
                    "kind": "update_entry",
                    "index": 2,
                    "entry": { "kind": "assistant_message", "markdown": "Done." }
                }
            })
        );

        let message = DesktopMessage::Ack {
            client_msg_id: "m1".into(),
            ok: false,
            error: Some(ErrorBody::new(ErrorCode::WorkspaceNotOpen, "Open it in Lathe")),
            thread_id: None,
        };
        assert_eq!(
            serde_json::to_value(&message).unwrap(),
            json!({
                "type": "ack",
                "client_msg_id": "m1",
                "ok": false,
                "error": { "code": "workspace_not_open", "message": "Open it in Lathe" }
            })
        );

        let message = DesktopMessage::Threads {
            list: vec![ThreadSummary {
                thread_id: "t1".into(),
                session_id: None,
                agent_id: "claude-acp".into(),
                title: "Fix the build".into(),
                workspace_id: "w1".into(),
                updated_at: "2026-10-20T14:00:00Z".into(),
                status: ThreadStatus::WaitingAuthorization,
                needs_attention: true,
                is_loaded: true,
            }],
        };
        assert_eq!(
            serde_json::to_value(&message).unwrap(),
            json!({
                "type": "threads",
                "list": [{
                    "thread_id": "t1",
                    "agent_id": "claude-acp",
                    "title": "Fix the build",
                    "workspace_id": "w1",
                    "updated_at": "2026-10-20T14:00:00Z",
                    "status": "waiting_authorization",
                    "needs_attention": true,
                    "is_loaded": true
                }]
            })
        );
    }

    #[test]
    fn phone_messages_parse_from_the_app_wire_format() {
        let parsed: PhoneEnvelope = serde_json::from_value(json!({
            "type": "authorize",
            "client_msg_id": "m1",
            "thread_id": "t1",
            "request_id": "r1",
            "option_id": "allow"
        }))
        .unwrap();
        assert_eq!(parsed.client_msg_id, "m1");
        assert_eq!(
            parsed.message,
            PhoneMessage::Authorize {
                thread_id: "t1".into(),
                request_id: "r1".into(),
                option_id: "allow".into(),
            }
        );

        let parsed: PhoneEnvelope = serde_json::from_value(json!({
            "type": "rename_thread", "client_msg_id": "m4", "thread_id": "t1", "title": "Relay fix"
        }))
        .unwrap();
        assert_eq!(
            parsed.message,
            PhoneMessage::RenameThread {
                thread_id: "t1".into(),
                title: "Relay fix".into(),
            }
        );

        let parsed: PhoneEnvelope =
            serde_json::from_value(json!({ "type": "refresh", "client_msg_id": "m2" })).unwrap();
        assert_eq!(parsed.message, PhoneMessage::Refresh {});

        let parsed: PhoneEnvelope = serde_json::from_value(json!({
            "type": "subscribe", "client_msg_id": "m3", "thread_id": "t1"
        }))
        .unwrap();
        assert_eq!(
            parsed.message,
            PhoneMessage::Subscribe {
                thread_id: "t1".into(),
                since_seq: None,
            }
        );
    }

    /// Writes one of every desktop message for the app's schemas to check:
    /// `LATHE_COMPANION_SAMPLES=/tmp/samples.jsonl cargo test -p lathe_companion
    /// -- --ignored write_samples`.
    #[test]
    #[ignore]
    fn write_samples_for_the_app() {
        let Ok(path) = std::env::var("LATHE_COMPANION_SAMPLES") else {
            return;
        };
        let entries = vec![
            Entry::UserMessage { text: "Fix it".into() },
            Entry::AssistantMessage { markdown: "Done.".into() },
            Entry::Thought { markdown: "Thinking".into() },
            Entry::ToolCall {
                tool_call_id: "1".into(),
                title: "Edit relay.rs".into(),
                tool_kind: "edit".into(),
                status: ToolCallStatus::Completed,
                diff: Some(vec![FileDiff { path: "relay.rs".into(), unified_diff: "@@ -1 +1 @@\n-a\n+b\n".into() }]),
            },
            Entry::Plan { items: vec![PlanItem { text: "Step".into(), status: PlanItemStatus::InProgress }] },
        ];
        let messages = vec![
            DesktopMessage::Hello { protocol_version: 1, lathe_version: "1.13.0".into(), channel: "beta".into(), desktop_name: "Lathe Beta on Mac".into() },
            DesktopMessage::Workspaces { list: vec![WorkspaceSummary { workspace_id: "w".into(), name: "lathe".into(), paths: vec!["/code/lathe".into()], is_open: true, agents: Some(vec![AgentSummary { agent_id: "claude-acp".into(), name: "Claude Code".into() }]), default_agent_id: Some("claude-acp".into()) }] },
            DesktopMessage::Threads { list: vec![ThreadSummary { thread_id: "t".into(), session_id: Some("s".into()), agent_id: "claude-acp".into(), title: "T".into(), workspace_id: "w".into(), updated_at: "2026-10-21T10:00:00+00:00".into(), status: ThreadStatus::Running, needs_attention: false, is_loaded: true }] },
            DesktopMessage::ThreadSnapshot { thread_id: "t".into(), seq: 0, start_index: 0, entries: entries.clone(), has_more_before: false, config_options: Some(sample_config_options()) },
            DesktopMessage::ThreadSnapshot { thread_id: "t".into(), seq: 0, start_index: 0, entries: Vec::new(), has_more_before: false, config_options: None },
            DesktopMessage::ThreadConfig { thread_id: "t".into(), options: sample_config_options() },
            DesktopMessage::ThreadOlder { thread_id: "t".into(), start_index: 0, entries, has_more_before: false },
            DesktopMessage::ThreadDelta { thread_id: "t".into(), seq: 1, op: DeltaOp::RemoveEntries { start: 1, end: 3 } },
            DesktopMessage::ThreadDelta { thread_id: "t".into(), seq: 2, op: DeltaOp::Status { status: ThreadStatus::WaitingAuthorization } },
            DesktopMessage::ThreadDelta { thread_id: "t".into(), seq: 3, op: DeltaOp::Title { title: "New".into() } },
            DesktopMessage::ToolAuthorization { thread_id: "t".into(), request_id: "r".into(), tool_title: "Run".into(), tool_kind: "execute".into(), options: vec![PermissionOption { option_id: "o".into(), label: "Allow".into(), kind: PermissionOptionKind::AllowOnce }], diff_preview: None },
            DesktopMessage::ToolAuthorizationResolved { thread_id: "t".into(), request_id: "r".into() },
            DesktopMessage::Elicitation { thread_id: "t".into(), elicitation_id: "e".into(), message: "Pick".into(), schema: serde_json::json!({ "type": "object", "properties": { "choice": { "type": "string", "enum": ["a", "b"] } }, "required": ["choice"] }) },
            DesktopMessage::ElicitationResolved { thread_id: "t".into(), elicitation_id: "e".into() },
            DesktopMessage::Ack { client_msg_id: "m".into(), ok: true, error: None, thread_id: Some("t".into()) },
            DesktopMessage::Ack { client_msg_id: "m".into(), ok: false, error: Some(ErrorBody::new(ErrorCode::AgentNotFound, "No")), thread_id: None },
            DesktopMessage::Error { code: ErrorCode::Internal, message: "Oops".into(), thread_id: Some("t".into()) },
        ];
        let lines = messages
            .iter()
            .map(|message| serde_json::to_string(message).unwrap())
            .collect::<Vec<_>>()
            .join("\n");
        std::fs::write(path, lines).unwrap();
    }

    fn sample_config_options() -> Vec<ConfigOption> {
        vec![
            ConfigOption {
                config_id: "model".into(),
                name: "Model".into(),
                description: None,
                category: ConfigCategory::Model,
                kind: ConfigKind::Select {
                    current_value: "opus".into(),
                    options: vec![
                        ConfigChoice { value: "opus".into(), name: "Opus".into(), description: None, group: None },
                        ConfigChoice { value: "sonnet".into(), name: "Sonnet".into(), description: Some("Faster".into()), group: None },
                    ],
                },
            },
            ConfigOption {
                config_id: "effort".into(),
                name: "Effort".into(),
                description: None,
                category: ConfigCategory::ThoughtLevel,
                kind: ConfigKind::Select {
                    current_value: "high".into(),
                    options: vec![ConfigChoice { value: "high".into(), name: "High".into(), description: None, group: None }],
                },
            },
        ]
    }

    #[test]
    fn config_options_use_the_wire_format() {
        let option = &sample_config_options()[0];
        assert_eq!(
            serde_json::to_value(option).unwrap(),
            json!({
                "config_id": "model",
                "name": "Model",
                "category": "model",
                "kind": "select",
                "current_value": "opus",
                "options": [
                    { "value": "opus", "name": "Opus" },
                    { "value": "sonnet", "name": "Sonnet", "description": "Faster" }
                ]
            })
        );
        let parsed: PhoneEnvelope = serde_json::from_value(json!({
            "type": "set_config_option", "client_msg_id": "c1", "thread_id": "t1", "config_id": "effort", "value": "low"
        }))
        .unwrap();
        assert_eq!(
            parsed.message,
            PhoneMessage::SetConfigOption { thread_id: "t1".into(), config_id: "effort".into(), value: json!("low") }
        );
    }

    #[test]
    fn rejects_unknown_types_and_missing_message_ids() {
        assert!(serde_json::from_value::<PhoneEnvelope>(json!({ "type": "rm_rf", "client_msg_id": "m" })).is_err());
        assert!(serde_json::from_value::<PhoneEnvelope>(json!({ "type": "refresh" })).is_err());
    }
}
