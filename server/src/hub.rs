//! 设备注册表与请求路由中心。
//!
//! 每个客户端连接由两个 tokio 任务持有：
//! - **read_loop**：分帧、处理 REGISTER / PING，并把结果帧路由到等待中的请求；
//! - **write_loop**：消费 per-connection 的 `mpsc`，串行写出。
//!
//! 因为只有 write_loop 持有写半端，命令、分块、文件数据之间不会互相交错。
//!
//! 请求-响应采用「先登记、后发送」：`Hub::reserve_request` 会在写出任何帧之前
//! 就把 `req_id -> oneshot` 登记好，因此即使客户端瞬间回包也不会丢结果。

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use serde::Serialize;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::tcp::{OwnedReadHalf, OwnedWriteHalf};
use tokio::net::TcpStream;
use tokio::sync::mpsc;
use tracing::{debug, info, warn};

use crate::protocol::{self, ClientFrame};

/// 单帧上限：超过这个长度仍未见到换行即判定协议异常并断开，避免内存被撑爆。
const MAX_FRAME_BYTES: usize = 128 * 1024 * 1024;
/// 读取缓冲大小：一次 syscall 尽量多取几帧。
const READ_CHUNK: usize = 64 * 1024;
/// 每个连接的写出队列长度。
const WRITE_QUEUE: usize = 64;
/// 单个请求的待投递帧队列长度。满了会让 read_loop 反向阻塞在 TCP 上，
/// 从而把背压传给客户端，而不是把帧丢掉或无限堆内存。
const REQUEST_QUEUE: usize = 64;
/// 离线设备在列表中保留的条数上限。
const MAX_OFFLINE_HISTORY: usize = 50;

/// 一个设备对外的可见信息。
#[derive(Debug, Clone, Serialize)]
pub struct DeviceInfo {
    pub uuid: String,
    pub hostname: String,
    pub ip: String,
    pub os: String,
    /// 最近一次心跳 / 任何帧到达的 Unix 毫秒时间戳。
    pub last_seen_ms: u64,
    pub online: bool,
}

/// Unix 毫秒时间戳。
pub fn now_ms() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|duration| duration.as_millis() as u64)
        .unwrap_or_default()
}

/// 一条活动的客户端连接。
struct Connection {
    /// 全局唯一的连接序号，用于识别「重连前的旧连接」。
    conn_id: u64,
    sender: mpsc::Sender<String>,
    /// 中止 read_loop 用（同 UUID 重连时踢掉旧连接）。
    reader: tokio::task::AbortHandle,
}

/// 一个已登记、等待客户端回应的请求。
///
/// 未调用 [`Request::finish`] 就 drop 时，会把自己的登记从挂起表中摘掉。
///
/// 结果用「有界 mpsc」而非 oneshot 投递：`FILE_CHUNK` 会让一个请求收到多个帧，
/// 队列满时 read_loop 会阻塞在投递上，进而把背压传给客户端。
pub struct Request {
    hub: Hub,
    pub req_id: u64,
    sender: mpsc::Sender<String>,
    results: Option<mpsc::Receiver<ClientFrame>>,
    finished: bool,
}

impl Request {
    /// 发送一帧。同一请求可多次调用（分块上传会连发多帧）。
    pub async fn send(&self, frame: String) -> Result<(), String> {
        self.sender
            .send(frame)
            .await
            .map_err(|_| "写通道已关闭，连接可能刚刚断开".to_string())
    }

    /// 等待客户端对这一号请求的下一个帧。
    ///
    /// 收到终止帧（`RESULT` / `FILE_END` / `ERROR`）时会自动摘除登记，
    /// 因此命令、落盘、下载三条路径都不需要调用方手动清理。
    pub async fn next_frame(&mut self, timeout: Duration) -> Result<ClientFrame, String> {
        let results = self
            .results
            .as_mut()
            .ok_or_else(|| "该请求已经结束".to_string())?;

        let frame = match tokio::time::timeout(timeout, results.recv()).await {
            Ok(Some(frame)) => frame,
            Ok(None) => {
                self.finished = true;
                return Err("连接在等待响应期间断开".to_string());
            }
            Err(_) => {
                self.finished = true;
                self.hub.cancel_pending(self.req_id);
                return Err(format!("等待客户端响应超时（{} 秒）", timeout.as_secs()));
            }
        };

        if is_terminal(&frame) {
            self.finished = true;
            self.hub.cancel_pending(self.req_id);
        }
        Ok(frame)
    }

