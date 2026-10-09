use crate::{
    agent::{Agent, AgentKind, AgentLocation, AgentState, Conversation, ToolCall},
    config::{Config, socket_path},
    files::{Files, Operation},
    model::{Model, ModelEvent},
    protocol::*,
    session::*,
    terminal::{CommandRun, Terminal, TerminalEvent, TerminalOptions},
};
use anyhow::{Context, Result, bail};
use fs2::FileExt;
use serde_json::{Value, json};
use std::{
    collections::BTreeMap,
    fs::{self, OpenOptions},
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::Arc,
    time::Duration,
};
use tokio::{
    io::{AsyncBufReadExt, AsyncWriteExt, BufReader},
    net::{UnixDatagram, UnixListener, UnixStream},
    sync::mpsc,
    task::JoinHandle,
};

pub struct Server {
    pub session: Session,
    connections: BTreeMap<Id, Connection>,
    clients: BTreeMap<Id, mpsc::Sender<ServerMessage>>,
    terminals: BTreeMap<Id, Terminal>,
    model: Option<Arc<Model>>,
    config: Config,
    files: Files,
    model_jobs: BTreeMap<Id, JoinHandle<()>>,
    service_children: Vec<Child>,
    terminal_tx: mpsc::UnboundedSender<TerminalEvent>,
    model_tx: mpsc::UnboundedSender<ModelEvent>,
    event_path: PathBuf,
    switches: BTreeMap<Id, Option<Id>>,
    dirty: bool,
    stopping: bool,
}

enum PeerEvent {
    Join {
        user: String,
        cols: u16,
        rows: u16,
        reply: mpsc::Sender<ServerMessage>,
        id: tokio::sync::oneshot::Sender<Result<Id, String>>,
    },
    Action {
        connection: Id,
        seq: u64,
        action: Action,
    },
    Leave(Id),
}

