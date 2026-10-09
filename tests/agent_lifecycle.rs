#[path = "support/model_service.rs"]
mod model_service;
mod support;
use anyhow::{Context, Result};
use crew::{
    agent::{AgentKind, AgentState},
    client::Client,
    config::ModelConfig,
    model::{Model, ModelEvent},
    protocol::{Action, Choice, Target},
    session::Id,
};
use model_service::{ModelService, Plan, latest_user};
use serde_json::json;
use std::{fs, sync::atomic::Ordering};
use support::{Workspace, approve, wait};

fn send(client: &mut Client, agent: Id, text: &str) -> Result<()> {
    client.update(Action::Acquire {
        target: Target::Agent(agent),
        terminal: false,
    })?;
    let revision = client.snapshot.session.agents[&agent]
        .conversation()
        .revision;
    client.request(Action::Draft {
        agent,
        text: text.into(),
        revision,
    })?;
    client.request(Action::Send {
        agent,
        interrupt: false,
        tree: false,
    })?;
    Ok(())
}
fn start(client: &mut Client) -> Result<Id> {
    let tab = client.snapshot.session.tabs[0].id;
    client.request(Action::StartAgent { tab, pane: None })?;
    approve(client)?;
    let agent = client.snapshot.session.tabs[0]
        .agent
        .context("tab agent 未创建")?;
    let pane = client.snapshot.session.agents[&agent]
        .pane
        .context("没有附属终端")?;
    wait(client, 5, |s| {
        s.terminals[&pane].status == "原 AI Bash 空闲"
    })?;
    Ok(agent)
}

#[test]
fn task_tree_takeover_keeps_children_running_and_resume_checks_handoff() -> Result<()> {
    let service = ModelService::new(|body, _| {
        if body["messages"]
            .as_array()
            .expect("消息数组")
            .last()
            .is_some_and(|m| m["role"] == "tool")
        {
            return Plan::Text("任务完成".into());
        }
        let user = latest_user(body);
        if user.contains("ROOT_TREE") {
            Plan::Tools(vec![
                (
                    "delegate",
                    json!({"task":"CHILD_TASK","title":"工作子任务"}),
                ),
                ("probe", json!({"task":"PROBE_TASK"})),
                (
                    "bash",
                    json!({"script":"sleep 25\nprintf 'SHOULD_NOT_RUN\\n'"}),
                ),
            ])
        } else if user.contains("CHILD_TASK") {
            Plan::Tools(vec![(
                "bash",
                json!({"script":"sleep 1\nprintf 'CHILD_FINISHED\\n'"}),
            )])
        } else if user.contains("PROBE_TASK") {
            Plan::Tools(vec![("read_file", json!({"path":"scope.txt"}))])
        } else if user.contains("BAD_DELEGATE") {
            Plan::Tools(vec![(
                "delegate",
                json!({"task":"禁止的委派","title":"不应创建"}),
            )])
        } else if let Some((_, id)) = user.split_once("CANCEL_TASK ") {
            Plan::Tools(vec![(
                "cancel_agent",
                json!({"agent":id.trim().parse::<u64>().expect("测试子任务 ID") }),
            )])
        } else {
            Plan::Text("状态检查完成".into())
        }
    })?;
    let mut w = Workspace::mock_model("agent-tree", &service.url)?;
    fs::write(w.path.join("scope.txt"), "read-only probe input\n")?;
    let mut a = w.client("xm")?;
    let root = start(&mut a)?;
    let pane = a.snapshot.session.agents[&root].pane.expect("附属终端存在");
    send(&mut a, root, "ROOT_TREE")?;
    wait(&mut a, 10, |s| {
        s.session.agents[&root].state == AgentState::Executing
            && s.session.agents[&root].children.len() == 2
    })?;
    let children = a.snapshot.session.agents[&root].children.clone();
    let child = *children
        .iter()
        .find(|id| a.snapshot.session.agents[id].kind == AgentKind::Pane)
        .expect("有工作子 agent");
    let probe = *children
        .iter()
        .find(|id| a.snapshot.session.agents[id].kind == AgentKind::Probe)
        .expect("有探测助手");
    assert!(a.snapshot.session.agents[&probe].pane.is_none());
    let mut b = w.client("ln")?;
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
        .context("运行命令接管必须确认")?;
    b.request(Action::Resolve {
        request,
        choice: Choice::KeepRunning,
    })?;
    wait(&mut a, 5, |s| {
        matches!(s.session.agents[&root].state, AgentState::Paused(_))
    })?;
    assert_eq!(a.snapshot.session.agents[&root].owner, Some(a.connection));
    assert_eq!(a.snapshot.session.panes[&pane].owner, Some(b.connection));
    wait(&mut a, 8, |s| {
        s.session.agents[&child].state == AgentState::Completed
            && s.session.agents[&probe].state == AgentState::Completed
    })?;
    assert!(matches!(
        a.snapshot.session.agents[&root].state,
        AgentState::Paused(_)
    ));
    let child_pane = a.snapshot.session.agents[&child]
        .pane
        .expect("子 agent pane 保留");
    assert!(
        a.request(Action::History {
            target: Target::Pane(child_pane)
        })?
        .contains("CHILD_FINISHED")
    );
    assert!(a.request(Action::Resume { agent: root }).is_err());
    b.request(Action::Input {
        pane,
        bytes: vec![3],
        cols: 90,
        rows: 24,
    })?;
    wait(&mut a, 5, |s| {
        s.terminals[&pane].status == "原 AI Bash 空闲"
    })?;
    fs::write(w.path.join("handoff-change.txt"), "shared modification\n")?;
    b.request(Action::Release)?;
    assert!(matches!(
        a.snapshot.session.agents[&root].state,
        AgentState::Paused(_)
    ));
    a.request(Action::Resume { agent: root })?;
    wait(&mut a, 5, |s| {
        s.session.agents[&root].state == AgentState::Completed
    })?;
    let record = a.snapshot.session.agents[&root]
        .conversation()
        .records
        .iter()
        .find(|r| r.kind == "交接")
        .expect("交接记录存在");
    assert!(record.text.contains("handoff-change.txt") && record.text.contains("多人修改"));
    send(&mut a, child, "BAD_DELEGATE")?;
    wait(&mut a, 5, |s| {
        s.session.agents[&child].state == AgentState::Completed
    })?;
    assert!(a.snapshot.session.agents[&child].children.is_empty());
    assert!(
        a.snapshot.session.agents[&child]
            .conversation()
            .records
            .iter()
            .any(|r| r.text.contains("此层级不能使用工具 delegate"))
    );
    b.request(Action::Acquire {
        target: Target::Pane(child_pane),
        terminal: true,
    })?;
    approve(&mut b)?;
    send(&mut a, root, &format!("CANCEL_TASK {child}"))?;
    wait(&mut a, 5, |s| {
        s.session.agents[&root].state == AgentState::Completed
    })?;
    assert_eq!(
        a.snapshot.session.panes[&child_pane].owner,
        Some(b.connection)
    );
    assert!(
        service
            .requests
            .lock()
            .expect("模型请求记录存在")
            .iter()
            .filter(|b| b["tools"].as_array().expect("工具集合").len() == 3)
            .all(
                |b| b["tools"].as_array().expect("工具集合").iter().all(|t| [
                    "read_file",
                    "list_directory",
                    "search_files"
                ]
                .contains(&t["function"]["name"].as_str().expect("工具名")))
            )
    );
    a.request(Action::StopAgent { agent: root })?;
    approve(&mut a)?;
    assert!(a.snapshot.session.tabs[0].agent.is_none());
    for id in a
        .snapshot
        .session
        .agents
        .keys()
        .copied()
        .collect::<Vec<_>>()
    {
        assert_eq!(a.snapshot.session.agents[&id].state, AgentState::Stopped);
        if let Some(p) = a.snapshot.session.agents[&id].pane {
            assert!(a.snapshot.session.panes[&p].agent.is_none());
            assert!(!a.snapshot.session.panes[&p].attached);
        }
    }
    assert_eq!(
        a.snapshot.session.panes[&child_pane].owner,
        Some(b.connection)
    );
    w.shutdown(&mut a)?;
    Ok(())
}

