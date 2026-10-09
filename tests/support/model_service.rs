use axum::{
    Json, Router,
    extract::State,
    http::{HeaderMap, StatusCode},
    response::{IntoResponse, Response, Sse, sse::Event},
    routing::post,
};
use serde_json::{Value, json};
use std::{
    convert::Infallible,
    sync::{
        Arc, Mutex,
        atomic::{AtomicUsize, Ordering},
    },
    thread,
};

pub enum Plan {
    Text(String),
    Tools(Vec<(&'static str, Value)>),
    Http(StatusCode),
    Partial,
}
type Responder = Arc<dyn Fn(&Value, usize) -> Plan + Send + Sync>;
#[derive(Clone)]
struct ApiState {
    responder: Responder,
    calls: Arc<AtomicUsize>,
    requests: Arc<Mutex<Vec<Value>>>,
}

pub struct ModelService {
    pub url: String,
    pub calls: Arc<AtomicUsize>,
    pub requests: Arc<Mutex<Vec<Value>>>,
    shutdown: Option<tokio::sync::oneshot::Sender<()>>,
    thread: Option<thread::JoinHandle<()>>,
}

impl ModelService {
    pub fn new(
        responder: impl Fn(&Value, usize) -> Plan + Send + Sync + 'static,
    ) -> anyhow::Result<Self> {
        let listener = std::net::TcpListener::bind("127.0.0.1:0")?;
        listener.set_nonblocking(true)?;
        let url = format!("http://{}", listener.local_addr()?);
        let calls = Arc::new(AtomicUsize::new(0));
        let requests = Arc::new(Mutex::new(Vec::new()));
        let state = ApiState {
            responder: Arc::new(responder),
            calls: calls.clone(),
            requests: requests.clone(),
        };
        let (shutdown, receive) = tokio::sync::oneshot::channel();
        let thread = thread::spawn(move || {
            let runtime = tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
                .expect("测试 runtime 可以启动");
            runtime.block_on(async move {
                let listener =
                    tokio::net::TcpListener::from_std(listener).expect("测试 listener 可接管");
                axum::serve(
                    listener,
                    Router::new()
                        .route("/chat/completions", post(handle))
                        .with_state(state),
                )
                .with_graceful_shutdown(async {
                    let _ = receive.await;
                })
                .await
                .expect("测试模型服务正常退出");
            });
        });
        Ok(Self {
            url,
            calls,
            requests,
            shutdown: Some(shutdown),
            thread: Some(thread),
        })
    }
}
impl Drop for ModelService {
    fn drop(&mut self) {
        if let Some(shutdown) = self.shutdown.take() {
            let _ = shutdown.send(());
        }
        if let Some(thread) = self.thread.take() {
            thread.join().expect("测试模型服务线程必须正常退出");
        }
    }
}

async fn handle(
    State(state): State<ApiState>,
    headers: HeaderMap,
    Json(body): Json<Value>,
) -> Response {
    assert_eq!(headers["authorization"], "Bearer test-key");
    assert_eq!(body["stream"], true);
    check_context(&body["messages"]);
    let index = state.calls.fetch_add(1, Ordering::SeqCst);
    state
        .requests
        .lock()
        .expect("请求记录可写")
        .push(body.clone());
    let plan = (state.responder)(&body, index);
    let mut events = Vec::new();
    match plan {
        Plan::Http(status) => {
            return (status, Json(json!({"error":"测试外部 HTTP 故障"}))).into_response();
        }
        Plan::Partial => events.push(
            Event::default()
                .json_data(
                    json!({"choices":[{"delta":{"content":"部分文本"},"finish_reason":null}]}),
                )
                .expect("测试 JSON 可序列化"),
        ),
        Plan::Text(text) => {
            events.push(
                Event::default()
                    .json_data(
                        json!({"choices":[{"delta":{"content":text},"finish_reason":"stop"}]}),
                    )
                    .expect("测试 JSON 可序列化"),
            );
            events.push(Event::default().data("[DONE]"));
        }
        Plan::Tools(calls) => {
            for (i, (name, args)) in calls.iter().enumerate() {
                let args = serde_json::to_string(args).expect("测试工具参数可序列化");
                let cut = args
                    .char_indices()
                    .nth(args.chars().count() / 2)
                    .map(|(i, _)| i)
                    .unwrap_or(0);
                events.push(Event::default().json_data(json!({"choices":[{"delta":{"tool_calls":[{"index":i,"id":format!("call-{index}-{i}"),"type":"function","function":{"name":&name[..2],"arguments":&args[..cut]}}]},"finish_reason":null}]})).expect("测试 JSON 可序列化"));
                events.push(Event::default().json_data(json!({"choices":[{"delta":{"tool_calls":[{"index":i,"function":{"name":&name[2..],"arguments":&args[cut..]}}]},"finish_reason":null}]})).expect("测试 JSON 可序列化"));
            }
            events.push(
                Event::default()
                    .json_data(json!({"choices":[{"delta":{},"finish_reason":"tool_calls"}]}))
                    .expect("测试 JSON 可序列化"),
            );
            events.push(Event::default().data("[DONE]"));
        }
    }
    Sse::new(futures_util::stream::iter(
        events.into_iter().map(Ok::<_, Infallible>),
    ))
    .into_response()
}

fn check_context(messages: &Value) {
    let mut pending = std::collections::BTreeSet::new();
    for message in messages.as_array().expect("messages 必须为数组") {
        match message["role"].as_str().expect("消息必须有 role") {
            "assistant" => {
                assert!(pending.is_empty(), "前一批工具结果必须先结束");
                if let Some(calls) = message["tool_calls"].as_array() {
                    for c in calls {
                        assert!(pending.insert(c["id"].as_str().expect("工具 ID").to_owned()));
                    }
                }
            }
            "tool" => {
                assert!(
                    pending.remove(message["tool_call_id"].as_str().expect("结果 ID")),
                    "工具结果必须有对应调用"
                );
            }
            "user" | "system" => assert!(pending.is_empty(), "插入人消息前必须完成工具结果"),
            role => panic!("未知消息 role {role}"),
        }
    }
    assert!(pending.is_empty(), "发送请求前必须完成所有工具结果");
}

pub fn latest_user(body: &Value) -> String {
    body["messages"]
        .as_array()
        .expect("消息数组")
        .iter()
        .rev()
        .find(|m| m["role"] == "user")
        .and_then(|m| m["content"].as_str())
        .unwrap_or("")
        .to_owned()
}
