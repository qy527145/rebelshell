//! REST 接口：设备列表、命令下发、文件收发。

use std::path::{Path, PathBuf};

use axum::body::Body;
use axum::extract::{Multipart, Query, State};
use axum::http::{header, StatusCode};
use axum::response::{IntoResponse, Response};
use axum::Json;
use serde::{Deserialize, Serialize};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tracing::{info, warn};

use crate::auth::Authed;
use crate::hub::{now_ms, DeviceInfo};
use crate::protocol::{parse_listing, ClientFrame, DirEntry, ServerFrame};
use crate::state::{AppState, CommandDone, Event, OutputChunk, Stream};

/// 分块大小：与客户端约定 64 KiB 原始数据一块。
const CHUNK_SIZE: usize = 64 * 1024;
/// 单次文件下发的大小上限（防止一个请求把内存吃满）。
const MAX_UPLOAD_BYTES: u64 = 512 * 1024 * 1024;

/// 统一的错误响应体。
#[derive(Serialize)]
struct ApiError {
    error: String,
}

fn error(status: StatusCode, message: impl Into<String>) -> Response {
    (
        status,
        Json(ApiError {
            error: message.into(),
        }),
    )
        .into_response()
}

fn bad_request(message: impl Into<String>) -> Response {
    error(StatusCode::BAD_REQUEST, message)
}

fn not_found(message: impl Into<String>) -> Response {
    error(StatusCode::NOT_FOUND, message)
}

fn gateway_timeout(message: impl Into<String>) -> Response {
    error(StatusCode::GATEWAY_TIMEOUT, message)
}

// ---- 设备列表 ----

#[derive(Serialize)]
struct DevicesResponse {
    devices: Vec<DeviceInfo>,
    server_time_ms: u64,
}

/// `GET /api/devices`
pub async fn devices(State(state): State<AppState>, _: Authed) -> Response {
    Json(DevicesResponse {
        devices: state.hub.devices(),
        server_time_ms: now_ms(),
    })
    .into_response()
}

// ---- 命令下发 ----

#[derive(Deserialize)]
pub struct CommandRequest {
    pub uuid: String,
    pub command: String,
}

#[derive(Serialize)]
struct CommandResponse {
    uuid: String,
    command: String,
    output: String,
}

/// `POST /api/command`
///
/// 命令结果同时通过 HTTP 返回（给发起方）与 WebSocket 广播（给所有打开的页面），
/// 因此多标签页都能看到回显。
pub async fn command(
    State(state): State<AppState>,
    _: Authed,
    Json(request): Json<CommandRequest>,
) -> Response {
    if request.command.trim().is_empty() {
        return bad_request("命令不能为空");
    }
    if !state.hub.is_online(&request.uuid) {
        return not_found(format!("设备 {} 当前不在线", request.uuid));
    }

    let command = request.command.clone();
    let uuid = request.uuid.clone();
    info!(uuid, command, "下发命令");

    let result = state
        .hub
        .request(
            &uuid,
            |req_id| {
                ServerFrame::Command {
                    req_id,
                    command: command.clone(),
                }
                .encode()
            },
            state.config.command_timeout,
        )
        .await;

    match result {
        Ok(ClientFrame::Result { output, .. }) => {
            state.publish(Event::Output(OutputChunk {
                device_uuid: uuid.clone(),
                command: command.clone(),
                stream: Stream::Stdout,
                text: output.clone(),
            }));
            state.publish(Event::Done(CommandDone {
                device_uuid: uuid.clone(),
                command: command.clone(),
                ok: true,
                message: "命令执行完成".to_string(),
            }));
            Json(CommandResponse {
                uuid,
                command,
                output,
            })
            .into_response()
        }
        Ok(ClientFrame::Error { message, .. }) => {
            state.publish(Event::Output(OutputChunk {
                device_uuid: uuid.clone(),
                command: command.clone(),
                stream: Stream::Stderr,
                text: message.clone(),
            }));
            state.publish(Event::Done(CommandDone {
                device_uuid: uuid.clone(),
                command: command.clone(),
                ok: false,
                message: message.clone(),
            }));
            error(
                StatusCode::BAD_GATEWAY,
                format!("客户端返回错误：{message}"),
            )
        }
        Ok(other) => {
            warn!(uuid, "命令请求收到非预期帧: {other:?}");
            error(
                StatusCode::BAD_GATEWAY,
                format!("客户端返回了非预期的响应：{other:?}"),
            )
        }
        Err(message) => {
            // 超时/断开也要出现在网页回显里，否则前端只会看到请求失败却不知道原因。
            state.publish(Event::Output(OutputChunk {
                device_uuid: uuid.clone(),
                command: command.clone(),
                stream: Stream::System,
                text: message.clone(),
            }));
            state.publish(Event::Done(CommandDone {
                device_uuid: uuid.clone(),
                command: command.clone(),
                ok: false,
                message: message.clone(),
            }));
            gateway_timeout(message)
        }
    }
}

