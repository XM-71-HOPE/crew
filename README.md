# CREW Relays Everyone's Workspace
***This is a project for personal development exercising and possible rust learning!***
***There will be ai slops, ugly codes and unsafe rust messing around!***

## Overview


## Milestones
- [ ] 内置终端、进程守护
- [ ] terminal multiplexing
- [ ] wemux/多人合作/服务器连接
- [ ] agent harness
- [ ] 所有权管理

## Run

Requires Rust and Bash.

```bash
cargo install --path .
crew
```

For development, use `cargo run`.

`crew` opens a full-screen Bash session with a local clock in the bottom-right
corner. Window resizing updates Bash's terminal size. Use `exit` or Ctrl-D to
close the session; Ctrl-C interrupts the running command.

The implementation is in `src/main.rs`:

- `portable-pty` starts Bash on a pseudo-terminal.
- Two reader threads send keyboard input and Bash output to the main loop.
- `vt100` turns Bash's output into a terminal screen, including colors and cursor position.
- `crossterm` draws that screen and restores the terminal when the session ends.
- `chrono` supplies the local clock.

This first version has one session; it doesn't yet support panes, detach, or scrollback.
