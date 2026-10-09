# 验证记录

验证日期：2026-10-09 至 2026-10-10。运行环境为 Linux Mint 22.3（Ubuntu noble 系列）、Rust 1.99.0、Bash 和真实 Linux PTY。所有测试输入与运行记录位于 Git 忽略的 `.work/`，模型凭据留在仓库外。

## 运行命令

```bash
cargo fmt --all -- --check
cargo clippy --offline --all-targets -- -D warnings
cargo test --offline --all-targets -- --nocapture
cargo test --offline --test live_model -- --ignored --nocapture
cargo build --offline --release --locked
./target/release/crew doctor
```

常规测试包括 12 项行为验证，全部通过；两项真实模型测试单独运行并通过。格式、静态检查、release 构建和 doctor 检查通过。真实模型测试需要 `.work/model.toml`；默认忽略这些测试不表示真实联调已经完成。

## 实际组件与状态验证

| 验证文件 | 覆盖行为与有效断言 |
| --- | --- |
| `tests/tui_clients.rs` | 在两个外部 PTY 启动编译后的全屏客户端；通过实际按键创建区域、独立切换 tab、调整共享比例与方向、输入真实 Bash、私有回看、确认接替和回到导航。第二个客户端缩放不改变公共 PTY 尺寸。实际命令入口启动保留服务、查看运行状态并确认重新启动交接；关闭两个客户端与服务进程后保留进程仍在运行。 |
| `tests/workspace.rs` | 两个相同 ID 的真实 socket 客户端保持独立导航；共享布局、输入占用确认、回看中接替、断线释放与重连保留输出。实际文件读写、差异与撤销拒绝并发变化，创建后撤销删除新文件。独立 HTTP 服务在 CREW 完全退出后仍返回文件内容；受管理的终端后台进程已停止。 |
| `tests/bash_boundaries.rs` | tab agent 的唯一执行 pane 不能单独关闭；共享草稿在两人接替后保留编辑参与者。接管暂停 AI，终端有人操作或尚在个人 shell 时拒绝恢复；退出个人 shell 后原 Bash 被正确识别。新建和恢复对话使用新的 Bash，旧临时环境变量不继承。 |
| `tests/persistence.rs` | 完全退出并重新启动真实服务，保存的标题、草稿、参与者与排队消息仍存在，占用已释放；agent 保持暂停，不自行发起模型请求。新 Bash 不恢复旧环境变量。 |
| `tests/agent_lifecycle.rs` | 真实服务、PTY 和文件配合可控 HTTP 模型响应，验证父 agent 接管后子任务继续、只读助手能力、pane 层级拒绝委派、目录请求仅阻塞请求者、取消及批准结果、任务树结束保留人工占用、恢复交接信息、有限 429 重试、部分流失败不重试、503 行 Bash 输出完整首尾与非零返回状态。 |
| `tests/ui_text.rs` | Ratatui 文字缓冲区断言：较窄区域同时显示读取上下文和写入差异；长脚本的活动行进入当前显示区域。没有使用截图或视觉检查。 |

可控 HTTP 响应用于明确触发状态与协议边界，实际工具仍使用真实 PTY、文件和进程。它不代替真实模型验证。

另一个真实全屏客户端验证 `connect` 在服务停止后自动启动独立服务，脱离客户端后服务继续；命令行 `stop` 发起的退出确认在发起连接关闭后仍保留，由另一连接执行 `stop --confirm` 完全退出。文件变化扫描另验证超过 8 MB 的实际文件仍被记录，修改内容后摘要发生变化。

## 真实模型

使用用户提供的 `https://api.deepseek.com` 和 `deepseek-flash`，通过仓库外密钥文件读取凭据，未将密钥写入配置、日志或提交。

`live_stream_tools_edit_and_undo` 验证实际 SSE 流、read_file、edit_file 和 Bash 工具调用；核对文件由 original value 写为 verified value、终端产生真实标记，再通过撤销恢复原文件。断言依赖 API 工具调用记录、实际文件及终端输出。

`live_delegation_probe_takeover_and_resume` 验证真实模型委派工作 agent 和只读探测助手；父 agent 运行命令时由第二人确认接管并保留命令，只暂停父 agent，子任务实际继续完成。随后中断父命令，写入接管期间的文件，明确恢复；模型通过文件工具读取该文件并执行恢复标记命令。检查助手无 pane、工具限于读取、列目录和搜索，交接记录包含文件变化。

本次有效模型记录位于 `.work/live-model-146529-1/` 与 `.work/live-hierarchy-146529-0/`，记录保留完整对话、工具结果和终端输出。使用密钥字面值检查运行 JSON 与日志，未发现该密钥。

## 验证边界

两个全屏客户端在本机真实 PTY 中运行；未进行跨机器 SSH 网络环境验证。Ubuntu 22.04、24.04 为部署目标，本次没有在两种独立系统安装环境中逐一运行；当前实际环境记录如上。Rust 1.88 为代码和依赖要求，本次使用 Rust 1.99 构建。

脚本进度是 DEBUG 提供的调用行，外部程序内部进度无法得知。目录边界没有操作系统隔离，Bash 的文件编辑和目录访问按工具说明约束。终端回看保留最多 10000 行历史，完整原始字节记录仍保存在运行目录；工具与交接读取上限 2 MB，超过 2 MB 时说明完整日志位置。

已经依赖 PTY 的服务通过确认终止后重新启动交接，没有验证或提供无损迁移终端描述符。独立服务继续运行的验证使用真实 HTTP 请求。专家人格与 skill 系统按设计保留 TODO。