// ---- 文件下发（管理端 → 虚拟机）----

#[derive(Serialize)]
struct PushResponse {
    uuid: String,
    name: String,
    size: u64,
    message: String,
}

/// `POST /api/files/push`（multipart/form-data）
///
/// 字段：`uuid`（设备）、`path`（目标路径，可省略，默认用文件名）、`file`（文件本体）。
///
/// 实现上先把上传流落到临时文件（避免把整个文件读进内存），再分块发给客户端。
pub async fn push_file(
    State(state): State<AppState>,
    _: Authed,
    mut multipart: Multipart,
) -> Response {
    let mut uuid: Option<String> = None;
    let mut target: Option<String> = None;
    let mut staged: Option<(PathBuf, u64)> = None;

    let outcome = async {
        while let Some(field) = multipart
            .next_field()
            .await
            .map_err(|err| bad_request(format!("解析上传内容失败：{err}")))?
        {
            match field.name().unwrap_or("") {
                "uuid" => {
                    uuid = Some(
                        field
                            .text()
                            .await
                            .map_err(|err| bad_request(format!("uuid 字段读取失败：{err}")))?,
                    );
                }
                "path" => {
                    let text = field
                        .text()
                        .await
                        .map_err(|err| bad_request(format!("path 字段读取失败：{err}")))?;
                    if !text.trim().is_empty() {
                        target = Some(text);
                    }
                }
                "file" => {
                    staged = Some(
                        stage_upload(field)
                            .await
                            .map_err(|err| bad_request(format!("暂存上传内容失败：{err}")))?,
                    );
                }
                other => {
                    warn!("忽略未知的表单字段 {other:?}");
                }
            }
        }
        Ok::<(), Response>(())
    }
    .await;

    if let Err(response) = outcome {
        cleanup(&staged).await;
        return response;
    }

    // staged 的所有权要留到最后的 cleanup，因此这里只借用而不消费它。
    let (Some(uuid), Some((temp_path, size))) = (uuid.as_deref(), staged.as_ref()) else {
        cleanup(&staged).await;
        return bad_request("缺少 uuid 或 file 字段");
    };
    let (temp_path, size) = (temp_path.clone(), *size);

    // 目标路径：显式给出则用它，否则用原文件名。
    let name = target.unwrap_or_else(|| {
        temp_path
            .file_name()
            .map(|name| name.to_string_lossy().into_owned())
            .unwrap_or_else(|| "upload.bin".to_string())
    });

    if !state.hub.is_online(uuid) {
        cleanup(&staged).await;
        return not_found(format!("设备 {uuid} 当前不在线"));
    }

    let result = send_file_to_device(&state, uuid, &name, &temp_path, size).await;
    cleanup(&staged).await;

    match result {
        Ok(message) => Json(PushResponse {
            uuid: uuid.to_string(),
            name,
            size,
            message,
        })
        .into_response(),
        Err(message) => gateway_timeout(message),
    }
}

