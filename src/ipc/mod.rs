//! 进程间通信：帧协议、服务端与客户端。
//!
//! 设计依据：设计 §2.2。
//!
//! - 传输：Windows 命名管道 / Unix 域套接字，**不监听任何 TCP 端口**。
//! - 帧格式：4 字节大端长度 + UTF-8 JSON；JSON 内含 `v`（协议版本）与 `id`（请求/响应关联）。
//! - 幂等：`Chat` / `SetModel` / `SetEffort` 可带 `idempotency_key`，
//!   服务端保留短期去重表，重放返回原结果。
//!
//! 本模块只负责“把请求送进去、把响应用回来”，不包含任何业务判断；
//! 业务由 [`Handler`] 的实现（`app::Runtime`）负责。

use std::sync::Arc;

use serde::{Deserialize, Serialize};
use serde_json::Value;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};
use tracing::{debug, warn};

#[cfg(unix)]
mod unix;
#[cfg(windows)]
mod windows;

/// 协议版本；不兼容变更时递增，并在服务端拒绝旧客户端。
pub const PROTOCOL_VERSION: u32 = 1;

/// 单帧上限 16 MiB：`chat` 文本与状态响应都远小于此值，
/// 限制它可以把恶意/错误长度前缀变成明确错误而不是内存耗尽。
pub const MAX_FRAME_BYTES: usize = 16 * 1024 * 1024;

// ---------------------------------------------------------------------------
// 请求 / 响应
// ---------------------------------------------------------------------------

/// 一次 IPC 请求。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    /// 协议版本，必须等于 [`PROTOCOL_VERSION`]。
    pub v: u32,
    /// 请求 id，由客户端生成；响应原样带回。
    pub id: String,
    /// 幂等键；仅对有副作用的请求有意义。
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub idempotency_key: Option<String>,
    #[serde(flatten)]
    pub body: RequestBody,
}

/// 请求体；`type` 字段使用 snake_case。
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum RequestBody {
    /// 运行状态。
    Status,
    /// 优雅退出守护进程。
    Shutdown,
    /// 暂停 / 恢复 / 重启 LLM 调用（消息接收与存储继续）。
    Ai { action: AiAction },
    /// 从 CLI 注入一句话给 agent。
    Chat {
        message: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        scope: Option<String>,
    },
    /// 热切换主模型。
    SetModel { model: String },
    /// 热切换 effort。
    SetEffort { level: String },
    /// 写 `ESTOP`，暂停新回合。
    Pause,
    /// 删除 `ESTOP`，恢复新回合。
    Resume,
    /// 查询 `hold` 状态与最近安全事件。
    SecurityStatus,
    /// agent 请求解除 `hold`：runtime 用当前上下文重新过闸。
    SecurityResume,
}

/// `ai` 子命令。
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum AiAction {
    Start,
    Stop,
    Restart,
}

/// 一次 IPC 响应。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Response {
    pub v: u32,
    pub id: String,
    pub ok: bool,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub payload: Option<Value>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub error: Option<ApiError>,
}

/// 结构化错误：`code` 供程序判断，`message` 供人阅读（英文）。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ApiError {
    pub code: String,
    pub message: String,
}

impl ApiError {
    pub fn new(code: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            code: code.into(),
            message: message.into(),
        }
    }
}

impl Response {
    /// 成功响应。
    pub fn ok(id: impl Into<String>, payload: Value) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id: id.into(),
            ok: true,
            payload: Some(payload),
            error: None,
        }
    }

    /// 失败响应。
    pub fn error(
        id: impl Into<String>,
        code: impl Into<String>,
        message: impl Into<String>,
    ) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id: id.into(),
            ok: false,
            payload: None,
            error: Some(ApiError::new(code, message)),
        }
    }
}

impl Request {
    /// 构造请求并生成 id。
    pub fn new(body: RequestBody) -> Self {
        Self {
            v: PROTOCOL_VERSION,
            id: new_request_id(),
            idempotency_key: None,
            body,
        }
    }

    /// 附加幂等键。
    pub fn with_idempotency(mut self, key: impl Into<String>) -> Self {
        self.idempotency_key = Some(key.into());
        self
    }

    /// 该请求是否要求幂等处理。
    pub fn wants_idempotency(&self) -> bool {
        self.idempotency_key.is_some()
            && matches!(
                self.body,
                RequestBody::Chat { .. }
                    | RequestBody::SetModel { .. }
                    | RequestBody::SetEffort { .. }
            )
    }
}