    /// 等待客户端对这一号请求的单个回应（命令、文件落盘结果等）。
    pub async fn finish(mut self, timeout: Duration) -> Result<ClientFrame, String> {
        self.next_frame(timeout).await
    }

    /// 不带超时地等待下一帧，供交互式终端使用。
    ///
    /// 终端会话天然是长连接：管理员可能几分钟不敲键盘，也可能连续刷屏几十秒。
    /// 用 [`Request::next_frame`] 的超时会在这两种情况下都误判失败，因此这里
    /// 一直等到帧到达或连接断开为止。
    pub async fn recv(&mut self) -> Result<ClientFrame, String> {
        let results = self
            .results
            .as_mut()
            .ok_or_else(|| "该请求已经结束".to_string())?;

        match results.recv().await {
            Some(frame) => {
                if is_terminal(&frame) {
                    self.finished = true;
                    self.hub.cancel_pending(self.req_id);
                }
                Ok(frame)
            }
            None => {
                self.finished = true;
                Err("连接在等待响应期间断开".to_string())
            }
        }
    }
}

/// 该帧是否意味着一个请求已经彻底结束。
fn is_terminal(frame: &ClientFrame) -> bool {
    matches!(
        frame,
        ClientFrame::Result { .. }
            | ClientFrame::FileEnd { .. }
            | ClientFrame::Error { .. }
            | ClientFrame::PtyExit { .. }
            | ClientFrame::DirListing { .. }
    )
}

impl Drop for Request {
    fn drop(&mut self) {
        if !self.finished {
            self.hub.cancel_pending(self.req_id);
        }
    }
}

/// 所有设备连接的注册中心，克隆成本极低（内部全是 Arc）。
#[derive(Clone)]
pub struct Hub {
    inner: Arc<Inner>,
}

struct Inner {
    /// UUID -> 活动连接。
    connections: Mutex<HashMap<String, Connection>>,
    /// UUID -> 设备元数据（含已离线设备）。
    meta: Mutex<HashMap<String, DeviceInfo>>,
    /// 已离线设备的 UUID，按离线顺序保留最近若干条。
    offline: Mutex<Vec<String>>,
    /// 挂起中的请求：req_id -> 结果投递通道。
    pending: Mutex<HashMap<u64, mpsc::Sender<ClientFrame>>>,
    next_req_id: AtomicU64,
    next_conn_id: AtomicU64,
}

impl Hub {
    pub fn new() -> Self {
        Self {
            inner: Arc::new(Inner {
                connections: Mutex::new(HashMap::new()),
                meta: Mutex::new(HashMap::new()),
                offline: Mutex::new(Vec::new()),
                pending: Mutex::new(HashMap::new()),
                next_req_id: AtomicU64::new(1),
                next_conn_id: AtomicU64::new(1),
            }),
        }
    }

    /// 当前所有设备的快照：在线设备在前，其余按主机名排序。
    ///
    /// 刻意不同时持有 `connections` 与 `meta` 两把锁：分两步取，
    /// 避免以后有人在别的路径上反序加锁而引出死锁。
    pub fn devices(&self) -> Vec<DeviceInfo> {
        let online: std::collections::HashSet<String> = self
            .connections()
            .lock()
            .expect("connections 锁被毒化")
            .keys()
            .cloned()
            .collect();

        let mut devices: Vec<DeviceInfo> = self
            .inner
            .meta
            .lock()
            .expect("meta 锁被毒化")
            .values()
            .map(|info| {
                let mut info = info.clone();
                info.online = online.contains(&info.uuid);
                info
            })
            .collect();

        devices.sort_by(|a, b| match (a.online, b.online) {
            (true, false) => std::cmp::Ordering::Less,
            (false, true) => std::cmp::Ordering::Greater,
            _ => a
                .hostname
                .cmp(&b.hostname)
                .then_with(|| a.uuid.cmp(&b.uuid)),
        });
        devices
    }

