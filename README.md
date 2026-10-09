# CREW

CREW 是可信团队与 AI 共用的终端工作空间。先 SSH 登录同一台 Linux 服务器，在同一项目目录连接同一个 session。参与者共享 tab、pane、布局、终端内容与 agent 对话，各连接独立导航、调整个人视口和私有回看。

## 构建与启动

部署目标为 Ubuntu 22.04、24.04。运行需要 Bash、util-linux 的 `setsid`、可读取的 `/proc` 以及 UTF-8 终端；构建需要 Rust 1.88 或更新版本及 C 编译器。TLS 使用 rustls。

```bash
cargo build --release --locked
./target/release/crew doctor
./target/release/crew connect --session main --user xm
```

可以把编译后的 `crew` 放入 PATH，也可使用 `cargo install --path . --locked` 安装。在需要协作的项目目录运行 `crew`，交互选择 session 和用户 ID。参数可以跳过选择：

```bash
crew connect --session main --user xm --project /path/to/project
```

每个 session 自动启动独立服务进程。其他人通过 SSH 登录后执行同一命令，使用自己的用户 ID。ID 自行声明，不做身份认证；同一 ID 的多个连接仍有独立焦点，接替输入位置需要确认。名称支持 1 至 40 个字母、数字、下划线或连字符；socket 路径不得超过 100 字节。

`Ctrl+B d` 脱离当前连接，释放人的操作位置，任务继续运行。再次连接观察当前内容。`crew serve --session main` 可在前台运行服务。

## 模型与个人配置

创建项目 `.crew/config.toml`。同一 session 共用模型与凭据；配置在服务启动时加载，更改后完全停止服务再连接。

```toml
instructions = []

[model]
base_url = "https://api.deepseek.com"
model = "deepseek-flash"
api_key_file = "/home/your-user/.credentials/api-key.env"
timeout_seconds = 120
context_bytes = 600000
```

API key 文件仅包含密钥本身，可带末尾换行。也可删除 `api_key_file`，改用 `api_key_env = "OPENAI_API_KEY"`，在启动服务的环境中设置该变量。文件配置优先。配置只保存凭据来源，密钥不写入记录；指定密钥环境变量不会传给终端或保留服务。

接口为 OpenAI-compatible Chat Completions：请求 `${base_url}/chat/completions`，需要 SSE streaming、`delta.tool_calls` 和按 `tool_call_id` 回传结果。只在未产生流内容时最多重试两次连接错误、网络超时、429 和 5xx；流中途失败会阻塞 agent，未完整生成的工具调用不会执行。完整上下文超过配置的字节限制时提示新建对话。

模型请求读取项目根目录 `AGENT.md` 和 `instructions` 指定的说明文件；相对路径以项目目录为基准，说明只能在固定工具能力内定制。未配置模型时可使用人工终端；启动 agent 会说明缺项。`crew doctor` 检查系统命令与凭据来源，不显示密钥。

个人配置为 `.crew/users/ID.toml`：

```toml
shell = "/bin/bash"
shell_args = ["-i"]
color = 33
abbreviation = "xm"
```

颜色为 256 色索引；缺省颜色按 ID 计算，简称取前两个字符。人工 pane 加载创建者的配置。AI 固定使用 Bash；接管后运行 `crew load-config` 启动当前操作者的个人 shell，`exit` 回到原 AI Bash。

## 操作

| 模式 | 按键与行为 |
| --- | --- |
| session 导航 | `h/j/k/l` 切换 tab，`i` 进入 tab，`a` 进入 tab agent 对话，`q` 脱离连接 |
| tab 导航 | `h/j/k/l` 选择 pane，`i` 接管终端，`a` 进入 pane agent 对话，`q` 返回 session |
| 终端输入 | 按键发给终端；`Ctrl+B q` 返回 tab 导航 |
| 对话输入 | Enter 排队发送，Alt+Enter 换行，Alt+S 打断当前 agent 并发送，Alt+T 暂停任务树并发送；`Ctrl+B q` 返回来源导航 |
| 布局编辑 | tab 导航按 `e`；`h/j/k/l` 调整排列，`+/-` 调整比例，`v` 切换横向或纵向，`q` 返回 |
| 私有回看 | 导航按 `r`，输入按 `Ctrl+B r`；`j/k`、PageUp/PageDown、`g/G` 滚动，`h/l` 水平移动，`/` 搜索，`n` 下一处，`y` 请求 OSC 52 复制，`w` 保存，`q` 返回 |

前缀为 Ctrl+B；`Ctrl+B Ctrl+B` 转发该键。输入时 `Ctrl+B a/i` 切换对话或终端，前缀加方向键移动个人终端视口。`F1` 查看完整帮助。

导航中 `n` 创建有标题的区域，`t` 改标题，`x` 请求关闭，`o` 查看完整参与者，`b` 查看子任务及探测结果。`Shift+A` 共享开关 agent 区域，输入时使用 `Ctrl+B A`。`a` 打开已有 agent 区域；没有 agent 时只提示。被人操作的对话区不能关闭。

