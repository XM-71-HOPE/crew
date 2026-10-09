use anyhow::{Context, Result, bail};
use clap::{Args, Parser, Subcommand};
use crew::{
    client::Client,
    config::{Config, socket_path, validate_name},
    protocol::{Action, Choice, ShellEvent},
    server::Server,
    ui::Ui,
};
use std::{
    fs,
    io::{self, IsTerminal, Write},
    os::unix::{net::UnixDatagram, process::CommandExt},
    path::PathBuf,
    process::Command,
};

#[derive(Parser)]
#[command(name = "crew", version, about = "人与 AI 共用的终端工作空间")]
struct Cli {
    #[command(subcommand)]
    command: Option<Commands>,
}

#[derive(Subcommand)]
enum Commands {
    Connect(ConnectionArgs),
    Serve {
        #[arg(long, default_value = "main")]
        session: String,
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    List {
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    Status(ConnectionArgs),
    Ctl {
        #[command(flatten)]
        connection: ConnectionArgs,
        #[arg(long)]
        action: String,
    },
    Stop {
        #[command(flatten)]
        connection: ConnectionArgs,
        #[arg(long)]
        confirm: bool,
    },
    LoadConfig {
        #[arg(long)]
        user: Option<String>,
    },
    Service {
        #[command(flatten)]
        connection: ConnectionArgs,
        #[arg(long)]
        script: String,
        #[arg(long, default_value = "服务")]
        title: String,
        #[arg(long, default_value = ".")]
        cwd: String,
    },
    Doctor {
        #[arg(long, default_value = ".")]
        project: PathBuf,
    },
    #[command(hide = true)]
    ShellEvent {
        socket: PathBuf,
        pane: u64,
        generation: u64,
        pid: u32,
        kind: String,
        cwd: String,
        source: String,
        line: usize,
        command: String,
        #[arg(allow_hyphen_values = true)]
        status: i32,
    },
}

#[derive(Args)]
struct ConnectionArgs {
    #[arg(long, default_value = "main")]
    session: String,
    #[arg(long)]
    user: Option<String>,
    #[arg(long, default_value = ".")]
    project: PathBuf,
}

fn main() -> Result<()> {
    match Cli::parse().command {
        Some(Commands::ShellEvent {
            socket,
            pane,
            generation,
            pid,
            kind,
            cwd,
            source,
            line,
            command,
            status,
        }) => {
            let event = ShellEvent {
                pane,
                generation,
                pid,
                kind,
                cwd,
                source,
                line,
                command,
                status,
            };
            UnixDatagram::unbound()?.send_to(&serde_json::to_vec(&event)?, socket)?;
        }
        Some(Commands::Serve { project, session }) => {
            validate_name(&session)?;
            runtime()?.block_on(Server::run(project, session))?;
        }
        Some(Commands::Connect(args)) => connect(args)?,
        Some(Commands::Status(args)) => {
            let client = make_client(&args, false)?;
            println!("{}", serde_json::to_string_pretty(&client.snapshot)?);
        }
        Some(Commands::Ctl { connection, action }) => {
            let mut client = make_client(&connection, false)?;
            println!("{}", client.request(serde_json::from_str(&action)?)?);
        }
        Some(Commands::Stop {
            connection,
            confirm,
        }) => {
            let mut client = make_client(&connection, false)?;
            println!("{}", client.request(Action::Shutdown)?);
            if confirm {
                client.sync()?;
                let id=client.snapshot.session.requests.iter().find(|(_,r)|matches!(&r.kind,crew::session::RequestKind::Action(a) if matches!(**a,Action::Shutdown))).map(|(id,_)|*id).context("退出请求不存在")?;
                println!(
                    "{}",
                    client.request(Action::Resolve {
                        request: id,
                        choice: Choice::Approve
                    })?
                );
            }
        }
        Some(Commands::List { project }) => list(project)?,
        Some(Commands::LoadConfig { user }) => load_config(user)?,
        Some(Commands::Service {
            connection,
            script,
            title,
            cwd,
        }) => {
            let mut client = make_client(&connection, false)?;
            println!(
                "{}",
                client.request(Action::Service { script, title, cwd })?
            );
        }
        Some(Commands::Doctor { project }) => {
            let project = project.canonicalize()?;
            let config = Config::load(&project)?;
            println!(
                "项目：{}；Bash：{}；setsid：{}",
                project.display(),
                Command::new("bash").arg("--version").output()?.status,
                Command::new("setsid").arg("--version").output()?.status
            );
            if let Some(model) = config.model {
                model.key()?;
                println!(
                    "模型：{}；base URL：{}；凭据来源可读取（不显示密钥）",
                    model.model, model.base_url
                );
            } else {
                println!("尚未配置模型");
            }
        }
        None => {
            if !io::stdin().is_terminal() {
                bail!("交互流程需要终端；使用 crew connect --session NAME --user ID");
            }
            list(PathBuf::from("."))?;
            let session = prompt("session 名称", "main")?;
            let user = prompt("用户 ID", &default_user())?;
            connect(ConnectionArgs {
                session,
                user: Some(user),
                project: PathBuf::from("."),
            })?;
        }
    }
    Ok(())
}

fn runtime() -> Result<tokio::runtime::Runtime> {
    Ok(tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()?)
}
fn make_client(args: &ConnectionArgs, start: bool) -> Result<Client> {
    let project = args.project.canonicalize()?;
    let user = args.user.clone().unwrap_or_else(default_user);
    validate_name(&user)?;
    let (cols, rows) = crossterm::terminal::size().unwrap_or((100, 30));
    Client::connect(&project, &args.session, &user, cols, rows, start)
}
fn connect(args: ConnectionArgs) -> Result<()> {
    if !io::stdin().is_terminal() || !io::stdout().is_terminal() {
        bail!("connect 需要交互终端；自动化请使用 crew ctl 或 status");
    }
    let mut client = make_client(&args, true)?;
    Ui::run(&mut client)
}
fn default_user() -> String {
    std::env::var("USER").unwrap_or_else(|_| "user".into())
}
fn prompt(label: &str, default: &str) -> Result<String> {
    print!("{label} [{default}]: ");
    io::stdout().flush()?;
    let mut line = String::new();
    io::stdin().read_line(&mut line)?;
    Ok(if line.trim().is_empty() {
        default.into()
    } else {
        line.trim().into()
    })
}
fn list(project: PathBuf) -> Result<()> {
    let project = project.canonicalize()?;
    let dir = project.join(".crew");
    if !dir.exists() {
        println!("尚无 session");
        return Ok(());
    }
    for entry in fs::read_dir(dir)? {
        let entry = entry?;
        if entry.path().extension().is_some_and(|e| e == "json") {
            let name = entry
                .path()
                .file_stem()
                .expect("session 文件有名称")
                .to_string_lossy()
                .into_owned();
            let running = socket_path(&project, &name)?.exists();
            println!(
                "{name}\t{}",
                if running {
                    "服务 socket 存在"
                } else {
                    "已保存，服务停止"
                }
            );
        }
    }
    Ok(())
}
fn load_config(user: Option<String>) -> Result<()> {
    let project =
        PathBuf::from(std::env::var("CREW_PROJECT").context("load-config 需要在 CREW 终端内运行")?);
    let session = std::env::var("CREW_SESSION")?;
    let pane = std::env::var("CREW_PANE_ID")?.parse()?;
    let user = if let Some(user) = user {
        user
    } else {
        let mut client = Client::connect(&project, &session, &default_user(), 100, 30, false)?;
        client.request(Action::Owner { pane })?
    };
    let config = Config::load_user(&project, &user)?;
    let error = Command::new(&config.shell)
        .args(&config.shell_args)
        .env("CREW_USER_ID", &user)
        .exec();
    Err(error).context("启动个人 shell 失败")
}