    /// 指定 UUID 是否在线。
    pub fn is_online(&self, uuid: &str) -> bool {
        self.connections()
            .lock()
            .expect("connections 锁被毒化")
            .contains_key(uuid)
    }

    /// 登记一个新请求：分配 req_id，并在发送任何帧之前挂好结果通道。
    pub fn reserve_request(&self, uuid: &str) -> Result<Request, String> {
        let sender = self
            .connections()
            .lock()
            .expect("connections 锁被毒化")
            .get(uuid)
            .map(|connection| connection.sender.clone())
            .ok_or_else(|| format!("设备 {uuid} 当前不在线"))?;

        let req_id = self.inner.next_req_id.fetch_add(1, Ordering::Relaxed);
        let (tx, rx) = mpsc::channel(REQUEST_QUEUE);
        self.inner
            .pending
            .lock()
            .expect("pending 锁被毒化")
            .insert(req_id, tx);

        Ok(Request {
            hub: self.clone(),
            req_id,
            sender,
            results: Some(rx),
            finished: false,
        })
    }

    /// 单帧请求的便捷封装：登记 → 发送 → 等待结果。
    pub async fn request(
        &self,
        uuid: &str,
        build: impl FnOnce(u64) -> String,
        timeout: Duration,
    ) -> Result<ClientFrame, String> {
        let request = self.reserve_request(uuid)?;
        request.send(build(request.req_id)).await?;
        request.finish(timeout).await
    }

    /// 摘掉一个仍在等待的登记（超时、写失败时调用）。
    fn cancel_pending(&self, req_id: u64) {
        self.inner
            .pending
            .lock()
            .expect("pending 锁被毒化")
            .remove(&req_id);
    }

    /// 把客户端回传的帧投递给等待者；返回 false 表示没人在等这一号请求。
    ///
    /// 一个请求可能收到多帧（下载文件会连发 `FILE_CHUNK`），因此这里**不能**
    /// 收到第一帧就把登记摘掉，只在终止帧（`Result` / `FileEnd` / `Error`）时摘除。
    ///
    /// 投递用的是有界通道：如果等待方消费不过来（例如浏览器下载慢），
    /// 这里会 await 到有空间为止，从而把背压一路传回客户端，
    /// 而不是把帧丢掉或让内存无界增长。
    async fn deliver(&self, req_id: u64, frame: ClientFrame) -> bool {
        let terminal = is_terminal(&frame);
        // 锁的作用域必须收窄：await 期间不能持有 std Mutex 的 guard。
        let sender = {
            let mut pending = self.inner.pending.lock().expect("pending 锁被毒化");
            if terminal {
                pending.remove(&req_id)
            } else {
                pending.get(&req_id).cloned()
            }
        };
        match sender {
            Some(sender) => {
                let delivered = sender.send(frame).await.is_ok();
                if !delivered {
                    self.cancel_pending(req_id);
                }
                delivered
            }
            None => false,
        }
    }

    fn connections(&self) -> &Mutex<HashMap<String, Connection>> {
        &self.inner.connections
    }

    /// REGISTER 到达：登记连接与设备元数据。
    fn attach(
        &self,
        uuid: &str,
        hostname: &str,
        os: &str,
        peer: SocketAddr,
        sender: mpsc::Sender<String>,
        reader: tokio::task::AbortHandle,
    ) -> u64 {
        let conn_id = self.inner.next_conn_id.fetch_add(1, Ordering::Relaxed);

        {
            let mut meta = self.inner.meta.lock().expect("meta 锁被毒化");
            meta.insert(
                uuid.to_string(),
                DeviceInfo {
                    uuid: uuid.to_string(),
                    hostname: hostname.to_string(),
                    ip: peer.ip().to_string(),
                    os: os.to_string(),
                    last_seen_ms: now_ms(),
                    online: true,
                },
            );
        }

        let previous = self
            .connections()
            .lock()
            .expect("connections 锁被毒化")
            .insert(
                uuid.to_string(),
                Connection {
                    conn_id,
                    sender,
                    reader,
                },
            );

        // 同一 UUID 重连：踢掉旧连接的 read_loop。
        if let Some(previous) = previous {
            warn!(uuid, "同一设备重复注册，断开旧连接");
            previous.reader.abort();
        }

        self.inner
            .offline
            .lock()
            .expect("offline 锁被毒化")
            .retain(|offline_uuid| offline_uuid != uuid);

        info!(uuid, hostname, os, %peer, "设备已上线");
        conn_id
    }

