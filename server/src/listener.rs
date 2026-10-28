//! 单端口复用：同一个端口同时接 Web 请求与虚拟机客户端的反向连接。
//!
//! 部署时最省事的形态是只暴露一个端口 —— 内网策略常常只放行一个端口，
//! 让管理员多开一个 `TCP_PORT` 反而成了负担。这里用**首帧嗅探**做到这点：
//!
//! - 虚拟机客户端的第一帧一定是 `REGISTER:<UUID>:<B64(主机名)>:<OS>`；
//! - 浏览器/HTTP 客户端的第一帧一定是 `GET ` / `POST ` / `PUT ` 之类的方法行。
//!
//! 两者在开头几个字节上绝不重叠，因此读一小段前缀就能判定该交给谁，
//! 判定后再把「已经读掉的那几个字节 + 剩余流」一起交出去，双方都察觉不到。
//!
//! ## 为什么要嗅探而不是按端口分
//!
//! 备选方案是「同一个端口上先按 TLS/配合 PROXY protocol 区分」，但客户端是我们
//! 自己写的，让它发一句 HTTP 请求纯属自找麻烦；而 `REGISTER` 帧本身就带冒号、
//! 与 HTTP 方法行的形态差别极大，嗅探几字节就够。
//!
//! ## 与 axum 的衔接
//!
//! [`axum::serve`] 接受任意 [`axum::serve::Listener`]。这里的 [`SniffingListener`]
//! 实现了它：属于 Web 的连接原样返回给 axum，属于客户端的连接则转交给 [`Hub`] 处理。
//!
//! ## 嗅探必须在 `accept()` 之外做
//!
//! 嗅探要等对端把首帧发过来，而有的连接**连上之后根本不发**：浏览器的预连接
//! （preconnect）、刷新页面时被取消的请求都是这样。如果把嗅探放在 `accept()` 里
//! 串行等待，一条这样的连接就会让 [`axum::serve`] 再也 accept 不到新连接 ——
//! 已经建好的连接照常收发（设备心跳、旧的 WebSocket 推送都还在），
//! 但新连接全部堵在内核 backlog 里，表现就是「刷新页面后什么也连不上」。
//!
//! 因此这里把 `accept` 循环放进后台任务：它只管收连接，每条连接再各自 spawn
//! 一个嗅探任务；判定为 Web 的连接通过通道交给 axum。这样一条不发数据的连接
//! 最多只占住它自己那个任务。

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};
use std::time::Duration;

use axum::serve::Listener;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tokio::sync::mpsc;
use tokio::task::JoinHandle;
use tracing::{debug, error, warn};

use crate::hub::{self, Hub};

/// 需要读多少字节才能判定协议。
///
/// 最长的方法名是 `OPTIONS`（7 字节），加上末尾空格是 8；而 `REGISTER` 是 8 字节。
/// 取 8 就足够区分，同时几乎不会让对端多等（HTTP 请求行一定一次发全）。
const SNIFF_BYTES: usize = 8;

/// 等首帧的上限。超时后按 HTTP 处理，交给 axum 当普通空闲连接对待。
///
/// 客户端连上后立刻发 `REGISTER`，正常远用不到这么久；这里只是给慢链路留余量。
const SNIFF_TIMEOUT: Duration = Duration::from_secs(10);

/// 已判定为 Web、等待 axum 取走的连接队列长度。
const HTTP_QUEUE: usize = 128;

/// 判定结果。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Kind {
    /// 看起来是 HTTP：交给 axum。
    Http,
    /// 看起来是客户端注册帧：交给 Hub。
    Client,
}

/// 依据已读到的前缀判定协议。
///
/// 判定不清时一律按 HTTP 处理：读到的字节会原样回灌给 axum，
/// 由一个正常的 400/404 结束连接，比误判成客户端、把连接挂进设备表要安全得多。
fn classify(prefix: &[u8]) -> Kind {
    // 客户端注册帧的固定开头。
    const REGISTER: &[u8] = b"REGISTER";
    if prefix.len() >= REGISTER.len() && prefix.starts_with(REGISTER) {
        return Kind::Client;
    }
    Kind::Http
}