#[test]
fn directory_request_blocks_only_requesting_agent_and_records_rejection() -> Result<()> {
    let service = ModelService::new(|body, _| {
        if body["messages"]
            .as_array()
            .expect("消息数组")
            .last()
            .is_some_and(|m| m["role"] == "tool")
        {
            return Plan::Text("目录请求已处理".into());
        }
        if let Some((_, path)) = latest_user(body).split_once("OUTSIDE_TASK ") {
            Plan::Tools(vec![("read_file", json!({"path":path.trim()}))])
        } else {
            Plan::Text("完成".into())
        }
    })?;
    let mut w = Workspace::mock_model("scope", &service.url)?;
    let external = w
        .path
        .with_file_name(format!("scope-outside-{}", std::process::id()));
    fs::create_dir_all(&external)?;
    let file = external.join("outside.txt");
    fs::write(&file, "external confirmed content\n")?;
    let mut a = w.client("xm")?;
    let agent = start(&mut a)?;
    let task = format!("OUTSIDE_TASK {}", file.display());
    send(&mut a, agent, &task)?;
    wait(&mut a, 5, |s| {
        matches!(s.session.agents[&agent].state, AgentState::Waiting(_))
    })?;
    let mut b = w.client("ln")?;
    let tab = b.snapshot.session.tabs[0].id;
    b.request(Action::CreatePane {
        tab,
        title: "其他任务继续".into(),
    })?;
    a.sync()?;
    assert_eq!(a.snapshot.session.tabs[0].panes.len(), 3);
    let request = *a
        .snapshot
        .session
        .requests
        .keys()
        .next()
        .expect("授权请求存在");
    assert!(
        b.request(Action::Resolve {
            request,
            choice: Choice::KeepRunning
        })
        .is_err()
    );
    b.sync()?;
    assert!(b.snapshot.session.requests.contains_key(&request));
    b.request(Action::Resolve {
        request,
        choice: Choice::Reject,
    })?;
    wait(&mut a, 5, |s| {
        s.session.agents[&agent].state == AgentState::Completed
    })?;
    assert!(
        !a.snapshot.session.agents[&agent]
            .allowed
            .contains(&external)
    );
    assert!(
        a.snapshot.session.agents[&agent]
            .conversation()
            .records
            .iter()
            .any(|r| r.text.contains("拒绝"))
    );
    send(&mut a, agent, &task)?;
    wait(&mut a, 5, |s| {
        matches!(s.session.agents[&agent].state, AgentState::Waiting(_))
    })?;
    approve(&mut b)?;
    wait(&mut a, 5, |s| {
        s.session.agents[&agent].state == AgentState::Completed
    })?;
    assert!(
        a.snapshot.session.agents[&agent]
            .allowed
            .contains(&external)
    );
    assert!(
        a.snapshot.session.agents[&agent]
            .conversation()
            .records
            .iter()
            .any(|r| r.text.contains("external confirmed content"))
    );
    w.shutdown(&mut a)?;
    Ok(())
}

