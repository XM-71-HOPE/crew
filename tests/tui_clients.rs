mod support;
use anyhow::{Context, Result, bail};
use crew::protocol::{Action, Choice, Mode, Target};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    io::{Read, Write},
    sync::{Arc, Mutex},
    thread,
    time::{Duration, Instant},
};
use support::{Workspace, wait};

struct TuiClient {
    child: Box<dyn Child + Send + Sync>,
    master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    output: Arc<Mutex<Vec<u8>>>,
}

#[test]
fn real_cli_automatically_starts_service_and_preserves_exit_confirmation() -> Result<()> {
    let mut w = Workspace::new("cli-start", false)?;
    let mut original = w.client("initial")?;
    w.shutdown(&mut original)?;
    drop(original);
    let mut tui = TuiClient::new(&w, "xm", 120, 32)?;
    let deadline = Instant::now() + Duration::from_secs(8);
    let mut inspector = loop {
        match w.client("inspector") {
            Ok(client) => break client,
            Err(error) if Instant::now() >= deadline => {
                return Err(error.context("命令行自动启动服务失败"));
            }
            Err(_) => thread::sleep(Duration::from_millis(20)),
        }
    };
    wait(&mut inspector, 5, |s| {
        s.connections.values().any(|c| c.user == "xm")
    })?;
    tui.wait_text("session 导航", 120, 32)?;
    tui.exit()?;
    let stop = std::process::Command::new(env!("CARGO_BIN_EXE_crew"))
        .args([
            "stop",
            "--session",
            "test",
            "--user",
            "requester",
            "--project",
        ])
        .arg(&w.path)
        .output()?;
    assert!(
        stop.status.success(),
        "{}",
        String::from_utf8_lossy(&stop.stderr)
    );
    wait(&mut inspector, 5, |s| {
        s.session.requests.values().any(|r|
        r.requester.is_none() && matches!(&r.kind, crew::session::RequestKind::Action(a) if matches!(**a, Action::Shutdown)))
    })?;
    let confirm = std::process::Command::new(env!("CARGO_BIN_EXE_crew"))
        .args([
            "stop",
            "--confirm",
            "--session",
            "test",
            "--user",
            "confirmer",
            "--project",
        ])
        .arg(&w.path)
        .output()?;
    assert!(
        confirm.status.success(),
        "{}",
        String::from_utf8_lossy(&confirm.stderr)
    );
    let deadline = Instant::now() + Duration::from_secs(8);
    while w.path.join(".crew/test.sock").exists() {
        assert!(Instant::now() < deadline, "自动启动的服务未完全退出");
        thread::sleep(Duration::from_millis(20));
    }
    Ok(())
}

