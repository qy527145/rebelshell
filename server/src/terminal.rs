//! 交互式终端的 WebSocket 桥。
//!
//! 浏览器里的 xterm.js 与虚拟机上的 PTY 之间是一条双向字节流。走 WebSocket
//! 而不是 REST，是因为它天然双向，且不必为每次按键开一个请求：
//!
//! ```text
//!   浏览器 (xterm.js)  <-- /ws/terminal -->  管理端  <-- TCP 控制通道 -->  客户端 (ConPTY/pty)
//! ```
//!
//! ## 消息格式
//!
//! 一端的一次「写」就是另一端的一条 WebSocket 消息，刻意不做缓冲合并：
//!
//! 浏览器 → 管理端（文本 JSON）：
//! - `{"type":"open","cols":120,"rows":30,"shell":"/bin/bash"}`
//! - `{"type":"input","data":"<Base64>"}`　按键与粘贴
//! - `{"type":"resize","cols":120,"rows":30}`
//! - `{"type":"close"}`
//!
//! 管理端 → 浏览器：
//! - `{"type":"ready"}`　终端已在客户端侧就绪
//! - `{"type":"output","data":"<Base64>"}`　终端输出（ANSI 原样透传）
//! - `{"type":"exit","code":0}`
//! - `{"type":"error","message":"…"}`
//!
//! 输出用 Base64 而非明文 JSON 字符串：PTY 按字节切输出，出现半个 UTF-8 字符
//! 是常态，直接塞进 JSON 会编码失败或变成替换字符。

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;
use futures_util::stream::SplitSink;
use futures_util::{SinkExt, StreamExt};
use serde::{Deserialize, Serialize};
use tracing::{debug, info};

use crate::auth::authorize;
use crate::hub::{RequestReceiver, RequestSender};
use crate::protocol::ServerFrame;
use crate::state::AppState;

/// 终端 WebSocket 的空闲心跳间隔，避免中间代理回收连接。
const TICK: Duration = Duration::from_secs(25);

/// 浏览器发来的控制消息。
#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ClientMessage {
    /// 打开终端；`shell` 留空表示用客户端平台的默认 shell。
    Open {
        cols: u16,
        rows: u16,
        #[serde(default)]
        shell: String,
    },
    /// 键盘 / 粘贴输入（Base64）。
    Input { data: String },
    /// 窗口大小变化。
    Resize { cols: u16, rows: u16 },
    /// 主动关闭终端。
    Close,
}

/// 发给浏览器的消息。
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    /// 终端已在客户端侧启动。
    Ready,
    /// 终端输出（Base64）。
    Output { data: String },
    /// 终端结束。
    Exit { code: i32 },
    /// 打开失败或中途出错。
    Error { message: String },
}

impl ServerMessage {
    fn error(message: impl Into<String>) -> Self {
        Self::Error {
            message: message.into(),
        }
    }
}

/// 终端查询参数：`?token=<ADMIN_TOKEN>&uuid=<设备>`。
#[derive(Deserialize)]
pub struct TerminalQuery {
    pub token: Option<String>,
    pub uuid: Option<String>,
}

/// `GET /ws/terminal?token=&uuid=`
pub async fn ws_terminal(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TerminalQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Err(unauthorized) = authorize(&state, &headers, query.token.as_deref()) {
        return unauthorized.into_response();
    }
    // 设备在握手阶段就要确定：少了它整条连接都没有意义，早点拒绝更清楚。
    let Some(uuid) = query.uuid.filter(|uuid| !uuid.trim().is_empty()) else {
        return (
            axum::http::StatusCode::BAD_REQUEST,
            "缺少 uuid 查询参数",
        )
            .into_response();
    };
    upgrade.on_upgrade(move |socket| serve(state, uuid, socket))
}

async fn serve(state: AppState, uuid: String, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();

    // 第一条消息必须是 open：在它到达之前不知道该开多大的终端、用什么 shell。
    let (cols, rows, shell) = match first_open(&mut stream).await {
        Ok(open) => open,
        // 空串表示对方只是走开了（在 open 之前就断开），不必再回消息。
        Err(message) if message.is_empty() => return,
        Err(message) => {
            let _ = send(&mut sink, &ServerMessage::error(message)).await;
            return;
        }
    };

    if !state.hub.is_online(&uuid) {
        let _ = send(&mut sink, &ServerMessage::error(format!("设备 {uuid} 当前不在线"))).await;
        return;
    }

    let request = match state.hub.reserve_request(&uuid) {
        Ok(request) => request,
        Err(message) => {
            let _ = send(&mut sink, &ServerMessage::error(message)).await;
            return;
        }
    };
    let req_id = request.req_id;
    // 拆成两半：主循环要同时「发键盘输入」与「收终端输出」，
    // 挤在同一个 Request 上借用会冲突（见 hub::RequestSender 的文档）。
    let (sender, mut receiver) = request.split();

    info!(uuid, req_id, cols, rows, shell = %shell, "打开交互式终端");

    if let Err(message) = sender
        .send(
            ServerFrame::PtyOpen {
                req_id,
                cols,
                rows,
                shell,
            }
            .encode(),
        )
        .await
    {
        let _ = send(&mut sink, &ServerMessage::error(message)).await;
        return;
    }

    let reason = pump(&mut sink, &mut stream, &sender, &mut receiver).await;

    // 无论因何结束，都请客户端收掉 PTY（重复关闭是幂等的）。
    let _ = sender
        .send(ServerFrame::PtyClose { req_id }.encode())
        .await;

    if let Some(message) = reason {
        let _ = send(&mut sink, &ServerMessage::error(message)).await;
    }
    info!(uuid, req_id, "交互式终端已关闭");
}

