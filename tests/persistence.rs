mod support;
use anyhow::Result;
use crew::{
    agent::AgentState,
    protocol::{Action, Target},
    session::Session,
};
use std::{fs, time::Duration};
use support::{Workspace, approve, wait};

#[test]
fn restart_preserves_layout_drafts_records_and_queued_messages_without_running_tasks() -> Result<()>
{
    let mut w = Workspace::local_model("persistence")?;
    let mut c = w.client("xm")?;
    let tab = c.snapshot.session.tabs[0].id;
    c.request(Action::StartAgent { tab, pane: None })?;
    approve(&mut c)?;
    let agent = c.snapshot.session.tabs[0].agent.expect("tab agent 存在");
    let pane = c.snapshot.session.agents[&agent]
        .pane
        .expect("附属终端存在");
    wait(&mut c, 5, |s| {
        s.terminals[&pane].status == "原 AI Bash 空闲"
    })?;
    c.request(Action::Acquire {
        target: Target::Pane(pane),
        terminal: true,
    })?;
    c.request(Action::Input {
        pane,
        bytes: b"export CREW_OLD_SHELL=value\r".to_vec(),
        cols: 80,
        rows: 24,
    })?;
    wait(&mut c, 5, |s| {
        s.terminals[&pane].status == "原 AI Bash 空闲"
    })?;
    c.request(Action::Release)?;
    c.request(Action::Acquire {
        target: Target::Agent(agent),
        terminal: false,
    })?;
    c.sync()?;
    let revision = c.snapshot.session.agents[&agent].conversation().revision;
    c.request(Action::Draft {
        agent,
        revision,
        text: "暂停期间排队消息".into(),
    })?;
    c.request(Action::Send {
        agent,
        interrupt: false,
        tree: false,
    })?;
    c.sync()?;
    let revision = c.snapshot.session.agents[&agent].conversation().revision;
    c.request(Action::Draft {
        agent,
        revision,
        text: "保存的共同草稿".into(),
    })?;
    c.request(Action::Rename {
        tab: None,
        pane: Some(pane),
        title: "保存的执行终端".into(),
    })?;
    w.shutdown(&mut c)?;
    let saved: Session = serde_json::from_slice(&fs::read(w.path.join(".crew/test.json"))?)?;
    assert!(saved.agents[&agent].owner.is_none());
    assert!(saved.panes[&pane].owner.is_none());
    assert_eq!(
        saved.agents[&agent].queued[0]["content"],
        "发送者：xm；编辑参与者：xm\n暂停期间排队消息"
    );
    w.restart()?;
    let mut next = w.client("ln")?;
    assert_eq!(next.snapshot.session.panes[&pane].title, "保存的执行终端");
    assert_eq!(
        next.snapshot.session.agents[&agent].conversation().draft,
        "保存的共同草稿"
    );
    assert_eq!(
        next.snapshot.session.agents[&agent].conversation().editors,
        ["xm"]
    );
    assert_eq!(next.snapshot.session.agents[&agent].queued.len(), 1);
    std::thread::sleep(Duration::from_millis(200));
    next.sync()?;
    assert!(matches!(
        next.snapshot.session.agents[&agent].state,
        AgentState::Paused(_)
    ));
    assert!(
        !next.snapshot.session.agents[&agent]
            .conversation()
            .records
            .iter()
            .any(|r| r.kind == "模型重试")
    );
    next.request(Action::Acquire {
        target: Target::Pane(pane),
        terminal: true,
    })?;
    next.request(Action::Input {
        pane,
        bytes: b"printf 'RESTART_STATE:%s\\n' ${CREW_OLD_SHELL-unset}\r".to_vec(),
        cols: 80,
        rows: 24,
    })?;
    wait(&mut next, 5, |s| {
        s.terminals[&pane].status == "原 AI Bash 空闲"
    })?;
    assert!(
        next.request(Action::History {
            target: Target::Pane(pane)
        })?
        .contains("RESTART_STATE:unset")
    );
    w.shutdown(&mut next)?;
    Ok(())
}
