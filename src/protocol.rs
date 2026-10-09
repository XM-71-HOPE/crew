use crate::session::{Connection, Id, Session};
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Mode {
    Session,
    Tab,
    Terminal,
    Chat,
    Layout,
    Review { origin: Box<Mode> },
}

#[derive(Clone, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum Target {
    Pane(Id),
    Agent(Id),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum Action {
    Focus {
        tab: Id,
        pane: Option<Id>,
        mode: Mode,
    },
    Acquire {
        target: Target,
        terminal: bool,
    },
    Release,
    Review {
        enter: bool,
    },
    Input {
        pane: Id,
        bytes: Vec<u8>,
        cols: u16,
        rows: u16,
    },
    Draft {
        agent: Id,
        text: String,
        revision: u64,
    },
    Send {
        agent: Id,
        interrupt: bool,
        tree: bool,
    },
    Toggle {
        agent: Id,
    },
    CreateTab {
        title: String,
    },
    CreatePane {
        tab: Id,
        title: String,
    },
    Rename {
        tab: Option<Id>,
        pane: Option<Id>,
        title: String,
    },
    Layout {
        tab: Id,
        pane: Id,
        movement: i8,
        weight: i16,
        vertical: Option<bool>,
    },
    StartAgent {
        tab: Id,
        pane: Option<Id>,
    },
    StopAgent {
        agent: Id,
    },
    Resume {
        agent: Id,
    },
    Pause {
        agent: Id,
        tree: bool,
    },
    Conversation {
        agent: Id,
        conversation: Option<Id>,
    },
    Close {
        tab: Option<Id>,
        pane: Option<Id>,
    },
    Resolve {
        request: Id,
        choice: Choice,
    },
    History {
        target: Target,
    },
    Undo {
        edit: Id,
    },
    Service {
        script: String,
        cwd: String,
        title: String,
    },
    Handoff {
        pane: Id,
        script: String,
        title: String,
    },
    Shutdown,
    Owner {
        pane: Id,
    },
    Ping,
}

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub enum Choice {
    Approve,
    Reject,
    KeepRunning,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ClientMessage {
    Hello { user: String, cols: u16, rows: u16 },
    Action { seq: u64, action: Action },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum ServerMessage {
    Welcome { connection: Id },
    Snapshot(Box<Snapshot>),
    Reply { seq: u64, ok: bool, text: String },
    History { seq: u64, text: String },
    Notice(String),
    Bye(String),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Snapshot {
    pub session: Session,
    pub connections: BTreeMap<Id, Connection>,
    pub terminals: BTreeMap<Id, TerminalView>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TerminalView {
    pub screen: Vec<u8>,
    pub cols: u16,
    pub rows: u16,
    pub cursor: (u16, u16),
    pub cursor_hidden: bool,
    pub status: String,
    pub foreground: Option<i32>,
    pub application_cursor: bool,
    pub bracketed_paste: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct ShellEvent {
    pub pane: Id,
    pub generation: Id,
    pub pid: u32,
    pub kind: String,
    pub cwd: String,
    pub source: String,
    pub line: usize,
    pub command: String,
    pub status: i32,
}