/// 把 multipart 字段写到临时文件，边写边统计大小。
async fn stage_upload(
    mut field: axum::extract::multipart::Field<'_>,
) -> Result<(PathBuf, u64), String> {
    let name = field
        .file_name()
        .map(|name| name.to_string())
        .unwrap_or_else(|| "upload.bin".to_string());
    let stem = Path::new(&name)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "upload.bin".to_string());

    let dir = std::env::temp_dir().join("rebelshell-uploads");
    tokio::fs::create_dir_all(&dir)
        .await
        .map_err(|err| format!("创建 {} 失败: {err}", dir.display()))?;
    let path = dir.join(format!("{}-{stem}", uuid_like()));
    let mut file = tokio::fs::File::create(&path)
        .await
        .map_err(|err| format!("创建 {} 失败: {err}", path.display()))?;

    let mut written: u64 = 0;
    while let Some(chunk) = field
        .chunk()
        .await
        .map_err(|err| format!("读取上传数据失败：{err}"))?
    {
        written += chunk.len() as u64;
        if written > MAX_UPLOAD_BYTES {
            let _ = tokio::fs::remove_file(&path).await;
            return Err(format!(
                "文件超过 {} MiB 上限",
                MAX_UPLOAD_BYTES / 1024 / 1024
            ));
        }
        file.write_all(&chunk)
            .await
            .map_err(|err| format!("写入临时文件失败：{err}"))?;
    }
    file.flush()
        .await
        .map_err(|err| format!("flush 失败：{err}"))?;
    drop(file);
    Ok((path, written))
}

fn uuid_like() -> String {
    let nanos = now_ms();
    format!("{:x}-{nanos:x}", std::process::id())
}

async fn cleanup(staged: &Option<(PathBuf, u64)>) {
    if let Some((path, _)) = staged {
        if let Err(err) = tokio::fs::remove_file(path).await {
            warn!("清理临时文件 {} 失败: {err}", path.display());
        }
    }
}

/// 按协议把暂存文件分块发给客户端。
///
/// 小文件走单帧 `SEND_FILE`，大文件走 `PUT_BEGIN`/`PUT_CHUNK`/`PUT_END`，
/// 避免一条超大帧长时间独占连接、把心跳挤掉。
async fn send_file_to_device(
    state: &AppState,
    uuid: &str,
    name: &str,
    path: &Path,
    size: u64,
) -> Result<String, String> {
    let request = state.hub.reserve_request(uuid)?;

    if size <= CHUNK_SIZE as u64 {
        let data = tokio::fs::read(path)
            .await
            .map_err(|err| format!("读取 {} 失败: {err}", path.display()))?;
        request
            .send(
                ServerFrame::SendFile {
                    req_id: request.req_id,
                    name: name.to_string(),
                    size: data.len() as u64,
                    data,
                }
                .encode(),
            )
            .await?;
    } else {
        request
            .send(
                ServerFrame::PutBegin {
                    req_id: request.req_id,
                    name: name.to_string(),
                    size,
                }
                .encode(),
            )
            .await?;

        let mut file = tokio::fs::File::open(path)
            .await
            .map_err(|err| format!("重新打开 {} 失败: {err}", path.display()))?;
        let mut buffer = vec![0u8; CHUNK_SIZE];
        loop {
            let read = file
                .read(&mut buffer)
                .await
                .map_err(|err| format!("读取 {} 失败: {err}", path.display()))?;
            if read == 0 {
                break;
            }
            request
                .send(
                    ServerFrame::PutChunk {
                        req_id: request.req_id,
                        data: buffer[..read].to_vec(),
                    }
                    .encode(),
                )
                .await?;
        }
        request
            .send(
                ServerFrame::PutEnd {
                    req_id: request.req_id,
                }
                .encode(),
            )
            .await?;
    }

    match request.finish(state.config.transfer_timeout).await? {
        ClientFrame::Result { output, .. } => Ok(output),
        ClientFrame::Error { message, .. } => Err(format!("客户端写入失败：{message}")),
        other => Err(format!("客户端返回了非预期的响应：{other:?}")),
    }
}

// ---- 文件提取（虚拟机 → 管理端）----

#[derive(Deserialize)]
pub struct PullQuery {
    pub uuid: String,
    pub path: String,
}

