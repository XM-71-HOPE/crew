use crate::{
    agent::{AgentKind, AgentState},
    client::Client,
    protocol::{Action, Choice, Mode, Snapshot, Target, TerminalView},
    session::{Connection, Display, Id},
};
use anyhow::{Context, Result};
use crossterm::{
    event::{
        self, DisableBracketedPaste, EnableBracketedPaste, Event, KeyCode, KeyEvent, KeyEventKind,
        KeyModifiers,
    },
    execute,
    terminal::{self, EnterAlternateScreen, LeaveAlternateScreen},
};
use ratatui::{
    Frame,
    backend::CrosstermBackend,
    layout::{Constraint, Direction, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span, Text},
    widgets::{Block, Borders, Clear, Paragraph, Wrap},
};
use std::{
    collections::BTreeMap,
    io::{self, Write},
    time::Duration,
};

#[derive(Default)]
pub struct Ui {
    prefix: bool,
    pub review: Option<Review>,
    overlay: Option<Overlay>,
    terminal_areas: BTreeMap<Id, Rect>,
    viewports: BTreeMap<Id, (u16, u16)>,
    pub done: bool,
}

pub struct Review {
    pub text: String,
    pub row: usize,
    pub column: u16,
    pub search: String,
    pub wrap: bool,
    width: u16,
}
impl Review {
    fn rows(&self) -> usize {
        if self.wrap {
            Paragraph::new(self.text.as_str())
                .wrap(Wrap { trim: false })
                .line_count(self.width.max(1))
        } else {
            self.text.lines().count()
        }
    }
    fn search_next(&self, after: Option<usize>) -> Option<usize> {
        let mut row = 0;
        for line in self.text.lines() {
            if after.is_none_or(|after| row > after) && line.contains(&self.search) {
                return Some(row);
            }
            row += if self.wrap {
                Paragraph::new(line)
                    .wrap(Wrap { trim: false })
                    .line_count(self.width.max(1))
                    .max(1)
            } else {
                1
            };
        }
        None
    }
}
enum Overlay {
    Text {
        title: String,
        text: String,
        row: u16,
    },
    Requests {
        selected: usize,
    },
    Prompt {
        label: String,
        text: String,
        kind: PromptKind,
    },
}
enum PromptKind {
    Command,
    Pane,
    Tab,
    Rename,
    Search,
}
struct TerminalGuard;

impl Drop for TerminalGuard {
    fn drop(&mut self) {
        if let Err(e) = execute!(io::stdout(), DisableBracketedPaste, LeaveAlternateScreen) {
            eprintln!("恢复终端显示失败：{e}");
        }
        if let Err(e) = terminal::disable_raw_mode() {
            eprintln!("恢复终端输入失败：{e}");
        }
    }
}