边框使用操作者颜色，无人操作时为白色；人和 AI 同时操作时分段着色。AI 完成后保留颜色和状态。焦点用标题标记表示；观察者简称位于右上角，空间不足显示 `+N`。

导航按 `:` 输入命令，输入模式按 `Ctrl+B :`：

| 命令 | 行为 |
| --- | --- |
| `tab 标题`、`pane 标题`、`rename 标题` | 创建与改名 |
| `agent tab`、`agent pane` | 请求创建对应层级 agent |
| `close`、`quit` | 请求关闭区域或完全退出 session |
| `conversations`、`view ID` | 列出对话、私有查看旧对话 |
| `service 脚本` | 在选中 pane 的目录启动独立保留服务 |
| `handoff 脚本` | 请求终止选中终端的程序，再用脚本重新启动交接 |
| `services` | 查看保留服务的 PID、状态、目录、脚本与日志 |

待处理请求在状态栏提示，不改变焦点。导航按 `p`，输入按 `Ctrl+B p`；方向键选择，`y` 确认，`n` 取消，接管运行命令时 `k` 保留命令。任何人都可以处理。

## Agent 协作

tab agent 有一个唯一附属执行 pane，可委派自带 pane 的工作 agent。pane agent 只能启动无 shell、无 pane 的只读探测助手；助手只有读取、列目录与搜索工具。tab agent 存在时不能单独关闭附属 pane，结束 agent 后终端可转人工使用。

草稿共享，接替者继续编辑；发送记录包括发送者和全部编辑参与者。右侧展示对话与工具记录，主工作区展示完整脚本、可定位活动行、终端输出、文件上下文和差异。右侧关闭后执行详情仍保留；窄屏按客户端尺寸覆盖显示。实际终端保持统一坐标，只由最近实际输入改变 PTY 尺寸，旁观与窗口缩放不改变该尺寸。

人进入对话时 AI 可执行；人接管终端只暂停所属 agent，已有子任务继续。有运行命令时选择中断或保留。离开输入、断线不会自动恢复 AI。对话框支持：

| 命令 | 行为 |
| --- | --- |
| `/resume` | 检查无人占用终端、原 AI Bash 空闲，交接目录、文件变化和终端记录后恢复 |
| `/pause`、`/pause tree` | 暂停当前 agent 或任务树；运行命令可继续，终端 Ctrl+C 可以中断 |
| `/new`、`/clear` | 确认处理命令与子任务，保存旧对话并新建活动对话 |
| `/restore ID` | 恢复旧上下文，启动新的 Bash，自动重新检查环境 |
| `/stop` | 请求结束 agent 与子任务，保留终端和记录 |
| `/undo ID` | 没有后续文件修改时撤销写入 |

新建和恢复对话保留当前文件；旧 Bash 临时状态不恢复。接管期间文件变化可能来自多人，不归因给接管者。私有回看不增加占用、不暂停 AI；从输入进入时保留原位置，被他人接替后继续回看，返回导航。

文件工具先读取上下文，再唯一精确替换、创建或应用单文件 unified diff。并发变化拒绝写入或撤销，重新读取后才能编辑。共享目录不锁文件、不隔离 agent。外部目录需确认，只有请求 agent 等待。Bash 的范围及禁止主动编辑文件规则由工具说明约束，没有操作系统隔离。

## 保留服务与完整退出

保留服务建立独立 Linux session，stdin 为 `/dev/null`，stdout/stderr 写入 `.crew/services/`，不依赖 CREW 的终端或服务进程。

```bash
crew service --session main --title web --cwd . --script 'exec python3 -m http.server 8080'
crew status --session main
crew stop --session main --confirm
```

终端中已有程序通过 `:handoff 脚本` 确认终止并重新启动；交接记录包含新的 PID、启动时间与日志。关闭区域或 session 显示受管理进程并要求停止确认，已交接服务继续运行。保留服务由团队通过通常的 Linux 进程或服务管理方式停止。

`Ctrl+B Q` 或 `:quit` 请求完整退出，随后处理确认；`crew stop --confirm` 可通过参数明确确认。关闭客户端仅释放占用。服务重新启动恢复配置与记录，agent 保持暂停，旧进程与运行任务不恢复。

## 数据与验证

项目 `.crew` 保存配置、布局、目录范围、草稿、对话、工具与服务记录，终端输出位于 `.crew/SESSION.runtime/`。`.crew`、`.work`、`target` 已被 Git 忽略。回看可保存到 `.crew/users/ID-review.txt`。记录包括项目内容与命令输出，按团队项目数据管理。

```bash
cargo fmt --all -- --check
cargo clippy --locked --all-targets -- -D warnings
cargo test --locked --all-targets
```

真实模型测试读取不提交的 `.work/model.toml`，格式与共享配置相同；用 `cargo test --test live_model -- --ignored --nocapture` 运行。PTY、Unix socket 和进程测试需要相应运行条件，HTTP 服务保留测试使用 Python 3 标准库。

产品行为见 [设计文档](docs/design.md)，工程选择见 [实现说明](docs/implementation.md)，实际验证范围见 [验证记录](docs/verification.md)。专家人格和 skill 系统保留 TODO。