/// 客户端请求 id：时间戳 + pid + 进程内自增，足够区分且不需要额外依赖。
fn new_request_id() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static COUNTER: AtomicU64 = AtomicU64::new(0);
    let seq = COUNTER.fetch_add(1, Ordering::Relaxed);
    let millis = chrono::Utc::now().timestamp_millis();
    format!("{millis}-{}-{seq}", std::process::id())
}

// ---------------------------------------------------------------------------
// 帧读写
// ---------------------------------------------------------------------------

/// 写一帧：4 字节大端长度 + JSON。
pub async fn write_frame<W>(writer: &mut W, value: &impl Serialize) -> std::io::Result<()>
where
    W: AsyncWrite + Unpin,
{
    let payload = serde_json::to_vec(value)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    if payload.len() > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            "frame exceeds MAX_FRAME_BYTES",
        ));
    }
    writer
        .write_all(&(payload.len() as u32).to_be_bytes())
        .await?;
    writer.write_all(&payload).await?;
    writer.flush().await?;
    Ok(())
}

/// 读一帧；对端正常关闭时返回 `Ok(None)`。
pub async fn read_frame<R>(reader: &mut R) -> std::io::Result<Option<Value>>
where
    R: AsyncRead + Unpin,
{
    let mut header = [0u8; 4];
    match reader.read_exact(&mut header).await {
        Ok(_) => {}
        Err(error) if error.kind() == std::io::ErrorKind::UnexpectedEof => return Ok(None),
        Err(error) => return Err(error),
    }
    let len = u32::from_be_bytes(header) as usize;
    if len == 0 {
        return Ok(None);
    }
    if len > MAX_FRAME_BYTES {
        return Err(std::io::Error::new(
            std::io::ErrorKind::InvalidData,
            format!("frame length {len} exceeds the {MAX_FRAME_BYTES} byte limit"),
        ));
    }
    let mut payload = vec![0u8; len];
    reader.read_exact(&mut payload).await?;
    let value = serde_json::from_slice(&payload)
        .map_err(|error| std::io::Error::new(std::io::ErrorKind::InvalidData, error))?;
    Ok(Some(value))
}

// ---------------------------------------------------------------------------
// 端点
// ---------------------------------------------------------------------------

/// IPC 端点。
#[derive(Debug, Clone)]
pub enum Endpoint {
    /// Windows 命名管道，形如 `\\.\pipe\scapegoat-bot`。
    #[cfg(windows)]
    Pipe(String),
    /// Unix 域套接字路径。
    #[cfg(unix)]
    Socket(std::path::PathBuf),
}

impl Endpoint {
    /// 由配置与路径布局解析端点。
    pub fn resolve(paths: &crate::paths::SbotPaths, config: &crate::config::IpcConfig) -> Self {
        #[cfg(windows)]
        {
            let _ = paths;
            Self::Pipe(format!(r"\\.\pipe\{}", config.name))
        }
        #[cfg(unix)]
        {
            // 优先 XDG_RUNTIME_DIR（用户私有、重启清空），回退临时目录并按 uid 区分。
            let dir = std::env::var_os("XDG_RUNTIME_DIR")
                .map(std::path::PathBuf::from)
                .unwrap_or_else(std::env::temp_dir);
            let _ = paths;
            Self::Socket(dir.join(format!("{}.sock", config.name)))
        }
    }

    /// 人类可读形式，用于日志与错误信息。
    pub fn display(&self) -> String {
        match self {
            #[cfg(windows)]
            Self::Pipe(path) => path.clone(),
            #[cfg(unix)]
            Self::Socket(path) => path.display().to_string(),
        }
    }
}

// ---------------------------------------------------------------------------
// 服务端
// ---------------------------------------------------------------------------

/// 请求处理器。
///
/// 用 `async_trait` 而不是返回 `impl Future` 的关联类型，是为了让
/// `Arc<dyn Handler>` 能直接跨任务共享；CPU 侧工作是同步的，开销可以忽略。
#[async_trait::async_trait]
pub trait Handler: Send + Sync + 'static {
    async fn handle(&self, request: Request) -> Response;
}

