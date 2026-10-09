mod support;
use anyhow::Result;
use crew::{session::Display, ui::Ui};
use ratatui::{Terminal, backend::TestBackend};
use std::collections::BTreeMap;
use support::Workspace;

#[test]
fn main_workspace_displays_diff_and_current_script_line_at_personal_width() -> Result<()> {
    let mut w = Workspace::new("ui-text", false)?;
    let mut client = w.client("xm")?;
    let pane = client.snapshot.session.tabs[0].panes[0];
    let mut snapshot = client.snapshot.clone();
    snapshot
        .session
        .panes
        .get_mut(&pane)
        .expect("pane 存在")
        .display = Display::Edit {
        id: 999,
        path: w.path.join("file.txt"),
        before: "old context\n".repeat(80),
        after: "updated\n".into(),
        diff: "-original\n+updated\n".into(),
    };
    let mut ui = Ui::default();
    let mut terminal = Terminal::new(TestBackend::new(60, 22))?;
    terminal.draw(|frame| ui.draw(frame, &snapshot, client.connection, "运行"))?;
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(
        text.contains("old context") && text.contains("-original") && text.contains("+updated"),
        "{text}"
    );
    snapshot
        .session
        .panes
        .get_mut(&pane)
        .expect("pane 存在")
        .display = Display::Script {
        script: (1..=90)
            .map(|n| format!("printf 'SCRIPT_LINE_{n}\\n'\n"))
            .collect(),
        path: w.path.join("script.sh"),
        active: BTreeMap::from([(1, 70)]),
        status: "执行中".into(),
    };
    terminal.draw(|frame| ui.draw(frame, &snapshot, client.connection, "运行"))?;
    let text = terminal
        .backend()
        .buffer()
        .content
        .iter()
        .map(|c| c.symbol())
        .collect::<String>();
    assert!(text.contains("SCRIPT_LINE_70") && text.contains("▶ 70"));
    w.shutdown(&mut client)?;
    Ok(())
}