/// 把「已读掉的前缀」与「剩余流」重新拼成一个可读的流。
///
/// 嗅探免不了先读几个字节，而下游（axum / Hub）需要看到完整的字节流，
/// 因此这里要透明地补回前缀。写方向直通。
pub struct PrefixedStream {
    prefix: Vec<u8>,
    /// 已经回灌出去多少字节。
    consumed: usize,
    inner: TcpStream,
}

impl PrefixedStream {
    fn new(prefix: Vec<u8>, inner: TcpStream) -> Self {
        Self {
            prefix,
            consumed: 0,
            inner,
        }
    }
}

impl AsyncRead for PrefixedStream {
    fn poll_read(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        // 先把前缀吐完，再交给真正的 socket。
        if self.consumed < self.prefix.len() {
            let rest = &self.prefix[self.consumed..];
            let take = rest.len().min(buf.remaining());
            buf.put_slice(&rest[..take]);
            self.consumed += take;
            return Poll::Ready(Ok(()));
        }
        Pin::new(&mut self.inner).poll_read(cx, buf)
    }
}

impl AsyncWrite for PrefixedStream {
    fn poll_write(
        mut self: Pin<&mut Self>,
        cx: &mut Context<'_>,
        buf: &[u8],
    ) -> Poll<io::Result<usize>> {
        Pin::new(&mut self.inner).poll_write(cx, buf)
    }

    fn poll_flush(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_flush(cx)
    }

    fn poll_shutdown(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<io::Result<()>> {
        Pin::new(&mut self.inner).poll_shutdown(cx)
    }
}

/// 嗅探式监听器：把同一个端口的连接分给 Web 与客户端两条路。
pub struct SniffingListener {
    /// 后台接收任务判定为 Web 的连接，按到达顺序交给 axum 取走。
    http: mpsc::Receiver<(PrefixedStream, SocketAddr)>,
    local_addr: Option<SocketAddr>,
    /// 后台接收任务。把它放在这里，是为了在监听器被丢弃时一并收掉。
    acceptor: JoinHandle<()>,
}

impl SniffingListener {
    pub fn new(listener: TcpListener, hub: Hub) -> Self {
        let local_addr = listener.local_addr().ok();
        // 有界队列：axum 一时消费不过来时，多出来的连接排在通道里等着；
        // 满员则让各自的嗅探任务稍等，不会无界占内存。
        let (http_tx, http_rx) = mpsc::channel(HTTP_QUEUE);
        let acceptor = tokio::spawn(accept_loop(listener, hub, http_tx));
        Self {
            http: http_rx,
            local_addr,
            acceptor,
        }
    }
}

impl Drop for SniffingListener {
    fn drop(&mut self) {
        // 否则 listener 被丢弃（例如测试里 abort 掉 axum::serve）之后，
        // 后台任务仍然占着端口。
        self.acceptor.abort();
    }
}

/// accept 循环：只负责收连接，判定协议的工作交给每条连接自己的任务。
async fn accept_loop(
    listener: TcpListener,
    hub: Hub,
    http: mpsc::Sender<(PrefixedStream, SocketAddr)>,
) {
    loop {
        let (stream, peer) = match listener.accept().await {
            Ok(pair) => pair,
            Err(err) => {
                // 单个 accept 失败（瞬时 fd 耗尽等）不该终止整个监听循环。
                warn!("accept 失败: {err}");
                tokio::time::sleep(Duration::from_millis(200)).await;
                continue;
            }
        };
        // 命令与心跳都是小帧，攒包只会增加延迟。
        let _ = stream.set_nodelay(true);

        // 关键：嗅探在独立任务里做。一条连上却不发数据的连接（浏览器的
        // preconnect、刷新时被取消的请求）只挡住它自己，堵不住别的连接。
        let hub = hub.clone();
        let http = http.clone();
        tokio::spawn(async move { route(stream, peer, hub, http).await });
    }
}

/// 嗅探一条连接，再按判定结果交给 Hub 或 axum。
async fn route(
    mut stream: TcpStream,
    peer: SocketAddr,
    hub: Hub,
    http: mpsc::Sender<(PrefixedStream, SocketAddr)>,
) {
    let prefix = match sniff(&mut stream, peer).await {
        Ok(prefix) => prefix,
        Err(err) => {
            debug!(%peer, "读取连接前缀失败: {err}");
            return;
        }
    };

    match classify(&prefix) {
        // 把嗅探读掉的字节接回去，Hub 才能看到完整的 REGISTER。
        Kind::Client => hub::serve_connection(hub, PrefixedStream::new(prefix, stream), peer).await,
        Kind::Http => {
            // 接收方已经走了（服务正在退出）时发送失败，连接就此丢弃即可。
            if http
                .send((PrefixedStream::new(prefix, stream), peer))
                .await
                .is_err()
            {
                debug!(%peer, "Web 连接接收方已退出，丢弃连接");
            }
        }
    }
}

/// 读一小段前缀判定协议，并把这几个字节原样带回。
///
/// 读不出 8 字节（对端连上就断开、或只发了半个方法名就停）时，
/// 用「已经读到的部分」判定；一个字节都没有就直接判为 HTTP，
/// 交给 axum 立刻以 400 收尾。
///
/// 等满 [`SNIFF_TIMEOUT`] 还没等到就按 HTTP 处理：这类连接几乎都是浏览器的
/// 预连接，本身不带请求，交给 axum 当空闲连接等就是了。
async fn sniff(stream: &mut TcpStream, peer: SocketAddr) -> io::Result<Vec<u8>> {
    let mut prefix = vec![0u8; SNIFF_BYTES];
    let mut filled = 0usize;

    // 超时的取消点只会在 read 的 await 上，而 read 是取消安全的：
    // 已经读出的字节都在 prefix 里，超时后仍可原样回灌。
    let read = tokio::time::timeout(SNIFF_TIMEOUT, async {
        while filled < SNIFF_BYTES {
            match stream.read(&mut prefix[filled..]).await {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(err) => return Err(err),
            }
        }
        Ok(())
    })
    .await;

    match read {
        Ok(Ok(())) => {}
        Ok(Err(err)) => return Err(err),
        Err(_) => {
            debug!(
                %peer,
                "连接建立后 {} 秒未发首帧，按 HTTP 处理",
                SNIFF_TIMEOUT.as_secs()
            );
        }
    }

    prefix.truncate(filled);
    Ok(prefix)
}

impl Listener for SniffingListener {
    type Io = PrefixedStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            match self.http.recv().await {
                Some(connection) => return connection,
                // 接收任务只会在监听器被丢弃时结束：与其 panic 掉整个
                // axum::serve，不如稍等再看一次。
                None => {
                    error!("连接接收任务意外结束，1 秒后重试");
                    tokio::time::sleep(Duration::from_secs(1)).await;
                }
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.local_addr
            .ok_or_else(|| io::Error::other("监听地址不可用"))
    }
}
#[cfg(test)]
mod tests {
    use super::*;
    use tokio::io::AsyncWriteExt;

