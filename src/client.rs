use crate::{
    config::socket_path,
    protocol::{Action, ClientMessage, ServerMessage, Snapshot},
    session::Id,
};
use anyhow::{Context, Result, bail};
use std::{
    fs,
    io::{BufRead, BufReader, Write},
    os::unix::net::UnixStream,
    path::Path,
    process::{Command, Stdio},
    sync::mpsc,
    time::{Duration, Instant},
};

pub struct Client {
    stream: UnixStream,
    pub rx: mpsc::Receiver<ServerMessage>,
    pub connection: Id,
    pub snapshot: Snapshot,
    seq: u64,
    pub notices: Vec<String>,
    pub closed: bool,
    pending_navigation: Option<(u64, crate::session::Connection)>,
}

impl Client {
    pub fn connect(
        project: &Path,
        session: &str,
        user: &str,
        cols: u16,
        rows: u16,
        start: bool,
    ) -> Result<Self> {
        let socket = socket_path(project, session)?;
        let stream = match UnixStream::connect(&socket) {
            Ok(stream) => stream,
            Err(e)
                if start
                    && (e.kind() == std::io::ErrorKind::NotFound
                        || e.kind() == std::io::ErrorKind::ConnectionRefused) =>
            {
                fs::create_dir_all(project.join(".crew"))?;
                let log = fs::OpenOptions::new()
                    .create(true)
                    .append(true)
                    .open(project.join(format!(".crew/{session}.server.log")))?;
                let mut child = Command::new("setsid")
                    .arg(std::env::current_exe()?)
                    .args(["serve", "--session", session, "--project"])
                    .arg(project)
                    .stdin(Stdio::null())
                    .stdout(log.try_clone()?)
                    .stderr(log)
                    .spawn()?;
                let deadline = Instant::now() + Duration::from_secs(5);
                loop {
                    if let Ok(stream) = UnixStream::connect(&socket) {
                        break stream;
                    }
                    if let Some(status) = child.try_wait()? {
                        bail!("服务启动失败 {status}；请查看 .crew/{session}.server.log");
                    }
                    if Instant::now() > deadline {
                        bail!("服务启动超时；请查看 .crew/{session}.server.log");
                    }
                    std::thread::sleep(Duration::from_millis(30));
                }
            }
            Err(error) => return Err(error).context("无法连接 session"),
        };
        Self::from_stream(stream, user, cols, rows)
    }
    pub fn from_stream(mut stream: UnixStream, user: &str, cols: u16, rows: u16) -> Result<Self> {
        write_message(
            &mut stream,
            &ClientMessage::Hello {
                user: user.into(),
                cols,
                rows,
            },
        )?;
        let reader = stream.try_clone()?;
        let (tx, rx) = mpsc::channel();
        std::thread::spawn(move || {
            let mut reader = BufReader::new(reader);
            let mut line = String::new();
            loop {
                line.clear();
                let result = match reader.read_line(&mut line) {
                    Ok(0) => break,
                    Ok(n) if n <= 20_000_000 => serde_json::from_str(&line),
                    Ok(_) => {
                        let _ = tx.send(ServerMessage::Bye("服务帧超过 20 MB".into()));
                        break;
                    }
                    Err(e) => {
                        let _ = tx.send(ServerMessage::Bye(format!("连接读取失败：{e}")));
                        break;
                    }
                };
                match result {
                    Ok(message) => {
                        if tx.send(message).is_err() {
                            return;
                        }
                    }
                    Err(e) => {
                        let _ = tx.send(ServerMessage::Bye(format!("服务 JSON 无效：{e}")));
                        return;
                    }
                }
            }
            let _ = tx.send(ServerMessage::Bye(
                "连接已断开，操作占用将释放；服务任务继续".into(),
            ));
        });
        let deadline = Instant::now() + Duration::from_secs(8);
        let mut connection = None;
        loop {
            let message = rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .context("等待 session 状态失败")?;
            match message {
                ServerMessage::Welcome { connection: id } => connection = Some(id),
                ServerMessage::Snapshot(snapshot) => {
                    return Ok(Self {
                        stream,
                        rx,
                        connection: connection.context("缺少连接 ID")?,
                        snapshot: *snapshot,
                        seq: 0,
                        notices: vec![],
                        closed: false,
                        pending_navigation: None,
                    });
                }
                ServerMessage::Bye(reason) => bail!("{reason}"),
                _ => {}
            }
        }
    }
    pub fn action(&mut self, action: Action) -> Result<u64> {
        self.seq += 1;
        write_message(
            &mut self.stream,
            &ClientMessage::Action {
                seq: self.seq,
                action: action.clone(),
            },
        )?;
        if let Some(c) = self.snapshot.connections.get_mut(&self.connection) {
            let changed = match action {
                Action::Focus { tab, pane, mode } => {
                    c.tab = tab;
                    c.pane = pane;
                    c.mode = mode;
                    c.chat = None;
                    true
                }
                Action::Review { enter: true } => {
                    c.mode = crate::protocol::Mode::Review {
                        origin: Box::new(c.mode.clone()),
                    };
                    true
                }
                Action::Review { enter: false } => {
                    if let crate::protocol::Mode::Review { origin } = &c.mode {
                        c.mode = *origin.clone();
                    }
                    true
                }
                _ => false,
            };
            if changed {
                self.pending_navigation = Some((self.seq, c.clone()));
            }
        }
        Ok(self.seq)
    }
    pub fn request(&mut self, action: Action) -> Result<String> {
        let seq = self.action(action)?;
        let deadline = Instant::now() + Duration::from_secs(15);
        loop {
            let message = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .context("等待操作结果超时")?;
            match message {
                ServerMessage::Reply {
                    seq: reply,
                    ok,
                    text,
                } if reply == seq => {
                    self.navigation_reply(reply);
                    if ok {
                        return Ok(text);
                    }
                    bail!("{text}");
                }
                other => self.apply(other),
            }
            if self.closed {
                bail!("session 连接已关闭");
            }
        }
    }
    pub fn apply(&mut self, message: ServerMessage) {
        match message {
            ServerMessage::Snapshot(snapshot) => {
                let mut snapshot = *snapshot;
                if let Some((_, c)) = &self.pending_navigation {
                    snapshot.connections.insert(self.connection, c.clone());
                }
                // 当前操作者尚未收到确认的草稿版本用于连续按键输入。
                if let Some(c) = snapshot.connections.get(&self.connection)
                    && c.mode == crate::protocol::Mode::Chat
                    && let Some(id) = c.chat
                    && let (Some(old), Some(new)) = (
                        self.snapshot.session.agents.get(&id),
                        snapshot.session.agents.get_mut(&id),
                    )
                    && old.active == new.active
                    && old.conversation().revision > new.conversation().revision
                {
                    let old = old.conversation();
                    let new = new.conversation_mut();
                    new.draft = old.draft.clone();
                    new.revision = old.revision;
                    new.editors = old.editors.clone();
                }
                self.snapshot = snapshot;
            }
            ServerMessage::Notice(text) => self.notices.push(text),
            ServerMessage::Reply { seq, ok, text } => {
                self.navigation_reply(seq);
                if !ok {
                    self.notices.push(format!("失败：{text}"));
                } else if text != "操作已应用" {
                    self.notices.push(text);
                }
            }
            ServerMessage::Bye(text) => {
                self.notices.push(text);
                self.closed = true;
            }
            _ => {}
        }
        if self.notices.len() > 100 {
            self.notices.drain(..self.notices.len() - 100);
        }
    }
    pub fn drain(&mut self) {
        while let Ok(message) = self.rx.try_recv() {
            self.apply(message);
        }
    }
    fn navigation_reply(&mut self, seq: u64) {
        if self
            .pending_navigation
            .as_ref()
            .is_some_and(|(pending, _)| *pending == seq)
        {
            self.pending_navigation = None;
        }
    }
    pub fn sync(&mut self) -> Result<()> {
        self.request(Action::Ping)?;
        let deadline = Instant::now() + Duration::from_secs(2);
        loop {
            let message = self
                .rx
                .recv_timeout(deadline.saturating_duration_since(Instant::now()))
                .context("等待最新公共状态失败")?;
            let done = matches!(message, ServerMessage::Snapshot(_));
            self.apply(message);
            if done {
                return Ok(());
            }
        }
    }
    pub fn update(&mut self, action: Action) -> Result<String> {
        let result = self.request(action)?;
        self.sync()?;
        Ok(result)
    }
}

impl Drop for Client {
    fn drop(&mut self) {
        if let Err(error) = self.stream.shutdown(std::net::Shutdown::Both)
            && error.kind() != std::io::ErrorKind::NotConnected
        {
            eprintln!("关闭客户端连接失败：{error}");
        }
    }
}

fn write_message(stream: &mut UnixStream, message: &ClientMessage) -> Result<()> {
    let mut bytes = serde_json::to_vec(message)?;
    bytes.push(b'\n');
    stream.write_all(&bytes)?;
    Ok(())
}
