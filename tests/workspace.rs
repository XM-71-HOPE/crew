mod support;
use anyhow::Result;
use crew::{
    config::Config,
    files::{Files, Operation},
    protocol::{Action, Choice, Mode, Target},
};
use std::{
    fs,
    net::TcpListener,
    time::{Duration, Instant},
};
use support::{Workspace, wait};

#[test]
fn real_clients_share_layout_and_transfer_input() -> Result<()> {
    let mut w = Workspace::new("workspace", false)?;
    let mut a = w.client("xm")?;
    let mut b = w.client("xm")?;
    let tab = a.snapshot.session.tabs[0].id;
    let pane = a.snapshot.session.tabs[0].panes[0];
    a.request(Action::CreateTab {
        title: "另一页".into(),
    })?;
    a.sync()?;
    let other = a.snapshot.session.tabs[1].id;
    b.request(Action::Focus {
        tab: other,
        pane: a.snapshot.session.tabs[1].panes.first().copied(),
        mode: Mode::Tab,
    })?;
    a.request(Action::Focus {
        tab,
        pane: Some(pane),
        mode: Mode::Tab,
    })?;
    a.sync()?;
    assert_eq!(a.snapshot.connections[&a.connection].tab, tab);
    assert_eq!(a.snapshot.connections[&b.connection].tab, other);
    a.request(Action::CreatePane {
        tab,
        title: "共享终端".into(),
    })?;
    a.sync()?;
    let second = a.snapshot.session.tabs[0].panes[1];
    a.request(Action::Layout {
        tab,
        pane: second,
        movement: -1,
        weight: 30,
        vertical: Some(true),
    })?;
    b.sync()?;
    assert_eq!(b.snapshot.session.tabs[0].panes, vec![second, pane]);
    assert_eq!(b.snapshot.session.tabs[0].weights[&second], 130);
    a.request(Action::Acquire {
        target: Target::Pane(pane),
        terminal: true,
    })?;
    a.sync()?;
    a.request(Action::Input {
        pane,
        bytes: b"printf 'CREW_SHARED_OUTPUT\\n'\r".to_vec(),
        cols: 72,
        rows: 16,
    })?;
    wait(&mut b, 3, |s| {
        s.terminals[&pane]
            .screen
            .windows(18)
            .any(|bytes| bytes == b"CREW_SHARED_OUTPUT")
    })?;
    assert_eq!(b.snapshot.terminals[&pane].cols, 72);
    assert_eq!(b.snapshot.terminals[&pane].rows, 16);
    assert_eq!(b.snapshot.session.panes[&pane].owner, Some(a.connection));
    a.request(Action::Review { enter: true })?;
    b.request(Action::Acquire {
        target: Target::Pane(pane),
        terminal: true,
    })?;
    b.sync()?;
    assert_eq!(b.snapshot.session.panes[&pane].owner, Some(a.connection));
    let request = *b
        .snapshot
        .session
        .requests
        .keys()
        .next()
        .expect("接替必须请求确认");
    b.request(Action::Resolve {
        request,
        choice: Choice::Approve,
    })?;
    a.sync()?;
    assert_eq!(a.snapshot.session.panes[&pane].owner, Some(b.connection));
    assert_eq!(
        a.snapshot.connections[&a.connection].mode,
        Mode::Review {
            origin: Box::new(Mode::Tab)
        }
    );
    a.request(Action::Review { enter: false })?;
    a.sync()?;
    assert_eq!(a.snapshot.connections[&a.connection].mode, Mode::Tab);
    let history = a.request(Action::History {
        target: Target::Pane(pane),
    })?;
    assert!(history.contains("CREW_SHARED_OUTPUT"));
    let bid = b.connection;
    drop(b);
    wait(&mut a, 3, |s| {
        !s.connections.contains_key(&bid) && s.session.panes[&pane].owner.is_none()
    })?;
    let mut reconnect = w.client("xm")?;
    assert_ne!(reconnect.connection, bid);
    assert!(
        reconnect.snapshot.terminals[&pane]
            .screen
            .windows(18)
            .any(|b| b == b"CREW_SHARED_OUTPUT")
    );
    reconnect.request(Action::Focus {
        tab,
        pane: Some(pane),
        mode: Mode::Tab,
    })?;
    reconnect.request(Action::Acquire {
        target: Target::Pane(pane),
        terminal: true,
    })?;
    w.shutdown(&mut a)?;
    assert!(!w.path.join(".crew/test.sock").exists());
    Ok(())
}