/// `GET /api/files/pull?uuid=&path=`
///
/// 客户端回传的 `FILE_CHUNK` 会被直接转成 HTTP 响应体流式送出，
/// 因此浏览器端能立即触发下载，服务端也不必把整个文件缓存在内存里。
pub async fn pull_file(
    State(state): State<AppState>,
    _: Authed,
    Query(query): Query<PullQuery>,
) -> Response {
    if query.path.trim().is_empty() {
        return bad_request("路径不能为空");
    }
    if !state.hub.is_online(&query.uuid) {
        return not_found(format!("设备 {} 当前不在线", query.uuid));
    }

    let mut request = match state.hub.reserve_request(&query.uuid) {
        Ok(request) => request,
        Err(message) => return not_found(message),
    };

    if let Err(message) = request
        .send(
            ServerFrame::GetFile {
                req_id: request.req_id,
                path: query.path.clone(),
            }
            .encode(),
        )
        .await
    {
        return gateway_timeout(message);
    }

    // 等 FILE_BEGIN（或错误帧）才知道总大小与文件名。
    let size = match request.next_frame(state.config.command_timeout).await {
        Ok(ClientFrame::FileBegin { size, .. }) => size,
        Ok(ClientFrame::Error { message, .. }) => {
            return error(
                StatusCode::BAD_GATEWAY,
                format!("客户端读取失败：{message}"),
            )
        }
        Ok(other) => {
            return error(
                StatusCode::BAD_GATEWAY,
                format!("客户端返回了非预期的响应：{other:?}"),
            )
        }
        Err(message) => return gateway_timeout(message),
    };

    let filename = Path::new(&query.path)
        .file_name()
        .map(|name| name.to_string_lossy().into_owned())
        .unwrap_or_else(|| "download.bin".to_string());

    // 用一个 channel 把后续的 FILE_CHUNK 转成响应体流；生产者任务负责收尾与清理。
    let (tx, rx) = tokio::sync::mpsc::channel::<Result<Vec<u8>, std::io::Error>>(8);
    let timeout = state.config.transfer_timeout;
    tokio::spawn(async move {
        let mut sent: u64 = 0;
        loop {
            match request.next_frame(timeout).await {
                Ok(ClientFrame::FileChunk { data, .. }) => {
                    sent += data.len() as u64;
                    if tx.send(Ok(data)).await.is_err() {
                        // 浏览器已断开；继续把连接上的剩余帧读完会让对端白等，
                        // 直接放弃即可（客户端会因写失败自行结束）。
                        break;
                    }
                }
                Ok(ClientFrame::FileEnd { ok, .. }) => {
                    if !ok {
                        let _ = tx
                            .send(Err(std::io::Error::other("客户端读取文件时出错")))
                            .await;
                    } else if sent != size {
                        let _ = tx
                            .send(Err(std::io::Error::other(format!(
                                "文件长度不符：声明 {size} 字节，实收 {sent}"
                            ))))
                            .await;
                    }
                    break;
                }
                Ok(ClientFrame::Error { message, .. }) => {
                    let _ = tx.send(Err(std::io::Error::other(message))).await;
                    break;
                }
                Ok(other) => {
                    let _ = tx
                        .send(Err(std::io::Error::other(format!("非预期帧：{other:?}"))))
                        .await;
                    break;
                }
                Err(message) => {
                    let _ = tx.send(Err(std::io::Error::other(message))).await;
                    break;
                }
            }
        }
    });

    let stream = tokio_stream::wrappers::ReceiverStream::new(rx);
    let body = Body::from_stream(stream);
    let disposition = format!("attachment; filename=\"{}\"", sanitize_filename(&filename));

    (
        [
            (header::CONTENT_TYPE, "application/octet-stream".to_string()),
            (header::CONTENT_LENGTH, size.to_string()),
            (header::CONTENT_DISPOSITION, disposition),
        ],
        body,
    )
        .into_response()
}

/// 去掉会破坏 `Content-Disposition` 的字符（引号、换行、路径分隔符）。
fn sanitize_filename(name: &str) -> String {
    name.chars()
        .map(|ch| match ch {
            '"' | '\\' | '\r' | '\n' | '/' => '_',
            other => other,
        })
        .collect()
}

// ---- 文件管理（浏览 / 新建 / 重命名 / 删除 / 在线编辑）----

#[derive(Deserialize)]
pub struct DevicePathQuery {
    pub uuid: String,
    /// 目标路径；留空表示客户端的工作目录。
    #[serde(default)]
    pub path: String,
}

#[derive(Serialize)]
struct DirEntryResponse {
    name: String,
    /// `d` 目录 / `f` 文件 / `l` 符号链接。
    kind: char,
    size: u64,
    modified_ms: u64,
}