/// 等 `open` 消息。返回的 Err 是要回给浏览器的一句话
/// （返回空串表示「对方只是走开了」，不必再回消息）。
async fn first_open(
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
) -> Result<(u16, u16, String), String> {
    loop {
        match stream.next().await {
            Some(Ok(Message::Text(text))) => match serde_json::from_str::<ClientMessage>(&text) {
                Ok(ClientMessage::Open { cols, rows, shell }) => {
                    return Ok((cols.max(2), rows.max(2), shell))
                }
                Ok(_) => return Err("第一条消息必须是 open".to_string()),
                Err(err) => return Err(format!("无法解析 open 消息: {err}")),
            },
            // 浏览器可能在 open 之前就关掉（用户快速切走标签页）。
            Some(Ok(Message::Close(_))) | None => return Err(String::new()),
            // ping/pong/二进制在 open 之前没有意义，忽略后继续等。
            Some(Ok(_)) => continue,
            Some(Err(err)) => return Err(format!("WebSocket 接收错误: {err}")),
        }
    }
}

/// 会话主循环：浏览器消息与客户端回帧二选一，直到任一侧结束。
///
/// 返回 `Some(message)` 表示要以「错误」的口吻告诉浏览器为什么结束；
/// `None` 表示正常结束（用户自己关的，或 shell 退出了）。
async fn pump(
    sink: &mut SplitSink<WebSocket, Message>,
    stream: &mut futures_util::stream::SplitStream<WebSocket>,
    sender: &RequestSender,
    receiver: &mut RequestReceiver,
) -> Option<String> {
    let mut ticker = tokio::time::interval(TICK);
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    // 第一拍会立刻触发，先吃掉它，否则刚连上就发一个心跳。
    ticker.tick().await;
    let mut ready = false;

    loop {
        tokio::select! {
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => return None,
                    Some(Ok(Message::Ping(payload))) => {
                        if sink.send(Message::Pong(payload)).await.is_err() {
                            return None;
                        }
                    }
                    Some(Ok(Message::Text(text))) => {
                        match serde_json::from_str::<ClientMessage>(&text) {
                            Ok(ClientMessage::Input { data }) => {
                                let bytes = match BASE64.decode(&data) {
                                    Ok(bytes) => bytes,
                                    Err(err) => return Some(format!("按键数据不是合法 Base64: {err}")),
                                };
                                if let Err(message) = sender
                                    .send(ServerFrame::PtyIn { req_id: receiver.req_id, data: bytes }.encode())
                                    .await
                                {
                                    return Some(message);
                                }
                            }
                            Ok(ClientMessage::Resize { cols, rows }) => {
                                // 尺寸交给客户端调整 PTY，被控程序会收到 SIGWINCH
                                // 并重新排版（`top` / `vi` 靠这个自适应）。
                                let cols = cols.max(2);
                                let rows = rows.max(2);
                                if let Err(message) = sender
                                    .send(ServerFrame::PtyResize { req_id: receiver.req_id, cols, rows }.encode())
                                    .await
                                {
                                    return Some(message);
                                }
                            }
                            Ok(ClientMessage::Close) => return None,
                            // 重复的 open 忽略掉，已经开过了。
                            Ok(ClientMessage::Open { .. }) => {}
                            Err(err) => return Some(format!("无法解析消息: {err}")),
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        debug!("终端 WebSocket 接收错误: {err}");
                        return None;
                    }
                }
            }
            frame = receiver.recv() => {
                match frame {
                    Ok(crate::protocol::ClientFrame::PtyReady { .. }) => {
                        ready = true;
                        if send(sink, &ServerMessage::Ready).await.is_err() {
                            return None;
                        }
                    }
                    Ok(crate::protocol::ClientFrame::PtyOut { data, .. }) => {
                        // 还没收到 ready 就先来了输出，说明客户端实现得比较急，
                        // 但内容本身是有效的：补一个 ready 再送输出，前端不会漏屏。
                        if !ready {
                            ready = true;
                            if send(sink, &ServerMessage::Ready).await.is_err() {
                                return None;
                            }
                        }
                        if send(sink, &ServerMessage::Output { data: BASE64.encode(&data) })
                            .await
                            .is_err()
                        {
                            return None;
                        }
                    }
                    Ok(crate::protocol::ClientFrame::PtyExit { code, .. }) => {
                        let _ = send(sink, &ServerMessage::Exit { code }).await;
                        return None;
                    }
                    Ok(crate::protocol::ClientFrame::Error { message, .. }) => {
                        return Some(message);
                    }
                    Ok(other) => {
                        debug!("终端请求收到非预期帧: {other:?}");
                    }
                    Err(message) => return Some(message),
                }
            }
            _ = ticker.tick() => {
                // WebSocket 层的 ping 不走 JSON，前端无需理会。
                if sink.send(Message::Ping(Vec::new().into())).await.is_err() {
                    return None;
                }
            }
        }
    }
}

