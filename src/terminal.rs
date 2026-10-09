use crate::{
    config::UserConfig,
    protocol::{ShellEvent, TerminalView},
    session::Id,
};
use anyhow::{Context, Result, bail};
use portable_pty::{Child, CommandBuilder, MasterPty, PtySize, native_pty_system};
use std::{
    fs::{self, File, OpenOptions},
    io::{Read, Write},
    path::{Path, PathBuf},
    thread,
};
use tokio::sync::mpsc::UnboundedSender;

pub struct Terminal {
    pub pane: Id,
    pub generation: Id,
    pub master: Box<dyn MasterPty + Send>,
    writer: Box<dyn Write + Send>,
    child: Box<dyn Child + Send + Sync>,
    parser: vt100::Parser,
    log: File,
    pub log_path: PathBuf,
    pub root_pid: u32,
    pub bash_pid: Option<u32>,
    pub ready: bool,
    pub prompt_status: Option<i32>,
    pub closed: bool,
    pub cwd: PathBuf,
    pub command: Option<CommandRun>,
    pub takeover_hashes: Option<std::collections::BTreeMap<PathBuf, String>>,
    pub takeover_offset: u64,
}

#[derive(Debug)]
pub struct TerminalEvent {
    pub pane: Id,
    pub generation: Id,
    pub result: std::io::Result<Vec<u8>>,
}

pub struct CommandRun {
    pub agent: Id,
    pub call: crate::agent::ToolCall,
    pub path: PathBuf,
    pub started: bool,
    pub offset: u64,
}

pub struct TerminalOptions<'a> {
    pub pane: Id,
    pub generation: Id,
    pub cwd: &'a Path,
    pub runtime: &'a Path,
    pub events: &'a Path,
    pub session: &'a str,
    pub project: &'a Path,
    pub user: &'a UserConfig,
    pub ai: bool,
    pub secret_env: Option<&'a str>,
}