#[derive(Serialize)]
struct ListingResponse {
    uuid: String,
    /// 客户端解析后的绝对路径，前端据此维护「当前目录」。
    path: String,
    entries: Vec<DirEntryResponse>,
}

impl From<DirEntry> for DirEntryResponse {
    fn from(entry: DirEntry) -> Self {
        Self {
            name: entry.name,
            kind: entry.kind,
            size: entry.size,
            modified_ms: entry.modified_ms,
        }
    }
}

/// 把一个「请求 → 期望 RESULT 文本」的动作收敛到一处。
///
/// 文件管理的六个动作返回形状完全一样（成功给一句提示、失败给原因），
/// 差别只在帧类型与超时，因此共用这条路径，避免六份几乎相同的 match。
async fn simple_action(
    state: &AppState,
    uuid: &str,
    build: impl FnOnce(u64) -> String,
) -> Response {
    if !state.hub.is_online(uuid) {
        return not_found(format!("设备 {uuid} 当前不在线"));
    }

    match state
        .hub
        .request(uuid, build, state.config.transfer_timeout)
        .await
    {
        Ok(ClientFrame::Result { output, .. }) => {
            Json(serde_json::json!({ "message": output })).into_response()
        }
        Ok(ClientFrame::Error { message, .. }) => {
            error(StatusCode::BAD_GATEWAY, format!("客户端操作失败：{message}"))
        }
        Ok(other) => {
            warn!(uuid, "文件管理请求收到非预期帧: {other:?}");
            error(
                StatusCode::BAD_GATEWAY,
                format!("客户端返回了非预期的响应：{other:?}"),
            )
        }
        Err(message) => gateway_timeout(message),
    }
}

/// `GET /api/fs/list?uuid=&path=`
///
/// `path` 留空时列客户端的工作目录，因此前端一打开就有内容可看。
pub async fn list_dir(
    State(state): State<AppState>,
    _: Authed,
    Query(query): Query<DevicePathQuery>,
) -> Response {
    if !state.hub.is_online(&query.uuid) {
        return not_found(format!("设备 {} 当前不在线", query.uuid));
    }

    let result = state
        .hub
        .request(
            &query.uuid,
            |req_id| {
                ServerFrame::ListDir {
                    req_id,
                    path: query.path.clone(),
                }
                .encode()
            },
            state.config.command_timeout,
        )
        .await;

    match result {
        Ok(ClientFrame::DirListing { listing, .. }) => match parse_listing(&listing) {
            Ok((path, entries)) => Json(ListingResponse {
                uuid: query.uuid,
                path,
                entries: entries.into_iter().map(Into::into).collect(),
            })
            .into_response(),
            Err(err) => {
                warn!(uuid = %query.uuid, "目录列表格式错误: {err}");
                error(
                    StatusCode::BAD_GATEWAY,
                    format!("客户端返回的目录列表无法解析：{err}"),
                )
            }
        },
        Ok(ClientFrame::Error { message, .. }) => {
            error(StatusCode::BAD_GATEWAY, format!("列目录失败：{message}"))
        }
        Ok(other) => {
            warn!(uuid = %query.uuid, "列目录收到非预期帧: {other:?}");
            error(
                StatusCode::BAD_GATEWAY,
                format!("客户端返回了非预期的响应：{other:?}"),
            )
        }
        Err(message) => gateway_timeout(message),
    }
}

#[derive(Deserialize)]
pub struct MkdirRequest {
    pub uuid: String,
    pub path: String,
}

/// `POST /api/fs/mkdir`
pub async fn make_dir(
    State(state): State<AppState>,
    _: Authed,
    Json(request): Json<MkdirRequest>,
) -> Response {
    if request.path.trim().is_empty() {
        return bad_request("路径不能为空");
    }
    let uuid = request.uuid.clone();
    let path = request.path.clone();
    simple_action(&state, &uuid, move |req_id| {
        ServerFrame::Mkdir { req_id, path }.encode()
    })
    .await
}

#[derive(Deserialize)]
pub struct RenameRequest {
    pub uuid: String,
    /// 源路径（绝对路径，前端从列表里带过来）。
    pub from: String,
    /// 目标：既可以是完整路径，也可以是同一目录下的新名字。
    pub to: String,
}

