mod support;
use anyhow::{Result, bail};
use crew::{
    agent::{AgentKind, AgentState},
    protocol::{Action, Choice, Target},
};
use std::fs;
use support::{Workspace, approve, wait};

#[test]
#[ignore = "需要 .work/model.toml 的真实模型配置及网络"]
fn live_stream_tools_edit_and_undo() -> Result<()> {
    let mut w = Workspace::new("live-model", true)?;
    fs::write(w.path.join("task.txt"), "original value\n")?;
    let mut a = w.client("xm")?;
    let tab = a.snapshot.session.tabs[0].id;
    a.request(Action::StartAgent { tab, pane: None })?;
    approve(&mut a)?;
    let agent = a.snapshot.session.tabs[0].agent.expect("tab agent 创建");
    let pane = a.snapshot.session.agents[&agent]
        .pane
        .expect("附属终端存在");
    wait(&mut a, 5, |s| {
        s.terminals[&pane].status == "原 AI Bash 空闲"
    })?;
    a.request(Action::Acquire {
        target: Target::Agent(agent),
        terminal: false,
    })?;
    a.sync()?;
    let revision = a.snapshot.session.agents[&agent].conversation().revision;
    a.request(Action::Draft{agent,revision,text:"请实际完成这些动作：用 read_file 读取 task.txt，然后用 edit_file 把 original value 精确替换成 verified value；用 bash 执行 printf 'CREW_LIVE_TOOL_OK\\n'; pwd。所有工具结束后用一句中文说明已完成，不需要其他动作。".into()})?;
    a.request(Action::Send {
        agent,
        interrupt: false,
        tree: false,
    })?;
    wait(&mut a, 120, |s| {
        matches!(
            s.session.agents[&agent].state,
            AgentState::Completed | AgentState::Blocked(_)
        )
    })?;
    if let AgentState::Blocked(reason) = &a.snapshot.session.agents[&agent].state {
        bail!("真实模型阻塞：{reason}");
    }
    assert_eq!(
        fs::read_to_string(w.path.join("task.txt"))?,
        "verified value\n"
    );
    let context = &a.snapshot.session.agents[&agent].conversation().context;
    for tool in ["read_file", "edit_file", "bash"] {
        assert!(
            context.iter().any(|msg| msg["tool_calls"]
                .as_array()
                .is_some_and(|calls| calls.iter().any(|c| c["function"]["name"] == tool))),
            "缺少真实工具调用 {tool}"
        );
    }
    assert!(
        a.request(Action::History {
            target: Target::Pane(pane)
        })?
        .contains("CREW_LIVE_TOOL_OK")
    );
    let edit = *a
        .snapshot
        .session
        .edits
        .keys()
        .next()
        .expect("编辑记录存在");
    a.request(Action::Undo { edit })?;
    assert_eq!(
        fs::read_to_string(w.path.join("task.txt"))?,
        "original value\n"
    );
    println!(
        "真实模型流、read_file、edit_file、Bash 执行及安全撤销通过；记录目录 {}",
        w.path.display()
    );
    w.shutdown(&mut a)?;
    Ok(())
}

