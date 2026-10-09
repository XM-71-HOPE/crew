use crate::session::Id;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::{collections::VecDeque, path::PathBuf};

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Agent {
    pub id: Id,
    pub kind: AgentKind,
    pub tab: Id,
    pub pane: Option<Id>,
    pub parent: Option<Id>,
    pub children: Vec<Id>,
    pub allowed: Vec<PathBuf>,
    pub state: AgentState,
    pub visible: bool,
    pub active: Id,
    pub conversations: Vec<Conversation>,
    #[serde(default)]
    pub owner: Option<Id>,
    #[serde(skip)]
    pub epoch: u64,
    #[serde(skip)]
    pub pending: VecDeque<ToolCall>,
    #[serde(default)]
    pub queued: VecDeque<Value>,
    #[serde(skip_deserializing, default)]
    pub streaming: String,
    #[serde(skip)]
    pub needs_model: bool,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentKind {
    Tab,
    Pane,
    Probe,
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum AgentState {
    Idle,
    Thinking,
    Executing,
    Waiting(Id),
    Completed,
    Paused(String),
    Blocked(String),
    Stopped,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Conversation {
    pub id: Id,
    pub title: String,
    pub cwd: PathBuf,
    pub context: Vec<Value>,
    pub records: Vec<Record>,
    pub draft: String,
    pub editors: Vec<String>,
    pub revision: u64,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Record {
    pub kind: String,
    pub text: String,
    pub participants: Vec<String>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ToolCall {
    pub id: String,
    pub name: String,
    pub arguments: String,
}

pub struct AgentLocation {
    pub tab: Id,
    pub pane: Option<Id>,
    pub parent: Option<Id>,
}

impl Agent {
    pub fn new(
        id: Id,
        conversation: Id,
        kind: AgentKind,
        location: AgentLocation,
        cwd: PathBuf,
        allowed: Vec<PathBuf>,
    ) -> Self {
        Self {
            id,
            kind,
            tab: location.tab,
            pane: location.pane,
            parent: location.parent,
            children: Vec::new(),
            allowed,
            state: AgentState::Idle,
            visible: true,
            active: conversation,
            conversations: vec![Conversation::new(conversation, cwd)],
            owner: None,
            epoch: 0,
            pending: VecDeque::new(),
            queued: VecDeque::new(),
            streaming: String::new(),
            needs_model: false,
        }
    }
    pub fn conversation(&self) -> &Conversation {
        self.conversations
            .iter()
            .find(|c| c.id == self.active)
            .expect("活动对话必须存在")
    }
    pub fn conversation_mut(&mut self) -> &mut Conversation {
        self.conversations
            .iter_mut()
            .find(|c| c.id == self.active)
            .expect("活动对话必须存在")
    }
    pub fn record(&mut self, kind: &str, text: impl Into<String>) {
        self.conversation_mut().records.push(Record {
            kind: kind.into(),
            text: text.into(),
            participants: vec![],
        });
    }
    pub fn tool_result(&mut self, call: &ToolCall, result: &str) {
        self.conversation_mut()
            .context
            .push(json!({"role":"tool","tool_call_id":call.id,"content":result}));
        self.record("工具结果", format!("{}: {result}", call.name));
    }
    pub fn cancel_pending(&mut self, reason: &str) {
        while let Some(call) = self.pending.pop_front() {
            self.tool_result(&call, &format!("工具未执行：{reason}"));
        }
    }
    pub fn runnable(&self) -> bool {
        matches!(self.state, AgentState::Idle | AgentState::Completed)
    }
}

impl Conversation {
    pub fn new(id: Id, cwd: PathBuf) -> Self {
        Self {
            id,
            title: format!("对话 {id}"),
            cwd,
            context: vec![],
            records: vec![],
            draft: String::new(),
            editors: vec![],
            revision: 0,
        }
    }
}
