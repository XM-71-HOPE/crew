use crate::{
    agent::{AgentKind, ToolCall},
    config::ModelConfig,
    session::Id,
};
use anyhow::{Context, Result, bail};
use eventsource_stream::Eventsource;
use futures_util::StreamExt;
use serde_json::{Value, json};
use std::{collections::BTreeMap, time::Duration};
use tokio::sync::mpsc::UnboundedSender;

pub struct Model {
    client: reqwest::Client,
    config: ModelConfig,
    key: String,
}

pub enum ModelEvent {
    Retry {
        agent: Id,
        epoch: u64,
        attempt: u32,
        reason: String,
    },
    Delta {
        agent: Id,
        epoch: u64,
        text: String,
    },
    Complete {
        agent: Id,
        epoch: u64,
        result: Result<Completion>,
    },
}

pub struct Completion {
    pub message: Value,
    pub calls: Vec<ToolCall>,
    pub text: String,
}

impl Model {
    pub fn new(config: ModelConfig) -> Result<Self> {
        if config.model.trim().is_empty() {
            bail!("模型名称为空");
        }
        let url = reqwest::Url::parse(&config.base_url)?;
        if !url.username().is_empty() || url.password().is_some() {
            bail!("base URL 禁止包含凭据");
        }
        let key = config.key()?;
        let client = reqwest::Client::builder()
            .connect_timeout(Duration::from_secs(15))
            .build()?;
        Ok(Self {
            client,
            config,
            key,
        })
    }
    pub async fn complete(
        &self,
        messages: Vec<Value>,
        kind: AgentKind,
        agent: Id,
        epoch: u64,
        tx: UnboundedSender<ModelEvent>,
    ) -> Result<Completion> {
        if serde_json::to_vec(&messages)?.len() > self.config.context_bytes {
            bail!("完整上下文超过配置大小，请新建对话");
        }
        let payload = json!({"model":self.config.model,"messages":messages,"tools":tools(&kind),"stream":true});
        for attempt in 0..3 {
            let mut produced = false;
            let result = tokio::time::timeout(
                Duration::from_secs(self.config.timeout_seconds),
                self.request(&payload, agent, epoch, &tx, &mut produced),
            )
            .await;
            match result {
                Ok(Ok(c)) => return Ok(c),
                Ok(Err(e)) if !produced && attempt < 2 && retryable(&e) => {
                    tx.send(ModelEvent::Retry {
                        agent,
                        epoch,
                        attempt: attempt + 1,
                        reason: e.to_string().replace(&self.key, "[API key]"),
                    })
                    .context("服务停止接收模型事件")?;
                    tokio::time::sleep(Duration::from_secs(1 << attempt)).await
                }
                Ok(Err(e)) => {
                    return Err(anyhow::anyhow!(
                        e.to_string().replace(&self.key, "[API key]")
                    ));
                }
                Err(_) => bail!(
                    "模型请求在 {} 秒后超时；没有执行未完成的工具调用",
                    self.config.timeout_seconds
                ),
            }
        }
        unreachable!("有限重试必须返回结果")
    }
    async fn request(
        &self,
        payload: &Value,
        agent: Id,
        epoch: u64,
        tx: &UnboundedSender<ModelEvent>,
        produced: &mut bool,
    ) -> Result<Completion> {
        let response = self
            .client
            .post(format!(
                "{}/chat/completions",
                self.config.base_url.trim_end_matches('/')
            ))
            .bearer_auth(&self.key)
            .json(payload)
            .send()
            .await?;
        let status = response.status();
        if !status.is_success() {
            let body = response.text().await?;
            bail!(
                "模型 HTTP {status}: {}",
                body.chars().take(2000).collect::<String>()
            );
        }
        let mut stream = response.bytes_stream().eventsource();
        let mut text = String::new();
        let mut reasoning = String::new();
        let mut calls: BTreeMap<u64, ToolCall> = BTreeMap::new();
        let mut finish = None;
        let mut done = false;
        while let Some(event) = stream.next().await {
            let event = event.context("SSE 流读取失败")?;
            if event.data == "[DONE]" {
                done = true;
                break;
            }
            let data: Value = serde_json::from_str(&event.data).context("模型 SSE JSON 无效")?;
            if data.get("error").is_some() {
                bail!("模型流错误：{}", data["error"]);
            }
            let Some(choice) = data["choices"].as_array().and_then(|v| v.first()) else {
                continue;
            };
            if let Some(reason) = choice["finish_reason"].as_str() {
                finish = Some(reason.to_owned());
            }
            let delta = &choice["delta"];
            if let Some(content) = delta["content"].as_str() {
                *produced = true;
                text.push_str(content);
                tx.send(ModelEvent::Delta {
                    agent,
                    epoch,
                    text: content.into(),
                })
                .context("服务停止接收模型事件")?;
            }
            if let Some(content) = delta["reasoning_content"].as_str() {
                *produced = true;
                reasoning.push_str(content);
            }
            if let Some(parts) = delta["tool_calls"].as_array() {
                *produced = true;
                for part in parts {
                    let index = part["index"].as_u64().context("tool_calls 缺少 index")?;
                    let call = calls.entry(index).or_insert_with(|| ToolCall {
                        id: String::new(),
                        name: String::new(),
                        arguments: String::new(),
                    });
                    if let Some(id) = part["id"].as_str() {
                        call.id.push_str(id);
                    }
                    if let Some(name) = part["function"]["name"].as_str() {
                        call.name.push_str(name);
                    }
                    if let Some(args) = part["function"]["arguments"].as_str() {
                        call.arguments.push_str(args);
                    }
                }
            }
        }
        if !done || !matches!(finish.as_deref(), Some("stop" | "tool_calls")) {
            bail!("模型流没有完整完成，finish_reason={finish:?}，DONE={done}");
        }
        let calls: Vec<_> = calls.into_values().collect();
        for call in &calls {
            if call.id.is_empty() || call.name.is_empty() {
                bail!("工具调用缺少 ID 或名称");
            }
            let args: Value =
                serde_json::from_str(&call.arguments).context("工具参数 JSON 无效")?;
            if !args.is_object() {
                bail!("工具参数必须是 JSON 对象");
            }
        }
        let mut message = json!({"role":"assistant","content":text});
        if !reasoning.is_empty() {
            message["reasoning_content"] = Value::String(reasoning);
        }
        if !calls.is_empty() {
            message["tool_calls"]=json!(calls.iter().map(|c|json!({"id":c.id,"type":"function","function":{"name":c.name,"arguments":c.arguments}})).collect::<Vec<_>>());
        }
        Ok(Completion {
            message,
            calls,
            text,
        })
    }
}