#[test]
#[ignore = "需要 .work/model.toml 的真实模型配置及网络"]
fn live_delegation_probe_takeover_and_resume() -> Result<()> {
    let mut w = Workspace::new("live-hierarchy", true)?;
    fs::write(w.path.join("task.txt"), "CREW_HIERARCHY_INPUT\n")?;
    let mut a = w.client("xm")?;
    let mut b = w.client("ln")?;
    let tab = a.snapshot.session.tabs[0].id;
    a.request(Action::StartAgent { tab, pane: None })?;
    approve(&mut a)?;
    let root = a.snapshot.session.tabs[0].agent.expect("tab agent 存在");
    let pane = a.snapshot.session.agents[&root].pane.expect("附属终端存在");
    wait(&mut a, 5, |s| {
        s.terminals[&pane].status == "原 AI Bash 空闲"
    })?;
    a.request(Action::Acquire {
        target: Target::Agent(root),
        terminal: false,
    })?;
    a.sync()?;
    let revision = a.snapshot.session.agents[&root].conversation().revision;
    a.request(Action::Draft { agent: root, revision, text: "这是一次真实协作检查。请严格依次完成：1. delegate 一个工作 agent，标题 worker，任务是先 read_file task.txt，再 bash 执行 sleep 3; printf 'CREW_WORKER_OK\\n'，最后说明完成。2. probe 一个只读助手，任务是 read_file task.txt 并报告内容。3. 你自己 bash 执行 printf 'CREW_PARENT_STARTED\\n'; sleep 30。无需等待子任务即可执行第三步。随后人会接管并中断你，收到恢复交接后不重复长时间 sleep，只 read_file handoff.txt 并 bash 执行 printf 'CREW_RESUMED_OK\\n'，确认子任务结果后报告完成。不编辑文件。".into() })?;
    a.request(Action::Send {
        agent: root,
        interrupt: false,
        tree: false,
    })?;
    wait(&mut a, 120, |s| {
        s.session.agents[&root].children.len() == 2
            && s.session.agents[&root].state == AgentState::Executing
    })?;
    let children = a.snapshot.session.agents[&root].children.clone();
    let worker = *children
        .iter()
        .find(|id| a.snapshot.session.agents[id].kind == AgentKind::Pane)
        .expect("工作 agent 存在");
    let probe = *children
        .iter()
        .find(|id| a.snapshot.session.agents[id].kind == AgentKind::Probe)
        .expect("探测助手存在");
    assert!(a.snapshot.session.agents[&probe].pane.is_none());
    b.request(Action::Acquire {
        target: Target::Pane(pane),
        terminal: true,
    })?;
    b.sync()?;
    let request = *b
        .snapshot
        .session
        .requests
        .keys()
        .next()
        .expect("运行命令的接管需要确认");
    b.request(Action::Resolve {
        request,
        choice: Choice::KeepRunning,
    })?;
    wait(&mut a, 120, |s| {
        s.session.agents[&worker].state == AgentState::Completed
            && s.session.agents[&probe].state == AgentState::Completed
    })?;
    assert!(matches!(
        a.snapshot.session.agents[&root].state,
        AgentState::Paused(_)
    ));
    assert_eq!(a.snapshot.session.panes[&pane].owner, Some(b.connection));
    let worker_pane = a.snapshot.session.agents[&worker]
        .pane
        .expect("工作终端存在");
    assert!(
        a.request(Action::History {
            target: Target::Pane(worker_pane)
        })?
        .contains("CREW_WORKER_OK")
    );
    for message in &a.snapshot.session.agents[&probe].conversation().context {
        if let Some(calls) = message["tool_calls"].as_array() {
            assert!(calls.iter().all(|call| crew::model::permits(
                &AgentKind::Probe,
                call["function"]["name"].as_str().expect("工具名称存在")
            )));
        }
    }
    b.request(Action::Input {
        pane,
        bytes: vec![3],
        cols: 90,
        rows: 24,
    })?;
    wait(&mut b, 5, |s| {
        s.terminals[&pane].status == "原 AI Bash 空闲"
    })?;
    fs::write(w.path.join("handoff.txt"), "CREW_HANDOFF_ACTUAL_CHANGE\n")?;
    b.request(Action::Release)?;
    assert!(a.request(Action::Resume { agent: root }).is_ok());
    wait(&mut a, 120, |s| {
        matches!(
            s.session.agents[&root].state,
            AgentState::Completed | AgentState::Blocked(_)
        )
    })?;
    if let AgentState::Blocked(reason) = &a.snapshot.session.agents[&root].state {
        bail!("真实模型恢复阻塞：{reason}");
    }
    let context = &a.snapshot.session.agents[&root].conversation().context;
    assert!(context.iter().any(|m| {
        m["role"] == "tool"
            && m["content"]
                .as_str()
                .is_some_and(|s| s.contains("CREW_HANDOFF_ACTUAL_CHANGE"))
    }));
    assert!(
        a.snapshot.session.agents[&root]
            .conversation()
            .records
            .iter()
            .any(|r| r.kind == "交接" && r.text.contains("handoff.txt"))
    );
    assert!(
        a.request(Action::History {
            target: Target::Pane(pane)
        })?
        .contains("CREW_RESUMED_OK")
    );
    println!(
        "真实模型委派、只读探测、接管父 agent 时子任务继续及交接恢复通过；记录目录 {}",
        w.path.display()
    );
    w.shutdown(&mut a)?;
    Ok(())
}