    /// 连接结束：仅当登记在表里的仍是这条连接时才摘除，避免误删重连后的新连接。
    fn detach(&self, uuid: &str, conn_id: u64) {
        let removed = {
            let mut connections = self.connections().lock().expect("connections 锁被毒化");
            match connections.get(uuid) {
                Some(connection) if connection.conn_id == conn_id => {
                    connections.remove(uuid);
                    true
                }
                // 已被新连接顶替，或早已摘除。
                _ => false,
            }
        };
        if !removed {
            return;
        }

        if let Some(info) = self.inner.meta.lock().expect("meta 锁被毒化").get_mut(uuid) {
            info.online = false;
            info.last_seen_ms = now_ms();
        }
        let mut offline = self.inner.offline.lock().expect("offline 锁被毒化");
        offline.retain(|offline_uuid| offline_uuid != uuid);
        offline.push(uuid.to_string());

        // 只保留最近 MAX_OFFLINE_HISTORY 台离线设备的历史。
        while offline.len() > MAX_OFFLINE_HISTORY {
            if let Some(stale) = offline.first().cloned() {
                offline.remove(0);
                self.inner
                    .meta
                    .lock()
                    .expect("meta 锁被毒化")
                    .remove(&stale);
            }
        }
        info!(uuid, "设备已离线");
    }

    /// 收到 PING / PONG / 任何结果帧时刷新最后活跃时间。
    fn touch(&self, uuid: &str) {
        let mut meta = self.inner.meta.lock().expect("meta 锁被毒化");
        if let Some(info) = meta.get_mut(uuid) {
            info.last_seen_ms = now_ms();
        }
    }
}

impl Default for Hub {
    fn default() -> Self {
        Self::new()
    }
}

/// 一条反向连接从建立到结束的完整生命周期。
pub async fn serve_connection(hub: Hub, stream: TcpStream, peer: SocketAddr) {
    let (read_half, write_half) = stream.into_split();
    let (tx, rx) = mpsc::channel::<String>(WRITE_QUEUE);

    info!(%peer, "客户端已接入，等待 REGISTER");

    let writer = tokio::spawn(write_loop(write_half, rx, peer));
    let reader = tokio::spawn(read_loop(
        hub.clone(),
        read_half,
        peer,
        tx,
        writer.abort_handle(),
    ));

    // read_loop 结束时会把 write_loop 一并中止。
    if let Err(err) = reader.await {
        debug!(%peer, "read_loop 任务结束: {err}");
    }
}

async fn write_loop(
    mut write_half: OwnedWriteHalf,
    mut rx: mpsc::Receiver<String>,
    peer: SocketAddr,
) {
    while let Some(frame) = rx.recv().await {
        if let Err(err) = write_half.write_all(frame.as_bytes()).await {
            debug!(%peer, "写出失败: {err}");
            break;
        }
        // 命令回显要尽快到达，因此每帧都 flush。
        if let Err(err) = write_half.flush().await {
            debug!(%peer, "flush 失败: {err}");
            break;
        }
    }
    let _ = write_half.shutdown().await;
}

