mod support;
use anyhow::Result;
use crew::{
    agent::AgentState,
    protocol::{Action, Mode, Target},
};
use support::{Workspace, approve, wait};

#[test]
fn bash_identity_personal_shell_and_conversation_restart() -> Result<()> {
    let mut w = Workspace::local_model("bash-boundary")?;
    let mut a = w.client("xm")?;
    let tab = a.snapshot.session.tabs[0].id;
    a.request(Action::StartAgent { tab, pane: None })?;
    approve(&mut a)?;
    let agent = a.snapshot.session.tabs[0]
        .agent
        .expect("tab agent 必须创建");
    let pane = a.snapshot.session.agents[&agent]
        .pane
        .expect("附属终端必须存在");
    wait(&mut a, 5, |s| {
        s.terminals[&pane].status == "原 AI Bash 空闲"
    })?;
    assert!(
        a.request(Action::Close {
            tab: None,
            pane: Some(pane)
        })
        .is_err()
    );
    let mut b = w.client("ln")?;
    a.request(Action::Acquire {
        target: Target::Agent(agent),
        terminal: false,
    })?;
    a.sync()?;
    let draft = a.snapshot.session.agents[&agent].conversation();
    a.request(Action::Draft {
        agent,
        text: "第一位参与者".into(),
        revision: draft.revision,
    })?;
    b.request(Action::Acquire {
        target: Target::Agent(agent),
        terminal: false,
    })?;
    approve(&mut b)?;
    let draft = b.snapshot.session.agents[&agent].conversation();
    assert_eq!(draft.draft, "第一位参与者");
    let revision = draft.revision;
    b.request(Action::Draft {
        agent,
        text: "两位参与者编辑".into(),
        revision,
    })?;
    b.sync()?;
    assert_eq!(
        b.snapshot.session.agents[&agent].conversation().editors,
        vec!["xm", "ln"]
    );
    b.request(Action::Draft {
        agent,
        text: "/pause".into(),
        revision: revision + 1,
    })?;
    b.request(Action::Send {
        agent,
        interrupt: false,
        tree: false,
    })?;
    a.request(Action::Acquire {
        target: Target::Pane(pane),
        terminal: true,
    })?;
    a.sync()?;
    assert!(matches!(
        a.snapshot.session.agents[&agent].state,
        AgentState::Paused(_)
    ));
    assert!(
        b.request(Action::Resume { agent })
            .unwrap_err()
            .to_string()
            .contains("占用")
    );
    a.request(Action::Input {
        pane,
        bytes: b"printf 'BASH_ROOT:%s\\n' $$; export CREW_TRANSIENT=present\r".to_vec(),
        cols: 90,
        rows: 24,
    })?;
    wait(&mut a, 5, |s| {
        s.terminals[&pane].status == "原 AI Bash 空闲"
    })?;
    let exe = env!("CARGO_BIN_EXE_crew");
    let input = format!("{exe} load-config --user xm\r");
    a.request(Action::Input {
        pane,
        bytes: input.into_bytes(),
        cols: 90,
        rows: 24,
    })?;
    wait(&mut a, 5, |s| {
        s.terminals[&pane].status != "原 AI Bash 空闲"
    })?;
    a.request(Action::Release)?;
    assert!(
        b.request(Action::Resume { agent })
            .unwrap_err()
            .to_string()
            .contains("原 AI Bash")
    );
    a.request(Action::Acquire {
        target: Target::Pane(pane),
        terminal: true,
    })?;
    approve(&mut a)?;
    a.request(Action::Input {
        pane,
        bytes: b"exit\r".to_vec(),
        cols: 90,
        rows: 24,
    })?;
    wait(&mut a, 5, |s| {
        s.terminals[&pane].status == "原 AI Bash 空闲"
    })?;
    let before = a.snapshot.session.agents[&agent].active;
    b.request(Action::Conversation {
        agent,
        conversation: None,
    })?;
    approve(&mut b)?;
    wait(&mut b, 5, |s| {
        s.session.agents[&agent].active != before && s.terminals[&pane].status == "原 AI Bash 空闲"
    })?;
    a.request(Action::Acquire {
        target: Target::Pane(pane),
        terminal: true,
    })?;
    a.request(Action::Input {
        pane,
        bytes: b"printf 'STATE_AFTER_NEW:%s\\n' ${CREW_TRANSIENT-unset}\r".to_vec(),
        cols: 90,
        rows: 24,
    })?;
    wait(&mut b, 5, |s| {
        s.terminals[&pane].status == "原 AI Bash 空闲"
    })?;
    assert!(
        b.request(Action::History {
            target: Target::Pane(pane)
        })?
        .contains("STATE_AFTER_NEW:unset")
    );
    b.request(Action::Conversation {
        agent,
        conversation: Some(before),
    })?;
    approve(&mut b)?;
    wait(&mut b, 5, |s| {
        s.session.agents[&agent].active == before && s.terminals[&pane].status == "原 AI Bash 空闲"
    })?;
    assert_eq!(b.snapshot.session.agents[&agent].conversations.len(), 2);
    assert_eq!(b.snapshot.connections[&b.connection].mode, Mode::Chat);
    w.shutdown(&mut b)?;
    Ok(())
}