async fn send(
    sink: &mut SplitSink<WebSocket, Message>,
    message: &ServerMessage,
) -> Result<(), ()> {
    let text = serde_json::to_string(message).map_err(|err| {
        debug!("序列化终端消息失败: {err}");
    })?;
    sink.send(Message::Text(text.into())).await.map_err(|err| {
        debug!("终端 WebSocket 发送失败: {err}");
    })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn open_message_parses_with_and_without_shell() {
        let with: ClientMessage =
            serde_json::from_str(r#"{"type":"open","cols":120,"rows":30,"shell":"/bin/bash"}"#)
                .unwrap();
        match with {
            ClientMessage::Open { cols, rows, shell } => {
                assert_eq!((cols, rows), (120, 30));
                assert_eq!(shell, "/bin/bash");
            }
            _ => panic!("应解析为 open"),
        }

        // shell 省略时应回落到空串（由客户端挑默认值），而不是解析失败。
        let without: ClientMessage =
            serde_json::from_str(r#"{"type":"open","cols":80,"rows":24}"#).unwrap();
        match without {
            ClientMessage::Open { shell, .. } => assert!(shell.is_empty()),
            _ => panic!("应解析为 open"),
        }
    }

    #[test]
    fn input_and_resize_parse() {
        let input: ClientMessage =
            serde_json::from_str(r#"{"type":"input","data":"YQ=="}"#).unwrap();
        match input {
            ClientMessage::Input { data } => assert_eq!(data, "YQ=="),
            _ => panic!("应解析为 input"),
        }

        let resize: ClientMessage =
            serde_json::from_str(r#"{"type":"resize","cols":100,"rows":40}"#).unwrap();
        match resize {
            ClientMessage::Resize { cols, rows } => assert_eq!((cols, rows), (100, 40)),
            _ => panic!("应解析为 resize"),
        }
    }

    #[test]
    fn server_messages_serialize_for_the_frontend() {
        // 前端按 `type` 字段 switch，标签名不能随手改。
        let ready = serde_json::to_string(&ServerMessage::Ready).unwrap();
        assert_eq!(ready, r#"{"type":"ready"}"#);

        let output = serde_json::to_string(&ServerMessage::Output {
            data: "aGk=".to_string(),
        })
        .unwrap();
        assert_eq!(output, r#"{"type":"output","data":"aGk="}"#);

        let exit = serde_json::to_string(&ServerMessage::Exit { code: -9 }).unwrap();
        assert_eq!(exit, r#"{"type":"exit","code":-9}"#);

        let error = serde_json::to_string(&ServerMessage::error("坏了")).unwrap();
        assert!(error.starts_with(r#"{"type":"error","message":"坏了""#));
    }

    #[test]
    fn terminal_output_survives_base64_round_trip() {
        // 终端输出常含 ANSI 转义、Ctrl-C、以及被切成半截的 UTF-8。
        let payloads: Vec<Vec<u8>> = vec![
            b"\x1b[31mred\x1b[0m".to_vec(),
            vec![0x03],
            // 「中」的前两个字节：单独拿出来不是合法 UTF-8。
            vec![0xe4, 0xb8],
            (0u8..=255).collect(),
        ];
        for payload in payloads {
            let message = ServerMessage::Output {
                data: BASE64.encode(&payload),
            };
            let json = serde_json::to_string(&message).unwrap();
            let parsed: serde_json::Value = serde_json::from_str(&json).unwrap();
            let encoded = parsed["data"].as_str().unwrap();
            assert_eq!(
                BASE64.decode(encoded).unwrap(),
                payload,
                "Base64 往返后字节应完全一致"
            );
        }
    }
}