    #[test]
    fn classifies_client_register_frames() {
        assert_eq!(classify(b"REGISTER"), Kind::Client);
        assert_eq!(classify(b"REGISTER:vm-1:AAAA:linux"), Kind::Client);
    }

    #[test]
    fn classifies_http_methods() {
        for prefix in [
            &b"GET / HT"[..],
            &b"POST /ap"[..],
            &b"PUT /x H"[..],
            &b"HEAD / H"[..],
            &b"OPTIONS "[..],
            &b"DELETE /"[..],
            &b"PATCH /x"[..],
        ] {
            assert_eq!(classify(prefix), Kind::Http, "前缀 {prefix:?} 应判为 HTTP");
        }
    }

    #[test]
    fn unknown_prefixes_fall_back_to_http() {
        // 判不出来时按 HTTP 处理：交给 axum 走正常错误路径，
        // 比误判成客户端、把连接挂进设备表安全得多。
        assert_eq!(classify(b""), Kind::Http);
        assert_eq!(classify(b"\x16\x03\x01\x00"), Kind::Http); // TLS 握手
        assert_eq!(classify(b"registe"), Kind::Http); // 只发了半个 REGISTER
    }

    /// 嗅探读掉的前缀必须原样回灌，否则下游会丢掉开头几个字节。
    #[tokio::test]
    async fn prefixed_stream_replays_prefix_before_socket() {
        use tokio::io::AsyncReadExt;
        use tokio::net::{TcpListener, TcpStream};

        // 真开一对 socket：一端当「已被嗅探过」的连接，另一端喂剩余字节。
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let mut client = TcpStream::connect(addr).await.unwrap();
        let (server_side, _) = listener.accept().await.unwrap();

        client.write_all(b":vm-1:AAAA:linux\n").await.unwrap();
        client.flush().await.unwrap();

        // 服务端侧把 "REGISTER" 当作已读掉的前缀：读出来必须是完整帧。
        let mut stream = PrefixedStream::new(b"REGISTER".to_vec(), server_side);
        let mut all = Vec::new();
        let mut chunk = [0u8; 64];
        while all.len() < b"REGISTER:vm-1:AAAA:linux\n".len() {
            let read = stream.read(&mut chunk).await.unwrap();
            assert!(read > 0, "连接不应提前结束");
            all.extend_from_slice(&chunk[..read]);
        }
        assert_eq!(
            String::from_utf8(all).unwrap(),
            "REGISTER:vm-1:AAAA:linux\n",
            "前缀应先于 socket 内容被读出"
        );
    }