impl Server {
    pub async fn run(project: PathBuf, name: String) -> Result<()> {
        let project = project.canonicalize()?;
        fs::create_dir_all(project.join(".crew/users"))?;
        let lock = OpenOptions::new()
            .create(true)
            .truncate(false)
            .read(true)
            .write(true)
            .open(project.join(format!(".crew/{name}.lock")))?;
        lock.try_lock_exclusive().context("session 服务已经运行")?;
        let socket = socket_path(&project, &name)?;
        let event_path = project.join(format!(".crew/{name}.events"));
        for path in [&socket, &event_path] {
            if path.exists() {
                fs::remove_file(path)?;
            }
        }
        let listener = UnixListener::bind(&socket)?;
        let datagram = UnixDatagram::bind(&event_path)?;
        let config = Config::load(&project)?;
        let (model, model_status) = match config.model.clone() {
            Some(c) => match Model::new(c) {
                Ok(m) => (Some(Arc::new(m)), "模型配置与凭据可用".into()),
                Err(e) => (None, format!("模型不可用：{e}")),
            },
            None => (None, "模型未配置".into()),
        };
        let (terminal_tx, mut terminal_rx) = mpsc::unbounded_channel();
        let (model_tx, mut model_rx) = mpsc::unbounded_channel();
        let (peer_tx, mut peer_rx) = mpsc::unbounded_channel();
        let mut session = Session::load(project, name)?;
        session.model_status = model_status;
        fs::create_dir_all(session.runtime_dir())?;
        let mut server = Self {
            session,
            connections: BTreeMap::new(),
            clients: BTreeMap::new(),
            terminals: BTreeMap::new(),
            model,
            config,
            files: Files::default(),
            model_jobs: BTreeMap::new(),
            service_children: vec![],
            terminal_tx,
            model_tx,
            event_path: event_path.clone(),
            switches: BTreeMap::new(),
            dirty: true,
            stopping: false,
        };
        let ids: Vec<_> = server.session.panes.keys().copied().collect();
        for id in ids {
            let ai = server.session.panes[&id]
                .agent
                .is_some_and(|a| server.session.agents[&a].state != AgentState::Stopped);
            server.spawn_terminal(id, ai)?;
        }
        server.session.save()?;
        let mut tick = tokio::time::interval(Duration::from_millis(60));
        let mut persist = tokio::time::interval(Duration::from_millis(500));
        let mut sigterm =
            tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())?;
        let mut sigint = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::interrupt())?;
        let mut buffer = vec![0; 65_536];
        while !server.stopping {
            tokio::select! {
                accepted=listener.accept()=>{let (stream,_)=accepted?;tokio::spawn(peer(stream,peer_tx.clone()));},
                Some(event)=peer_rx.recv()=>server.peer_event(event)?,
                Some(event)=terminal_rx.recv()=>server.terminal_event(event)?,
                Some(event)=model_rx.recv()=>server.model_event(event)?,
                incoming=datagram.recv(&mut buffer)=>{
                    let n=incoming?;
                    match serde_json::from_slice::<ShellEvent>(&buffer[..n]) {
                        Ok(event)=>server.shell_event(event)?,
                        Err(error)=>server.notice_all(format!("Bash 事件 JSON 无效：{error}")),
                    }
                },
                _=tick.tick()=>{
                    while let Ok(event)=terminal_rx.try_recv(){server.terminal_event(event)?;}
                    server.poll_terminals()?;server.advance_agents()?;server.finish_switches()?;
                    server.reap_services()?;
                    if server.dirty {server.broadcast();server.dirty=false;}
                },
                _=persist.tick()=>server.session.save()?,
                _=sigterm.recv()=>{server.request_shutdown(None);},
                _=sigint.recv()=>{server.request_shutdown(None);},
            }
            server.session.check();
        }
        for job in server.model_jobs.values() {
            job.abort();
        }
        for terminal in server.terminals.values_mut() {
            terminal.stop()?;
        }
        server.session.requests.clear();
        for agent in server.session.agents.values_mut() {
            if agent.state != AgentState::Stopped {
                agent.state = AgentState::Paused("session 服务已退出，运行任务不恢复".into());
            }
        }
        server.session.save()?;
        for tx in server.clients.values() {
            if let Err(e) = tx.try_send(ServerMessage::Bye(
                "session 服务已退出；保留服务继续运行".into(),
            )) {
                eprintln!("退出通知未送达：{e}");
            }
        }
        fs::remove_file(socket)?;
        fs::remove_file(event_path)?;
        drop(lock);
        Ok(())
    }
    fn spawn_terminal(&mut self, pane: Id, ai: bool) -> Result<()> {
        let generation = self.session.id();
        let p = self.session.pane(pane)?.clone();
        let user = Config::load_user(self.session.project(), &p.creator)?;
        let terminal = Terminal::spawn(
            TerminalOptions {
                pane,
                generation,
                cwd: &p.cwd,
                runtime: &self.session.runtime_dir(),
                events: &self.event_path,
                session: &self.session.name,
                project: self.session.project(),
                user: &user,
                ai,
                secret_env: self
                    .config
                    .model
                    .as_ref()
                    .and_then(|m| m.api_key_env.as_deref()),
            },
            self.terminal_tx.clone(),
        )?;
        self.terminals.insert(pane, terminal);
        self.dirty = true;
        Ok(())
    }
    fn peer_event(&mut self, event: PeerEvent) -> Result<()> {
        match event {
            PeerEvent::Join {
                user,
                cols,
                rows,
                reply,
                id,
            } => {
                let result = self
                    .join(user, cols, rows, reply)
                    .map_err(|e| e.to_string());
                let _ = id.send(result);
            }
            PeerEvent::Leave(id) => {
                self.release(id);
                self.connections.remove(&id);
                self.clients.remove(&id);
                self.session.requests.retain(|_, r| {
                    if r.requester == Some(id) {
                        if matches!(&r.kind, RequestKind::Action(a) if matches!(**a, Action::Acquire { .. })) {
                            return false;
                        }
                        r.requester = None;
                    }
                    true
                });
                self.dirty = true;
            }
            PeerEvent::Action {
                connection,
                seq,
                action,
            } => {
                if !self.connections.contains_key(&connection) {
                    return Ok(());
                }
                let result = self.action(connection, action, false, Choice::Approve);
                match result {
                    Ok(Some(text)) => self.send(
                        connection,
                        ServerMessage::Reply {
                            seq,
                            ok: true,
                            text,
                        },
                    ),
                    Ok(None) => {}
                    Err(error) => self.send(
                        connection,
                        ServerMessage::Reply {
                            seq,
                            ok: false,
                            text: error.to_string(),
                        },
                    ),
                }
                self.dirty = true;
            }
        }
        Ok(())
    }
    fn join(
        &mut self,
        user: String,
        cols: u16,
        rows: u16,
        tx: mpsc::Sender<ServerMessage>,
    ) -> Result<Id> {
        let personal = Config::load_user(self.session.project(), &user)?;
        let tab = self
            .session
            .tabs
            .first()
            .expect("session 至少包含一个 tab")
            .id;
        if self.session.tab(tab)?.panes.is_empty() {
            let pane = self.session.create_pane(
                tab,
                String::new(),
                user.clone(),
                self.session.project.clone(),
            )?;
            self.spawn_new_terminal(pane, false)?;
        }
        let pane = self.session.tab(tab)?.panes.first().copied();
        let id = self.session.id();
        self.connections.insert(
            id,
            Connection {
                id,
                user,
                color: personal.color,
                short: personal.abbreviation,
                tab,
                pane,
                chat: None,
                mode: Mode::Session,
                cols,
                rows,
            },
        );
        self.clients.insert(id, tx);
        self.send(id, ServerMessage::Welcome { connection: id });
        self.send(id, ServerMessage::Snapshot(Box::new(self.snapshot())));
        self.dirty = true;
        Ok(id)
    }
    fn snapshot(&self) -> Snapshot {
        Snapshot {
            session: self.session.clone(),
            connections: self.connections.clone(),
            terminals: self
                .terminals
                .iter()
                .map(|(id, t)| (*id, t.view()))
                .collect(),
        }
    }
    fn send(&mut self, id: Id, message: ServerMessage) {
        if let Some(tx) = self.clients.get(&id)
            && tx.try_send(message).is_err()
        {
            self.clients.remove(&id);
            self.release(id);
            self.connections.remove(&id);
        }
    }
    fn broadcast(&mut self) {
        let snapshot = ServerMessage::Snapshot(Box::new(self.snapshot()));
        let ids: Vec<_> = self.clients.keys().copied().collect();
        for id in ids {
            self.send(id, snapshot.clone());
        }
    }
    fn notice_all(&mut self, text: String) {
        let ids: Vec<_> = self.clients.keys().copied().collect();
        for id in ids {
            self.send(id, ServerMessage::Notice(text.clone()));
        }
        self.dirty = true;
    }
    fn notice(&mut self, id: Id, text: String) {
        self.send(id, ServerMessage::Notice(text));
    }
    fn release(&mut self, id: Id) {
        for pane in self.session.panes.values_mut() {
            if pane.owner == Some(id) {
                pane.owner = None;
            }
        }
        for agent in self.session.agents.values_mut() {
            if agent.owner == Some(id) {
                agent.owner = None;
            }
        }
        if let Some(c) = self.connections.get_mut(&id) {
            let source = if c.chat.is_some_and(|a| {
                self.session
                    .agents
                    .get(&a)
                    .is_some_and(|a| a.kind == AgentKind::Tab)
            }) {
                Mode::Session
            } else {
                Mode::Tab
            };
            match &mut c.mode {
                Mode::Review { origin } => {
                    if matches!(**origin, Mode::Chat | Mode::Terminal) {
                        **origin = source;
                    }
                }
                Mode::Chat | Mode::Terminal => c.mode = source,
                _ => {}
            }
        }
    }
    fn pane_terminal_owner(&self, pane: Id) -> bool {
        self.session.panes[&pane].owner.is_some_and(|id| {
            self.connections.get(&id).is_some_and(|c| {
                c.mode == Mode::Terminal
                    || matches!(&c.mode,Mode::Review{origin} if **origin==Mode::Terminal)
            })
        })
    }
    fn chat_owned(&self, id: Id, agent: Id) -> bool {
        let a = &self.session.agents[&agent];
        if a.kind == AgentKind::Tab {
            a.owner == Some(id)
        } else {
            a.pane
                .is_some_and(|p| self.session.panes[&p].owner == Some(id))
        }
    }
    fn request_action(&mut self, id: Option<Id>, action: Action, summary: String) -> String {
        let request =
            self.session
                .request(summary, id, None, RequestKind::Action(Box::new(action)));
        self.dirty = true;
        format!("请求 {request} 等待确认；p 打开请求列表")
    }
    fn action(
        &mut self,
        id: Id,
        action: Action,
        confirmed: bool,
        choice: Choice,
    ) -> Result<Option<String>> {
        match action.clone() {
            Action::Ping => return Ok(Some("服务运行中".into())),
            Action::Focus { tab, pane, mode } => {
                if !matches!(mode, Mode::Session | Mode::Tab | Mode::Layout) {
                    bail!("输入与回看必须通过占用入口进入");
                }
                self.session.tab(tab)?;
                if let Some(pane) = pane
                    && self.session.pane(pane)?.tab != tab
                {
                    bail!("pane 不属于选中的 tab");
                }
                self.release(id);
                let c = self.connections.get_mut(&id).expect("连接存在");
                c.tab = tab;
                c.pane = pane;
                c.mode = mode;
                c.chat = None;
            }
            Action::Release => {
                self.release(id);
            }
            Action::Review { enter } => {
                let c = self.connections.get_mut(&id).expect("连接存在");
                if enter {
                    if !matches!(c.mode, Mode::Review { .. }) {
                        c.mode = Mode::Review {
                            origin: Box::new(c.mode.clone()),
                        };
                    }
                } else if let Mode::Review { origin } = &c.mode {
                    c.mode = *origin.clone();
                }
            }
            Action::Acquire { target, terminal } => {
                let (pane, agent) = match target {
                    Target::Pane(p) => {
                        let p = self.session.pane(p)?;
                        (Some(p.id), p.agent)
                    }
                    Target::Agent(a) => {
                        let a = self.session.agent(a)?;
                        if a.state == AgentState::Stopped || a.kind == AgentKind::Probe {
                            bail!("agent 不可操作");
                        }
                        (
                            if terminal || a.kind != AgentKind::Tab {
                                a.pane
                            } else {
                                None
                            },
                            Some(a.id),
                        )
                    }
                };
                if !terminal && agent.is_none() {
                    bail!("这个区域没有 agent；请明确创建 agent");
                }
                let owner = if let Some(p) = pane {
                    self.session.panes[&p].owner
                } else {
                    self.session.agents[&agent.expect("对话必须有 agent")].owner
                };
                let occupied = owner.is_some_and(|o| o != id);
                let ai_busy = terminal
                    && pane.is_some_and(|p| {
                        self.session.panes[&p].agent.is_some() && self.terminals[&p].busy()
                    });
                if !confirmed && (occupied || ai_busy) {
                    return Ok(Some(self.request_action(
                        Some(id),
                        action,
                        format!(
                            "接替区域操作{}{}。y 确认并中断当前命令，k 确认并保留命令，n 取消",
                            if occupied {
                                "，原连接将返回导航"
                            } else {
                                ""
                            },
                            if ai_busy { "，AI 将暂停" } else { "" }
                        ),
                    )));
                }
                if let Some(owner) = owner.filter(|o| *o != id) {
                    self.release(owner);
                    self.notice(
                        owner,
                        "操作占用已由另一连接接替；回看可继续，返回时进入导航".into(),
                    );
                }
                self.release(id);
                if terminal {
                    let p = pane.context("没有执行终端")?;
                    if let Some(a) = self.session.panes[&p].agent {
                        self.pause(a, false, "人已进入终端输入")?;
                        let t = self.terminals.get_mut(&p).expect("pane 必须有终端");
                        if t.takeover_hashes.is_none() {
                            t.takeover_hashes = Some(Files::hashes(&t.cwd)?);
                            t.takeover_offset = t.offset()?;
                        }
                        if confirmed && choice != Choice::KeepRunning && t.busy() {
                            t.interrupt()?;
                        }
                    }
                    self.session.panes.get_mut(&p).expect("pane 存在").owner = Some(id);
                    self.session.panes.get_mut(&p).expect("pane 存在").display = Display::Terminal;
                } else {
                    let a = agent.expect("对话必须有 agent");
                    self.session.agents.get_mut(&a).expect("agent 存在").visible = true;
                    if let Some(p) = pane {
                        self.session.panes.get_mut(&p).expect("pane 存在").owner = Some(id);
                    } else {
                        self.session.agents.get_mut(&a).expect("agent 存在").owner = Some(id);
                    }
                }
                let tab = if let Some(p) = pane {
                    self.session.panes[&p].tab
                } else {
                    self.session.agents[&agent.expect("agent 存在")].tab
                };
                let c = self.connections.get_mut(&id).expect("连接存在");
                c.tab = tab;
                if pane.is_some() {
                    c.pane = pane;
                }
                c.chat = agent;
                c.mode = if terminal { Mode::Terminal } else { Mode::Chat };
            }
            Action::Input {
                pane,
                bytes,
                cols,
                rows,
            } => {
                if self.connections[&id].mode != Mode::Terminal
                    || self.session.pane(pane)?.owner != Some(id)
                {
                    bail!("没有终端输入占用");
                }
                if self.connections[&id].pane != Some(pane) {
                    bail!("输入目标与当前 pane 不一致");
                }
                self.terminals
                    .get_mut(&pane)
                    .expect("pane 必须有终端")
                    .input(&bytes, cols, rows)?;
            }
            Action::Draft {
                agent,
                text,
                revision,
            } => {
                self.session.agent(agent)?;
                if !self.chat_owned(id, agent) || self.connections[&id].mode != Mode::Chat {
                    bail!("没有对话输入占用");
                }
                if text.len() > 100_000 {
                    bail!("草稿超过 100 KB");
                }
                let user = self.connections[&id].user.clone();
                let c = self
                    .session
                    .agents
                    .get_mut(&agent)
                    .expect("agent 存在")
                    .conversation_mut();
                if revision != c.revision {
                    bail!("草稿已由其他连接修改，请读取最新草稿");
                }
                c.draft = text;
                c.revision += 1;
                if !c.editors.contains(&user) {
                    c.editors.push(user);
                }
            }
            Action::Send {
                agent,
                interrupt,
                tree,
            } => {
                self.session.agent(agent)?;
                if !self.chat_owned(id, agent) || self.connections[&id].mode != Mode::Chat {
                    bail!("没有对话输入占用");
                }
                let text = self.session.agent(agent)?.conversation().draft.clone();
                if text.trim().is_empty() {
                    bail!("草稿为空");
                }
                if self.slash(id, agent, &text)? {
                    self.clear_draft(agent);
                    return Ok(Some("对话命令已处理".into()));
                }
                if interrupt {
                    self.pause(agent, tree, "人通过对话打断任务")?;
                    if let Some(p) = self.session.agents[&agent].pane
                        && self.terminals[&p].command.is_some()
                    {
                        self.terminals.get_mut(&p).expect("终端存在").interrupt()?;
                    }
                }
                let executing = self.session.agents[&agent]
                    .pane
                    .is_some_and(|p| self.terminals[&p].command.is_some());
                let user = self.connections[&id].user.clone();
                let a = self.session.agents.get_mut(&agent).expect("agent 存在");
                let c = a.conversation_mut();
                let mut participants = c.editors.clone();
                if !participants.contains(&user) {
                    participants.push(user.clone());
                }
                c.records.push(crate::agent::Record {
                    kind: "人".into(),
                    text: text.clone(),
                    participants: participants.clone(),
                });
                let message = json!({"role":"user","content":format!("发送者：{user}；编辑参与者：{}\n{text}",participants.join("、"))});
                if (a.runnable() || interrupt) && !executing {
                    a.conversation_mut().context.push(message);
                    a.needs_model = true;
                } else {
                    a.queued.push_back(message);
                    a.record(
                        "排队",
                        "消息等待当前工具批次完成；暂停的 agent 需要 /resume",
                    );
                }
                if interrupt {
                    a.state = AgentState::Idle;
                }
                self.clear_draft(agent);
            }
            Action::Toggle { agent } => {
                let a = self.session.agent(agent)?;
                if a.visible
                    && self.connections.values().any(|c| {
                        c.chat == Some(agent)
                            && (c.mode == Mode::Chat
                                || matches!(&c.mode,Mode::Review{origin} if **origin==Mode::Chat))
                    })
                {
                    bail!("对话区正在被人操作，不能关闭");
                }
                let a = self.session.agents.get_mut(&agent).expect("agent 存在");
                a.visible = !a.visible;
            }
            Action::CreateTab { title } => {
                let tab = self.session.create_tab(if title.trim().is_empty() {
                    "工作区".into()
                } else {
                    title
                });
                let pane = self.session.create_pane(
                    tab,
                    String::new(),
                    self.connections[&id].user.clone(),
                    self.session.project.clone(),
                )?;
                self.spawn_new_terminal(pane, false)?;
                return Ok(Some(format!("tab {tab}，pane {pane}")));
            }
            Action::CreatePane { tab, title } => {
                let pane = self.session.create_pane(
                    tab,
                    title,
                    self.connections[&id].user.clone(),
                    self.session.project.clone(),
                )?;
                self.spawn_new_terminal(pane, false)?;
                return Ok(Some(format!("pane {pane}")));
            }
            Action::Rename { tab, pane, title } => {
                if title.trim().is_empty() {
                    bail!("标题不能为空");
                }
                if let Some(p) = pane {
                    self.session.panes.get_mut(&p).context("pane 不存在")?.title = title;
                } else if let Some(t) = tab {
                    self.session.tab_mut(t)?.title = title;
                } else {
                    bail!("需要指定 tab 或 pane");
                }
            }
            Action::Layout {
                tab,
                pane,
                movement,
                weight,
                vertical,
            } => {
                let t = self.session.tab_mut(tab)?;
                let i = t
                    .panes
                    .iter()
                    .position(|p| *p == pane)
                    .context("pane 不属于 tab")?;
                let next =
                    (i as isize + movement as isize).clamp(0, t.panes.len() as isize - 1) as usize;
                t.panes.swap(i, next);
                let w = t.weights.get_mut(&pane).expect("布局必须有权重");
                *w = (*w as i32 + weight as i32).clamp(10, 1000) as u16;
                if let Some(v) = vertical {
                    t.vertical = v;
                }
            }
            Action::StartAgent { tab, pane } => {
                if self.model.is_none() {
                    bail!(
                        "{}；请编辑 .crew/config.toml 后重新启动服务",
                        self.session.model_status
                    );
                }
                self.session.tab(tab)?;
                if let Some(p) = pane {
                    let p = self.session.pane(p)?;
                    if p.tab != tab || p.agent.is_some() {
                        bail!("pane 已有 agent 或不属于 tab");
                    }
                } else if self.session.tab(tab)?.agent.is_some() {
                    bail!("tab 已有 agent");
                }
                if !confirmed {
                    return Ok(Some(self.request_action(Some(id),action,"创建 agent 并将终端交给 AI；现有人工 shell 内启动 Bash，输入占用将释放。y 确认，n 取消".into())));
                }
                let agent = self.create_agent(
                    tab,
                    pane,
                    None,
                    if pane.is_some() {
                        AgentKind::Pane
                    } else {
                        AgentKind::Tab
                    },
                    self.connections[&id].user.clone(),
                    None,
                )?;
                return Ok(Some(format!("agent {agent}")));
            }
            Action::Resume { agent } => {
                self.resume(agent)?;
            }
            Action::Pause { agent, tree } => {
                self.pause(agent, tree, "人明确暂停任务")?;
            }
            Action::StopAgent { agent } => {
                self.session.agent(agent)?;
                if !confirmed {
                    return Ok(Some(self.request_action(Some(id),action,"结束 agent 与子任务，停止尚在运行的命令；保留终端和对话记录。y 确认，n 取消".into())));
                }
                self.end_agent_tree(agent)?;
            }
            Action::Conversation {
                agent,
                conversation,
            } => {
                let a = self.session.agent(agent)?;
                if a.kind == AgentKind::Probe || a.state == AgentState::Stopped {
                    bail!("agent 不可切换对话");
                }
                if let Some(c) = conversation
                    && !a.conversations.iter().any(|old| old.id == c)
                {
                    bail!("对话不存在");
                }
                if !confirmed {
                    return Ok(Some(self.request_action(Some(id),action,"切换活动对话：暂停子任务并结束当前命令，保留当前文件，启动新的 Bash；需要保留的服务请先交接。y 确认，n 取消".into())));
                }
                self.pause(agent, true, "切换对话")?;
                for a in self.descendants(agent) {
                    if let Some(p) = self.session.agents[&a].pane
                        && self.terminals[&p].command.is_some()
                    {
                        self.terminals.get_mut(&p).expect("终端存在").interrupt()?;
                    }
                }
                self.switches.insert(agent, conversation);
            }
            Action::Close { tab, pane } => {
                if let Some(p) = pane
                    && self.session.pane(p)?.attached
                {
                    bail!("tab agent 存在时不能关闭唯一附属终端；请先结束 agent");
                }
                let ids = if let Some(p) = pane {
                    vec![p]
                } else {
                    self.session
                        .tab(tab.context("需要指定区域")?)?
                        .panes
                        .clone()
                };
                if !confirmed {
                    return Ok(Some(self.request_action(
                        Some(id),
                        action,
                        format!(
                            "关闭区域并停止其中受管理的进程：{}。保留服务继续运行。y 确认，n 取消",
                            self.process_summary(&ids)?
                        ),
                    )));
                }
                for p in ids {
                    self.close_pane(p)?;
                }
                if pane.is_none() {
                    let tab = tab.expect("tab 已验证");
                    self.session.tabs.retain(|t| t.id != tab);
                    if self.session.tabs.is_empty() {
                        self.session.create_tab("工作区".into());
                    }
                }
                self.normalize_connections();
            }
            Action::Resolve { request, choice } => {
                if choice == Choice::KeepRunning
                    && !matches!(self.session.requests.get(&request).map(|r| &r.kind),
                        Some(RequestKind::Action(a)) if matches!(**a, Action::Acquire { .. }))
                {
                    bail!("此请求只能确认或取消；保留命令仅用于终端接管");
                }
                let r = self
                    .session
                    .requests
                    .remove(&request)
                    .context("请求已经处理或不存在")?;
                match r.kind {
                    RequestKind::Action(action) => {
                        if choice != Choice::Reject {
                            let requester = r.requester.unwrap_or(id);
                            if !self.connections.contains_key(&requester) {
                                bail!("请求者已断线");
                            }
                            let result = self.action(requester, *action, true, choice)?;
                            if let Some(text) = result {
                                self.notice(requester, text);
                            }
                        } else if let Some(requester) = r.requester {
                            self.notice(requester, "请求已取消".into());
                        }
                    }
                    RequestKind::Directory(path) => {
                        let a = r.agent.context("目录请求必须有所属 agent")?;
                        if self.session.agent(a)?.state != AgentState::Waiting(request) {
                            bail!("agent 已离开等待状态，请求已失效");
                        }
                        if choice == Choice::Approve {
                            let mut ancestor = Some(a);
                            while let Some(id) = ancestor {
                                let a = self.session.agents.get_mut(&id).expect("agent 存在");
                                if !a.allowed.contains(&path) {
                                    a.allowed.push(path.clone());
                                }
                                ancestor = a.parent;
                            }
                            let state =
                                &mut self.session.agents.get_mut(&a).expect("agent 存在").state;
                            if *state == AgentState::Waiting(request) {
                                *state = AgentState::Idle;
                            }
                        } else {
                            let a = self.session.agents.get_mut(&a).context("agent 不存在")?;
                            if let Some(call) = a.pending.pop_front() {
                                a.tool_result(&call, "人拒绝了目录范围请求");
                            }
                            a.state = AgentState::Idle;
                            a.needs_model = true;
                        }
                    }
                }
            }
            Action::History { target } => {
                let text = match target {
                    Target::Pane(p) => {
                        let details = match &self.session.pane(p)?.display {
                            Display::Terminal => String::new(),
                            Display::Script { script, status, .. } => {
                                format!("脚本（{status}）：\n{script}\n\n")
                            }
                            Display::Edit {
                                id,
                                path,
                                before,
                                after,
                                diff,
                            } => format!(
                                "修改 {id} {}\n读取上下文：\n{before}\n写入结果：\n{after}\n差异：\n{diff}\n\n",
                                path.display()
                            ),
                        };
                        details + &self.terminals.get(&p).context("终端不存在")?.history()?
                    }
                    Target::Agent(a) => self
                        .session
                        .agent(a)?
                        .conversation()
                        .records
                        .iter()
                        .map(|r| format!("[{} {}]\n{}", r.kind, r.participants.join("、"), r.text))
                        .collect::<Vec<_>>()
                        .join("\n\n"),
                };
                return Ok(Some(text));
            }
            Action::Undo { edit } => {
                Files::undo(
                    self.session
                        .edits
                        .get_mut(&edit)
                        .context("修改记录不存在")?,
                )?;
                let record = &self.session.edits[&edit];
                let restored = record.before.clone().unwrap_or_default();
                let path = record.path.clone();
                let mut agents = Vec::new();
                for pane in self.session.panes.values_mut() {
                    if let Display::Edit {
                        id,
                        before,
                        after,
                        diff,
                        ..
                    } = &mut pane.display
                        && *id == edit
                    {
                        *before = std::mem::replace(after, restored.clone());
                        *diff = diffy::create_patch(before, after).to_string();
                        if let Some(agent) = pane.agent {
                            agents.push(agent);
                        }
                    }
                }
                for agent in agents {
                    let a = self.session.agents.get_mut(&agent).expect("agent 存在");
                    let text = format!(
                        "人撤销了修改 {edit}，文件 {} 已恢复；再次编辑前必须读取当前内容。",
                        path.display()
                    );
                    a.record("撤销", &text);
                    a.queued.push_back(json!({"role":"user","content":text}));
                }
                self.notice_all(format!("修改 {edit} 已撤销"));
            }
            Action::Service { script, cwd, title } => {
                let cwd = Files::resolve(self.session.project(), &cwd)?;
                let service = self.detach_service(script, cwd, title)?;
                return Ok(Some(format!("保留服务 {service} 已启动")));
            }
            Action::Handoff {
                pane,
                script,
                title,
            } => {
                self.session.pane(pane)?;
                if !confirmed {
                    return Ok(Some(self.request_action(Some(id),action,"交接终端服务：停止当前受管理程序，使用给定脚本在独立 Linux session 重新启动，输入关闭，输出写入文件。y 确认，n 取消".into())));
                }
                let cwd = self.terminals[&pane].cwd.clone();
                if let Some(a) = self.session.panes[&pane].agent {
                    self.pause(a, false, "交接服务")?;
                }
                self.terminals.get_mut(&pane).expect("终端存在").stop()?;
                let service = self.detach_service(script, cwd, title)?;
                self.spawn_terminal(pane, self.session.panes[&pane].agent.is_some())?;
                return Ok(Some(format!("服务 {service} 已交接")));
            }
            Action::Shutdown => {
                if !confirmed {
                    return Ok(Some(self.request_shutdown(Some(id))));
                }
                self.stopping = true;
            }
            Action::Owner { pane } => {
                let owner = self
                    .session
                    .pane(pane)?
                    .owner
                    .context("pane 没有人类操作者，请使用 --user")?;
                return Ok(Some(self.connections[&owner].user.clone()));
            }
        }
        Ok(Some("操作已应用".into()))
    }
    fn request_shutdown(&mut self, id: Option<Id>) -> String {
        if self
            .session
            .requests
            .values()
            .any(|r| matches!(&r.kind,RequestKind::Action(a) if matches!(**a,Action::Shutdown)))
        {
            return "已有退出请求，等待确认".into();
        }
        let ids: Vec<_> = self.terminals.keys().copied().collect();
        match self.process_summary(&ids){Ok(processes)=>self.request_action(id,Action::Shutdown,format!("完全退出 session 服务，停止受管理的终端进程：{processes}。已交接服务继续运行。y 确认，n 取消")),Err(e)=>{self.notice_all(format!("无法检查关闭状态：{e}"));format!("无法检查关闭状态：{e}")}}
    }
    fn process_summary(&self, ids: &[Id]) -> Result<String> {
        let mut out = Vec::new();
        for id in ids {
            for (pid, cmd) in self.terminals[id].processes()? {
                out.push(format!("pane {id} PID {pid} {cmd}"));
            }
        }
        Ok(if out.is_empty() {
            "没有运行中的外部程序".into()
        } else {
            out.join("；")
        })
    }
    fn clear_draft(&mut self, a: Id) {
        let c = self
            .session
            .agents
            .get_mut(&a)
            .expect("agent 存在")
            .conversation_mut();
        c.draft.clear();
        c.editors.clear();
        c.revision += 1;
    }
    fn slash(&mut self, id: Id, agent: Id, text: &str) -> Result<bool> {
        if !text.starts_with('/') {
            return Ok(false);
        }
        let words: Vec<_> = text.split_whitespace().collect();
        let action = match words.first().copied() {
            Some("/resume") => Action::Resume { agent },
            Some("/pause") => Action::Pause {
                agent,
                tree: words.get(1) == Some(&"tree"),
            },
            Some("/stop") => Action::StopAgent { agent },
            Some("/new") | Some("/clear") => Action::Conversation {
                agent,
                conversation: None,
            },
            Some("/restore") => Action::Conversation {
                agent,
                conversation: Some(words.get(1).context("用法 /restore 对话ID")?.parse()?),
            },
            Some("/undo") => Action::Undo {
                edit: words.get(1).context("用法 /undo 修改ID")?.parse()?,
            },
            Some("/interrupt") => {
                bail!("使用 Alt+S 打断当前 agent 并发送草稿；Alt+T 打断任务树");
            }
            Some("/help") => {
                self.notice(id,"/resume /pause [tree] /new /clear /restore ID /stop /undo ID；Alt+S 打断发送，Alt+T 打断任务树".into());
                return Ok(true);
            }
            _ => bail!("未知对话命令；使用 /help"),
        };
        if let Some(text) = self.action(id, action, false, Choice::Approve)? {
            self.notice(id, text);
        }
        Ok(true)
    }
    fn descendants(&self, agent: Id) -> Vec<Id> {
        let mut out = vec![agent];
        let mut i = 0;
        while i < out.len() {
            out.extend(self.session.agents[&out[i]].children.clone());
            i += 1;
        }
        out
    }
    fn pause(&mut self, agent: Id, tree: bool, reason: &str) -> Result<()> {
        self.session.agent(agent)?;
        let ids = if tree {
            self.descendants(agent)
        } else {
            vec![agent]
        };
        for id in ids {
            self.session.requests.retain(|_, r| r.agent != Some(id));
            if let Some(job) = self.model_jobs.remove(&id) {
                job.abort();
            }
            let a = self.session.agents.get_mut(&id).expect("agent 存在");
            if a.state == AgentState::Stopped {
                continue;
            }
            a.epoch += 1;
            if !a.streaming.is_empty() {
                let partial = std::mem::take(&mut a.streaming);
                a.record("模型流中断", partial);
            }
            a.cancel_pending(reason);
            a.state = AgentState::Paused(reason.into());
            a.record("暂停", reason);
            if let Some(parent) = a.parent {
                self.session
                    .agents
                    .get_mut(&parent)
                    .expect("父 agent 存在")
                    .record("子任务通知", format!("agent {id} 暂停：{reason}"));
            }
        }
        self.dirty = true;
        Ok(())
    }
    fn resume(&mut self, id: Id) -> Result<()> {
        let a = self.session.agent(id)?;
        if self.model.is_none() {
            bail!("{}", self.session.model_status);
        }
        if a.state == AgentState::Stopped {
            bail!("agent 已结束");
        }
        let pane = a.pane;
        if let Some(p) = pane {
            if self.pane_terminal_owner(p) {
                bail!("请先结束终端输入占用；对话输入可以保留");
            }
            let t = &self.terminals[&p];
            if !t.bash_ready() {
                bail!(
                    "尚未回到原 AI Bash；请退出个人 shell 或等待当前命令结束，且保留 CREW Bash hooks"
                );
            }
            if t.command.is_some() {
                bail!("当前命令尚未结束");
            }
            let current = Files::hashes(&t.cwd)?;
            let mut changed = Vec::new();
            if let Some(before) = &t.takeover_hashes {
                for (path, digest) in &current {
                    if before.get(path) != Some(digest) {
                        changed.push(path.display().to_string());
                    }
                }
                for path in before.keys() {
                    if !current.contains_key(path) {
                        changed.push(format!("{}（已删除）", path.display()));
                    }
                }
            }
            let record = format!(
                "恢复前检查：cwd={}，原 AI Bash PID={}，前台进程组={:?}。接管期间文件变化可能包含多人修改：{}\n终端记录：\n{}",
                t.cwd.display(),
                t.bash_pid.expect("原 Bash 已确认"),
                t.master.process_group_leader(),
                changed.join("、"),
                t.output_since(t.takeover_offset)?
            );
            let cwd = t.cwd.clone();
            let a = self.session.agents.get_mut(&id).expect("agent 存在");
            a.conversation_mut().cwd = cwd;
            a.conversation_mut().context.push(json!({"role":"user","content":format!("{record}\n请重新检查环境与任务状态后继续。不要把文件变化归因给某个人。") }));
            a.record("交接", record);
            self.terminals
                .get_mut(&p)
                .expect("终端存在")
                .takeover_hashes = None;
        }
        let a = self.session.agents.get_mut(&id).expect("agent 存在");
        a.state = AgentState::Idle;
        a.needs_model = true;
        self.dirty = true;
        Ok(())
    }
    fn create_agent(
        &mut self,
        tab: Id,
        pane: Option<Id>,
        parent: Option<Id>,
        kind: AgentKind,
        creator: String,
        task: Option<String>,
    ) -> Result<Id> {
        let cwd = parent
            .map(|a| self.session.agents[&a].conversation().cwd.clone())
            .or_else(|| pane.map(|p| self.terminals[&p].cwd.clone()))
            .unwrap_or_else(|| self.session.project.clone());
        let pane = if kind == AgentKind::Probe {
            None
        } else if let Some(p) = pane {
            if let Some(owner) = self.session.panes[&p].owner {
                self.release(owner);
                self.notice(owner, "终端已交给 AI，操作占用已释放".into());
            }
            let rc = self.session.runtime_dir().join("bashrc");
            fs::write(&rc, include_str!("bashrc"))?;
            let line = format!(
                "/bin/bash --noprofile --rcfile {} -i\n",
                shell_quote(rc.to_string_lossy().as_ref())
            );
            let terminal = self.terminals.get_mut(&p).expect("终端存在");
            if terminal.busy() {
                bail!("人工 shell 仍有运行命令，请先结束命令再创建 agent");
            }
            terminal.bash_pid = None;
            terminal.ready = false;
            terminal.prompt_status = None;
            terminal.write(line.as_bytes())?;
            Some(p)
        } else {
            let p = self.session.create_pane(
                tab,
                if kind == AgentKind::Tab {
                    "tab agent 执行终端".into()
                } else {
                    "工作 agent".into()
                },
                creator,
                cwd.clone(),
            )?;
            self.spawn_new_terminal(p, true)?;
            Some(p)
        };
        let id = self.session.id();
        let conversation = self.session.id();
        let allowed = parent
            .map(|p| self.session.agents[&p].allowed.clone())
            .unwrap_or_else(|| vec![self.session.project.clone()]);
        let mut a = Agent::new(
            id,
            conversation,
            kind.clone(),
            AgentLocation { tab, pane, parent },
            cwd,
            allowed,
        );
        if let Some(task) = task {
            a.conversation_mut()
                .context
                .push(json!({"role":"user","content":task}));
            a.record("委派任务", task);
            a.needs_model = true;
        }
        self.session.agents.insert(id, a);
        if let Some(p) = pane {
            let p = self.session.panes.get_mut(&p).expect("pane 存在");
            p.agent = Some(id);
            p.attached = kind == AgentKind::Tab;
        }
        if kind == AgentKind::Tab {
            self.session.tab_mut(tab)?.agent = Some(id);
        }
        if let Some(parent) = parent {
            self.session
                .agents
                .get_mut(&parent)
                .expect("父 agent 存在")
                .children
                .push(id);
        }
        self.dirty = true;
        Ok(id)
    }
    fn terminal_event(&mut self, event: TerminalEvent) -> Result<()> {
        let Some(t) = self.terminals.get_mut(&event.pane) else {
            return Ok(());
        };
        if t.generation != event.generation {
            return Ok(());
        }
        match event.result {
            Ok(bytes) if !bytes.is_empty() => t.output(&bytes)?,
            Ok(_) => {}
            Err(e) if e.raw_os_error() == Some(libc::EIO) => {}
            Err(e) => {
                self.notice_all(format!("pane {} 终端读取失败：{e}", event.pane));
            }
        }
        self.dirty = true;
        Ok(())
    }
    fn shell_event(&mut self, event: ShellEvent) -> Result<()> {
        let Some(t) = self.terminals.get_mut(&event.pane) else {
            return Ok(());
        };
        if t.generation != event.generation {
            return Ok(());
        }
        t.shell_event(&event);
        self.session
            .panes
            .get_mut(&event.pane)
            .expect("pane 存在")
            .cwd = t.cwd.clone();
        if let Some(a) = self.session.panes[&event.pane].agent {
            self.session
                .agents
                .get_mut(&a)
                .expect("agent 存在")
                .conversation_mut()
                .cwd = t.cwd.clone();
        }
        if let Display::Script { path, active, .. } = &mut self
            .session
            .panes
            .get_mut(&event.pane)
            .expect("pane 存在")
            .display
            && event.kind == "trace"
            && std::path::Path::new(&event.source) == path
        {
            active.insert(event.pid, event.line);
        }
        self.dirty = true;
        Ok(())
    }
    fn finish_bash_command(&mut self, pane: Id) -> Result<()> {
        let t = self.terminals.get_mut(&pane).expect("终端存在");
        if t.command.is_some() && t.bash_ready() && t.prompt_status.is_some() {
            let status = t.prompt_status.take().expect("返回状态存在");
            let run = t.command.take().expect("命令存在");
            let output = t.output_since(run.offset)?;
            let result = format!("Bash 返回状态 {status}；cwd={}\n{output}", t.cwd.display());
            self.finish_tool(
                run.agent,
                &run.call,
                if status == 0 {
                    Ok(result)
                } else {
                    Err(anyhow::anyhow!(result))
                },
            );
            if let Display::Script {
                active,
                status: display_status,
                ..
            } = &mut self
                .session
                .panes
                .get_mut(&pane)
                .expect("pane 存在")
                .display
            {
                active.clear();
                *display_status = format!(
                    "{}，返回状态 {status}",
                    if status == 0 { "完成" } else { "失败" }
                );
            }
        }
        Ok(())
    }
    fn poll_terminals(&mut self) -> Result<()> {
        let ids: Vec<_> = self.terminals.keys().copied().collect();
        for id in ids {
            self.finish_bash_command(id)?;
            if let Some(status) = self.terminals.get_mut(&id).expect("终端存在").poll()? {
                if let Some(run) = self
                    .terminals
                    .get_mut(&id)
                    .expect("终端存在")
                    .command
                    .take()
                {
                    self.finish_tool(run.agent, &run.call, Err(anyhow::anyhow!(status.clone())));
                }
                if let Some(a) = self.session.panes[&id].agent {
                    self.block(a, status.clone());
                }
                self.notice_all(format!("pane {id}: {status}"));
            }
        }
        Ok(())
    }
    fn block(&mut self, id: Id, reason: String) {
        let a = self.session.agents.get_mut(&id).expect("agent 存在");
        a.state = AgentState::Blocked(reason.clone());
        a.record("阻塞", reason.clone());
        if let Some(parent) = a.parent {
            let message = format!("子 agent {id} 阻塞：{reason}");
            let parent = self.session.agents.get_mut(&parent).expect("父 agent 存在");
            parent.record("子任务阻塞", &message);
            parent
                .queued
                .push_back(json!({"role":"user","content":message}));
        }
        self.notice_all(format!("agent {id} 阻塞：{reason}"));
    }
    fn model_event(&mut self, event: ModelEvent) -> Result<()> {
        match event {
            ModelEvent::Retry {
                agent,
                epoch,
                attempt,
                reason,
            } => {
                if let Some(a) = self.session.agents.get_mut(&agent)
                    && a.epoch == epoch
                {
                    a.record(
                        "模型重试",
                        format!("第 {attempt}/2 次重试，尚未产生流内容：{reason}"),
                    );
                }
            }
            ModelEvent::Delta { agent, epoch, text } => {
                if let Some(a) = self.session.agents.get_mut(&agent)
                    && a.epoch == epoch
                {
                    a.streaming.push_str(&text);
                }
            }
            ModelEvent::Complete {
                agent,
                epoch,
                result,
            } => {
                if !self.session.agents.contains_key(&agent)
                    || self.session.agents[&agent].epoch != epoch
                {
                    return Ok(());
                }
                self.model_jobs.remove(&agent);
                match result {
                    Ok(c) => {
                        let a = self.session.agents.get_mut(&agent).expect("agent 存在");
                        a.streaming.clear();
                        a.conversation_mut().context.push(c.message);
                        if !c.text.is_empty() {
                            a.record("AI", c.text.clone());
                        }
                        if c.calls.is_empty() {
                            a.state = AgentState::Completed;
                            a.needs_model = false;
                            self.notify_parent(agent, &c.text);
                        } else {
                            a.pending = c.calls.into();
                            a.state = AgentState::Idle;
                        }
                    }
                    Err(e) => {
                        let a = self.session.agents.get_mut(&agent).expect("agent 存在");
                        if !a.streaming.is_empty() {
                            let partial = std::mem::take(&mut a.streaming);
                            a.record("模型流失败", partial);
                        }
                        self.block(agent, e.to_string());
                    }
                }
            }
        }
        self.dirty = true;
        Ok(())
    }
    fn notify_parent(&mut self, agent: Id, text: &str) {
        let parent = self.session.agents[&agent].parent;
        if let Some(parent) = parent {
            let a = self.session.agents.get_mut(&parent).expect("父 agent 存在");
            let message = format!("子 agent {agent} 已完成：\n{text}");
            a.record("子任务结果", &message);
            a.queued.push_back(json!({"role":"user","content":message}));
        }
    }
    fn finish_tool(&mut self, agent: Id, call: &ToolCall, result: Result<String>) {
        let a = self
            .session
            .agents
            .get_mut(&agent)
            .expect("工具所属 agent 存在");
        let result = match result {
            Ok(text) => text,
            Err(e) => format!("工具失败：{e}"),
        };
        a.tool_result(call, &result);
        if !matches!(
            a.state,
            AgentState::Paused(_) | AgentState::Stopped | AgentState::Blocked(_)
        ) {
            a.state = AgentState::Idle;
        }
        a.needs_model = true;
        self.dirty = true;
    }
    fn advance_agents(&mut self) -> Result<()> {
        let ids: Vec<_> = self.session.agents.keys().copied().collect();
        for id in ids {
            if !self.session.agents[&id].runnable() || self.switches.contains_key(&id) {
                continue;
            }
            if self.session.agents[&id].pane.is_some_and(|p| {
                self.terminals[&p].command.is_some() || !self.terminals[&p].bash_ready()
            }) {
                continue;
            }
            if let Some(call) = self.session.agents[&id].pending.front().cloned() {
                if let Err(e) = self.execute_tool(id, call.clone()) {
                    let a = self.session.agents.get_mut(&id).expect("agent 存在");
                    if a.pending.front().is_some_and(|c| c.id == call.id) {
                        a.pending.pop_front();
                    }
                    self.finish_tool(id, &call, Err(e));
                }
                continue;
            }
            let a = self.session.agents.get_mut(&id).expect("agent 存在");
            while let Some(message) = a.queued.pop_front() {
                a.conversation_mut().context.push(message);
                a.needs_model = true;
            }
            if a.needs_model {
                let messages = match self.messages(id) {
                    Ok(messages) => messages,
                    Err(e) => {
                        self.block(id, e.to_string());
                        continue;
                    }
                };
                let a = self.session.agents.get_mut(&id).expect("agent 存在");
                a.state = AgentState::Thinking;
                a.needs_model = false;
                a.streaming.clear();
                let kind = a.kind.clone();
                let epoch = a.epoch;
                let model = self.model.clone().context("运行 agent 必须配置模型")?;
                let tx = self.model_tx.clone();
                let job = tokio::spawn(async move {
                    let result = model.complete(messages, kind, id, epoch, tx.clone()).await;
                    let _ = tx.send(ModelEvent::Complete {
                        agent: id,
                        epoch,
                        result,
                    });
                });
                self.model_jobs.insert(id, job);
                self.dirty = true;
            }
        }
        Ok(())
    }
    fn messages(&self, id: Id) -> Result<Vec<Value>> {
        let a = &self.session.agents[&id];
        let mut instructions = format!(
            "你是 CREW 的 {:?} agent。工作目录 {}。允许目录：{:?}。固定工具能力不可更改。人与人没有权限模型。共享工作目录不锁文件。禁止主动通过 Bash 编辑文件，必须先 read_file 再精确替换或补丁。探测助手只读且无 shell。任务结束保留上下文。人接管后暂停，不得自行恢复，等待 /resume。不能收回人正在操作的 pane。访问范围外目录先申请确认，无系统隔离。使用 bash 工具执行测试；长驻服务使用 detach_service，不更改 CREW hooks。委派安全并行且互不依赖的任务。失败时根据实际错误恢复，不能继续则明确说明阻塞。",
            a.kind,
            a.conversation().cwd.display(),
            a.allowed
        );
        let mut paths = vec![self.session.project.join("AGENT.md")];
        paths.extend(self.config.instructions.iter().map(|p| {
            if p.is_absolute() {
                p.clone()
            } else {
                self.session.project.join(p)
            }
        }));
        for path in paths {
            if path.exists() {
                if fs::metadata(&path)?.len() > 100_000 {
                    bail!("agent 说明文件超过 100 KB");
                }
                instructions.push_str(&format!(
                    "\n项目说明（受固定能力限制）：\n{}",
                    fs::read_to_string(path)?
                ));
            }
        }
        let mut messages = vec![json!({"role":"system","content":instructions})];
        messages.extend(a.conversation().context.clone());
        Ok(messages)
    }
    fn execute_tool(&mut self, id: Id, call: ToolCall) -> Result<()> {
        let a = self.session.agent(id)?;
        if !crate::model::permits(&a.kind, &call.name) {
            bail!("此层级不能使用工具 {}", call.name);
        }
        let args: Value = serde_json::from_str(&call.arguments)?;
        let mut path = None;
        if [
            "read_file",
            "list_directory",
            "search_files",
            "edit_file",
            "apply_patch",
            "create_file",
            "request_directory",
        ]
        .contains(&call.name.as_str())
        {
            let resolved = Files::resolve(&a.conversation().cwd, string(&args, "path")?)?;
            if !Files::authorized(&resolved, &a.allowed) {
                let directory = if resolved.is_dir() {
                    resolved.clone()
                } else {
                    resolved.parent().expect("文件有父目录").into()
                };
                let request = self.session.request(
                    format!(
                        "agent {id} 请求访问 {}；任何参与者均可确认",
                        directory.display()
                    ),
                    None,
                    Some(id),
                    RequestKind::Directory(directory),
                );
                self.session.agents.get_mut(&id).expect("agent 存在").state =
                    AgentState::Waiting(request);
                self.dirty = true;
                return Ok(());
            }
            path = Some(resolved);
        }
        let a = self.session.agents.get_mut(&id).expect("agent 存在");
        let next = a.pending.pop_front().expect("工具必须处于待执行队列");
        assert_eq!(next.id, call.id);
        a.record("工具调用", format!("{} {}", call.name, call.arguments));
        match call.name.as_str() {
            "read_file" => {
                let result = self.files.read(
                    id,
                    path.as_ref().expect("路径已解析"),
                    args["start"].as_u64().unwrap_or(1) as usize,
                    args["end"].as_u64().unwrap_or(usize::MAX as u64) as usize,
                );
                self.finish_tool(id, &call, result);
            }
            "list_directory" => {
                self.finish_tool(id, &call, Files::list(path.as_ref().expect("路径已解析")));
            }
            "search_files" => {
                self.finish_tool(
                    id,
                    &call,
                    Files::search(
                        path.as_ref().expect("路径已解析"),
                        string(&args, "pattern")?,
                    ),
                );
            }
            "request_directory" => {
                self.finish_tool(id, &call, Ok("目录已授权".into()));
            }
            "edit_file" | "apply_patch" | "create_file" => {
                let operation = match call.name.as_str() {
                    "edit_file" => Operation::Replace {
                        old: string(&args, "old")?.into(),
                        new: string(&args, "new")?.into(),
                    },
                    "apply_patch" => Operation::Patch(string(&args, "patch")?.into()),
                    _ => Operation::Create(string(&args, "content")?.into()),
                };
                let edit_id = self.session.id();
                let path = path.expect("路径已解析");
                let (edit, before, after, diff) = self.files.edit(id, edit_id, &path, operation)?;
                let p = self.session.agents[&id]
                    .pane
                    .expect("可写 agent 必须有 pane");
                self.session.panes.get_mut(&p).expect("pane 存在").display = Display::Edit {
                    id: edit_id,
                    path,
                    before,
                    after,
                    diff: diff.clone(),
                };
                self.session.edits.insert(edit_id, edit);
                self.finish_tool(id, &call, Ok(format!("已写入；修改 ID {edit_id}\n{diff}")));
            }
            "bash" => {
                let pane = self.session.agents[&id]
                    .pane
                    .expect("执行 agent 必须有 pane");
                if self.pane_terminal_owner(pane) {
                    bail!("人正在终端输入，AI 不可执行");
                }
                let t = self.terminals.get_mut(&pane).expect("终端存在");
                if !t.bash_ready() {
                    bail!("原 AI Bash 未就绪；请检查命令或个人 shell 状态");
                }
                let script = string(&args, "script")?.to_owned();
                if script.len() > 100_000 {
                    bail!("脚本超过 100 KB");
                }
                let script_path = self
                    .session
                    .runtime_dir()
                    .join(format!("script-{id}-{}.sh", self.session.next_id));
                self.session.next_id += 1;
                fs::write(&script_path, &script)?;
                let offset = t.offset()?;
                t.command = Some(CommandRun {
                    agent: id,
                    call: call.clone(),
                    path: script_path.clone(),
                    started: false,
                    offset,
                });
                t.ready = false;
                t.prompt_status = None;
                self.session
                    .panes
                    .get_mut(&pane)
                    .expect("pane 存在")
                    .display = Display::Script {
                    script,
                    path: script_path.clone(),
                    active: BTreeMap::new(),
                    status: "执行中；活动位置来自 Bash DEBUG，外部程序显示调用行".into(),
                };
                let line = format!(
                    "source {}\n",
                    shell_quote(script_path.to_string_lossy().as_ref())
                );
                t.write(line.as_bytes())?;
                self.session.agents.get_mut(&id).expect("agent 存在").state = AgentState::Executing;
            }
            "detach_service" => {
                let cwd = self.session.agents[&id].conversation().cwd.clone();
                let service = self.detach_service(
                    string(&args, "script")?.into(),
                    cwd,
                    string(&args, "title")?.into(),
                )?;
                self.finish_tool(
                    id,
                    &call,
                    Ok(format!(
                        "保留服务 {service} 已启动，输出与进程交接记录已保存"
                    )),
                );
            }
            "probe" | "delegate" => {
                let kind = if call.name == "probe" {
                    AgentKind::Probe
                } else {
                    AgentKind::Pane
                };
                let tab = self.session.agents[&id].tab;
                let child = self.create_agent(
                    tab,
                    None,
                    Some(id),
                    kind,
                    "agent".into(),
                    Some(string(&args, "task")?.into()),
                )?;
                if let Some(p) = self.session.agents[&child].pane {
                    self.session.panes.get_mut(&p).expect("pane 存在").title =
                        string(&args, "title")?.into();
                }
                self.finish_tool(
                    id,
                    &call,
                    Ok(format!(
                        "已启动 agent {child}；状态与结果将记录在所属工具区"
                    )),
                );
            }
            "cancel_agent" | "agent_status" => {
                let child = args["agent"].as_u64().context("需要 agent ID")?;
                if !self.session.agents[&id].children.contains(&child) {
                    bail!("只能管理直接子 agent");
                }
                if call.name == "cancel_agent" {
                    self.pause(child, true, "父 agent 取消子任务")?;
                    if let Some(p) = self.session.agents[&child].pane
                        && !self.pane_terminal_owner(p)
                        && self.terminals[&p].command.is_some()
                    {
                        self.terminals.get_mut(&p).expect("终端存在").interrupt()?;
                    }
                }
                let a = &self.session.agents[&child];
                let result = format!(
                    "状态 {:?}\n{}",
                    a.state,
                    a.conversation()
                        .records
                        .iter()
                        .map(|r| r.text.as_str())
                        .collect::<Vec<_>>()
                        .join("\n")
                );
                self.finish_tool(id, &call, Ok(result));
            }
            _ => unreachable!("工具集合必须覆盖全部可调用工具"),
        }
        self.dirty = true;
        Ok(())
    }
    fn detach_service(&mut self, script: String, cwd: PathBuf, title: String) -> Result<Id> {
        if script.trim().is_empty() {
            bail!("服务脚本为空");
        }
        let id = self.session.id();
        let dir = self.session.project.join(".crew/services");
        fs::create_dir_all(&dir)?;
        let log_path = dir.join(format!("{}-{id}.log", self.session.name));
        let log = OpenOptions::new()
            .create_new(true)
            .write(true)
            .open(&log_path)?;
        let mut command = Command::new("setsid");
        command
            .args(["/bin/bash", "--noprofile", "--norc", "-c", &script])
            .current_dir(&cwd)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log);
        if let Some(name) = self
            .config
            .model
            .as_ref()
            .and_then(|m| m.api_key_env.as_deref())
        {
            command.env_remove(name);
        }
        let mut child = command.spawn()?;
        std::thread::sleep(Duration::from_millis(80));
        if let Some(status) = child.try_wait()? {
            bail!("服务命令已退出：{status}；输出见 {}", log_path.display());
        }
        let stat = procfs::process::Process::new(child.id() as i32)?.stat()?;
        if stat.session != child.id() as i32 {
            bail!("服务尚未建立独立 Linux session");
        }
        self.session.services.push(Service {
            id,
            title,
            pid: child.id(),
            start_time: stat.starttime,
            cwd,
            script,
            log: log_path,
        });
        self.service_children.push(child);
        self.session.save()?;
        self.dirty = true;
        Ok(id)
    }
    fn reap_services(&mut self) -> Result<()> {
        let mut i = 0;
        while i < self.service_children.len() {
            if self.service_children[i].try_wait()?.is_some() {
                self.service_children.remove(i).wait()?;
            } else {
                i += 1;
            }
        }
        Ok(())
    }
    fn close_pane(&mut self, p: Id) -> Result<()> {
        let pane = self.session.pane(p)?.clone();
        if let Some(a) = pane.agent {
            self.end_agent_tree(a)?;
        }
        self.terminals.get_mut(&p).expect("终端存在").stop()?;
        self.terminals.remove(&p);
        self.session.panes.remove(&p);
        if let Some(a) = pane.agent {
            self.session.agents.get_mut(&a).expect("agent 存在").pane = None;
            if self.session.tab(pane.tab)?.agent == Some(a) {
                self.session.tab_mut(pane.tab)?.agent = None;
            }
        }
        let t = self.session.tab_mut(pane.tab)?;
        t.panes.retain(|id| *id != p);
        t.weights.remove(&p);
        Ok(())
    }
    fn spawn_new_terminal(&mut self, pane: Id, ai: bool) -> Result<()> {
        if let Err(error) = self.spawn_terminal(pane, ai) {
            let p = self.session.panes.remove(&pane).expect("新 pane 存在");
            let tab = self.session.tab_mut(p.tab)?;
            tab.panes.retain(|id| *id != pane);
            tab.weights.remove(&pane);
            return Err(error);
        }
        Ok(())
    }
    fn end_agent_tree(&mut self, agent: Id) -> Result<()> {
        self.pause(agent, true, "agent 已结束")?;
        for id in self.descendants(agent) {
            let a = &self.session.agents[&id];
            let (pane, tab) = (a.pane, a.tab);
            let owners: Vec<_> = self
                .connections
                .values()
                .filter(|c| {
                    c.chat == Some(id)
                        && (c.mode == Mode::Chat
                            || matches!(&c.mode, Mode::Review { origin } if **origin == Mode::Chat))
                })
                .map(|c| c.id)
                .collect();
            for owner in owners {
                self.release(owner);
                self.notice(owner, format!("agent {id} 已结束，对话占用已释放"));
            }
            if let Some(pane) = pane {
                if self.terminals[&pane].command.is_some() {
                    self.terminals
                        .get_mut(&pane)
                        .expect("终端存在")
                        .interrupt()?;
                }
                let p = self.session.panes.get_mut(&pane).expect("pane 存在");
                p.agent = None;
                p.attached = false;
            }
            if self
                .session
                .tabs
                .iter()
                .any(|t| t.id == tab && t.agent == Some(id))
            {
                self.session.tab_mut(tab)?.agent = None;
            }
            self.session.agents.get_mut(&id).expect("agent 存在").state = AgentState::Stopped;
        }
        Ok(())
    }
    fn normalize_connections(&mut self) {
        for c in self.connections.values_mut() {
            if !self.session.tabs.iter().any(|t| t.id == c.tab) {
                c.tab = self.session.tabs[0].id;
                match &mut c.mode {
                    Mode::Review { origin } => **origin = Mode::Session,
                    mode => *mode = Mode::Session,
                }
                c.chat = None;
            }
            if c.pane.is_some_and(|p| !self.session.panes.contains_key(&p)) {
                c.pane = self
                    .session
                    .tab(c.tab)
                    .expect("tab 存在")
                    .panes
                    .first()
                    .copied();
                if c.mode != Mode::Session {
                    match &mut c.mode {
                        Mode::Review { origin } if **origin != Mode::Session => {
                            **origin = Mode::Tab
                        }
                        Mode::Review { .. } => {}
                        mode => *mode = Mode::Tab,
                    }
                }
                c.chat = None;
            }
        }
    }
    fn finish_switches(&mut self) -> Result<()> {
        let ids: Vec<_> = self.switches.keys().copied().collect();
        for id in ids {
            if self.descendants(id).iter().any(|a| {
                self.session.agents[a]
                    .pane
                    .is_some_and(|p| self.terminals[&p].command.is_some())
            }) {
                continue;
            }
            let conversation = self.switches.remove(&id).expect("切换存在");
            let pane = self.session.agents[&id].pane.expect("对话必须有终端");
            let owner = self.session.panes[&pane].owner;
            if let Some(owner) = owner {
                self.release(owner);
                self.notice(owner, "切换对话将启动新的 Bash，终端占用已释放".into());
            }
            self.terminals.get_mut(&pane).expect("终端存在").stop()?;
            let a = self.session.agents.get_mut(&id).expect("agent 存在");
            while let Some(message) = a.queued.pop_front() {
                a.conversation_mut().context.push(message);
            }
            if let Some(c) = conversation {
                self.session.agents.get_mut(&id).expect("agent 存在").active = c;
            } else {
                let c = self.session.id();
                let cwd = self.session.agents[&id].conversation().cwd.clone();
                let a = self.session.agents.get_mut(&id).expect("agent 存在");
                a.conversations.push(Conversation::new(c, cwd));
                a.active = c;
            }
            let cwd = self.session.agents[&id].conversation().cwd.clone();
            self.session.panes.get_mut(&pane).expect("pane 存在").cwd = cwd;
            self.spawn_terminal(pane, true)?;
            let a = self.session.agents.get_mut(&id).expect("agent 存在");
            a.state = AgentState::Idle;
            a.needs_model = conversation.is_some();
            a.conversation_mut().context.push(json!({"role":"user","content":"此对话在新的 Bash 中成为活动对话。当前文件保留，旧 shell 临时状态没有恢复；下一次工作前重新检查目录和文件。"}));
            a.record(
                "对话切换",
                "新的 Bash 已启动；当前文件保留，旧 shell 临时状态不恢复",
            );
            self.dirty = true;
        }
        Ok(())
    }
}