async fn read_loop(
    hub: Hub,
    mut read_half: OwnedReadHalf,
    peer: SocketAddr,
    tx: mpsc::Sender<String>,
    writer: tokio::task::AbortHandle,
) {
    let mut pending: Vec<u8> = Vec::new();
    let mut chunk = vec![0u8; READ_CHUNK];
    let mut registered: Option<(String, u64)> = None;

    loop {
        // 一次 syscall 尽量多取几帧。
        let read = match read_half.read(&mut chunk).await {
            Ok(0) => break, // 对端关闭
            Ok(read) => read,
            Err(err) => {
                debug!(%peer, "读取失败: {err}");
                break;
            }
        };
        pending.extend_from_slice(&chunk[..read]);

        // 按换行切帧；最后一段不完整的数据留在 pending 里等下一轮。
        let mut start = 0usize;
        while let Some(offset) = pending[start..].iter().position(|byte| *byte == b'\n') {
            let end = start + offset;
            let frame = &pending[start..end];
            start = end + 1;
            // 帧尾可能带一个 '\r'（某些客户端会写 CRLF）。
            let frame = frame.strip_suffix(b"\r").unwrap_or(frame);
            if frame.is_empty() {
                continue;
            }
            handle_frame(&hub, &tx, &mut registered, frame, peer, &writer).await;
        }
        pending.drain(..start);

        // 只在不完整帧超限时才断开，避免对端一直不发换行把内存吃满。
        if pending.len() > MAX_FRAME_BYTES {
            warn!(
                %peer,
                "单帧累积 {} 字节仍无换行，超过上限 {}，断开连接",
                pending.len(),
                MAX_FRAME_BYTES
            );
            break;
        }
    }

    if let Some((uuid, conn_id)) = registered {
        hub.detach(&uuid, conn_id);
    }
    writer.abort();
    debug!(%peer, "连接已关闭");
}

/// 处理一帧。
async fn handle_frame(
    hub: &Hub,
    tx: &mpsc::Sender<String>,
    registered: &mut Option<(String, u64)>,
    frame: &[u8],
    peer: SocketAddr,
    writer: &tokio::task::AbortHandle,
) {
    let text = match std::str::from_utf8(frame) {
        Ok(text) => text,
        Err(err) => {
            warn!(%peer, "收到非 UTF-8 帧: {err}");
            return;
        }
    };

    // 心跳不要求已注册，立即回 PONG。
    if text == "PING" {
        let _ = tx.send("PONG\n".to_string()).await;
        if let Some((uuid, _)) = registered.as_ref() {
            hub.touch(uuid);
        }
        return;
    }

    match protocol::parse_client_frame(text) {
        Ok(ClientFrame::Pong) => {
            if let Some((uuid, _)) = registered.as_ref() {
                hub.touch(uuid);
            }
        }
        Ok(ClientFrame::Register { uuid, hostname, os }) => {
            if registered.is_some() {
                warn!(%peer, "重复 REGISTER，忽略");
                return;
            }
            let conn_id = hub.attach(&uuid, &hostname, &os, peer, tx.clone(), writer.clone());
            *registered = Some((uuid, conn_id));
        }
        Ok(frame) => {
            let req_id = frame_req_id(&frame);
            if !hub.deliver(req_id, frame).await {
                debug!(%peer, req_id, "没有等待中的请求，丢弃结果帧");
            }
            if let Some((uuid, _)) = registered.as_ref() {
                hub.touch(uuid);
            }
        }
        Err(protocol::ProtocolError::Unexpected(tag)) => {
            debug!(%peer, "未知帧 {tag:?}，忽略");
        }
        Err(err) => {
            warn!(%peer, "解析失败，忽略该帧: {err}");
        }
    }
}