/// `POST /api/fs/rename`
pub async fn rename(
    State(state): State<AppState>,
    _: Authed,
    Json(request): Json<RenameRequest>,
) -> Response {
    if request.from.trim().is_empty() || request.to.trim().is_empty() {
        return bad_request("源路径与目标名都不能为空");
    }
    let uuid = request.uuid.clone();
    let (from, to) = (request.from.clone(), request.to.clone());
    simple_action(&state, &uuid, move |req_id| {
        ServerFrame::Rename { req_id, from, to }.encode()
    })
    .await
}

#[derive(Deserialize)]
pub struct DeleteRequest {
    pub uuid: String,
    pub path: String,
    /// 删除目录时必须显式给出 `true`，避免误删整棵树。
    #[serde(default)]
    pub recursive: bool,
}

/// `POST /api/fs/delete`
pub async fn delete(
    State(state): State<AppState>,
    _: Authed,
    Json(request): Json<DeleteRequest>,
) -> Response {
    if request.path.trim().is_empty() {
        return bad_request("路径不能为空");
    }
    let uuid = request.uuid.clone();
    let path = request.path.clone();
    let recursive = request.recursive;
    simple_action(&state, &uuid, move |req_id| {
        ServerFrame::Delete {
            req_id,
            path,
            recursive,
        }
        .encode()
    })
    .await
}

#[derive(Serialize)]
struct TextResponse {
    path: String,
    content: String,
}

/// `GET /api/fs/read?uuid=&path=`：读文本文件供在线编辑。
pub async fn read_text(
    State(state): State<AppState>,
    _: Authed,
    Query(query): Query<DevicePathQuery>,
) -> Response {
    if query.path.trim().is_empty() {
        return bad_request("路径不能为空");
    }
    if !state.hub.is_online(&query.uuid) {
        return not_found(format!("设备 {} 当前不在线", query.uuid));
    }

    let result = state
        .hub
        .request(
            &query.uuid,
            |req_id| {
                ServerFrame::ReadText {
                    req_id,
                    path: query.path.clone(),
                }
                .encode()
            },
            state.config.transfer_timeout,
        )
        .await;

    match result {
        Ok(ClientFrame::Result { output, .. }) => Json(TextResponse {
            path: query.path,
            content: output,
        })
        .into_response(),
        Ok(ClientFrame::Error { message, .. }) => {
            error(StatusCode::BAD_GATEWAY, format!("读取失败：{message}"))
        }
        Ok(other) => error(
            StatusCode::BAD_GATEWAY,
            format!("客户端返回了非预期的响应：{other:?}"),
        ),
        Err(message) => gateway_timeout(message),
    }
}

#[derive(Deserialize)]
pub struct WriteTextRequest {
    pub uuid: String,
    pub path: String,
    pub content: String,
}

/// `POST /api/fs/write`：把在线编辑的内容写回（客户端会先留一份 `.bak`）。
pub async fn write_text(
    State(state): State<AppState>,
    _: Authed,
    Json(request): Json<WriteTextRequest>,
) -> Response {
    if request.path.trim().is_empty() {
        return bad_request("路径不能为空");
    }
    let uuid = request.uuid.clone();
    let path = request.path.clone();
    let content = request.content.clone();
    simple_action(&state, &uuid, move |req_id| {
        ServerFrame::WriteText {
            req_id,
            path,
            content,
        }
        .encode()
    })
    .await
}

// ---- 前端需要的其它常量 ----

/// `GET /api/health`：不需要鉴权，方便反向代理与容器做存活探测。
pub async fn health() -> Response {
    Json(serde_json::json!({
        "status": "ok",
        "service": "rebelshell-server",
        "version": env!("CARGO_PKG_VERSION"),
        "revision": env!("REBELSHELL_GIT_REVISION"),
    }))
    .into_response()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sanitizes_filename_for_header() {
        assert_eq!(sanitize_filename("a\"b\\c\nd"), "a_b_c_d");
        assert_eq!(sanitize_filename("../../etc/passwd"), ".._.._etc_passwd");
        assert_eq!(sanitize_filename("正常名称.log"), "正常名称.log");
    }

    #[test]
    fn error_bodies_are_json() {
        let response = bad_request("坏了");
        assert_eq!(response.status(), StatusCode::BAD_REQUEST);
    }
}