impl Terminal {
    pub fn spawn(options: TerminalOptions<'_>, tx: UnboundedSender<TerminalEvent>) -> Result<Self> {
        let TerminalOptions {
            pane,
            generation,
            cwd,
            runtime,
            events,
            session,
            project,
            user,
            ai,
            secret_env,
        } = options;
        fs::create_dir_all(runtime)?;
        let rc = runtime.join("bashrc");
        fs::write(&rc, include_str!("bashrc"))?;
        let pair = native_pty_system().openpty(size(100, 28))?;
        let mut cmd = CommandBuilder::new(if ai { "/bin/bash" } else { &user.shell });
        if ai {
            cmd.args(["--noprofile", "--rcfile"]);
            cmd.arg(&rc);
            cmd.arg("-i");
        } else {
            cmd.args(&user.shell_args);
        }
        cmd.cwd(cwd);
        if let Some(name) = secret_env {
            cmd.env_remove(name);
        }
        cmd.env("TERM", "xterm-256color");
        cmd.env("CREW_EXE", std::env::current_exe()?);
        cmd.env("CREW_RUNTIME", runtime);
        cmd.env("CREW_PROJECT", project);
        cmd.env("CREW_EVENT_SOCKET", events);
        cmd.env("CREW_SESSION", session);
        cmd.env("CREW_PANE_ID", pane.to_string());
        cmd.env("CREW_GENERATION", generation.to_string());
        let child = pair.slave.spawn_command(cmd)?;
        let root_pid = child.process_id().context("PTY 缺少子进程 PID")?;
        drop(pair.slave);
        let mut reader = pair.master.try_clone_reader()?;
        let writer = pair.master.take_writer()?;
        thread::spawn(move || {
            let mut buf = [0; 8192];
            loop {
                let result = match reader.read(&mut buf) {
                    Ok(n) => Ok(buf[..n].to_vec()),
                    Err(e) if e.kind() == std::io::ErrorKind::Interrupted => continue,
                    Err(e) => Err(e),
                };
                let finished = match &result {
                    Ok(v) => v.is_empty(),
                    Err(_) => true,
                };
                if tx
                    .send(TerminalEvent {
                        pane,
                        generation,
                        result,
                    })
                    .is_err()
                    || finished
                {
                    break;
                }
            }
        });
        let log_path = runtime.join(format!("terminal-{pane}.log"));
        let log = OpenOptions::new()
            .create(true)
            .append(true)
            .open(&log_path)?;
        Ok(Self {
            pane,
            generation,
            master: pair.master,
            writer,
            child,
            parser: vt100::Parser::new(28, 100, 10_000),
            log,
            log_path,
            root_pid,
            bash_pid: None,
            ready: false,
            prompt_status: None,
            closed: false,
            cwd: cwd.into(),
            command: None,
            takeover_hashes: None,
            takeover_offset: 0,
        })
    }
    pub fn output(&mut self, bytes: &[u8]) -> Result<()> {
        self.log.write_all(bytes)?;
        self.parser.process(bytes);
        Ok(())
    }
    pub fn input(&mut self, bytes: &[u8], cols: u16, rows: u16) -> Result<()> {
        if self.closed {
            bail!("终端已经退出");
        }
        let cols = cols.clamp(1, 500);
        let rows = rows.clamp(1, 200);
        self.master.resize(size(cols, rows))?;
        self.parser.screen_mut().set_size(rows, cols);
        if bytes.contains(&b'\r') || bytes.contains(&b'\n') {
            self.ready = false;
        }
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }
    pub fn write(&mut self, bytes: &[u8]) -> Result<()> {
        if self.closed {
            bail!("终端已经退出");
        }
        self.writer.write_all(bytes)?;
        self.writer.flush()?;
        Ok(())
    }
    pub fn shell_event(&mut self, event: &ShellEvent) {
        if event.generation != self.generation {
            return;
        }
        if event.kind == "register" && self.bash_pid.is_none() {
            self.bash_pid = Some(event.pid);
        }
        if self.bash_pid == Some(event.pid) {
            self.cwd = PathBuf::from(&event.cwd);
        }
        if event.kind == "prompt" && self.bash_pid == Some(event.pid) {
            self.ready = true;
            self.prompt_status = Some(event.status);
        } else if event.kind == "trace" && !event.command.starts_with("__crew_") {
            if self.bash_pid == Some(event.pid) {
                self.ready = false;
            }
            if let Some(run) = &mut self.command
                && run.path == Path::new(&event.source)
            {
                run.started = true;
            }
        }
    }
    pub fn bash_ready(&self) -> bool {
        self.ready
            && !self.closed
            && self
                .bash_pid
                .is_some_and(|pid| Some(pid as i32) == self.master.process_group_leader())
    }
    pub fn busy(&self) -> bool {
        self.command.is_some()
            || (!self.closed
                && (self.bash_pid.is_some() && !self.bash_ready()
                    || self
                        .master
                        .process_group_leader()
                        .is_some_and(|p| p != self.root_pid as i32)))
    }
    pub fn interrupt(&mut self) -> Result<()> {
        self.write(&[3])
    }
    pub fn poll(&mut self) -> Result<Option<String>> {
        if self.closed {
            return Ok(None);
        }
        if let Some(exit) = self.child.try_wait()? {
            self.closed = true;
            self.ready = false;
            return Ok(Some(format!("终端退出：{exit}")));
        }
        Ok(None)
    }
    pub fn view(&self) -> TerminalView {
        let screen = self.parser.screen();
        let (rows, cols) = screen.size();
        TerminalView {
            screen: screen.contents_formatted(),
            cols,
            rows,
            cursor: screen.cursor_position(),
            cursor_hidden: screen.hide_cursor(),
            application_cursor: screen.application_cursor(),
            bracketed_paste: screen.bracketed_paste(),
            foreground: self.master.process_group_leader(),
            status: if self.closed {
                "终端已退出"
            } else if self.bash_ready() {
                "原 AI Bash 空闲"
            } else if self.bash_pid.is_some() {
                "命令运行或个人 shell；尚未回到原 AI Bash"
            } else {
                "人工 shell；前台状态由进程组确定"
            }
            .into(),
        }
    }
    pub fn offset(&self) -> Result<u64> {
        Ok(self.log.metadata()?.len())
    }
    pub fn output_since(&self, offset: u64) -> Result<String> {
        use std::io::{Seek, SeekFrom};
        let mut file = File::open(&self.log_path)?;
        file.seek(SeekFrom::Start(offset))?;
        let truncated = file.metadata()?.len().saturating_sub(offset) > 2_000_000;
        let mut bytes = Vec::new();
        file.take(2_000_000).read_to_end(&mut bytes)?;
        let (rows, cols) = self.parser.screen().size();
        let mut parser = vt100::Parser::new(rows, cols, 10_000);
        parser.process(&bytes);
        let mut content = terminal_history(&mut parser);
        if truncated {
            content.push_str(&format!(
                "\n输出超过 2 MB，工具结果已截取；完整输出保存于 {}",
                self.log_path.display()
            ));
        }
        Ok(content)
    }
    pub fn history(&self) -> Result<String> {
        let mut parser = vt100::Parser::new(28, self.parser.screen().size().1, 10_000);
        let mut file = File::open(&self.log_path)?;
        let mut bytes = [0; 8192];
        loop {
            let n = file.read(&mut bytes)?;
            if n == 0 {
                break;
            }
            parser.process(&bytes[..n]);
        }
        Ok(terminal_history(&mut parser))
    }
    pub fn processes(&self) -> Result<Vec<(i32, String)>> {
        let mut result = Vec::new();
        for process in procfs::process::all_processes()? {
            let process = match process {
                Ok(p) => p,
                Err(procfs::ProcError::NotFound(_)) => continue,
                Err(procfs::ProcError::InternalError(e)) => panic!("procfs 内部错误：{e:?}"),
                Err(e) => return Err(e.into()),
            };
            let stat = match process.stat() {
                Ok(s) => s,
                Err(procfs::ProcError::NotFound(_)) => continue,
                Err(procfs::ProcError::InternalError(e)) => panic!("procfs 内部错误：{e:?}"),
                Err(e) => return Err(e.into()),
            };
            if stat.session == self.root_pid as i32
                && stat.pid != self.root_pid as i32
                && stat.state != 'Z'
                && stat.comm != "crew"
            {
                let command = match process.cmdline() {
                    Ok(parts) => parts.join(" "),
                    Err(procfs::ProcError::NotFound(_)) => continue,
                    Err(procfs::ProcError::InternalError(e)) => panic!("procfs 内部错误：{e:?}"),
                    Err(e) => format!("{}（命令行读取失败：{e}）", stat.comm),
                };
                result.push((stat.pid, command));
            }
        }
        Ok(result)
    }
    pub fn stop(&mut self) -> Result<()> {
        for (pid, _) in self.processes()? {
            signal(pid, libc::SIGTERM)?;
        }
        if !self.closed {
            signal(self.root_pid as i32, libc::SIGTERM)?;
            self.child.kill()?;
            self.child.wait()?;
            self.closed = true;
        }
        for (pid, _) in self.processes()? {
            signal(pid, libc::SIGKILL)?;
        }
        self.log.sync_all()?;
        Ok(())
    }
}

fn terminal_history(parser: &mut vt100::Parser) -> String {
    let (rows, cols) = parser.screen().size();
    parser.screen_mut().set_scrollback(10_000);
    let mut position = parser.screen().scrollback();
    let mut result = String::new();
    while position > 0 {
        parser.screen_mut().set_scrollback(position);
        let count = position.min(rows as usize);
        for row in parser.screen().rows(0, cols).take(count) {
            result.push_str(&row);
            result.push('\n');
        }
        position -= count;
    }
    parser.screen_mut().set_scrollback(0);
    result.push_str(&parser.screen().contents());
    result
}

pub fn signal(pid: i32, sig: i32) -> Result<()> {
    // PID 来自当前 session 的实际进程表。
    if unsafe { libc::kill(pid, sig) } != 0 {
        let error = std::io::Error::last_os_error();
        if error.raw_os_error() != Some(libc::ESRCH) {
            return Err(error.into());
        }
    }
    Ok(())
}

fn size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows,
        cols,
        pixel_width: 0,
        pixel_height: 0,
    }
}
