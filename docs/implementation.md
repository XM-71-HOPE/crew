# 实现选择

CREW 在 Linux 上运行，部署目标为 Ubuntu 22.04、24.04。需要 Bash、util-linux 的 setsid、/proc 和 UTF-8 终端。构建需要 Rust 1.88 或更新版本及 C 编译器；运行使用编译后的 `crew`。TLS 使用 rustls。

每个 session 有一个独立服务进程，通过项目 `.crew` 下的 Unix socket 接受连接。服务串行应用共享状态变化，PTY 读取和模型请求并行运行。连接保存自己的导航、回看、搜索和终端视口。JSON 保存布局、对话、草稿、文件授权和服务交接记录；终端输出单独记录。运行任务、连接占用和旧 shell 状态不恢复。

Rust 模块按主要类型组织：配置、协议、共享 session、终端、文件工具、模型客户端、服务、客户端与界面。内部状态不变量立即失败；网络、文件并发变化、外部命令和模型协议错误明确返回失败。

模型协议选择 Chat Completions：向配置的 base URL 下 `chat/completions` 发送 `stream: true`，读取 SSE 的文本及 `delta.tool_calls`，按 `tool_call_id` 回传工具结果。eventsource-stream 解析 SSE，serde_json 解析 JSON；完整收到 finish_reason 与 DONE 后执行工具。reasoning_content 保留在 API 上下文。凭据来自密钥文件或指定环境变量，不写入记录；指定变量从 PTY 和保留服务环境移除。只重试尚未产生流内容的连接错误、网络超时、429 和 5xx，最多两次，延迟为 1、2 秒，记录次数与原因。产生内容后失败或整个请求超时会阻塞 agent。完整上下文超过配置的字节限制时请求新建对话，不擅自摘要或丢弃记录。

协议参考：[Chat Completions](https://developers.openai.com/api/reference/resources/chat/subresources/completions/methods/create)、[Function calling](https://developers.openai.com/api/docs/guides/function-calling)。

前缀键为 Ctrl+B，再按 Ctrl+B 转发前缀。`r` 私有回看，`e` 布局编辑，`p` 待处理请求，F1 帮助。对话用 Alt+Enter 换行。布局使用共享方向及权重，客户端按自身宽度计算区域；实际终端坐标统一，只有终端输入改变 PTY 尺寸。默认终端为 100×28。

tab 边框表示 tab 对话操作者及 tab agent；pane 边框表示该 pane 的人类操作者及 AI。多人颜色由用户配置指定；AI 使用固定青色或紫色。选中区域用标题中的标记表示，空闲边框仍为白色。右上角观察者按连接列出，完整列表可在参与者列表查看。

服务通过独立 Linux session 启动，标准输入关闭，标准输出和错误写入 `.crew/services/`。保留服务不依赖 PTY 或 CREW 服务进程。已运行且依赖终端的程序需要明确确认后重新启动交接，不能声称可以无损转移其终端描述符。关闭区域或 session 前显示仍受管理的进程，要求明确停止确认；已经交接的服务不参与停止。

专家人格和 skill 系统：TODO。

## Bash 与进程状态

portable-pty 管理真实 PTY，vt100 解释屏幕与历史。AI Bash 使用 --noprofile 和专用 rc 文件；register、DEBUG 和 PROMPT_COMMAND 向 Unix datagram socket 发送结构化事件，包含终端生成编号、Bash PID、目录、脚本来源和行号。个人 shell 或子 shell 的提示不能替换原 AI Bash 身份。提示事件与 PTY 前台进程组共同判定 Bash 空闲；只有原 Bash 返回提示且前台组归还后结束工具。

DEBUG 可定位脚本中的调用行，每个执行 PID 可有独立标记。外部程序只表示所在调用行，不宣称掌握程序内部进度。完整脚本保存并可回看。Bash 非零状态明确标记工具失败，模型可根据实际错误继续处理。

procfs 读取进程身份、Linux session、前台组与命令。终止作用于受管理 PTY 的 Linux session，先 SIGTERM，再清理剩余进程。独立保留服务记录 PID 和启动时间，避免把复用 PID 当作原服务。SIGINT/SIGTERM 请求 CREW 退出确认，断线只释放人的占用。

## 对话、编辑与持久化

模型输出、工具结果与人类发送保存为 API 上下文及可显示记录。发送记录包括发送者和编辑参与者，暂停期间的排队消息独立持久化。切换对话先暂停任务树并中断命令，等待工具结果保存，再把排队消息留在原对话。恢复旧对话触发环境检查，新对话等待人发送任务。服务重新启动补全未返回的工具历史，全部未结束 agent 保持暂停。

文件工具通过 canonical 路径检查目录范围，diffy 生成差异和应用 unified diff，SHA-256 判断读取后与撤销前是否变化。UTF-8 编辑文件上限 2 MB，搜索上限 500 条；交接以流式 SHA-256 扫描文件变化，跳过 .git、.crew、target 和符号链接。终端交接读取上限 2 MB，超过时提示完整日志路径。目录是行为边界，没有操作系统隔离。

JSON 经临时文件、同步与 rename 保存，连接占用不进入持久化状态。每个 session 使用 fs2 文件锁，共享动作串行应用，慢客户端断开并释放占用。客户端退出主动关闭 socket。
