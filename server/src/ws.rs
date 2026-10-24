//! WebSocket：把设备列表变化与命令回显实时推给浏览器。
//!
//! 鉴权接受 `Authorization: Bearer` 头，也接受 `?token=` 查询参数
//! （浏览器无法为 WebSocket 握手设置自定义头）。未通过鉴权时直接以 401 拒绝握手。

use std::time::Duration;

use axum::extract::ws::{Message, WebSocket, WebSocketUpgrade};
use axum::extract::{Query, State};
use axum::http::HeaderMap;
use axum::response::{IntoResponse, Response};
use futures_util::{SinkExt, StreamExt};
use serde::Serialize;
use tokio::sync::broadcast::error::RecvError;
use tracing::{debug, info};

use crate::auth::{authorize, TokenQuery};
use crate::state::{AppState, CommandDone, Event, OutputChunk};

/// 客户端下行消息。`type` 字段供前端 switch。
#[derive(Serialize)]
#[serde(tag = "type", rename_all = "snake_case")]
enum ServerMessage {
    /// 设备列表快照。
    Devices {
        devices: Vec<crate::hub::DeviceInfo>,
        server_time_ms: u64,
    },
    /// 命令输出片段。
    Output {
        uuid: String,
        command: String,
        stream: &'static str,
        text: String,
    },
    /// 命令结束。
    Done {
        uuid: String,
        command: String,
        ok: bool,
        message: String,
    },
    /// 心跳，保持中间代理的连接不被回收。
    Ping,
}

impl From<&OutputChunk> for ServerMessage {
    fn from(chunk: &OutputChunk) -> Self {
        Self::Output {
            uuid: chunk.device_uuid.clone(),
            command: chunk.command.clone(),
            stream: chunk.stream.as_str(),
            text: chunk.text.clone(),
        }
    }
}

impl From<&CommandDone> for ServerMessage {
    fn from(done: &CommandDone) -> Self {
        Self::Done {
            uuid: done.device_uuid.clone(),
            command: done.command.clone(),
            ok: done.ok,
            message: done.message.clone(),
        }
    }
}

/// `GET /ws?token=<ADMIN_TOKEN>`
pub async fn ws_handler(
    State(state): State<AppState>,
    headers: HeaderMap,
    Query(query): Query<TokenQuery>,
    upgrade: WebSocketUpgrade,
) -> Response {
    if let Err(unauthorized) = authorize(&state, &headers, query.token.as_deref()) {
        return unauthorized.into_response();
    }
    upgrade.on_upgrade(move |socket| serve(state, socket))
}

async fn serve(state: AppState, socket: WebSocket) {
    let (mut sink, mut stream) = socket.split();
    let mut events = state.events.subscribe();
    let mut ticker = tokio::time::interval(Duration::from_secs(25));
    ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    info!("WebSocket 客户端已连接");

    // 一连上就推一次快照，前端不必再单独拉一次接口。
    if send_devices(&state, &mut sink).await.is_err() {
        return;
    }

    loop {
        tokio::select! {
            // 浏览器发来的消息目前只需要处理 ping/pong 与关闭。
            incoming = stream.next() => {
                match incoming {
                    Some(Ok(Message::Close(_))) | None => break,
                    Some(Ok(Message::Ping(payload))) => {
                        if sink.send(Message::Pong(payload)).await.is_err() {
                            break;
                        }
                    }
                    Some(Ok(_)) => {}
                    Some(Err(err)) => {
                        debug!("WebSocket 接收错误: {err}");
                        break;
                    }
                }
            }
            event = events.recv() => {
                match event {
                    Ok(Event::Devices) => {
                        if send_devices(&state, &mut sink).await.is_err() {
                            break;
                        }
                    }
                    Ok(Event::Output(chunk)) => {
                        if send_json(&mut sink, &ServerMessage::from(&chunk)).await.is_err() {
                            break;
                        }
                    }
                    Ok(Event::Done(done)) => {
                        if send_json(&mut sink, &ServerMessage::from(&done)).await.is_err() {
                            break;
                        }
                    }
                    // 跟不上广播进度时重推一次设备快照，让前端能自愈。
                    Err(RecvError::Lagged(skipped)) => {
                        debug!("WebSocket 落后 {skipped} 条事件，重推设备快照");
                        if send_devices(&state, &mut sink).await.is_err() {
                            break;
                        }
                    }
                    Err(RecvError::Closed) => break,
                }
            }
            _ = ticker.tick() => {
                if send_json(&mut sink, &ServerMessage::Ping).await.is_err() {
                    break;
                }
            }
        }
    }

    info!("WebSocket 客户端已断开");
}

async fn send_devices(
    state: &AppState,
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
) -> Result<(), ()> {
    let message = ServerMessage::Devices {
        devices: state.hub.devices(),
        server_time_ms: crate::hub::now_ms(),
    };
    send_json(sink, &message).await
}

async fn send_json(
    sink: &mut futures_util::stream::SplitSink<WebSocket, Message>,
    message: &ServerMessage,
) -> Result<(), ()> {
    let text = serde_json::to_string(message).map_err(|err| {
        debug!("序列化推送消息失败: {err}");
    })?;
    sink.send(Message::Text(text.into())).await.map_err(|err| {
        debug!("WebSocket 发送失败: {err}");
    })
}