fn frame_req_id(frame: &ClientFrame) -> u64 {
    match frame {
        ClientFrame::Result { req_id, .. }
        | ClientFrame::FileBegin { req_id, .. }
        | ClientFrame::FileChunk { req_id, .. }
        | ClientFrame::FileEnd { req_id, .. }
        | ClientFrame::Error { req_id, .. }
        | ClientFrame::PtyOut { req_id, .. }
        | ClientFrame::PtyExit { req_id, .. }
        | ClientFrame::DirListing { req_id, .. } => *req_id,
        ClientFrame::Register { .. } | ClientFrame::Pong => 0,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn device(uuid: &str, hostname: &str, online: bool) -> DeviceInfo {
        DeviceInfo {
            uuid: uuid.to_string(),
            hostname: hostname.to_string(),
            ip: "10.0.0.1".to_string(),
            os: "linux".to_string(),
            last_seen_ms: 0,
            online,
        }
    }

    /// 用一条假的 mpsc + 一个可中止的空任务冒充活动连接，
    /// 以便在不建真 TCP 连接的前提下测试注册表逻辑。
    fn fake_register(hub: &Hub, uuid: &str, hostname: &str) -> mpsc::Receiver<String> {
        let (sender, receiver) = mpsc::channel::<String>(8);
        let peer: SocketAddr = "10.0.0.9:40000".parse().unwrap();
        test_runtime().block_on(async {
            let task = tokio::spawn(std::future::pending::<()>());
            hub.attach(uuid, hostname, "linux", peer, sender, task.abort_handle());
        });
        receiver
    }

    /// 需要计时器的单线程运行时（`Request` 的等待路径会用到 timer）。
    fn test_runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_current_thread()
            .enable_time()
            .build()
            .unwrap()
    }

    #[test]
    fn devices_list_merges_meta_and_connection_state() {
        let hub = Hub::new();
        {
            let mut meta = hub.inner.meta.lock().unwrap();
            meta.insert("b".into(), device("b", "beta", true));
            meta.insert("a".into(), device("a", "alpha", true));
        }
        // 没有任何活动连接，两台都应报告为离线。
        let devices = hub.devices();
        assert_eq!(devices.len(), 2);
        assert!(devices.iter().all(|device| !device.online));
        assert_eq!(devices[0].hostname, "alpha");
    }

    #[test]
    fn reserve_request_fails_when_offline() {
        let hub = Hub::new();
        assert!(hub.reserve_request("missing").is_err());
        assert!(!hub.is_online("missing"));
    }

    #[test]
    fn attach_marks_online_and_detach_marks_offline() {
        let hub = Hub::new();
        // 先手工塞一条元数据，模拟「曾经见过」的设备。
        hub.inner
            .meta
            .lock()
            .unwrap()
            .insert("vm-1".into(), device("vm-1", "vm-1", true));

        let _receiver = fake_register(&hub, "vm-1", "vm-1");
        assert!(hub.is_online("vm-1"));
        assert!(hub.devices()[0].online);

        let conn_id = {
            let connections = hub.connections().lock().unwrap();
            connections.get("vm-1").unwrap().conn_id
        };
        hub.detach("vm-1", conn_id);
        assert!(!hub.is_online("vm-1"));
        assert!(!hub.devices()[0].online);
    }

    #[test]
    fn detach_ignores_stale_connection_after_reconnect() {
        let hub = Hub::new();
        hub.inner
            .meta
            .lock()
            .unwrap()
            .insert("vm-1".into(), device("vm-1", "vm-1", true));

        let _first = fake_register(&hub, "vm-1", "vm-1");
        let first_conn = hub
            .connections()
            .lock()
            .unwrap()
            .get("vm-1")
            .unwrap()
            .conn_id;
        // 重连：新连接顶替旧的。
        let _second = fake_register(&hub, "vm-1", "vm-1");
        let second_conn = hub
            .connections()
            .lock()
            .unwrap()
            .get("vm-1")
            .unwrap()
            .conn_id;
        assert_ne!(first_conn, second_conn);

        // 旧连接的 read_loop 迟到的收尾不应该把新连接摘掉。
        hub.detach("vm-1", first_conn);
        assert!(hub.is_online("vm-1"), "旧连接的 detach 不应影响新连接");

        hub.detach("vm-1", second_conn);
        assert!(!hub.is_online("vm-1"));
    }

    #[test]
    fn request_send_and_deliver_roundtrip() {
        let hub = Hub::new();
        hub.inner
            .meta
            .lock()
            .unwrap()
            .insert("vm-1".into(), device("vm-1", "vm-1", true));
        let mut receiver = fake_register(&hub, "vm-1", "vm-1");

        let request = hub.reserve_request("vm-1").unwrap();
        let req_id = request.req_id;
        test_runtime().block_on(async {
            request.send(format!("CMD:{req_id}:AAAA\n")).await.unwrap();
            // 客户端方向收到的正是刚刚发出的帧。
            assert_eq!(
                receiver.recv().await.unwrap(),
                format!("CMD:{req_id}:AAAA\n")
            );

            // 模拟客户端回包，再等 finish。
            assert!(
                hub.deliver(
                    req_id,
                    ClientFrame::Result {
                        req_id,
                        output: "ok\n".to_string()
                    }
                )
                .await
            );
            let frame = request.finish(Duration::from_secs(1)).await.unwrap();
            assert_eq!(
                frame,
                ClientFrame::Result {
                    req_id,
                    output: "ok\n".to_string()
                }
            );
        });
    }

    #[test]
    fn dropping_unfinished_request_clears_pending_entry() {
        let hub = Hub::new();
        hub.inner
            .meta
            .lock()
            .unwrap()
            .insert("vm-1".into(), device("vm-1", "vm-1", true));
        let _receiver = fake_register(&hub, "vm-1", "vm-1");

        let request = hub.reserve_request("vm-1").unwrap();
        let req_id = request.req_id;
        assert!(hub.inner.pending.lock().unwrap().contains_key(&req_id));
        drop(request);
        assert!(
            !hub.inner.pending.lock().unwrap().contains_key(&req_id),
            "未完成的请求应把自己的登记摘掉，否则挂起表会无界增长"
        );
    }

    #[test]
    fn pty_output_is_not_terminal_but_exit_is() {
        // 终端会话会连续回传多帧 PTY_OUT，登记必须一直保留到 PTY_EXIT，
        // 否则交互式终端会在第一屏输出后就「丢掉」后续内容。
        assert!(!is_terminal(&ClientFrame::PtyOut {
            req_id: 1,
            data: vec![b'x']
        }));
        assert!(is_terminal(&ClientFrame::PtyExit { req_id: 1, code: 0 }));
        assert!(is_terminal(&ClientFrame::DirListing {
            req_id: 1,
            listing: "PATH\t/tmp".to_string()
        }));
    }

    #[test]
    fn delivers_multiple_pty_frames_to_one_request() {
        let hub = Hub::new();
        hub.inner
            .meta
            .lock()
            .unwrap()
            .insert("vm-1".into(), device("vm-1", "vm-1", true));
        let _receiver = fake_register(&hub, "vm-1", "vm-1");

        let mut request = hub.reserve_request("vm-1").unwrap();
        let req_id = request.req_id;
        test_runtime().block_on(async {
            for chunk in ["first", "second"] {
                assert!(
                    hub.deliver(
                        req_id,
                        ClientFrame::PtyOut {
                            req_id,
                            data: chunk.as_bytes().to_vec()
                        }
                    )
                    .await
                );
            }
            assert!(
                hub.deliver(req_id, ClientFrame::PtyExit { req_id, code: 0 })
                    .await
            );

            let mut seen = Vec::new();
            loop {
                match request.next_frame(Duration::from_secs(1)).await.unwrap() {
                    ClientFrame::PtyOut { data, .. } => {
                        seen.push(String::from_utf8(data).unwrap())
                    }
                    ClientFrame::PtyExit { .. } => break,
                    other => panic!("unexpected: {other:?}"),
                }
            }
            assert_eq!(seen, vec!["first".to_string(), "second".to_string()]);
        });
    }

    #[test]
    fn offline_history_is_bounded() {
        let hub = Hub::new();
        for index in 0..(MAX_OFFLINE_HISTORY + 10) {
            let uuid = format!("vm-{index}");
            hub.inner
                .meta
                .lock()
                .unwrap()
                .insert(uuid.clone(), device(&uuid, &uuid, true));
            let _receiver = fake_register(&hub, &uuid, &uuid);
            let conn_id = hub
                .connections()
                .lock()
                .unwrap()
                .get(&uuid)
                .unwrap()
                .conn_id;
            hub.detach(&uuid, conn_id);
        }
        assert!(hub.inner.offline.lock().unwrap().len() <= MAX_OFFLINE_HISTORY);
        // 最早上线的设备已被淘汰出元数据表。
        assert!(!hub.inner.meta.lock().unwrap().contains_key("vm-0"));
    }

    /// 走真实 TCP 的读循环测试：注册、多帧粘包、CRLF、PING/PONG。
    #[tokio::test]
    async fn read_loop_frames_coalesced_crlf_and_heartbeat() {
        use tokio::net::{TcpListener, TcpStream};

        let hub = Hub::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let serving = hub.clone();
        let server = tokio::spawn(async move {
            let (stream, peer) = listener.accept().await.unwrap();
            serve_connection(serving, stream, peer).await;
        });

        let mut client = TcpStream::connect(addr).await.unwrap();

        // 故意把 REGISTER 拆成两次写入，再紧接着塞入 PING 与 PONG：
        // 服务端必须在一次读取里正确切出这几帧，而不是要求「一帧一包」。
        let register = format!(
            "REGISTER:vm-9:{}:linux\r\n",
            protocol::encode_text("测试机")
        );
        let (first, rest) = register.split_at(12);
        client.write_all(first.as_bytes()).await.unwrap();
        client.flush().await.unwrap();
        // 给服务端一次「只有半个帧」的机会：此时不应误判为损坏帧。
        tokio::time::sleep(Duration::from_millis(50)).await;
        client.write_all(rest.as_bytes()).await.unwrap();
        client.write_all(b"\nPING\nPONG\n").await.unwrap();
        client.flush().await.unwrap();

        // 等设备上线。
        for _ in 0..50 {
            if hub.is_online("vm-9") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(hub.is_online("vm-9"), "拆包写入的 REGISTER 应被正确重组");

        let devices = hub.devices();
        let device = devices.iter().find(|d| d.uuid == "vm-9").unwrap();
        assert_eq!(device.hostname, "测试机", "主机名应经 Base64 正确解码");
        assert_eq!(device.os, "linux");

        // 服务端应回 PONG。
        let mut buffer = [0u8; 64];
        let mut collected = Vec::new();
        let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
        while tokio::time::Instant::now() < deadline {
            match tokio::time::timeout(Duration::from_millis(200), client.read(&mut buffer)).await {
                Ok(Ok(0)) => break,
                Ok(Ok(read)) => {
                    collected.extend_from_slice(&buffer[..read]);
                    if collected.windows(5).any(|window| window == b"PONG\n") {
                        break;
                    }
                }
                Ok(Err(err)) => panic!("客户端读取失败: {err}"),
                Err(_) => {}
            }
        }
        assert!(
            collected.windows(5).any(|window| window == b"PONG\n"),
            "PING 应收到 PONG 回应，实际收到: {:?}",
            String::from_utf8_lossy(&collected)
        );

        // 客户端断开后应被标记离线并转入历史。
        drop(client);
        for _ in 0..50 {
            if !hub.is_online("vm-9") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(!hub.is_online("vm-9"), "断开连接后设备应离线");

        server.abort();
    }

    /// 同 UUID 重连时，旧连接不应把新连接顶掉。
    #[tokio::test]
    async fn reconnect_replaces_old_connection() {
        use tokio::net::{TcpListener, TcpStream};

        let hub = Hub::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let serving = hub.clone();
        tokio::spawn(async move {
            loop {
                let (stream, peer) = match listener.accept().await {
                    Ok(pair) => pair,
                    Err(_) => break,
                };
                let hub = serving.clone();
                tokio::spawn(async move { serve_connection(hub, stream, peer).await });
            }
        });

        let register = format!("REGISTER:vm-7:{}:linux\n", protocol::encode_text("vm-7"));

        let mut first = TcpStream::connect(addr).await.unwrap();
        first.write_all(register.as_bytes()).await.unwrap();
        first.flush().await.unwrap();
        for _ in 0..50 {
            if hub.is_online("vm-7") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(hub.is_online("vm-7"));

        // 第二次连接用同一个 UUID：应顶替第一条。
        let mut second = TcpStream::connect(addr).await.unwrap();
        second.write_all(register.as_bytes()).await.unwrap();
        second.flush().await.unwrap();
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(hub.is_online("vm-7"), "重连后设备应仍在线");

        // 现在把「新连接」断开，设备应当离线——说明表里存的确实是第二条连接。
        drop(second);
        for _ in 0..50 {
            if !hub.is_online("vm-7") {
                break;
            }
            tokio::time::sleep(Duration::from_millis(20)).await;
        }
        assert!(
            !hub.is_online("vm-7"),
            "断开新连接后设备应离线；若仍在线说明旧连接抢占了登记"
        );

        drop(first);
    }
}
