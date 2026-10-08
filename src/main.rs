use std::io::{self, IsTerminal, Read, Write};
use std::sync::mpsc::{self, RecvTimeoutError};
use std::thread;
use std::time::Duration;

use chrono::Local;
use crossterm::{
    cursor::{Hide, MoveTo, Show},
    execute, queue,
    style::{Attribute, SetAttribute},
    terminal::{self, Clear, ClearType, EnterAlternateScreen, LeaveAlternateScreen},
};
use portable_pty::{CommandBuilder, PtySize, native_pty_system};

type Result<T> = std::result::Result<T, Box<dyn std::error::Error>>;

struct Terminal;

impl Terminal {
    fn enter() -> io::Result<Self> {
        terminal::enable_raw_mode()?;
        let guard = Self;
        execute!(io::stdout(), EnterAlternateScreen, Hide)?;
        Ok(guard)
    }
}

impl Drop for Terminal {
    fn drop(&mut self) {
        let _ = execute!(
            io::stdout(),
            SetAttribute(Attribute::Reset),
            Show,
            LeaveAlternateScreen
        );
        let _ = terminal::disable_raw_mode();
    }
}

enum Event {
    Input(Vec<u8>),
    Output(Vec<u8>),
    Closed,
}

fn pump(mut reader: impl Read + Send + 'static, tx: mpsc::Sender<Event>, input: bool) {
    thread::spawn(move || {
        let mut buffer = [0; 8192];
        loop {
            match reader.read(&mut buffer) {
                Ok(0) => break,
                Ok(n) => {
                    let bytes = buffer[..n].to_vec();
                    let event = if input {
                        Event::Input(bytes)
                    } else {
                        Event::Output(bytes)
                    };
                    if tx.send(event).is_err() {
                        return;
                    }
                }
                Err(error) if error.kind() == io::ErrorKind::Interrupted => continue,
                Err(_) => break,
            }
        }
        let _ = tx.send(Event::Closed);
    });
}

fn pty_size(cols: u16, rows: u16) -> PtySize {
    PtySize {
        rows: rows.saturating_sub(1).max(1),
        cols: cols.max(1),
        pixel_width: 0,
        pixel_height: 0,
    }
}

fn draw(screen: &vt100::Screen, cols: u16, rows: u16, clock: &str) -> io::Result<()> {
    let mut out = io::stdout().lock();
    queue!(out, Hide)?;
    for (row, bytes) in screen.rows_formatted(0, cols).enumerate() {
        queue!(out, MoveTo(0, row as u16))?;
        out.write_all(&bytes)?;
    }
    queue!(
        out,
        SetAttribute(Attribute::Reset),
        MoveTo(0, rows.saturating_sub(1)),
        Clear(ClearType::CurrentLine)
    )?;
    let visible = &clock[clock.len().saturating_sub(cols as usize)..];
    queue!(
        out,
        MoveTo(
            cols.saturating_sub(visible.len() as u16),
            rows.saturating_sub(1)
        )
    )?;
    out.write_all(visible.as_bytes())?;
    let (row, col) = screen.cursor_position();
    queue!(out, MoveTo(col.min(cols.saturating_sub(1)), row))?;
    if !screen.hide_cursor() {
        queue!(out, Show)?;
    }
    out.flush()
}

fn run() -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        return Err("crew needs an interactive terminal".into());
    }
    let (mut cols, mut rows) = terminal::size()?;
    let pty = native_pty_system().openpty(pty_size(cols, rows))?;
    let mut command = CommandBuilder::new("bash");
    command.arg("-i");
    command.env("TERM", "xterm-256color");
    let mut child = pty.slave.spawn_command(command)?;
    drop(pty.slave);

    let result = (|| -> Result<()> {
        let reader = pty.master.try_clone_reader()?;
        let mut writer = pty.master.take_writer()?;
        let _terminal = Terminal::enter()?;
        let (tx, rx) = mpsc::channel();
        pump(reader, tx.clone(), false);
        pump(io::stdin(), tx, true);
        let size = pty_size(cols, rows);
        let mut parser = vt100::Parser::new(size.rows, size.cols, 0);
        let mut last_clock = String::new();
        loop {
            let mut dirty = false;
            match rx.recv_timeout(Duration::from_millis(30)) {
                Ok(Event::Input(bytes)) => writer.write_all(&bytes)?,
                Ok(Event::Output(bytes)) => {
                    parser.process(&bytes);
                    dirty = true;
                }
                Ok(Event::Closed) | Err(RecvTimeoutError::Disconnected) => break,
                Err(RecvTimeoutError::Timeout) => {}
            }
            let new_size = terminal::size()?;
            if new_size != (cols, rows) {
                (cols, rows) = new_size;
                let size = pty_size(cols, rows);
                pty.master.resize(size)?;
                parser.screen_mut().set_size(size.rows, size.cols);
                execute!(io::stdout(), Clear(ClearType::All))?;
                dirty = true;
            }
            let clock = Local::now().format("%H:%M:%S").to_string();
            if dirty || clock != last_clock {
                draw(parser.screen(), cols, rows, &clock)?;
                last_clock = clock;
            }
        }
        Ok(())
    })();
    let _ = child.kill();
    let _ = child.wait();
    result
}

fn main() -> Result<()> {
    run()
}