impl TuiClient {
    fn new(w: &Workspace, user: &str, cols: u16, rows: u16) -> Result<Self> {
        let pty = native_pty_system().openpty(PtySize {
            cols,
            rows,
            pixel_width: 0,
            pixel_height: 0,
        })?;
        let mut cmd = CommandBuilder::new(env!("CARGO_BIN_EXE_crew"));
        cmd.args(["connect", "--session", "test", "--user", user, "--project"]);
        cmd.arg(&w.path);
        cmd.env("TERM", "xterm-256color");
        let child = pty.slave.spawn_command(cmd)?;
        drop(pty.slave);
        let mut reader = pty.master.try_clone_reader()?;
        let writer = pty.master.take_writer()?;
        let output = Arc::new(Mutex::new(Vec::new()));
        let capture = output.clone();
        let mut log = std::fs::File::create(w.path.join(format!("tui-{user}.log")))?;
        thread::spawn(move || {
            let mut buf = [0; 8192];
            loop {
                match reader.read(&mut buf) {
                    Ok(0) => break,
                    Ok(n) => {
                        log.write_all(&buf[..n]).expect("测试客户端记录必须可写");
                        capture
                            .lock()
                            .expect("输出互斥状态正常")
                            .extend_from_slice(&buf[..n]);
                    }
                    Err(_) => break,
                }
            }
        });
        Ok(Self {
            child,
            master: pty.master,
            writer,
            output,
        })
    }
    fn keys(&mut self, bytes: &[u8]) -> Result<()> {
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }
    fn wait_text(&self, text: &str, cols: u16, rows: u16) -> Result<()> {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let mut parser = vt100::Parser::new(rows, cols, 0);
            parser.process(&self.output.lock().expect("客户端输出可读取"));
            if parser.screen().contents().contains(text) {
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!(
                    "客户端没有展示 {text}；当前文字：{}",
                    parser.screen().contents()
                );
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
    fn exit(&mut self) -> Result<()> {
        self.keys(b"\x02d")?;
        let until = Instant::now() + Duration::from_secs(5);
        loop {
            if let Some(exit) = self.child.try_wait()? {
                assert!(exit.success());
                return Ok(());
            }
            if Instant::now() > until {
                bail!("TUI 客户端没有退出");
            }
            thread::sleep(Duration::from_millis(20));
        }
    }
}
impl Drop for TuiClient {
    fn drop(&mut self) {
        if self.child.try_wait().is_ok_and(|s| s.is_none()) {
            if let Err(e) = self.child.kill() {
                eprintln!("测试客户端关闭失败：{e}");
            }
            if let Err(e) = self.child.wait() {
                eprintln!("测试客户端回收失败：{e}");
            }
        }
    }
}

#[test]
fn two_real_tui_clients_navigate_review_resize_and_transfer() -> Result<()> {
    let mut w = Workspace::new("tui", false)?;
    let mut inspector = w.client("inspector")?;
    let mut a = TuiClient::new(&w, "xm", 140, 42)?;
    let mut b = TuiClient::new(&w, "ln", 72, 26)?;
    wait(&mut inspector, 5, |s| {
        s.connections.values().any(|c| c.user == "xm")
            && s.connections.values().any(|c| c.user == "ln")
    })?;
    let aid = inspector
        .snapshot
        .connections
        .values()
        .find(|c| c.user == "xm")
        .context("xm 连接不存在")?
        .id;
    let bid = inspector
        .snapshot
        .connections
        .values()
        .find(|c| c.user == "ln")
        .context("ln 连接不存在")?
        .id;
    let tab = inspector.snapshot.session.tabs[0].id;
    let pane = inspector.snapshot.session.tabs[0].panes[0];
    inspector.request(Action::CreateTab {
        title: "第二页".into(),
    })?;
    inspector.sync()?;
    let second = inspector.snapshot.session.tabs[1].id;
    b.wait_text("第二页", 72, 26)?;
    b.keys(b"l")?;
    wait(&mut inspector, 5, |s| s.connections[&bid].tab == second)
        .context("第二个客户端切换 tab")?;
    assert_eq!(inspector.snapshot.connections[&aid].tab, tab);
    a.keys(b"i")?;
    wait(&mut inspector, 5, |s| s.connections[&aid].mode == Mode::Tab)?;
    a.keys(b"nTUI pane\r")?;
    wait(&mut inspector, 5, |s| s.session.tabs[0].panes.len() == 2)?;
    assert!(
        inspector
            .snapshot
            .session
            .panes
            .values()
            .any(|p| p.title == "TUI pane")
    );
    a.keys(b"e")?;
    wait(&mut inspector, 5, |s| {
        s.connections[&aid].mode == Mode::Layout
    })?;
    a.keys(b"+v")?;
    wait(&mut inspector, 5, |s| {
        s.session.tabs[0].vertical && s.session.tabs[0].weights[&pane] == 110
    })?;
    a.keys(b"q")?;
    wait(&mut inspector, 5, |s| s.connections[&aid].mode == Mode::Tab)?;
    a.keys(b"i")?;
    wait(&mut inspector, 5, |s| {
        s.connections[&aid].mode == Mode::Terminal
    })?;
    a.keys(b"printf 'TUI_CLIENT_ALIVE\\n'\r")?;
    wait(&mut inspector, 5, |s| {
        s.terminals[&pane]
            .screen
            .windows(16)
            .any(|b| b == b"TUI_CLIENT_ALIVE")
    })?;
    let terminal_cols = inspector.snapshot.terminals[&pane].cols;
    b.master.resize(PtySize {
        cols: 40,
        rows: 18,
        pixel_width: 0,
        pixel_height: 0,
    })?;
    inspector.sync()?;
    assert_eq!(inspector.snapshot.terminals[&pane].cols, terminal_cols);
    a.keys(b"\x02r")?;
    wait(&mut inspector, 5, |s| {
        matches!(s.connections[&aid].mode, Mode::Review { .. })
    })?;
    b.keys(b"h")?;
    wait(&mut inspector, 5, |s| s.connections[&bid].tab == tab)?;
    b.keys(b"i")?;
    wait(&mut inspector, 5, |s| s.connections[&bid].mode == Mode::Tab)?;
    b.keys(b"i")?;
    wait(&mut inspector, 5, |s| !s.session.requests.is_empty())?;
    let request = *inspector
        .snapshot
        .session
        .requests
        .keys()
        .next()
        .expect("接替需要请求");
    inspector.request(Action::Resolve {
        request,
        choice: Choice::Approve,
    })?;
    wait(&mut inspector, 5, |s| {
        s.session.panes[&pane].owner == Some(bid)
    })?;
    assert_eq!(
        inspector.snapshot.connections[&aid].mode,
        Mode::Review {
            origin: Box::new(Mode::Tab)
        }
    );
    a.keys(b"q")?;
    wait(&mut inspector, 5, |s| s.connections[&aid].mode == Mode::Tab)?;
    assert!(
        inspector
            .request(Action::History {
                target: Target::Pane(pane)
            })?
            .contains("TUI_CLIENT_ALIVE")
    );
    assert!(!a.output.lock().expect("输出可读取").is_empty());
    assert!(!b.output.lock().expect("输出可读取").is_empty());
    a.keys(b":service exec sleep 120\r")?;
    a.wait_text("保留服务", 140, 42)?;
    wait(&mut inspector, 5, |s| s.session.services.len() == 1).context("TUI 启动保留服务")?;
    let first_service = inspector.snapshot.session.services[0].pid;
    a.keys(b":services\r")?;
    a.wait_text("运行中", 140, 42)?;
    a.keys(b"q:handoff exec sleep 120\r")?;
    wait(&mut inspector, 5, |s| !s.session.requests.is_empty()).context("TUI 请求服务交接")?;
    let handoff = *inspector
        .snapshot
        .session
        .requests
        .keys()
        .next()
        .expect("交接需要确认");
    inspector.request(Action::Resolve {
        request: handoff,
        choice: Choice::Approve,
    })?;
    wait(&mut inspector, 5, |s| s.session.services.len() == 2).context("TUI 确认服务交接")?;
    let second_service = inspector.snapshot.session.services[1].pid;
    a.exit()?;
    b.exit()?;
    wait(&mut inspector, 5, |s| {
        !s.connections.contains_key(&aid) && !s.connections.contains_key(&bid)
    })?;
    w.shutdown(&mut inspector)?;
    for pid in [first_service, second_service] {
        assert!(procfs::process::Process::new(pid as i32)?.stat()?.state != 'Z');
        crew::terminal::signal(pid as i32, libc::SIGTERM)?;
    }
    Ok(())
}