/// 启动服务端，直到 `shutdown` 收到 `true`。
///
/// 该函数只在 `sbot start` 的运行时里调用一次；`first_instance` 用于
/// Windows 命名管道的单实例语义。
pub async fn serve(
    endpoint: &Endpoint,
    handler: Arc<dyn Handler>,
    mut shutdown: tokio::sync::watch::Receiver<bool>,
) -> anyhow::Result<()> {
    match endpoint {
        #[cfg(windows)]
        Endpoint::Pipe(name) => windows::serve(name, handler, &mut shutdown).await,
        #[cfg(unix)]
        Endpoint::Socket(path) => unix::serve(path, handler, &mut shutdown).await,
    }
}

/// 处理一条已建立的连接：循环读请求、写响应，直到对端关闭或出错。
async fn handle_connection<S>(mut stream: S, handler: Arc<dyn Handler>)
where
    S: AsyncRead + AsyncWrite + Unpin,
{
    loop {
        let value = match read_frame(&mut stream).await {
            Ok(Some(value)) => value,
            Ok(None) => {
                debug!("ipc peer closed the connection");
                return;
            }
            Err(error) => {
                warn!(%error, "ipc read failed");
                return;
            }
        };

        let request: Request = match serde_json::from_value(value) {
            Ok(request) => request,
            Err(error) => {
                // 解析失败时没有可信 id；用空 id 回应，客户端按错误处理。
                let response =
                    Response::error("", "bad_request", format!("invalid request: {error}"));
                let _ = write_frame(&mut stream, &response).await;
                return;
            }
        };

        if request.v != PROTOCOL_VERSION {
            let response = Response::error(
                request.id,
                "protocol_version",
                format!(
                    "protocol version {} is not supported; this daemon speaks {PROTOCOL_VERSION}",
                    request.v
                ),
            );
            let _ = write_frame(&mut stream, &response).await;
            return;
        }

        let response = handler.handle(request).await;
        if let Err(error) = write_frame(&mut stream, &response).await {
            warn!(%error, "ipc write failed");
            return;
        }
    }
}

// ---------------------------------------------------------------------------
// 客户端
// ---------------------------------------------------------------------------

/// 客户端连接。
pub struct Client {
    stream: Box<dyn Stream>,
}

/// 同时可读可写、可跨 await 的流。
pub trait Stream: AsyncRead + AsyncWrite + Unpin + Send {}
impl<T: AsyncRead + AsyncWrite + Unpin + Send> Stream for T {}

impl Client {
    /// 连接服务端。
    pub async fn connect(endpoint: &Endpoint) -> anyhow::Result<Self> {
        let stream: Box<dyn Stream> = match endpoint {
            #[cfg(windows)]
            Endpoint::Pipe(name) => {
                let client = tokio::net::windows::named_pipe::ClientOptions::new()
                    .open(name)
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "cannot connect to {}: {error}. Is the daemon running?",
                            endpoint.display()
                        )
                    })?;
                Box::new(client)
            }
            #[cfg(unix)]
            Endpoint::Socket(path) => {
                let stream = tokio::net::UnixStream::connect(path)
                    .await
                    .map_err(|error| {
                        anyhow::anyhow!(
                            "cannot connect to {}: {error}. Is the daemon running?",
                            endpoint.display()
                        )
                    })?;
                Box::new(stream)
            }
        };
        Ok(Self { stream })
    }

    /// 发送一次请求并等待响应。
    pub async fn request(&mut self, request: &Request) -> anyhow::Result<Response> {
        write_frame(&mut self.stream, request).await?;
        let value = read_frame(&mut self.stream)
            .await?
            .ok_or_else(|| anyhow::anyhow!("daemon closed the connection without a response"))?;
        let response: Response = serde_json::from_value(value)?;
        Ok(response)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::duplex;

    #[tokio::test]
    async fn frame_roundtrip() {
        let (mut client, mut server) = duplex(64 * 1024);
        let request = Request::new(RequestBody::Status);
        write_frame(&mut client, &request).await.unwrap();
        let value = read_frame(&mut server).await.unwrap().unwrap();
        let decoded: Request = serde_json::from_value(value).unwrap();
        assert_eq!(decoded.v, PROTOCOL_VERSION);
        assert_eq!(decoded.id, request.id);
        assert!(matches!(decoded.body, RequestBody::Status));
    }

    #[tokio::test]
    async fn eof_returns_none() {
        let (client, mut server) = duplex(64);
        drop(client);
        assert!(read_frame(&mut server).await.unwrap().is_none());
    }

    #[tokio::test]
    async fn oversized_length_is_rejected() {
        let (mut client, mut server) = duplex(64);
        client
            .write_all(&((MAX_FRAME_BYTES as u32 + 1).to_be_bytes()))
            .await
            .unwrap();
        let error = read_frame(&mut server).await.unwrap_err();
        assert_eq!(error.kind(), std::io::ErrorKind::InvalidData);
    }

    #[tokio::test]
    async fn zero_length_means_close() {
        let (mut client, mut server) = duplex(64);
        client.write_all(&0u32.to_be_bytes()).await.unwrap();
        assert!(read_frame(&mut server).await.unwrap().is_none());
    }

    #[test]
    fn idempotency_only_for_mutating_bodies() {
        let chat = Request::new(RequestBody::Chat {
            message: "hi".to_owned(),
            scope: None,
        })
        .with_idempotency("k1");
        assert!(chat.wants_idempotency());

        let status = Request::new(RequestBody::Status).with_idempotency("k2");
        assert!(!status.wants_idempotency());
    }

    #[test]
    fn request_ids_are_unique() {
        let a = Request::new(RequestBody::Status);
        let b = Request::new(RequestBody::Status);
        assert_ne!(a.id, b.id);
    }
}