#[test]
fn model_retries_before_output_and_blocks_incomplete_stream() -> Result<()> {
    let root = std::path::PathBuf::from(env!("CARGO_MANIFEST_DIR"))
        .join(format!(".work/model-protocol-{}", std::process::id()));
    fs::create_dir_all(&root)?;
    let key = root.join("key");
    fs::write(&key, "test-key")?;
    let service = ModelService::new(|_, index| {
        if index < 2 {
            Plan::Http(axum::http::StatusCode::TOO_MANY_REQUESTS)
        } else {
            Plan::Text("已恢复".into())
        }
    })?;
    let config = ModelConfig {
        base_url: service.url.clone(),
        model: "test".into(),
        api_key_env: None,
        api_key_file: Some(key.clone()),
        ..Default::default()
    };
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let (tx, mut receiver) = tokio::sync::mpsc::unbounded_channel();
    let completion = runtime.block_on(Model::new(config)?.complete(
        vec![json!({"role":"user","content":"test"})],
        AgentKind::Probe,
        1,
        0,
        tx,
    ))?;
    assert_eq!(completion.text, "已恢复");
    assert_eq!(service.calls.load(Ordering::SeqCst), 3);
    let mut retries = Vec::new();
    while let Ok(event) = receiver.try_recv() {
        if let ModelEvent::Retry {
            attempt, reason, ..
        } = event
        {
            assert!(reason.contains("429"));
            retries.push(attempt);
        }
    }
    assert_eq!(retries, [1, 2]);
    let broken = ModelService::new(|_, _| Plan::Partial)?;
    let (tx, _receiver) = tokio::sync::mpsc::unbounded_channel();
    let config = ModelConfig {
        base_url: broken.url.clone(),
        model: "test".into(),
        api_key_env: None,
        api_key_file: Some(key),
        ..Default::default()
    };
    assert!(
        runtime
            .block_on(Model::new(config)?.complete(
                vec![json!({"role":"user","content":"test"})],
                AgentKind::Probe,
                1,
                0,
                tx
            ))
            .is_err()
    );
    assert_eq!(broken.calls.load(Ordering::SeqCst), 1);
    Ok(())
}

#[test]
fn bash_tool_keeps_long_output_and_reports_nonzero_exit() -> Result<()> {
    let service = ModelService::new(|_, index| {
        if index == 0 {
            Plan::Tools(vec![(
                "bash",
                json!({"script":"printf 'TRACE_FIRST\\n'; seq 1 503; printf 'TRACE_LAST\\n'; false"}),
            )])
        } else {
            Plan::Text("已检查失败状态".into())
        }
    })?;
    let mut w = Workspace::mock_model("long-output", &service.url)?;
    let mut a = w.client("xm")?;
    let agent = start(&mut a)?;
    send(&mut a, agent, "执行并检查长输出")?;
    wait(&mut a, 5, |s| {
        s.session.agents[&agent].state == AgentState::Completed
    })?;
    let result = a.snapshot.session.agents[&agent]
        .conversation()
        .context
        .iter()
        .find(|m| m["role"] == "tool")
        .context("工具结果存在")?["content"]
        .as_str()
        .context("工具结果为文字")?;
    assert!(result.contains("TRACE_FIRST") && result.contains("TRACE_LAST"));
    assert!(result.contains("\n1\n") && result.contains("\n503\n"));
    assert!(result.starts_with("工具失败：Bash 返回状态 1"));
    let pane = a.snapshot.session.agents[&agent]
        .pane
        .expect("执行终端存在");
    let history = a.request(Action::History {
        target: Target::Pane(pane),
    })?;
    assert_eq!(history.matches("\n211\n").count(), 1);
    w.shutdown(&mut a)?;
    Ok(())
}