fn shell_quote(value: &str) -> String {
    shell_escape::unix::escape(value.into()).into_owned()
}
fn string<'a>(args: &'a Value, name: &str) -> Result<&'a str> {
    args[name]
        .as_str()
        .with_context(|| format!("参数 {name} 必须为字符串"))
}

async fn peer(stream: UnixStream, tx: mpsc::UnboundedSender<PeerEvent>) {
    let (read, mut write) = stream.into_split();
    let mut reader = BufReader::new(read);
    let mut line = String::new();
    let hello = match reader.read_line(&mut line).await {
        Ok(n) if n > 0 && n <= 1_000_000 => serde_json::from_str::<ClientMessage>(&line),
        _ => return,
    };
    let (user, cols, rows) = match hello {
        Ok(ClientMessage::Hello { user, cols, rows }) => (user, cols, rows),
        _ => return,
    };
    let (reply, mut messages) = mpsc::channel::<ServerMessage>(32);
    let (send_id, receive_id) = tokio::sync::oneshot::channel();
    if tx
        .send(PeerEvent::Join {
            user,
            cols,
            rows,
            reply,
            id: send_id,
        })
        .is_err()
    {
        return;
    }
    let id = match receive_id.await {
        Ok(Ok(id)) => id,
        Ok(Err(error)) => {
            if let Ok(mut bytes) = serde_json::to_vec(&ServerMessage::Bye(error)) {
                bytes.push(b'\n');
                let _ = write.write_all(&bytes).await;
            }
            return;
        }
        Err(_) => return,
    };
    loop {
        line.clear();
        tokio::select! {
            message=messages.recv()=>{
                let Some(message)=message else{break};let bye=matches!(message,ServerMessage::Bye(_));
                let mut bytes=serde_json::to_vec(&message).expect("服务消息必须可以序列化");bytes.push(b'\n');
                if write.write_all(&bytes).await.is_err()||bye{break;}
            },
            incoming=reader.read_line(&mut line)=>{
                match incoming {
                    Ok(n) if n>0&&n<=1_000_000=>{
                        match serde_json::from_str(&line){Ok(ClientMessage::Action{seq,action})=>{if tx.send(PeerEvent::Action{connection:id,seq,action}).is_err(){break;}},_=>break}
                    },
                    _=>break,
                }
            }
        }
    }
    let _ = tx.send(PeerEvent::Leave(id));
}
