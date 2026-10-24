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
//! 实现了它：`accept()` 内部完成嗅探，属于 Web 的连接原样返回给 axum，
//! 属于客户端的连接则转交给 [`Hub`] 处理，然后继续等下一个连接。
//! 因此 Web 侧完全不知道还有另一半流量的存在。

use std::io;
use std::net::SocketAddr;
use std::pin::Pin;
use std::task::{Context, Poll};

use axum::serve::Listener;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, ReadBuf};
use tokio::net::{TcpListener, TcpStream};
use tracing::{debug, warn};

use crate::hub::{self, Hub};

/// 需要读多少字节才能判定协议。
///
/// 最长的方法名是 `OPTIONS`（7 字节），加上末尾空格是 8；而 `REGISTER` 是 8 字节。
/// 取 8 就足够区分，同时几乎不会让对端多等（HTTP 请求行一定一次发全）。
const SNIFF_BYTES: usize = 8;

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
    listener: TcpListener,
    hub: Hub,
}

impl SniffingListener {
    pub fn new(listener: TcpListener, hub: Hub) -> Self {
        Self { listener, hub }
    }

    /// 读一小段前缀判定协议，并把这几个字节原样带回。
    ///
    /// 读不出 8 字节（对端连上就断开、或只发了半个方法名就停）时，
    /// 用「已经读到的部分」判定；一个字节都没有就直接判为 HTTP，
    /// 交给 axum 立刻以 400 收尾。
    async fn sniff(stream: &mut TcpStream) -> io::Result<Vec<u8>> {
        let mut prefix = vec![0u8; SNIFF_BYTES];
        let mut filled = 0usize;
        while filled < SNIFF_BYTES {
            match stream.read(&mut prefix[filled..]).await {
                Ok(0) => break,
                Ok(read) => filled += read,
                Err(err) => return Err(err),
            }
        }
        prefix.truncate(filled);
        Ok(prefix)
    }
}

impl Listener for SniffingListener {
    type Io = PrefixedStream;
    type Addr = SocketAddr;

    async fn accept(&mut self) -> (Self::Io, Self::Addr) {
        loop {
            let (mut stream, peer) = match self.listener.accept().await {
                Ok(pair) => pair,
                Err(err) => {
                    // 单个 accept 失败（瞬时 fd 耗尽等）不该终止整个监听循环。
                    warn!("accept 失败: {err}");
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    continue;
                }
            };
            // 命令与心跳都是小帧，攒包只会增加延迟。
            let _ = stream.set_nodelay(true);

            let prefix = match Self::sniff(&mut stream).await {
                Ok(prefix) => prefix,
                Err(err) => {
                    debug!(%peer, "读取连接前缀失败: {err}");
                    continue;
                }
            };

            match classify(&prefix) {
                Kind::Client => {
                    let hub = self.hub.clone();
                    tokio::spawn(async move {
                        // 把嗅探读掉的字节接回去，Hub 才能看到完整的 REGISTER。
                        let stream = PrefixedStream::new(prefix, stream);
                        hub::serve_connection(hub, stream, peer).await;
                    });
                }
                Kind::Http => return (PrefixedStream::new(prefix, stream), peer),
            }
        }
    }

    fn local_addr(&self) -> io::Result<Self::Addr> {
        self.listener.local_addr()
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
}