#[test]
fn real_edits_require_context_and_refuse_unsafe_undo() -> Result<()> {
    let mut w = Workspace::new("files", false)?;
    let mut c = w.client("xm")?;
    let path = w.path.join("code.txt");
    fs::write(&path, "alpha\nbeta\n")?;
    let mut files = Files::default();
    assert!(
        files
            .edit(
                1,
                1,
                &path,
                Operation::Replace {
                    old: "alpha".into(),
                    new: "gamma".into()
                }
            )
            .is_err()
    );
    assert!(files.read(1, &path, 1, 10)?.contains("1: alpha"));
    let (mut edit, _, _, diff) = files.edit(
        1,
        1,
        &path,
        Operation::Replace {
            old: "alpha".into(),
            new: "gamma".into(),
        },
    )?;
    assert_eq!(fs::read_to_string(&path)?, "gamma\nbeta\n");
    assert!(diff.contains("-alpha") && diff.contains("+gamma"));
    fs::write(&path, "other modification\n")?;
    assert!(Files::undo(&mut edit).is_err());
    assert!(
        files
            .edit(
                1,
                2,
                &path,
                Operation::Replace {
                    old: "gamma".into(),
                    new: "alpha".into()
                }
            )
            .is_err()
    );
    files.read(1, &path, 1, 10)?;
    let (mut edit, _, _, _) = files.edit(
        1,
        3,
        &path,
        Operation::Replace {
            old: "other".into(),
            new: "safe".into(),
        },
    )?;
    Files::undo(&mut edit)?;
    assert_eq!(fs::read_to_string(&path)?, "other modification\n");
    let missing = w.path.join("new.txt");
    files.read(1, &missing, 1, 10)?;
    let (mut edit, _, _, _) = files.edit(1, 4, &missing, Operation::Create("new\n".into()))?;
    Files::undo(&mut edit)?;
    assert!(!missing.exists());
    let outside = Files::resolve(&w.path, "../")?;
    assert!(!Files::authorized(&outside, std::slice::from_ref(&w.path)));
    let large = w.path.join("large.bin");
    fs::write(&large, vec![1; 8_100_000])?;
    let before = Files::hashes(&w.path)?;
    assert!(before.contains_key(&large));
    fs::write(&large, vec![2; 8_100_000])?;
    assert_ne!(Files::hashes(&w.path)?[&large], before[&large]);
    assert!(Config::load(&w.path)?.model.is_none());
    w.shutdown(&mut c)?;
    Ok(())
}

#[test]
fn retained_process_survives_complete_server_exit() -> Result<()> {
    let mut w = Workspace::new("service", false)?;
    let mut c = w.client("xm")?;
    fs::write(w.path.join("service.txt"), "CREW_RETAINED_HTTP_OK\n")?;
    let reservation = TcpListener::bind("127.0.0.1:0")?;
    let port = reservation.local_addr()?.port();
    drop(reservation);
    c.request(Action::Service {
        script: format!("exec python3 -m http.server {port} --bind 127.0.0.1"),
        cwd: ".".into(),
        title: "保留服务".into(),
    })?;
    c.sync()?;
    let service = c.snapshot.session.services[0].clone();
    let pid = service.pid as i32;
    assert_eq!(procfs::process::Process::new(pid)?.stat()?.session, pid);
    let runtime = tokio::runtime::Builder::new_current_thread()
        .enable_all()
        .build()?;
    let url = format!("http://127.0.0.1:{port}/service.txt");
    runtime.block_on(async {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            match reqwest::get(&url).await {
                Ok(response) => {
                    assert_eq!(
                        response.error_for_status()?.text().await?,
                        "CREW_RETAINED_HTTP_OK\n"
                    );
                    break;
                }
                Err(error) if Instant::now() >= deadline => return Err(error.into()),
                Err(_) => tokio::time::sleep(Duration::from_millis(20)).await,
            }
        }
        anyhow::Ok(())
    })?;
    let pane = c.snapshot.session.tabs[0].panes[0];
    c.request(Action::Acquire {
        target: Target::Pane(pane),
        terminal: true,
    })?;
    c.request(Action::Input {
        pane,
        bytes: b"sleep 120 &\r".to_vec(),
        cols: 80,
        rows: 24,
    })?;
    let managed = {
        let deadline = Instant::now() + Duration::from_secs(5);
        loop {
            let processes: Vec<_> = procfs::process::all_processes()?
                .filter_map(|p| p.ok())
                .filter_map(|p| p.stat().ok().map(|stat| (p.pid, stat)))
                .filter(|(_, stat)| stat.comm == "sleep" && stat.session != pid)
                .filter(|(p, _)| {
                    procfs::process::Process::new(*p)
                        .and_then(|p| p.cwd())
                        .is_ok_and(|cwd| cwd == w.path)
                })
                .map(|(pid, stat)| (pid, stat.starttime))
                .collect();
            if !processes.is_empty() {
                break processes;
            }
            assert!(Instant::now() < deadline, "人工终端的后台进程没有启动");
            std::thread::sleep(Duration::from_millis(20));
        }
    };
    w.shutdown(&mut c)?;
    std::thread::sleep(Duration::from_millis(100));
    assert_eq!(
        procfs::process::Process::new(pid)?.stat()?.starttime,
        service.start_time
    );
    runtime.block_on(async {
        assert_eq!(
            reqwest::get(&url).await?.error_for_status()?.text().await?,
            "CREW_RETAINED_HTTP_OK\n"
        );
        anyhow::Ok(())
    })?;
    for (pid, start) in managed {
        assert!(
            !procfs::process::Process::new(pid)
                .and_then(|p| p.stat())
                .is_ok_and(|stat| stat.starttime == start && stat.state != 'Z'),
            "受管理后台进程仍在运行"
        );
    }
    crew::terminal::signal(pid, libc::SIGTERM)?;
    Ok(())
}
