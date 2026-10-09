use crate::{
    agent::{Agent, AgentState},
    config::save_json,
    protocol::{Action, Mode},
};
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::{
    collections::BTreeMap,
    fs,
    path::{Path, PathBuf},
};

pub type Id = u64;

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Session {
    pub version: u32,
    pub name: String,
    pub project: PathBuf,
    pub next_id: Id,
    pub tabs: Vec<Tab>,
    pub panes: BTreeMap<Id, Pane>,
    pub agents: BTreeMap<Id, Agent>,
    pub requests: BTreeMap<Id, Request>,
    pub services: Vec<Service>,
    pub edits: BTreeMap<Id, Edit>,
    #[serde(default)]
    pub model_status: String,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Tab {
    pub id: Id,
    pub title: String,
    pub panes: Vec<Id>,
    pub vertical: bool,
    pub weights: BTreeMap<Id, u16>,
    pub agent: Option<Id>,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Pane {
    pub id: Id,
    pub tab: Id,
    pub title: String,
    pub cwd: PathBuf,
    pub creator: String,
    pub agent: Option<Id>,
    pub attached: bool,
    pub display: Display,
    #[serde(default)]
    pub owner: Option<Id>,
}

#[derive(Clone, Debug, Serialize, Deserialize, Default)]
pub enum Display {
    #[default]
    Terminal,
    Script {
        script: String,
        path: PathBuf,
        active: BTreeMap<u32, usize>,
        status: String,
    },
    Edit {
        id: Id,
        path: PathBuf,
        before: String,
        after: String,
        diff: String,
    },
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Request {
    pub id: Id,
    pub summary: String,
    pub requester: Option<Id>,
    pub agent: Option<Id>,
    pub kind: RequestKind,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum RequestKind {
    Action(Box<Action>),
    Directory(PathBuf),
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Service {
    pub id: Id,
    pub title: String,
    pub pid: u32,
    pub start_time: u64,
    pub cwd: PathBuf,
    pub script: String,
    pub log: PathBuf,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Edit {
    pub id: Id,
    pub path: PathBuf,
    pub before: Option<String>,
    pub after_hash: String,
    pub undone: bool,
}

#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct Connection {
    pub id: Id,
    pub user: String,
    pub color: u8,
    pub short: String,
    pub tab: Id,
    pub pane: Option<Id>,
    pub chat: Option<Id>,
    pub mode: Mode,
    pub cols: u16,
    pub rows: u16,
}

impl Session {
    pub fn load(project: PathBuf, name: String) -> Result<Self> {
        let path = project.join(format!(".crew/{name}.json"));
        if path.exists() {
            let mut session: Self = serde_json::from_slice(&fs::read(path)?)?;
            if session.version != 1 || session.project != project {
                bail!("session 格式或项目路径不匹配");
            }
            session.requests.clear();
            for pane in session.panes.values_mut() {
                pane.owner = None;
                if let Display::Script { active, status, .. } = &mut pane.display {
                    active.clear();
                    *status = "历史脚本；服务已退出，命令没有恢复".into();
                }
            }
            for agent in session.agents.values_mut() {
                agent.owner = None;
                if agent.state != AgentState::Stopped {
                    agent.state = AgentState::Paused(
                        "服务重新启动；旧进程与任务不恢复，请检查环境后 /resume".into(),
                    );
                }
                // 未完成的工具调用在新的 Bash 中只能作为历史记录恢复。
                let context = &mut agent.conversation_mut().context;
                let mut outstanding = BTreeMap::new();
                for msg in context.iter() {
                    if let Some(calls) = msg["tool_calls"].as_array() {
                        for call in calls {
                            if let Some(id) = call["id"].as_str() {
                                outstanding.insert(id.to_owned(), ());
                            }
                        }
                    }
                    if let Some(id) = msg["tool_call_id"].as_str() {
                        outstanding.remove(id);
                    }
                }
                for (id, ()) in outstanding {
                    context.push(serde_json::json!({"role":"tool","tool_call_id":id,"content":"服务已退出，旧任务没有恢复"}));
                }
            }
            return Ok(session);
        }
        let mut s = Self {
            version: 1,
            name,
            project,
            next_id: 1,
            tabs: vec![],
            panes: BTreeMap::new(),
            agents: BTreeMap::new(),
            requests: BTreeMap::new(),
            services: vec![],
            edits: BTreeMap::new(),
            model_status: "模型未配置".into(),
        };
        s.create_tab("工作区".into());
        Ok(s)
    }
    pub fn id(&mut self) -> Id {
        let id = self.next_id;
        self.next_id = self.next_id.checked_add(1).expect("ID 耗尽");
        id
    }
    pub fn save(&self) -> Result<()> {
        let mut saved = self.clone();
        for pane in saved.panes.values_mut() {
            pane.owner = None;
        }
        for agent in saved.agents.values_mut() {
            agent.owner = None;
            agent.streaming.clear();
        }
        save_json(
            &self.project.join(format!(".crew/{}.json", self.name)),
            &saved,
        )
    }
    pub fn tab(&self, id: Id) -> Result<&Tab> {
        self.tabs
            .iter()
            .find(|t| t.id == id)
            .ok_or_else(|| anyhow::anyhow!("tab {id} 不存在"))
    }
    pub fn tab_mut(&mut self, id: Id) -> Result<&mut Tab> {
        self.tabs
            .iter_mut()
            .find(|t| t.id == id)
            .ok_or_else(|| anyhow::anyhow!("tab {id} 不存在"))
    }
    pub fn pane(&self, id: Id) -> Result<&Pane> {
        self.panes
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("pane {id} 不存在"))
    }
    pub fn agent(&self, id: Id) -> Result<&Agent> {
        self.agents
            .get(&id)
            .ok_or_else(|| anyhow::anyhow!("agent {id} 不存在"))
    }
    pub fn create_tab(&mut self, title: String) -> Id {
        let id = self.id();
        self.tabs.push(Tab {
            id,
            title,
            panes: vec![],
            vertical: false,
            weights: BTreeMap::new(),
            agent: None,
        });
        id
    }
    pub fn create_pane(
        &mut self,
        tab: Id,
        title: String,
        creator: String,
        cwd: PathBuf,
    ) -> Result<Id> {
        self.tab(tab)?;
        let id = self.id();
        let title = if title.trim().is_empty() {
            format!("终端 {id}")
        } else {
            title
        };
        self.panes.insert(
            id,
            Pane {
                id,
                tab,
                title,
                cwd,
                creator,
                agent: None,
                attached: false,
                display: Display::Terminal,
                owner: None,
            },
        );
        let tab = self.tab_mut(tab)?;
        tab.panes.push(id);
        tab.weights.insert(id, 100);
        Ok(id)
    }
    pub fn request(
        &mut self,
        summary: String,
        requester: Option<Id>,
        agent: Option<Id>,
        kind: RequestKind,
    ) -> Id {
        let id = self.id();
        self.requests.insert(
            id,
            Request {
                id,
                summary,
                requester,
                agent,
                kind,
            },
        );
        id
    }
    pub fn check(&self) {
        for tab in &self.tabs {
            for pane in &tab.panes {
                assert_eq!(self.panes[pane].tab, tab.id);
                assert!(tab.weights.contains_key(pane));
            }
            if let Some(id) = tab.agent {
                let agent = &self.agents[&id];
                if agent.state != AgentState::Stopped {
                    let p = &self.panes[&agent.pane.expect("tab agent 必须拥有附属终端")];
                    assert!(p.attached);
                    assert_eq!(p.agent, Some(id));
                }
            }
        }
        for pane in self.panes.values() {
            assert!(
                self.tab(pane.tab)
                    .expect("pane 所属 tab 必须存在")
                    .panes
                    .contains(&pane.id)
            );
        }
    }
    pub fn runtime_dir(&self) -> PathBuf {
        self.project.join(format!(".crew/runtime/{}", self.name))
    }
    pub fn project(&self) -> &Path {
        &self.project
    }
}