impl Ui {
    pub fn run(client: &mut Client) -> Result<()> {
        terminal::enable_raw_mode()?;
        let _guard = TerminalGuard;
        execute!(io::stdout(), EnterAlternateScreen, EnableBracketedPaste)?;
        let mut terminal = ratatui::Terminal::new(CrosstermBackend::new(io::stdout()))?;
        let mut ui = Self::default();
        while !ui.done && !client.closed {
            client.drain();
            terminal.draw(|frame| {
                ui.draw(
                    frame,
                    &client.snapshot,
                    client.connection,
                    client
                        .notices
                        .last()
                        .map(String::as_str)
                        .unwrap_or("F1 帮助 · Ctrl+B 前缀 · p 请求"),
                )
            })?;
            if event::poll(Duration::from_millis(30))? {
                match event::read()? {
                    Event::Key(key) if key.kind != KeyEventKind::Release => {
                        if let Err(e) = ui.key(client, key) {
                            client.notices.push(format!("失败：{e}"));
                        }
                    }
                    Event::Paste(text) => {
                        if let Err(e) = ui.paste(client, text) {
                            client.notices.push(format!("失败：{e}"));
                        }
                    }
                    _ => {}
                }
            }
        }
        Ok(())
    }
    pub fn key(&mut self, client: &mut Client, key: KeyEvent) -> Result<()> {
        let c = client
            .snapshot
            .connections
            .get(&client.connection)
            .context("当前连接不存在")?
            .clone();
        if key.code == KeyCode::F(1) {
            self.overlay = Some(Overlay::Text {
                title: "帮助".into(),
                text: help().into(),
                row: 0,
            });
            return Ok(());
        }
        if self.overlay.is_some() {
            return self.overlay_key(client, key, &c);
        }
        if self.review.is_some() || matches!(c.mode, Mode::Review { .. }) {
            return self.review_key(client, key);
        }
        let is_prefix =
            key.code == KeyCode::Char('b') && key.modifiers.contains(KeyModifiers::CONTROL);
        if self.prefix {
            self.prefix = false;
            if is_prefix {
                return self.send_terminal(client, &c, vec![2]);
            }
            return match key.code {
                KeyCode::Char('q') => {
                    client.update(Action::Release)?;
                    Ok(())
                }
                KeyCode::Char('d') => {
                    self.done = true;
                    Ok(())
                }
                KeyCode::Char('Q') => {
                    client.action(Action::Shutdown)?;
                    self.overlay = Some(Overlay::Requests { selected: 0 });
                    Ok(())
                }
                KeyCode::Char('i') => {
                    if let Some(p) = c
                        .chat
                        .and_then(|a| client.snapshot.session.agents[&a].pane)
                        .or(c.pane)
                    {
                        client.update(Action::Acquire {
                            target: Target::Pane(p),
                            terminal: true,
                        })?;
                    }
                    Ok(())
                }
                KeyCode::Char('a') => self.acquire_chat(client, &c),
                KeyCode::Char('A') => self.toggle(client, &c),
                KeyCode::Char('r') => self.enter_review(client, &c),
                KeyCode::Char('p') => {
                    self.overlay = Some(Overlay::Requests { selected: 0 });
                    Ok(())
                }
                KeyCode::Char(':') => {
                    self.command_prompt();
                    Ok(())
                }
                KeyCode::Left | KeyCode::Right | KeyCode::Up | KeyCode::Down => {
                    if let Some(p) = c.pane {
                        let offset = self.viewports.entry(p).or_default();
                        match key.code {
                            KeyCode::Left => offset.0 = offset.0.saturating_sub(5),
                            KeyCode::Right => offset.0 = offset.0.saturating_add(5),
                            KeyCode::Up => offset.1 = offset.1.saturating_sub(3),
                            KeyCode::Down => offset.1 = offset.1.saturating_add(3),
                            _ => {}
                        }
                    }
                    Ok(())
                }
                _ => Ok(()),
            };
        }
        if is_prefix {
            self.prefix = true;
            return Ok(());
        }
        match c.mode {
            Mode::Terminal => self.send_terminal(
                client,
                &c,
                terminal_key(key, c.pane.and_then(|p| client.snapshot.terminals.get(&p))),
            ),
            Mode::Chat => self.chat_key(client, &c, key),
            Mode::Session | Mode::Tab | Mode::Layout => self.navigation_key(client, &c, key),
            Mode::Review { .. } => unreachable!("回看已处理"),
        }
    }
    fn send_terminal(&self, client: &mut Client, c: &Connection, bytes: Vec<u8>) -> Result<()> {
        if bytes.is_empty() {
            return Ok(());
        }
        let pane = c.pane.context("没有选中的终端")?;
        let area = self
            .terminal_areas
            .get(&pane)
            .copied()
            .unwrap_or(Rect::new(0, 0, 100, 28));
        client.action(Action::Input {
            pane,
            bytes,
            cols: area.width.max(1),
            rows: area.height.max(1),
        })?;
        Ok(())
    }
    fn paste(&mut self, client: &mut Client, text: String) -> Result<()> {
        if let Some(Overlay::Prompt { text: input, .. }) = &mut self.overlay {
            input.push_str(&text);
            return Ok(());
        }
        let c = client.snapshot.connections[&client.connection].clone();
        if c.mode == Mode::Chat {
            let a = c.chat.context("没有对话")?;
            let mut draft = client.snapshot.session.agents[&a]
                .conversation()
                .draft
                .clone();
            draft.push_str(&text);
            self.draft(client, a, draft)?;
        } else if c.mode == Mode::Terminal {
            let bracketed = c
                .pane
                .is_some_and(|p| client.snapshot.terminals[&p].bracketed_paste);
            self.send_terminal(
                client,
                &c,
                if bracketed {
                    format!("\x1b[200~{text}\x1b[201~").into_bytes()
                } else {
                    text.into_bytes()
                },
            )?;
        }
        Ok(())
    }
    fn draft(&self, client: &mut Client, agent: Id, text: String) -> Result<()> {
        let revision = client.snapshot.session.agents[&agent]
            .conversation()
            .revision;
        client.action(Action::Draft {
            agent,
            text: text.clone(),
            revision,
        })?;
        let c = client
            .snapshot
            .session
            .agents
            .get_mut(&agent)
            .expect("agent 存在")
            .conversation_mut();
        c.draft = text;
        c.revision += 1;
        Ok(())
    }
    fn chat_key(&mut self, client: &mut Client, c: &Connection, key: KeyEvent) -> Result<()> {
        let agent = c.chat.context("没有对话目标")?;
        let mut draft = client.snapshot.session.agents[&agent]
            .conversation()
            .draft
            .clone();
        match key.code {
            KeyCode::Enter if key.modifiers.contains(KeyModifiers::ALT) => draft.push('\n'),
            KeyCode::Enter => {
                client.update(Action::Send {
                    agent,
                    interrupt: false,
                    tree: false,
                })?;
                return Ok(());
            }
            KeyCode::Char('s' | 't') if key.modifiers.contains(KeyModifiers::ALT) => {
                client.update(Action::Send {
                    agent,
                    interrupt: true,
                    tree: key.code == KeyCode::Char('t'),
                })?;
                return Ok(());
            }
            KeyCode::Char('u') if key.modifiers.contains(KeyModifiers::CONTROL) => draft.clear(),
            KeyCode::Backspace => {
                draft.pop();
            }
            KeyCode::Char(ch)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                draft.push(ch)
            }
            _ => return Ok(()),
        }
        self.draft(client, agent, draft)
    }
    fn agent_target(snapshot: &Snapshot, c: &Connection) -> Option<Id> {
        if c.mode == Mode::Session {
            snapshot.session.tab(c.tab).ok()?.agent
        } else {
            c.chat.or_else(|| {
                c.pane.and_then(|p| {
                    snapshot
                        .session
                        .panes
                        .get(&p)
                        .filter(|p| !p.attached)
                        .and_then(|p| p.agent)
                })
            })
        }
    }
    fn acquire_chat(&self, client: &mut Client, c: &Connection) -> Result<()> {
        if let Some(a) = Self::agent_target(&client.snapshot, c) {
            client.update(Action::Acquire {
                target: Target::Agent(a),
                terminal: false,
            })?;
        } else {
            client.notices.push(
                "这个区域没有独立 agent；附属终端的对话从 session 层访问。:agent 创建 agent".into(),
            );
        }
        Ok(())
    }
    fn toggle(&self, client: &mut Client, c: &Connection) -> Result<()> {
        if let Some(a) = Self::agent_target(&client.snapshot, c) {
            client.action(Action::Toggle { agent: a })?;
        }
        Ok(())
    }
    fn navigation_key(&mut self, client: &mut Client, c: &Connection, key: KeyEvent) -> Result<()> {
        match key.code {
            KeyCode::Char('q') if c.mode == Mode::Session => self.done = true,
            KeyCode::Char('q') => {
                client.action(Action::Focus {
                    tab: c.tab,
                    pane: c.pane,
                    mode: if c.mode == Mode::Layout {
                        Mode::Tab
                    } else {
                        Mode::Session
                    },
                })?;
            }
            KeyCode::Char('i') if c.mode == Mode::Session => {
                client.action(Action::Focus {
                    tab: c.tab,
                    pane: c.pane,
                    mode: Mode::Tab,
                })?;
            }
            KeyCode::Char('i') => {
                if let Some(p) = c.pane {
                    client.update(Action::Acquire {
                        target: Target::Pane(p),
                        terminal: true,
                    })?;
                }
            }
            KeyCode::Char('a') => self.acquire_chat(client, c)?,
            KeyCode::Char('A') => self.toggle(client, c)?,
            KeyCode::Char('r') => self.enter_review(client, c)?,
            KeyCode::Char('e') if c.mode == Mode::Tab => {
                client.action(Action::Focus {
                    tab: c.tab,
                    pane: c.pane,
                    mode: Mode::Layout,
                })?;
            }
            KeyCode::Char('p') => self.overlay = Some(Overlay::Requests { selected: 0 }),
            KeyCode::Char(':') => self.command_prompt(),
            KeyCode::Char('n') => {
                self.overlay = Some(Overlay::Prompt {
                    label: if c.mode == Mode::Session {
                        "新 tab 标题"
                    } else {
                        "新 pane 标题"
                    }
                    .into(),
                    text: String::new(),
                    kind: if c.mode == Mode::Session {
                        PromptKind::Tab
                    } else {
                        PromptKind::Pane
                    },
                })
            }
            KeyCode::Char('t') => {
                self.overlay = Some(Overlay::Prompt {
                    label: "修改标题".into(),
                    text: String::new(),
                    kind: PromptKind::Rename,
                })
            }
            KeyCode::Char('x') => {
                client.action(Action::Close {
                    tab: if c.mode == Mode::Session {
                        Some(c.tab)
                    } else {
                        None
                    },
                    pane: if c.mode == Mode::Session {
                        None
                    } else {
                        c.pane
                    },
                })?;
                self.overlay = Some(Overlay::Requests { selected: 0 });
            }
            KeyCode::Char('o') => {
                let text = client
                    .snapshot
                    .connections
                    .values()
                    .map(|c| {
                        format!(
                            "{} · 连接 {} · tab {} · pane {:?} · {:?}",
                            c.user, c.id, c.tab, c.pane, c.mode
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                self.overlay = Some(Overlay::Text {
                    title: "完整参与者列表".into(),
                    text,
                    row: 0,
                });
            }
            KeyCode::Char('b') => {
                let text = Self::agent_target(&client.snapshot, c)
                    .map(|a| {
                        client.snapshot.session.agents[&a]
                            .children
                            .iter()
                            .map(|id| {
                                let a = &client.snapshot.session.agents[id];
                                format!(
                                    "agent {} {:?} {:?}\n{}",
                                    a.id,
                                    a.kind,
                                    a.state,
                                    a.conversation()
                                        .records
                                        .iter()
                                        .map(|r| format!("[{}] {}", r.kind, r.text))
                                        .collect::<Vec<_>>()
                                        .join("\n")
                                )
                            })
                            .collect::<Vec<_>>()
                            .join("\n\n")
                    })
                    .unwrap_or_else(|| "没有所属 agent".into());
                self.overlay = Some(Overlay::Text {
                    title: "所属 agent 的探测助手与子任务详情".into(),
                    text,
                    row: 0,
                });
            }
            KeyCode::Char('h' | 'j' | 'k' | 'l')
            | KeyCode::Left
            | KeyCode::Right
            | KeyCode::Up
            | KeyCode::Down => {
                let delta = if matches!(
                    key.code,
                    KeyCode::Char('h' | 'k') | KeyCode::Left | KeyCode::Up
                ) {
                    -1
                } else {
                    1
                };
                if c.mode == Mode::Layout {
                    if let Some(p) = c.pane {
                        client.action(Action::Layout {
                            tab: c.tab,
                            pane: p,
                            movement: delta,
                            weight: 0,
                            vertical: None,
                        })?;
                    }
                } else if c.mode == Mode::Session {
                    let tabs = &client.snapshot.session.tabs;
                    let index = tabs.iter().position(|t| t.id == c.tab).expect("tab 存在");
                    let t =
                        &tabs[(index as i32 + delta as i32).rem_euclid(tabs.len() as i32) as usize];
                    client.action(Action::Focus {
                        tab: t.id,
                        pane: t.panes.first().copied(),
                        mode: Mode::Session,
                    })?;
                } else {
                    let t = client.snapshot.session.tab(c.tab)?;
                    if !t.panes.is_empty() {
                        let index = t.panes.iter().position(|p| Some(*p) == c.pane).unwrap_or(0);
                        let pane = t.panes[(index as i32 + delta as i32)
                            .rem_euclid(t.panes.len() as i32)
                            as usize];
                        client.action(Action::Focus {
                            tab: c.tab,
                            pane: Some(pane),
                            mode: Mode::Tab,
                        })?;
                    }
                }
            }
            KeyCode::Char('+' | '-' | 'v') if c.mode == Mode::Layout => {
                if let Some(p) = c.pane {
                    client.action(Action::Layout {
                        tab: c.tab,
                        pane: p,
                        movement: 0,
                        weight: if key.code == KeyCode::Char('+') {
                            10
                        } else if key.code == KeyCode::Char('-') {
                            -10
                        } else {
                            0
                        },
                        vertical: if key.code == KeyCode::Char('v') {
                            Some(!client.snapshot.session.tab(c.tab)?.vertical)
                        } else {
                            None
                        },
                    })?;
                }
            }
            _ => {}
        }
        Ok(())
    }
    fn enter_review(&mut self, client: &mut Client, c: &Connection) -> Result<()> {
        let target = if c.mode == Mode::Chat || c.mode == Mode::Session {
            Self::agent_target(&client.snapshot, c)
                .map(Target::Agent)
                .or(c.pane.map(Target::Pane))
        } else {
            c.pane.map(Target::Pane)
        };
        let target = target.context("没有可以回看的内容")?;
        let wrap = matches!(target, Target::Agent(_));
        let text = client.request(Action::History { target })?;
        let row = text.lines().count().saturating_sub(20);
        self.review = Some(Review {
            text,
            row,
            column: 0,
            search: String::new(),
            wrap,
            width: c.cols.saturating_sub(2),
        });
        client.update(Action::Review { enter: true })?;
        Ok(())
    }
    fn review_key(&mut self, client: &mut Client, key: KeyEvent) -> Result<()> {
        let Some(r) = self.review.as_mut() else {
            client.update(Action::Review { enter: false })?;
            return Ok(());
        };
        match key.code {
            KeyCode::Char('q') | KeyCode::Esc => {
                self.review = None;
                client.update(Action::Review { enter: false })?;
            }
            KeyCode::Char('j') | KeyCode::Down => {
                r.row = (r.row + 1).min(r.rows().saturating_sub(1))
            }
            KeyCode::Char('k') | KeyCode::Up => r.row = r.row.saturating_sub(1),
            KeyCode::PageDown => r.row = (r.row + 20).min(r.rows().saturating_sub(1)),
            KeyCode::PageUp => r.row = r.row.saturating_sub(20),
            KeyCode::Char('g') | KeyCode::Home => r.row = 0,
            KeyCode::Char('G') | KeyCode::End => r.row = r.rows().saturating_sub(20),
            KeyCode::Char('h') | KeyCode::Left => r.column = r.column.saturating_sub(5),
            KeyCode::Char('l') | KeyCode::Right => r.column = r.column.saturating_add(5),
            KeyCode::Char('/') => {
                self.overlay = Some(Overlay::Prompt {
                    label: "回看搜索".into(),
                    text: String::new(),
                    kind: PromptKind::Search,
                })
            }
            KeyCode::Char('n') => {
                if !r.search.is_empty()
                    && let Some(i) = r.search_next(Some(r.row))
                {
                    r.row = i;
                }
            }
            KeyCode::Char('y') => {
                use base64::Engine;
                write!(
                    io::stdout(),
                    "\x1b]52;c;{}\x07",
                    base64::engine::general_purpose::STANDARD.encode(r.text.as_bytes())
                )?;
                io::stdout().flush()?;
                client
                    .notices
                    .push("已请求终端通过 OSC 52 复制回看内容".into());
            }
            KeyCode::Char('w') => {
                let c = &client.snapshot.connections[&client.connection];
                let path = client
                    .snapshot
                    .session
                    .project
                    .join(format!(".crew/users/{}-review.txt", c.user));
                std::fs::write(&path, &r.text)?;
                client
                    .notices
                    .push(format!("回看已保存到 {}", path.display()));
            }
            _ => {}
        }
        Ok(())
    }
    fn command_prompt(&mut self) {
        self.overlay = Some(Overlay::Prompt {
            label: "命令：tab / pane / rename / agent / close / quit / view ID".into(),
            text: String::new(),
            kind: PromptKind::Command,
        });
    }
    fn overlay_key(&mut self, client: &mut Client, key: KeyEvent, c: &Connection) -> Result<()> {
        let overlay = self.overlay.take().expect("交互区域存在");
        match overlay {
            Overlay::Text {
                title,
                text,
                mut row,
            } => {
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Char('j') | KeyCode::Down => row = row.saturating_add(1),
                    KeyCode::Char('k') | KeyCode::Up => row = row.saturating_sub(1),
                    KeyCode::PageDown => row = row.saturating_add(20),
                    KeyCode::PageUp => row = row.saturating_sub(20),
                    _ => {}
                }
                self.overlay = Some(Overlay::Text { title, text, row });
            }
            Overlay::Requests { mut selected } => {
                let requests: Vec<_> = client.snapshot.session.requests.keys().copied().collect();
                match key.code {
                    KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                    KeyCode::Down | KeyCode::Char('j') => {
                        selected = (selected + 1).min(requests.len().saturating_sub(1))
                    }
                    KeyCode::Up => selected = selected.saturating_sub(1),
                    KeyCode::Char('y' | 'n' | 'k') => {
                        if let Some(id) = requests.get(selected) {
                            client.update(Action::Resolve {
                                request: *id,
                                choice: match key.code {
                                    KeyCode::Char('y') => Choice::Approve,
                                    KeyCode::Char('k') => Choice::KeepRunning,
                                    _ => Choice::Reject,
                                },
                            })?;
                            return Ok(());
                        }
                    }
                    _ => {}
                }
                self.overlay = Some(Overlay::Requests { selected });
            }
            Overlay::Prompt {
                label,
                mut text,
                kind,
            } => {
                match key.code {
                    KeyCode::Esc => return Ok(()),
                    KeyCode::Backspace => {
                        text.pop();
                    }
                    KeyCode::Enter => {
                        match kind {
                            PromptKind::Pane => {
                                client.action(Action::CreatePane {
                                    tab: c.tab,
                                    title: text,
                                })?;
                            }
                            PromptKind::Tab => {
                                client.action(Action::CreateTab { title: text })?;
                            }
                            PromptKind::Rename => {
                                client.action(Action::Rename {
                                    tab: if c.mode == Mode::Session {
                                        Some(c.tab)
                                    } else {
                                        None
                                    },
                                    pane: if c.mode == Mode::Session {
                                        None
                                    } else {
                                        c.pane
                                    },
                                    title: text,
                                })?;
                            }
                            PromptKind::Search => {
                                if let Some(r) = &mut self.review {
                                    r.search = text;
                                    if let Some(i) = r.search_next(None) {
                                        r.row = i;
                                    }
                                }
                            }
                            PromptKind::Command => self.command(client, c, &text)?,
                        }
                        return Ok(());
                    }
                    KeyCode::Char(ch) => text.push(ch),
                    _ => {}
                }
                self.overlay = Some(Overlay::Prompt { label, text, kind });
            }
        }
        Ok(())
    }
    fn command(&mut self, client: &mut Client, c: &Connection, text: &str) -> Result<()> {
        let (name, arg) = text.split_once(' ').unwrap_or((text, ""));
        let action = match name {
            "tab" => Action::CreateTab { title: arg.into() },
            "pane" => Action::CreatePane {
                tab: c.tab,
                title: arg.into(),
            },
            "rename" => Action::Rename {
                tab: if c.mode == Mode::Session {
                    Some(c.tab)
                } else {
                    None
                },
                pane: if c.mode == Mode::Session {
                    None
                } else {
                    c.pane
                },
                title: arg.into(),
            },
            "agent" => Action::StartAgent {
                tab: c.tab,
                pane: if arg == "tab" || c.mode == Mode::Session {
                    None
                } else {
                    c.pane
                },
            },
            "close" => Action::Close {
                tab: if c.mode == Mode::Session {
                    Some(c.tab)
                } else {
                    None
                },
                pane: if c.mode == Mode::Session {
                    None
                } else {
                    c.pane
                },
            },
            "quit" => Action::Shutdown,
            "service" => Action::Service {
                script: arg.into(),
                cwd: c
                    .pane
                    .map(|p| client.snapshot.session.panes[&p].cwd.clone())
                    .unwrap_or_else(|| client.snapshot.session.project.clone())
                    .to_string_lossy()
                    .into_owned(),
                title: "保留服务".into(),
            },
            "handoff" => Action::Handoff {
                pane: c.pane.context("请选中服务所在终端")?,
                script: arg.into(),
                title: "交接服务".into(),
            },
            "services" => {
                let text = client
                    .snapshot
                    .session
                    .services
                    .iter()
                    .map(|s| {
                        let status = match procfs::process::Process::new(s.pid as i32)
                            .and_then(|p| p.stat())
                        {
                            Ok(stat) if stat.starttime == s.start_time && stat.state != 'Z' => {
                                "运行中".to_owned()
                            }
                            Ok(_) | Err(procfs::ProcError::NotFound(_)) => "已退出".to_owned(),
                            Err(procfs::ProcError::InternalError(error)) => {
                                panic!("procfs 内部错误：{error:?}")
                            }
                            Err(error) => format!("状态检查失败：{error}"),
                        };
                        format!(
                            "{} {} · PID {} · {}\n目录 {}\n日志 {}\n脚本 {}",
                            s.id,
                            s.title,
                            s.pid,
                            status,
                            s.cwd.display(),
                            s.log.display(),
                            s.script
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n\n");
                self.overlay = Some(Overlay::Text {
                    title: "保留服务".into(),
                    text,
                    row: 0,
                });
                return Ok(());
            }
            "view" => {
                let id: Id = arg.parse()?;
                let a = Self::agent_target(&client.snapshot, c).context("没有所属 agent")?;
                let conv = client.snapshot.session.agents[&a]
                    .conversations
                    .iter()
                    .find(|conv| conv.id == id)
                    .context("对话不存在")?;
                let text = conv
                    .records
                    .iter()
                    .map(|r| format!("[{}] {}", r.kind, r.text))
                    .collect::<Vec<_>>()
                    .join("\n\n");
                self.review = Some(Review {
                    text,
                    row: 0,
                    column: 0,
                    search: String::new(),
                    wrap: true,
                    width: c.cols.saturating_sub(2),
                });
                client.update(Action::Review { enter: true })?;
                return Ok(());
            }
            "conversations" => {
                let a = Self::agent_target(&client.snapshot, c).context("没有所属 agent")?;
                let agent = &client.snapshot.session.agents[&a];
                let text = agent
                    .conversations
                    .iter()
                    .map(|conv| {
                        format!(
                            "{} {} {}",
                            conv.id,
                            conv.title,
                            if agent.active == conv.id {
                                "活动"
                            } else {
                                "历史"
                            }
                        )
                    })
                    .collect::<Vec<_>>()
                    .join("\n");
                self.overlay = Some(Overlay::Text {
                    title: "对话列表；:view ID 查看，/restore ID 恢复".into(),
                    text,
                    row: 0,
                });
                return Ok(());
            }
            _ => anyhow::bail!("未知命令；F1 查看帮助"),
        };
        client.action(action)?;
        Ok(())
    }
    pub fn draw(
        &mut self,
        frame: &mut Frame<'_>,
        snapshot: &Snapshot,
        connection: Id,
        notice: &str,
    ) {
        let Some(c) = snapshot.connections.get(&connection) else {
            return;
        };
        self.terminal_areas.clear();
        let area = frame.area();
        let zones = Layout::vertical([
            Constraint::Length(1),
            Constraint::Min(1),
            Constraint::Length(2),
        ])
        .split(area);
        let tabs = Line::from(
            snapshot
                .session
                .tabs
                .iter()
                .map(|t| {
                    Span::styled(
                        format!(" {}:{} ", t.id, t.title),
                        if t.id == c.tab {
                            Style::default().add_modifier(Modifier::REVERSED)
                        } else {
                            Style::default()
                        },
                    )
                })
                .collect::<Vec<_>>(),
        );
        frame.render_widget(Paragraph::new(tabs), zones[0]);
        let Ok(tab) = snapshot.session.tab(c.tab) else {
            return;
        };
        let tab_agent = tab.agent.and_then(|id| snapshot.session.agents.get(&id));
        let owner = tab_agent
            .and_then(|a| a.owner)
            .and_then(|id| snapshot.connections.get(&id));
        let ai = tab_agent.is_some_and(|a| a.state != AgentState::Stopped);
        let border = owner.map(|c| Color::Indexed(c.color)).unwrap_or(if ai {
            Color::Cyan
        } else {
            Color::White
        });
        let outer = Block::default()
            .borders(Borders::ALL)
            .border_style(Style::default().fg(border))
            .title(format!(
                "tab {} {} · {:?}",
                tab.id,
                tab.title,
                tab_agent.map(|a| &a.state)
            ))
            .title_top(
                observers(
                    snapshot,
                    c.tab,
                    None,
                    owner.map(|c| c.id),
                    zones[1].width.saturating_sub(24) as usize,
                )
                .right_aligned(),
            );
        let body = outer.inner(zones[1]);
        frame.render_widget(outer, zones[1]);
        let (workspace, tab_chat) = if tab_agent.is_some_and(|a| a.visible) && body.width >= 100 {
            let split =
                Layout::horizontal([Constraint::Min(20), Constraint::Length(38)]).split(body);
            (split[0], Some(split[1]))
        } else {
            (body, None)
        };
        let constraints: Vec<_> = tab
            .panes
            .iter()
            .map(|id| {
                Constraint::Ratio(
                    tab.weights[id] as u32,
                    tab.weights.values().map(|n| *n as u32).sum::<u32>().max(1),
                )
            })
            .collect();
        let panes = Layout::default()
            .direction(if tab.vertical {
                Direction::Vertical
            } else {
                Direction::Horizontal
            })
            .constraints(constraints)
            .split(workspace);
        for (pane, rect) in tab.panes.iter().zip(panes.iter()) {
            self.draw_pane(frame, *rect, snapshot, c, *pane);
        }
        if let Some(a) = tab_agent.filter(|a| a.visible) {
            let rect = tab_chat.unwrap_or_else(|| overlay_right(body));
            if tab_chat.is_none() {
                frame.render_widget(Clear, rect);
            }
            self.draw_agent(frame, rect, a, c);
        }
        let mode = match &c.mode {
            Mode::Session => "session 导航",
            Mode::Tab => "tab 导航",
            Mode::Terminal => "终端输入",
            Mode::Chat => "对话输入",
            Mode::Layout => "布局编辑",
            Mode::Review { .. } => "私有回看",
        };
        frame.render_widget(
            Paragraph::new(format!(
                "{} · 连接 {} · {}{} · 待处理请求 {}\n{}",
                c.user,
                c.id,
                mode,
                if self.prefix {
                    " · 等待前缀命令"
                } else {
                    ""
                },
                snapshot.session.requests.len(),
                notice
            ))
            .style(Style::default().fg(Color::Gray)),
            zones[2],
        );
        if let Some(review) = &mut self.review {
            let rect = zones[1];
            review.width = rect.width.saturating_sub(2);
            frame.render_widget(Clear, rect);
            let text = review
                .text
                .lines()
                .skip(if review.wrap { 0 } else { review.row })
                .map(|line| {
                    line.chars()
                        .skip(review.column as usize)
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n");
            let mut paragraph = Paragraph::new(text).block(Block::bordered().title(format!(
                "私有回看 · 行 {} · q 返回 · / 搜索 · n 下一个 · y 复制 · w 保存",
                review.row + 1
            )));
            if review.wrap {
                paragraph = paragraph
                    .wrap(Wrap { trim: false })
                    .scroll((review.row.min(u16::MAX as usize) as u16, 0));
            }
            frame.render_widget(paragraph, rect);
        }
        if let Some(overlay) = &self.overlay {
            let rect = popup(area);
            frame.render_widget(Clear, rect);
            match overlay {
                Overlay::Text { title, text, row } => frame.render_widget(
                    Paragraph::new(text.as_str())
                        .wrap(Wrap { trim: false })
                        .scroll((*row, 0))
                        .block(Block::bordered().title(format!("{title} · q 返回"))),
                    rect,
                ),
                Overlay::Prompt { label, text, .. } => {
                    frame.render_widget(
                        Paragraph::new(text.as_str())
                            .wrap(Wrap { trim: false })
                            .block(
                                Block::bordered().title(format!("{label} · Enter 应用 · Esc 取消")),
                            ),
                        rect,
                    );
                    frame.set_cursor_position((
                        rect.x
                            + 1
                            + (text.chars().count() as u16).min(rect.width.saturating_sub(3)),
                        rect.y + 1,
                    ));
                }
                Overlay::Requests { selected } => {
                    let text = snapshot
                        .session
                        .requests
                        .values()
                        .enumerate()
                        .map(|(i, r)| {
                            Line::from(Span::styled(
                                format!(
                                    "{} 请求 {}：{}",
                                    if i == *selected { "▶" } else { " " },
                                    r.id,
                                    r.summary
                                ),
                                if i == *selected {
                                    Style::default().fg(Color::Yellow)
                                } else {
                                    Style::default()
                                },
                            ))
                        })
                        .collect::<Vec<_>>();
                    frame.render_widget(
                        Paragraph::new(Text::from(text))
                            .wrap(Wrap { trim: false })
                            .block(Block::bordered().title(
                            "待处理请求 · ↑↓ 选择 · y 确认并中断 · k 保留命令 · n 取消 · q 返回",
                        )),
                        rect,
                    );
                }
            }
        }
    }
    fn draw_pane(
        &mut self,
        frame: &mut Frame<'_>,
        rect: Rect,
        snapshot: &Snapshot,
        c: &Connection,
        id: Id,
    ) {
        let p = &snapshot.session.panes[&id];
        let agent = p.agent.and_then(|id| snapshot.session.agents.get(&id));
        let owner = p.owner.and_then(|id| snapshot.connections.get(&id));
        let ai = agent.is_some_and(|a| a.state != AgentState::Stopped);
        let ai_color = if agent.is_some_and(|a| a.kind == AgentKind::Tab) {
            Color::Cyan
        } else {
            Color::Magenta
        };
        let border = owner.map(|c| Color::Indexed(c.color)).unwrap_or(if ai {
            ai_color
        } else {
            Color::White
        });
        let block = Block::bordered()
            .border_style(Style::default().fg(border))
            .title(format!(
                "{} {}:{} {}",
                if c.pane == Some(id) && c.mode != Mode::Session {
                    "▶"
                } else {
                    ""
                },
                p.id,
                p.title,
                agent.map(|a| format!("{:?}", a.state)).unwrap_or_default()
            ))
            .title_top(
                observers(
                    snapshot,
                    p.tab,
                    Some(id),
                    owner.map(|c| c.id),
                    rect.width.saturating_sub(22) as usize,
                )
                .right_aligned(),
            );
        let body = block.inner(rect);
        frame.render_widget(block, rect);
        if ai && owner.is_some() && rect.height > 2 && rect.width > 0 {
            for y in rect.y + 1..rect.bottom().saturating_sub(1) {
                frame.buffer_mut()[(rect.x, y)].set_fg(ai_color);
            }
        }
        let pane_agent = agent.filter(|a| a.kind != AgentKind::Tab && a.visible);
        let (main, chat) = if pane_agent.is_some() && body.width >= 80 {
            let split =
                Layout::horizontal([Constraint::Min(10), Constraint::Length(35)]).split(body);
            (split[0], Some(split[1]))
        } else {
            (body, None)
        };
        let term = match &p.display {
            Display::Terminal => main,
            Display::Script {
                script,
                active,
                status,
                ..
            } => {
                let split =
                    Layout::vertical([Constraint::Percentage(45), Constraint::Percentage(55)])
                        .split(main);
                let lines = script
                    .lines()
                    .enumerate()
                    .map(|(i, line)| {
                        let current = active.values().any(|n| *n == i + 1);
                        Line::from(Span::styled(
                            format!("{}{:>3} {line}", if current { "▶" } else { " " }, i + 1),
                            if current {
                                Style::default().fg(Color::Yellow)
                            } else {
                                Style::default()
                            },
                        ))
                    })
                    .collect::<Vec<_>>();
                let active_line = active
                    .values()
                    .min()
                    .copied()
                    .unwrap_or(1)
                    .saturating_sub(1)
                    .min(lines.len());
                let row = Paragraph::new(Text::from(lines[..active_line].to_vec()))
                    .wrap(Wrap { trim: false })
                    .line_count(split[0].width.max(1))
                    .saturating_sub(split[0].height as usize / 2)
                    .min(u16::MAX as usize) as u16;
                frame.render_widget(
                    Paragraph::new(Text::from(lines))
                        .wrap(Wrap { trim: false })
                        .scroll((row, 0))
                        .block(
                            Block::default()
                                .borders(Borders::BOTTOM)
                                .title(status.as_str()),
                        ),
                    split[0],
                );
                split[1]
            }
            Display::Edit {
                id,
                path,
                before,
                diff,
                ..
            } => {
                let split =
                    Layout::vertical([Constraint::Percentage(70), Constraint::Percentage(30)])
                        .split(main);
                let detail =
                    Layout::vertical([Constraint::Percentage(40), Constraint::Percentage(60)])
                        .split(split[0]);
                frame.render_widget(
                    Paragraph::new(before.as_str())
                        .wrap(Wrap { trim: false })
                        .block(
                            Block::default()
                                .borders(Borders::BOTTOM)
                                .title("读取上下文"),
                        ),
                    detail[0],
                );
                frame.render_widget(
                    Paragraph::new(diff.as_str())
                        .wrap(Wrap { trim: false })
                        .block(Block::default().borders(Borders::BOTTOM).title(format!(
                            "{} {} · {} · /undo {}",
                            if snapshot.session.edits.get(id).is_some_and(|e| e.undone) {
                                "已撤销"
                            } else {
                                "写入差异"
                            },
                            id,
                            path.display(),
                            id
                        ))),
                    detail[1],
                );
                split[1]
            }
        };
        self.terminal_areas.insert(id, term);
        if let Some(view) = snapshot.terminals.get(&id) {
            let viewport = self.viewports.get(&id).copied().unwrap_or_default();
            draw_terminal(frame, term, view, viewport);
            if c.pane == Some(id) && c.mode == Mode::Terminal && !view.cursor_hidden {
                let (row, col) = view.cursor;
                if row >= viewport.1
                    && col >= viewport.0
                    && row - viewport.1 < term.height
                    && col - viewport.0 < term.width
                {
                    frame.set_cursor_position((
                        term.x + col - viewport.0,
                        term.y + row - viewport.1,
                    ));
                }
            }
        }
        if let Some(a) = pane_agent {
            let rect = chat.unwrap_or_else(|| overlay_right(body));
            if chat.is_none() {
                frame.render_widget(Clear, rect);
            }
            self.draw_agent(frame, rect, a, c);
        }
    }
    fn draw_agent(
        &self,
        frame: &mut Frame<'_>,
        rect: Rect,
        a: &crate::agent::Agent,
        c: &Connection,
    ) {
        let block = Block::bordered().title(format!("agent {} {:?} · {:?}", a.id, a.kind, a.state));
        let body = block.inner(rect);
        frame.render_widget(block, rect);
        let split = Layout::vertical([Constraint::Min(1), Constraint::Length(5.min(body.height))])
            .split(body);
        let mut text = a
            .conversation()
            .records
            .iter()
            .map(|r| format!("[{} {}]\n{}\n", r.kind, r.participants.join("、"), r.text))
            .collect::<Vec<_>>()
            .join("\n");
        text.push_str(&a.streaming);
        let paragraph = Paragraph::new(text).wrap(Wrap { trim: false });
        let row = paragraph
            .line_count(split[0].width.max(1))
            .saturating_sub(split[0].height as usize)
            .min(u16::MAX as usize) as u16;
        frame.render_widget(paragraph.scroll((row, 0)), split[0]);
        frame.render_widget(
            Paragraph::new(a.conversation().draft.as_str())
                .wrap(Wrap { trim: false })
                .block(Block::bordered().title(format!(
                    "共享草稿 · 对话 {} · {}",
                    a.active,
                    if c.mode == Mode::Chat && c.chat == Some(a.id) {
                        "Enter 排队发送"
                    } else {
                        "a 进入对话"
                    }
                ))),
            split[1],
        );
    }
}

fn draw_terminal(frame: &mut Frame<'_>, area: Rect, view: &TerminalView, viewport: (u16, u16)) {
    let mut parser = vt100::Parser::new(view.rows, view.cols, 0);
    parser.process(&view.screen);
    for y in 0..area.height {
        for x in 0..area.width {
            if let Some(cell) = parser
                .screen()
                .cell(y.saturating_add(viewport.1), x.saturating_add(viewport.0))
            {
                if cell.is_wide_continuation() {
                    continue;
                }
                let mut style = Style::default()
                    .fg(color(cell.fgcolor()))
                    .bg(color(cell.bgcolor()));
                if cell.bold() {
                    style = style.add_modifier(Modifier::BOLD);
                }
                if cell.italic() {
                    style = style.add_modifier(Modifier::ITALIC);
                }
                if cell.underline() {
                    style = style.add_modifier(Modifier::UNDERLINED);
                }
                if cell.inverse() {
                    style = style.add_modifier(Modifier::REVERSED);
                }
                let symbol = if cell.contents().is_empty() {
                    " "
                } else {
                    cell.contents()
                };
                frame.buffer_mut().set_stringn(
                    area.x + x,
                    area.y + y,
                    symbol,
                    area.width.saturating_sub(x) as usize,
                    style,
                );
            }
        }
    }
}
fn color(color: vt100::Color) -> Color {
    match color {
        vt100::Color::Default => Color::Reset,
        vt100::Color::Idx(c) => Color::Indexed(c),
        vt100::Color::Rgb(r, g, b) => Color::Rgb(r, g, b),
    }
}
fn observers(
    snapshot: &Snapshot,
    tab: Id,
    pane: Option<Id>,
    owner: Option<Id>,
    width: usize,
) -> Line<'static> {
    let observers:Vec<_>=snapshot.connections.values().filter(|c|c.tab==tab&&Some(c.id)!=owner&&match pane{Some(p)=>c.pane==Some(p)&&!(c.mode==Mode::Session||matches!(&c.mode,Mode::Review{origin} if **origin==Mode::Session)),None=>true}).collect();
    let mut spans = Vec::new();
    let mut used = 0;
    let mut count = 0;
    for c in &observers {
        let length = c.short.chars().count() + 3;
        if used + length + 4 > width {
            break;
        }
        spans.push(Span::styled(
            format!("{}{}", if count > 0 { " · " } else { "" }, c.short),
            Style::default().fg(Color::Indexed(c.color)),
        ));
        used += length;
        count += 1;
    }
    if count < observers.len() {
        spans.push(Span::raw(format!(" +{}", observers.len() - count)));
    }
    Line::from(spans)
}
fn popup(area: Rect) -> Rect {
    Rect::new(
        area.x + area.width / 10,
        area.y + area.height / 8,
        area.width.saturating_mul(8) / 10,
        area.height.saturating_mul(3) / 4,
    )
}
fn overlay_right(area: Rect) -> Rect {
    let width = area.width.min(42);
    Rect::new(
        area.right().saturating_sub(width),
        area.y,
        width,
        area.height,
    )
}
fn terminal_key(key: KeyEvent, view: Option<&TerminalView>) -> Vec<u8> {
    let application = view.is_some_and(|v| v.application_cursor);
    let mut bytes = match key.code {
        KeyCode::Char(ch) if key.modifiers.contains(KeyModifiers::CONTROL) && ch.is_ascii() => {
            vec![(ch.to_ascii_lowercase() as u8) & 31]
        }
        KeyCode::Char(ch) => ch.to_string().into_bytes(),
        KeyCode::Enter => vec![b'\r'],
        KeyCode::Backspace => vec![127],
        KeyCode::Tab => vec![9],
        KeyCode::Esc => vec![27],
        KeyCode::Left
        | KeyCode::Right
        | KeyCode::Up
        | KeyCode::Down
        | KeyCode::Home
        | KeyCode::End => format!(
            "\x1b{}{}",
            if application { "O" } else { "[" },
            match key.code {
                KeyCode::Left => 'D',
                KeyCode::Right => 'C',
                KeyCode::Up => 'A',
                KeyCode::Down => 'B',
                KeyCode::Home => 'H',
                KeyCode::End => 'F',
                _ => unreachable!(),
            }
        )
        .into_bytes(),
        KeyCode::Delete => b"\x1b[3~".to_vec(),
        KeyCode::Insert => b"\x1b[2~".to_vec(),
        KeyCode::PageUp => b"\x1b[5~".to_vec(),
        KeyCode::PageDown => b"\x1b[6~".to_vec(),
        KeyCode::BackTab => b"\x1b[Z".to_vec(),
        KeyCode::F(n) => match n {
            1 => b"\x1bOP".to_vec(),
            2 => b"\x1bOQ".to_vec(),
            3 => b"\x1bOR".to_vec(),
            4 => b"\x1bOS".to_vec(),
            5..=12 => {
                format!("\x1b[{}~", [15, 17, 18, 19, 20, 21, 23, 24][n as usize - 5]).into_bytes()
            }
            _ => vec![],
        },
        _ => vec![],
    };
    if key.modifiers.contains(KeyModifiers::ALT) {
        bytes.insert(0, 27);
    }
    bytes
}
fn help() -> &'static str {
    concat!(
        "session：h/j/k/l 切换 tab，i 进入 tab，a 进入 tab agent 对话，q 脱离连接。\n",
        "tab：h/j/k/l 选择 pane，i 进入终端，a 进入 pane agent 对话，q 返回 session。\n",
        "Ctrl+B 前缀：q 返回导航，i/a 切换输入目标，r 私有回看，A 开关 agent 区，p 请求，: 命令，d 脱离，Q 完全退出；Ctrl+B Ctrl+B 转发前缀；方向键移动终端视口。\n",
        "对话：Enter 排队发送，Alt+Enter 换行，Alt+S 打断当前 agent 并发送，Alt+T 暂停任务树并发送，Ctrl+U 清空草稿。\n",
        "导航：Shift+A 共享开关 agent 区，r 回看，e 布局编辑，n 创建区域，t 改标题，x 关闭区域，o 完整参与者，b 子任务与探测详情，: 命令。\n",
        "布局：h/j/k/l 移动 pane，+/- 调整比例，v 切换横向/纵向，q 返回。\n",
        "回看：j/k 滚动，h/l 水平移动，PageUp/PageDown，g/G 首尾，/ 搜索，n 下一处，y OSC 52 复制，w 保存，q 返回来源模式。\n",
        "命令：tab 标题、pane 标题、rename 标题、agent tab、agent pane、close、quit、conversations、view 对话ID、service 脚本、handoff 脚本、services。\n",
        "对话命令：/resume、/pause [tree]、/new、/clear、/restore ID、/stop、/undo ID、/help。\n",
        "crew load-config：接管后启动个人 shell，exit 返回原 AI Bash。\n",
        "请求：p 或 Ctrl+B p 打开列表，↑↓ 选择，y 确认，n 取消；终端接管可用 k 保留运行命令。仅请求者等待，不抢焦点。"
    )
}
