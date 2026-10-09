use anyhow::{Context, Result, bail};
use crew::{
    client::Client,
    protocol::{Action, Choice, Snapshot},
    session::Id,
};
use std::{
    fs,
    path::PathBuf,
    process::{Child, Command, Stdio},
    sync::atomic::{AtomicUsize, Ordering},
    time::{Duration, Instant},
};

static COUNTER: AtomicUsize = AtomicUsize::new(0);

pub struct Workspace {
    pub path: PathBuf,
    pub process: Child,
}

#[allow(dead_code)]
impl Workspace {
    pub fn new(name: &str, model: bool) -> Result<Self> {
        Self::build(name, model, false, None)
    }
    pub fn local_model(name: &str) -> Result<Self> {
        Self::build(name, false, true, None)
    }
    pub fn mock_model(name: &str, url: &str) -> Result<Self> {
        Self::build(name, false, true, Some(url))
    }
    fn build(name: &str, model: bool, local: bool, url: Option<&str>) -> Result<Self> {
        let path = PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(format!(
            ".work/{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::SeqCst)
        ));
        fs::create_dir_all(path.join(".crew/users"))?;
        if model {
            let config = fs::read_to_string(
                PathBuf::from(env!("CARGO_MANIFEST_DIR")).join(".work/model.toml"),
            )?;
            fs::write(path.join(".crew/config.toml"), config)?;
        }
        if local {
            let key = path.join("model-key");
            fs::write(&key, "test-key")?;
            let config = crew::config::Config {
                model: Some(crew::config::ModelConfig {
                    base_url: url.unwrap_or("http://127.0.0.1:1").into(),
                    model: "test".into(),
                    api_key_env: None,
                    api_key_file: Some(key),
                    ..Default::default()
                }),
                instructions: vec![],
            };
            fs::write(path.join(".crew/config.toml"), toml::to_string(&config)?)?;
        }
        let log = fs::File::create(path.join("server.log"))?;
        let process = Command::new(env!("CARGO_BIN_EXE_crew"))
            .args(["serve", "--session", "test", "--project"])
            .arg(&path)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        let mut workspace = Self {
            path: path.canonicalize()?,
            process,
        };
        let deadline = Instant::now() + Duration::from_secs(8);
        while !workspace.path.join(".crew/test.sock").exists() {
            if let Some(status) = workspace.process.try_wait()? {
                bail!(
                    "服务退出 {status}: {}",
                    fs::read_to_string(workspace.path.join("server.log"))?
                );
            }
            if Instant::now() > deadline {
                bail!("服务没有启动");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(workspace)
    }
    pub fn client(&self, user: &str) -> Result<Client> {
        Client::connect(&self.path, "test", user, 120, 40, false)
    }
    pub fn restart(&mut self) -> Result<()> {
        assert!(self.process.try_wait()?.is_some());
        let log = fs::OpenOptions::new()
            .append(true)
            .open(self.path.join("server.log"))?;
        self.process = Command::new(env!("CARGO_BIN_EXE_crew"))
            .args(["serve", "--session", "test", "--project"])
            .arg(&self.path)
            .stdin(Stdio::null())
            .stdout(log.try_clone()?)
            .stderr(log)
            .spawn()?;
        let deadline = Instant::now() + Duration::from_secs(8);
        while !self.path.join(".crew/test.sock").exists() {
            if let Some(status) = self.process.try_wait()? {
                bail!("服务重新启动失败：{status}");
            }
            if Instant::now() > deadline {
                bail!("服务重新启动超时");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
        Ok(())
    }
    pub fn shutdown(&mut self, client: &mut Client) -> Result<()> {
        client.request(Action::Shutdown)?;
        client.sync()?;
        let request=client.snapshot.session.requests.iter().find(|(_,r)|matches!(&r.kind,crew::session::RequestKind::Action(a) if matches!(**a,Action::Shutdown))).map(|(id,_)|*id).context("缺少关闭请求")?;
        client.request(Action::Resolve {
            request,
            choice: Choice::Approve,
        })?;
        let deadline = Instant::now() + Duration::from_secs(8);
        loop {
            if let Some(status) = self.process.try_wait()? {
                assert!(
                    status.success(),
                    "服务错误退出：{}",
                    fs::read_to_string(self.path.join("server.log"))?
                );
                return Ok(());
            }
            if Instant::now() > deadline {
                bail!("服务未退出");
            }
            std::thread::sleep(Duration::from_millis(20));
        }
    }
}

impl Drop for Workspace {
    fn drop(&mut self) {
        if self.path.join(".crew/test.sock").exists()
            && let Ok(mut c) = self.client("cleanup")
            && let Err(e) = self.shutdown(&mut c)
        {
            eprintln!("测试服务关闭失败：{e}");
        }
    }
}

#[allow(dead_code)]
pub fn wait(
    client: &mut Client,
    seconds: u64,
    predicate: impl Fn(&Snapshot) -> bool,
) -> Result<()> {
    let deadline = Instant::now() + Duration::from_secs(seconds);
    loop {
        client.sync()?;
        if predicate(&client.snapshot) {
            return Ok(());
        }
        if Instant::now() > deadline {
            bail!("等待状态超时；通知：{:?}", client.notices);
        }
        std::thread::sleep(Duration::from_millis(30));
    }
}

#[allow(dead_code)]
pub fn approve(client: &mut Client) -> Result<Id> {
    client.sync()?;
    let request = *client
        .snapshot
        .session
        .requests
        .keys()
        .next()
        .context("缺少待处理请求")?;
    client.request(Action::Resolve {
        request,
        choice: Choice::Approve,
    })?;
    client.sync()?;
    Ok(request)
}