fn retryable(error: &anyhow::Error) -> bool {
    error
        .downcast_ref::<reqwest::Error>()
        .is_some_and(|e| e.is_connect() || e.is_timeout())
        || error.to_string().starts_with("模型 HTTP 429")
        || error.to_string().starts_with("模型 HTTP 5")
}

pub fn tools(kind: &AgentKind) -> Vec<Value> {
    let mut tools = vec![
        tool(
            "read_file",
            "读取文件上下文。编辑前必须读取；访问未授权目录会等待人确认。",
            json!({"path":{"type":"string"},"start":{"type":"integer"},"end":{"type":"integer"}}),
            &["path"],
        ),
        tool(
            "list_directory",
            "列出目录；不执行 shell。",
            json!({"path":{"type":"string"}}),
            &["path"],
        ),
        tool(
            "search_files",
            "在目录内搜索正则表达式；跳过 .git、.crew、target 及符号链接。",
            json!({"path":{"type":"string"},"pattern":{"type":"string"}}),
            &["path", "pattern"],
        ),
    ];
    if *kind != AgentKind::Probe {
        tools.extend([
            tool("bash","在原 AI Bash 中执行完整脚本。禁止主动通过 shell 编辑文件；使用编辑工具。访问工作范围外目录先 request_directory。长驻服务使用 detach_service。不要更改 CREW hooks。",json!({"script":{"type":"string"}}),&["script"]),
            tool("edit_file","对刚读取且没有并发变化的文件做一次精确替换。old 必须唯一；new 可以为空。",json!({"path":{"type":"string"},"old":{"type":"string"},"new":{"type":"string"}}),&["path","old","new"]),
            tool("apply_patch","对刚读取的文件应用单文件 unified diff。",json!({"path":{"type":"string"},"patch":{"type":"string"}}),&["path","patch"]),
            tool("create_file","创建文件；先 read_file 确认文件不存在，不能覆盖已有文件。",json!({"path":{"type":"string"},"content":{"type":"string"}}),&["path","content"]),
            tool("detach_service","启动脱离 session 的服务；stdin 为 /dev/null，输出保存在 .crew/services。CREW 退出后继续运行。",json!({"script":{"type":"string"},"title":{"type":"string"}}),&["script","title"]),
            tool("probe","启动仅可读取、列目录和搜索的探测助手；无 shell、无 pane、不能委派。",json!({"task":{"type":"string"}}),&["task"]),
            tool("request_directory","请求人确认新增工作目录范围。",json!({"path":{"type":"string"}}),&["path"]),
        ]);
    }
    if *kind == AgentKind::Tab {
        tools.extend([
            tool("delegate","委派独立任务给有 pane 的工作 agent。只委派安全并行、互不依赖的任务，共享文件无锁。",json!({"task":{"type":"string"},"title":{"type":"string"}}),&["task","title"]),
            tool("cancel_agent","暂停子任务，不能收回人正在操作的 pane。",json!({"agent":{"type":"integer"}}),&["agent"]),
            tool("agent_status","读取子 agent 的状态、结果与记录。",json!({"agent":{"type":"integer"}}),&["agent"]),
        ]);
    }
    tools
}

fn tool(name: &str, description: &str, properties: Value, required: &[&str]) -> Value {
    json!({"type":"function","function":{"name":name,"description":description,"parameters":{"type":"object","properties":properties,"required":required,"additionalProperties":false}}})
}

pub fn permits(kind: &AgentKind, name: &str) -> bool {
    tools(kind).iter().any(|t| t["function"]["name"] == name)
}