    /// 端到端：同一个 listener 上，HTTP 连接与 REGISTER 连接各走各的路。
    #[tokio::test]
    async fn routes_http_and_client_connections_apart() {
        use axum::routing::get;
        use axum::Router;
        use tokio::io::AsyncReadExt;

        let hub = Hub::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = Router::new().route("/ping", get(|| async { "pong" }));
        let sniffing = SniffingListener::new(listener, hub.clone());
        let serving = tokio::spawn(async move {
            let _ = axum::serve(sniffing, app).await;
        });

        // 一条 HTTP 连接：应当拿到 pong。
        let mut http = TcpStream::connect(addr).await.unwrap();
        http.write_all(b"GET /ping HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();
        let mut response = String::new();
        http.read_to_string(&mut response).await.unwrap();
        assert!(response.contains("pong"), "HTTP 请求应被 axum 处理：{response:?}");

        // 一条客户端连接：应当被登记成设备，而不是被 axum 当成坏请求。
        let mut client = TcpStream::connect(addr).await.unwrap();
        let register = format!(
            "REGISTER:vm-sniff:{}:linux\n",
            crate::protocol::encode_text("嗅探机")
        );
        client.write_all(register.as_bytes()).await.unwrap();
        client.flush().await.unwrap();

        for _ in 0..50 {
            if hub.is_online("vm-sniff") {
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
        assert!(hub.is_online("vm-sniff"), "REGISTER 连接应被识别为客户端并上线");

        let device = hub
            .devices()
            .into_iter()
            .find(|device| device.uuid == "vm-sniff")
            .expect("设备应出现在列表里");
        assert_eq!(device.hostname, "嗅探机", "被嗅探吞掉的前缀必须回灌，主机名才完整");

        drop(client);
        serving.abort();
    }

    /// 一条连上但不发任何字节的连接不能把 accept 循环堵死。
    ///
    /// 浏览器的预连接就是这样：它只为「以后可能发的请求」占住一条 TCP 连接。
    /// 以前嗅探串在 `accept()` 里，这种连接会让整个端口再也接不到新连接 ——
    /// 表现就是刷新页面后终端一直停在「连接中…」。
    #[tokio::test]
    async fn idle_connection_does_not_block_others() {
        use axum::routing::get;
        use axum::Router;
        use tokio::io::AsyncReadExt;

        let hub = Hub::new();
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();

        let app = Router::new().route("/ping", get(|| async { "pong" }));
        let sniffing = SniffingListener::new(listener, hub);
        let serving = tokio::spawn(async move {
            let _ = axum::serve(sniffing, app).await;
        });

        // 只连不发，像浏览器的预连接那样一直挂着。
        let _idle = TcpStream::connect(addr).await.unwrap();
        // 给接收任务一点时间把它收进去并开始嗅探。
        tokio::time::sleep(Duration::from_millis(100)).await;

        let mut http = TcpStream::connect(addr).await.unwrap();
        http.write_all(b"GET /ping HTTP/1.1\r\nHost: x\r\nConnection: close\r\n\r\n")
            .await
            .unwrap();

        let mut response = String::new();
        tokio::time::timeout(Duration::from_secs(2), http.read_to_string(&mut response))
            .await
            .expect("空闲连接不应阻塞其它连接")
            .unwrap();
        assert!(response.contains("pong"), "HTTP 请求应正常被处理：{response:?}");

        drop(http);
        serving.abort();
    }
}