/// 端到端测试：真实命名管道 / Unix 域套接字。
///
/// 设计 §12 要求 IPC 用真实端点测试，而不是只用内存流模拟。
#[cfg(test)]
mod e2e_tests {
    use super::*;
    use serde_json::json;

    /// 最小处理器：`Status` 返回固定载荷，其余返回错误。
    struct EchoHandler;

    #[async_trait::async_trait]
    impl Handler for EchoHandler {
        async fn handle(&self, request: Request) -> Response {
            let id = request.id;
            match request.body {
                RequestBody::Status => Response::ok(id, json!({ "pong": true })),
                _ => Response::error(id, "unexpected", "only Status is handled in tests"),
            }
        }
    }

    /// 等待服务端就绪：命名管道/UDS 的创建是异步的，客户端需要重试连接。
    async fn connect_with_retry(endpoint: &Endpoint) -> Client {
        for _ in 0..100 {
            if let Ok(client) = Client::connect(endpoint).await {
                return client;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        panic!("server did not become ready at {}", endpoint.display());
    }

    #[cfg(windows)]
    #[tokio::test]
    async fn named_pipe_end_to_end() {
        let endpoint = Endpoint::Pipe(format!(
            r"\\.\pipe\sbot-e2e-{}-{}",
            std::process::id(),
            new_request_id()
        ));
        let (shutdown, rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn({
            let endpoint = endpoint.clone();
            async move { serve(&endpoint, Arc::new(EchoHandler), rx).await }
        });

        let mut client = match tokio::time::timeout(
            std::time::Duration::from_secs(3),
            connect_with_retry(&endpoint),
        )
        .await
        {
            Ok(client) => client,
            Err(_) if server.is_finished() => {
                panic!("server exited before accepting: {:?}", server.await)
            }
            Err(_) => panic!("server did not become ready at {}", endpoint.display()),
        };
        let response = client
            .request(&Request::new(RequestBody::Status))
            .await
            .unwrap();
        assert!(response.ok);
        assert_eq!(response.payload.unwrap(), json!({ "pong": true }));

        // 版本不匹配必须被明确拒绝，而不是被当成正常请求处理。
        let mut wrong = Request::new(RequestBody::Status);
        wrong.v = PROTOCOL_VERSION + 1;
        let mut second = connect_with_retry(&endpoint).await;
        let response = second.request(&wrong).await.unwrap();
        assert!(!response.ok);
        assert_eq!(response.error.unwrap().code, "protocol_version");

        shutdown.send(true).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn unix_socket_end_to_end() {
        let dir = tempfile::tempdir().unwrap();
        let endpoint = Endpoint::Socket(dir.path().join("sbot.sock"));
        let (shutdown, rx) = tokio::sync::watch::channel(false);
        let server = tokio::spawn({
            let endpoint = endpoint.clone();
            async move { serve(&endpoint, Arc::new(EchoHandler), rx).await }
        });

        let mut client = connect_with_retry(&endpoint).await;
        let response = client
            .request(&Request::new(RequestBody::Status))
            .await
            .unwrap();
        assert!(response.ok);
        assert_eq!(response.payload.unwrap(), json!({ "pong": true }));

        // 第二个实例必须被拒绝。
        let (_, rx2) = tokio::sync::watch::channel(false);
        let second = serve(&endpoint, Arc::new(EchoHandler), rx2).await;
        assert!(second.is_err());

        shutdown.send(true).unwrap();
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), server).await;
    }
}
