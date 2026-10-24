//! 协议帧的编解码。
//!
//! 与 [docs/protocol.md](../../docs/protocol.md) 保持一致：面向行、字段以 `:` 分隔、
//! 二进制与自由文本一律 Base64。

use base64::engine::general_purpose::STANDARD as BASE64;
use base64::Engine as _;

/// 编解码错误的统一类型。
#[derive(Debug)]
pub enum ProtocolError {
    /// 字段数不足 / 结构不符合约定。
    Malformed(String),
    /// 数字字段解析失败。
    Number(String),
    /// Base64 解码失败。
    Base64(String),
    /// 解码结果不是合法 UTF-8。
    Utf8(String),
    /// 对端返回了预期之外的帧。
    Unexpected(String),
}

impl std::fmt::Display for ProtocolError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Malformed(msg) => write!(f, "帧结构错误: {msg}"),
            Self::Number(msg) => write!(f, "数字字段错误: {msg}"),
            Self::Base64(msg) => write!(f, "Base64 解码失败: {msg}"),
            Self::Utf8(msg) => write!(f, "字段不是合法 UTF-8: {msg}"),
            Self::Unexpected(msg) => write!(f, "非预期的帧: {msg}"),
        }
    }
}

impl std::error::Error for ProtocolError {}

pub type Result<T> = std::result::Result<T, ProtocolError>;

/// 客户端上行帧。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ClientFrame {
    Register {
        uuid: String,
        hostname: String,
        os: String,
    },
    Pong,
    Result {
        req_id: u64,
        output: String,
    },
    /// 客户端开始回传文件：`FILE_BEGIN:<REQ_ID>:<SIZE>`
    FileBegin {
        req_id: u64,
        size: u64,
    },
    /// `FILE_CHUNK:<REQ_ID>:<B64(DATA)>`
    FileChunk {
        req_id: u64,
        data: Vec<u8>,
    },
    /// `FILE_END:<REQ_ID>:<OK|ERR>`
    FileEnd {
        req_id: u64,
        ok: bool,
    },
    Error {
        req_id: u64,
        message: String,
    },
    /// 交互式终端的输出：`PTY_OUT:<REQ_ID>:<B64(DATA)>`
    PtyOut {
        req_id: u64,
        data: Vec<u8>,
    },
    /// 交互式终端结束：`PTY_EXIT:<REQ_ID>:<CODE>`
    PtyExit {
        req_id: u64,
        code: i32,
    },
    /// 终端已在客户端侧启动：`PTY_READY:<REQ_ID>`
    PtyReady {
        req_id: u64,
    },
    /// 目录列表：`DIR_LISTING:<REQ_ID>:<B64(LISTING)>`，格式见 `parse_listing`。
    DirListing {
        req_id: u64,
        listing: String,
    },
}

/// 目录列表里的一项。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DirEntry {
    pub name: String,
    /// `d` 目录 / `f` 文件 / `l` 符号链接。
    pub kind: char,
    pub size: u64,
    /// Unix 毫秒时间戳；客户端取不到时为 0。
    pub modified_ms: u64,
}

/// 解析客户端回传的目录列表。
///
/// 格式刻意做得极简，好让零依赖的客户端能直接拼字符串：
/// 第一行是 `PATH\t<解析后的绝对路径>`，其后每行是
/// `<kind>\t<size>\t<mtime_ms>\t<name>`（name 里允许有制表符以外的任何字符）。
pub fn parse_listing(text: &str) -> Result<(String, Vec<DirEntry>)> {
    let mut lines = text.lines();
    let head = lines.next().unwrap_or("");
    let path = head
        .strip_prefix("PATH\t")
        .ok_or_else(|| ProtocolError::Malformed(format!("目录列表缺少 PATH 头：{head:?}")))?;

    let mut entries = Vec::new();
    for line in lines {
        if line.is_empty() {
            continue;
        }
        let mut fields = line.splitn(4, '\t');
        let (Some(kind), Some(size), Some(modified), Some(name)) =
            (fields.next(), fields.next(), fields.next(), fields.next())
        else {
            return Err(ProtocolError::Malformed(format!("目录项字段不足：{line:?}")));
        };
        let kind = kind
            .chars()
            .next()
            .ok_or_else(|| ProtocolError::Malformed("目录项缺少类型".to_string()))?;
        entries.push(DirEntry {
            name: name.to_string(),
            kind,
            size: parse_size(size)?,
            modified_ms: parse_size(modified)?,
        });
    }
    Ok((path.to_string(), entries))
}

/// 从一行文本解析出客户端帧（不含结尾换行）。
pub fn parse_client_frame(line: &str) -> Result<ClientFrame> {
    let (tag, rest) = line.split_once(':').unwrap_or((line, ""));

    match tag {
        "REGISTER" => {
            // hostname 是 Base64，所以按第 3 个冒号切分，且 hostname 本身不含 ':'。
            let fields: Vec<&str> = rest.splitn(3, ':').collect();
            if fields.len() != 3 {
                return Err(ProtocolError::Malformed(format!(
                    "REGISTER 需要 3 个字段，收到 {}：{line:?}",
                    fields.len()
                )));
            }
            Ok(ClientFrame::Register {
                uuid: fields[0].to_string(),
                hostname: decode_text(fields[1])?,
                os: fields[2].to_string(),
            })
        }
        "PONG" => Ok(ClientFrame::Pong),
        "RESULT" => {
            let (req_id, encoded) = split_pair(rest, "RESULT", line)?;
            Ok(ClientFrame::Result {
                req_id: parse_id(req_id)?,
                output: decode_text(encoded)?,
            })
        }
        "FILE_BEGIN" => {
            let (req_id, size) = split_pair(rest, "FILE_BEGIN", line)?;
            Ok(ClientFrame::FileBegin {
                req_id: parse_id(req_id)?,
                size: parse_size(size)?,
            })
        }
        "FILE_CHUNK" => {
            let (req_id, encoded) = split_pair(rest, "FILE_CHUNK", line)?;
            Ok(ClientFrame::FileChunk {
                req_id: parse_id(req_id)?,
                data: decode_bytes(encoded)?,
            })
        }
        "FILE_END" => {
            let (req_id, status) = split_pair(rest, "FILE_END", line)?;
            Ok(ClientFrame::FileEnd {
                req_id: parse_id(req_id)?,
                ok: status.eq_ignore_ascii_case("OK"),
            })
        }
        "ERROR" => {
            let (req_id, encoded) = split_pair(rest, "ERROR", line)?;
            Ok(ClientFrame::Error {
                req_id: parse_id(req_id)?,
                message: decode_text(encoded)?,
            })
        }
        "PTY_OUT" => {
            let (req_id, encoded) = split_pair(rest, "PTY_OUT", line)?;
            Ok(ClientFrame::PtyOut {
                req_id: parse_id(req_id)?,
                data: decode_bytes(encoded)?,
            })
        }
        "PTY_EXIT" => {
            let (req_id, code) = split_pair(rest, "PTY_EXIT", line)?;
            // 退出码可能为负（被信号杀死），因此按 i32 解析而不是 u64。
            Ok(ClientFrame::PtyExit {
                req_id: parse_id(req_id)?,
                code: code
                    .parse()
                    .map_err(|_| ProtocolError::Number(format!("退出码非法：{code:?}")))?,
            })
        }
        "PTY_READY" => Ok(ClientFrame::PtyReady {
            req_id: parse_id(rest.split(':').next().unwrap_or(rest))?,
        }),
        "DIR_LISTING" => {
            let (req_id, encoded) = split_pair(rest, "DIR_LISTING", line)?;
            Ok(ClientFrame::DirListing {
                req_id: parse_id(req_id)?,
                listing: decode_text(encoded)?,
            })
        }
        // 其它帧与单个连接无关，交回调用方自行处理（例如 PING）。
        other => Err(ProtocolError::Unexpected(other.to_string())),
    }
}

/// 服务端下行帧。
///
/// 既是下行帧的编码入口（[`ServerFrame::encode`]），也是解析器的产出类型，
/// 保证「构造」与「解析」两侧对帧格式的理解始终一致。
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ServerFrame {
    Command {
        req_id: u64,
        command: String,
    },
    /// 单帧下发文件：`SEND_FILE:<REQ_ID>:<B64(NAME)>:<SIZE>:<B64(DATA)>`
    SendFile {
        req_id: u64,
        name: String,
        size: u64,
        data: Vec<u8>,
    },
    /// 分块下发开始：`PUT_BEGIN:<REQ_ID>:<B64(NAME)>:<SIZE>`
    PutBegin {
        req_id: u64,
        name: String,
        size: u64,
    },
    /// 分块数据：`PUT_CHUNK:<REQ_ID>:<B64(DATA)>`
    PutChunk {
        req_id: u64,
        data: Vec<u8>,
    },
    /// 分块下发结束：`PUT_END:<REQ_ID>`
    PutEnd {
        req_id: u64,
    },
    GetFile {
        req_id: u64,
        path: String,
    },
    /// 打开交互式终端：`PTY_OPEN:<REQ_ID>:<COLS>:<ROWS>:<B64(SHELL)>`（shell 为空表示用默认）。
    PtyOpen {
        req_id: u64,
        cols: u16,
        rows: u16,
        shell: String,
    },
    /// 键盘输入：`PTY_IN:<REQ_ID>:<B64(DATA)>`
    PtyIn {
        req_id: u64,
        data: Vec<u8>,
    },
    /// 窗口大小变化：`PTY_RESIZE:<REQ_ID>:<COLS>:<ROWS>`
    PtyResize {
        req_id: u64,
        cols: u16,
        rows: u16,
    },
    /// 关闭终端：`PTY_CLOSE:<REQ_ID>`
    PtyClose {
        req_id: u64,
    },
    /// 列目录：`LIST_DIR:<REQ_ID>:<B64(PATH)>`
    ListDir {
        req_id: u64,
        path: String,
    },
    /// 新建目录：`MKDIR:<REQ_ID>:<B64(PATH)>`
    Mkdir {
        req_id: u64,
        path: String,
    },
    /// 重命名 / 移动：`RENAME:<REQ_ID>:<B64(FROM)>:<B64(TO)>`
    Rename {
        req_id: u64,
        from: String,
        to: String,
    },
    /// 删除：`DELETE:<REQ_ID>:<B64(PATH)>:<RECURSIVE>`
    Delete {
        req_id: u64,
        path: String,
        recursive: bool,
    },
    /// 读文本文件：`READ_TEXT:<REQ_ID>:<B64(PATH)>`
    ReadText {
        req_id: u64,
        path: String,
    },
    /// 写文本文件：`WRITE_TEXT:<REQ_ID>:<B64(PATH)>:<B64(CONTENT)>`
    WriteText {
        req_id: u64,
        path: String,
        content: String,
    },
}

impl ServerFrame {
    /// 编码成一行（含结尾换行）。
    pub fn encode(&self) -> String {
        match self {
            Self::Command { req_id, command } => {
                format!("CMD:{req_id}:{}\n", encode_text(command))
            }
            Self::SendFile {
                req_id,
                name,
                size,
                data,
            } => format!(
                "SEND_FILE:{req_id}:{}:{size}:{}\n",
                encode_text(name),
                encode_bytes(data)
            ),
            Self::PutBegin { req_id, name, size } => {
                format!("PUT_BEGIN:{req_id}:{}:{size}\n", encode_text(name))
            }
            Self::PutChunk { req_id, data } => {
                format!("PUT_CHUNK:{req_id}:{}\n", encode_bytes(data))
            }
            Self::PutEnd { req_id } => format!("PUT_END:{req_id}\n"),
            Self::GetFile { req_id, path } => {
                format!("GET_FILE:{req_id}:{}\n", encode_text(path))
            }
            Self::PtyOpen {
                req_id,
                cols,
                rows,
                shell,
            } => format!(
                "PTY_OPEN:{req_id}:{cols}:{rows}:{}\n",
                encode_text(shell)
            ),
            Self::PtyIn { req_id, data } => {
                format!("PTY_IN:{req_id}:{}\n", encode_bytes(data))
            }
            Self::PtyResize { req_id, cols, rows } => {
                format!("PTY_RESIZE:{req_id}:{cols}:{rows}\n")
            }
            Self::PtyClose { req_id } => format!("PTY_CLOSE:{req_id}\n"),
            Self::ListDir { req_id, path } => {
                format!("LIST_DIR:{req_id}:{}\n", encode_text(path))
            }
            Self::Mkdir { req_id, path } => {
                format!("MKDIR:{req_id}:{}\n", encode_text(path))
            }
            Self::Rename { req_id, from, to } => format!(
                "RENAME:{req_id}:{}:{}\n",
                encode_text(from),
                encode_text(to)
            ),
            Self::Delete {
                req_id,
                path,
                recursive,
            } => format!(
                "DELETE:{req_id}:{}:{}\n",
                encode_text(path),
                if *recursive { 1 } else { 0 }
            ),
            Self::ReadText { req_id, path } => {
                format!("READ_TEXT:{req_id}:{}\n", encode_text(path))
            }
            Self::WriteText {
                req_id,
                path,
                content,
            } => format!(
                "WRITE_TEXT:{req_id}:{}:{}\n",
                encode_text(path),
                encode_text(content)
            ),
        }
    }
}

/// 从一行文本解析出服务端帧（不含结尾换行）。
///
/// 生产代码只负责「编码下行帧」，不需要解析自己发出去的帧；
/// 保留这个解析器是为了让测试能验证编码与解析严格互逆。
#[cfg(test)]
pub fn parse_server_frame(line: &str) -> Result<ServerFrame> {
    let (tag, rest) = line.split_once(':').unwrap_or((line, ""));

    match tag {
        "CMD" => {
            let (req_id, encoded) = split_pair(rest, "CMD", line)?;
            Ok(ServerFrame::Command {
                req_id: parse_id(req_id)?,
                command: decode_text(encoded)?,
            })
        }
        "SEND_FILE" => {
            let fields: Vec<&str> = rest.splitn(4, ':').collect();
            if fields.len() != 4 {
                return Err(ProtocolError::Malformed(format!(
                    "SEND_FILE 需要 4 个字段，收到 {}：{line:?}",
                    fields.len()
                )));
            }
            Ok(ServerFrame::SendFile {
                req_id: parse_id(fields[0])?,
                name: decode_text(fields[1])?,
                size: parse_size(fields[2])?,
                data: decode_bytes(fields[3])?,
            })
        }
        "PUT_BEGIN" => {
            let fields: Vec<&str> = rest.splitn(3, ':').collect();
            if fields.len() != 3 {
                return Err(ProtocolError::Malformed(format!(
                    "PUT_BEGIN 需要 3 个字段，收到 {}：{line:?}",
                    fields.len()
                )));
            }
            Ok(ServerFrame::PutBegin {
                req_id: parse_id(fields[0])?,
                name: decode_text(fields[1])?,
                size: parse_size(fields[2])?,
            })
        }
        "PUT_CHUNK" => {
            let (req_id, encoded) = split_pair(rest, "PUT_CHUNK", line)?;
            Ok(ServerFrame::PutChunk {
                req_id: parse_id(req_id)?,
                data: decode_bytes(encoded)?,
            })
        }
        "PUT_END" => Ok(ServerFrame::PutEnd {
            req_id: parse_id(rest.split(':').next().unwrap_or(rest))?,
        }),
        "GET_FILE" => {
            let (req_id, encoded) = split_pair(rest, "GET_FILE", line)?;
            Ok(ServerFrame::GetFile {
                req_id: parse_id(req_id)?,
                path: decode_text(encoded)?,
            })
        }
        "PTY_OPEN" => {
            let fields: Vec<&str> = rest.splitn(4, ':').collect();
            if fields.len() != 4 {
                return Err(ProtocolError::Malformed(format!(
                    "PTY_OPEN 需要 4 个字段，收到 {}：{line:?}",
                    fields.len()
                )));
            }
            Ok(ServerFrame::PtyOpen {
                req_id: parse_id(fields[0])?,
                cols: parse_size(fields[1])? as u16,
                rows: parse_size(fields[2])? as u16,
                shell: decode_text(fields[3])?,
            })
        }
        "PTY_IN" => {
            let (req_id, encoded) = split_pair(rest, "PTY_IN", line)?;
            Ok(ServerFrame::PtyIn {
                req_id: parse_id(req_id)?,
                data: decode_bytes(encoded)?,
            })
        }
        "PTY_RESIZE" => {
            let fields: Vec<&str> = rest.splitn(3, ':').collect();
            if fields.len() != 3 {
                return Err(ProtocolError::Malformed(format!(
                    "PTY_RESIZE 需要 3 个字段，收到 {}：{line:?}",
                    fields.len()
                )));
            }
            Ok(ServerFrame::PtyResize {
                req_id: parse_id(fields[0])?,
                cols: parse_size(fields[1])? as u16,
                rows: parse_size(fields[2])? as u16,
            })
        }
        "PTY_CLOSE" => Ok(ServerFrame::PtyClose {
            req_id: parse_id(rest.split(':').next().unwrap_or(rest))?,
        }),
        "LIST_DIR" => {
            let (req_id, encoded) = split_pair(rest, "LIST_DIR", line)?;
            Ok(ServerFrame::ListDir {
                req_id: parse_id(req_id)?,
                path: decode_text(encoded)?,
            })
        }
        "MKDIR" => {
            let (req_id, encoded) = split_pair(rest, "MKDIR", line)?;
            Ok(ServerFrame::Mkdir {
                req_id: parse_id(req_id)?,
                path: decode_text(encoded)?,
            })
        }
        "RENAME" => {
            let fields: Vec<&str> = rest.splitn(3, ':').collect();
            if fields.len() != 3 {
                return Err(ProtocolError::Malformed(format!(
                    "RENAME 需要 3 个字段，收到 {}：{line:?}",
                    fields.len()
                )));
            }
            Ok(ServerFrame::Rename {
                req_id: parse_id(fields[0])?,
                from: decode_text(fields[1])?,
                to: decode_text(fields[2])?,
            })
        }
        "DELETE" => {
            let fields: Vec<&str> = rest.splitn(3, ':').collect();
            if fields.len() != 3 {
                return Err(ProtocolError::Malformed(format!(
                    "DELETE 需要 3 个字段，收到 {}：{line:?}",
                    fields.len()
                )));
            }
            Ok(ServerFrame::Delete {
                req_id: parse_id(fields[0])?,
                path: decode_text(fields[1])?,
                recursive: fields[2] == "1",
            })
        }
        "READ_TEXT" => {
            let (req_id, encoded) = split_pair(rest, "READ_TEXT", line)?;
            Ok(ServerFrame::ReadText {
                req_id: parse_id(req_id)?,
                path: decode_text(encoded)?,
            })
        }
        "WRITE_TEXT" => {
            let fields: Vec<&str> = rest.splitn(3, ':').collect();
            if fields.len() != 3 {
                return Err(ProtocolError::Malformed(format!(
                    "WRITE_TEXT 需要 3 个字段，收到 {}：{line:?}",
                    fields.len()
                )));
            }
            Ok(ServerFrame::WriteText {
                req_id: parse_id(fields[0])?,
                path: decode_text(fields[1])?,
                content: decode_text(fields[2])?,
            })
        }
        other => Err(ProtocolError::Unexpected(other.to_string())),
    }
}

fn split_pair<'a>(rest: &'a str, frame: &str, line: &str) -> Result<(&'a str, &'a str)> {
    rest.split_once(':')
        .ok_or_else(|| ProtocolError::Malformed(format!("{frame} 缺少字段：{line:?}")))
}

fn parse_id(raw: &str) -> Result<u64> {
    raw.parse()
        .map_err(|_| ProtocolError::Number(format!("请求号非法：{raw:?}")))
}

fn parse_size(raw: &str) -> Result<u64> {
    raw.parse()
        .map_err(|_| ProtocolError::Number(format!("大小非法：{raw:?}")))
}

fn decode_text(encoded: &str) -> Result<String> {
    let bytes = decode_bytes(encoded)?;
    String::from_utf8(bytes).map_err(|err| ProtocolError::Utf8(err.to_string()))
}

fn decode_bytes(encoded: &str) -> Result<Vec<u8>> {
    BASE64
        .decode(encoded)
        .map_err(|err| ProtocolError::Base64(err.to_string()))
}

/// Base64 编码自由文本字段（主机名、命令、文件名、路径）。
pub fn encode_text(text: &str) -> String {
    BASE64.encode(text.as_bytes())
}

/// Base64 编码二进制字段（文件数据）。
pub fn encode_bytes(data: &[u8]) -> String {
    BASE64.encode(data)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_register_with_base64_hostname() {
        let line = format!("REGISTER:vm-1:{}:linux", encode_text("测试机"));
        match parse_client_frame(&line).unwrap() {
            ClientFrame::Register { uuid, hostname, os } => {
                assert_eq!(uuid, "vm-1");
                assert_eq!(hostname, "测试机");
                assert_eq!(os, "linux");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_pong_and_ignores_ping() {
        assert_eq!(parse_client_frame("PONG").unwrap(), ClientFrame::Pong);
        assert!(matches!(
            parse_client_frame("PING"),
            Err(ProtocolError::Unexpected(_))
        ));
    }

    #[test]
    fn parses_result_with_multiline_output() {
        let line = format!("RESULT:7:{}", encode_text("line1\nline2\n"));
        match parse_client_frame(&line).unwrap() {
            ClientFrame::Result { req_id, output } => {
                assert_eq!(req_id, 7);
                assert_eq!(output, "line1\nline2\n");
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_file_stream_frames() {
        assert_eq!(
            parse_client_frame("FILE_BEGIN:10:52310").unwrap(),
            ClientFrame::FileBegin {
                req_id: 10,
                size: 52310
            }
        );
        let chunk = format!("FILE_CHUNK:10:{}", encode_bytes(&[0, 1, 255]));
        assert_eq!(
            parse_client_frame(&chunk).unwrap(),
            ClientFrame::FileChunk {
                req_id: 10,
                data: vec![0, 1, 255]
            }
        );
        assert_eq!(
            parse_client_frame("FILE_END:10:OK").unwrap(),
            ClientFrame::FileEnd {
                req_id: 10,
                ok: true
            }
        );
        assert_eq!(
            parse_client_frame("FILE_END:10:ERR").unwrap(),
            ClientFrame::FileEnd {
                req_id: 10,
                ok: false
            }
        );
    }

    #[test]
    fn parses_error_frame() {
        let line = format!("ERROR:3:{}", encode_text("打开 /etc/x 失败"));
        match parse_client_frame(&line).unwrap() {
            ClientFrame::Error { req_id, message } => {
                assert_eq!(req_id, 3);
                assert!(message.contains("/etc/x"));
            }
            other => panic!("unexpected: {other:?}"),
        }
    }

    #[test]
    fn parses_server_frames() {
        let line = format!("CMD:9:{}", encode_text("echo hi"));
        assert_eq!(
            parse_server_frame(&line).unwrap(),
            ServerFrame::Command {
                req_id: 9,
                command: "echo hi".to_string()
            }
        );
        let line = format!("GET_FILE:11:{}", encode_text("/var/log/syslog"));
        assert_eq!(
            parse_server_frame(&line).unwrap(),
            ServerFrame::GetFile {
                req_id: 11,
                path: "/var/log/syslog".to_string()
            }
        );
    }

    #[test]
    fn server_frames_roundtrip_through_encode_and_parse() {
        // 编码与解析必须互逆，否则客户端会看不懂服务端发出的帧。
        let frames = [
            ServerFrame::Command {
                req_id: 1,
                command: "echo 中文:with:colons\nand newline".to_string(),
            },
            ServerFrame::SendFile {
                req_id: 2,
                name: "a:b.bin".to_string(),
                size: 3,
                data: vec![0, 255, 10],
            },
            ServerFrame::PutBegin {
                req_id: 3,
                name: "../rel/path.txt".to_string(),
                size: 123_456_789,
            },
            ServerFrame::PutChunk {
                req_id: 3,
                data: vec![0xde, 0xad, 0xbe, 0xef],
            },
            ServerFrame::PutEnd { req_id: 3 },
            ServerFrame::GetFile {
                req_id: 4,
                path: r"C:\Windows\Temp\x.log".to_string(),
            },
        ];

        for frame in frames {
            let encoded = frame.encode();
            assert!(encoded.ends_with('\n'), "帧必须以换行结尾：{encoded:?}");
            // 去掉结尾换行后应能解析回原帧。
            let parsed = parse_server_frame(encoded.trim_end_matches('\n')).unwrap();
            assert_eq!(parsed, frame, "编码/解析不互逆：{encoded:?}");
        }
    }

    #[test]
    fn parses_pty_and_listing_frames() {
        let line = format!("PTY_OUT:5:{}", encode_bytes(&[0x1b, b'[', b'3', b'1', b'm']));
        assert_eq!(
            parse_client_frame(&line).unwrap(),
            ClientFrame::PtyOut {
                req_id: 5,
                data: vec![0x1b, b'[', b'3', b'1', b'm']
            }
        );
        assert_eq!(
            parse_client_frame("PTY_EXIT:5:0").unwrap(),
            ClientFrame::PtyExit { req_id: 5, code: 0 }
        );
        // 被信号杀死时退出码是负数。
        assert_eq!(
            parse_client_frame("PTY_EXIT:5:-1").unwrap(),
            ClientFrame::PtyExit {
                req_id: 5,
                code: -1
            }
        );
    }

    #[test]
    fn parses_directory_listing() {
        let listing = "PATH\t/home/vm\n\
                       d\t0\t1700000000000\tsrc\n\
                       f\t1024\t1700000001000\tREADME.md\n\
                       l\t0\t0\tlink";
        let line = format!("DIR_LISTING:6:{}", encode_text(listing));
        let ClientFrame::DirListing { listing, .. } = parse_client_frame(&line).unwrap() else {
            panic!("应解析为 DirListing");
        };

        let (path, entries) = parse_listing(&listing).unwrap();
        assert_eq!(path, "/home/vm");
        assert_eq!(entries.len(), 3);
        assert_eq!(
            entries[0],
            DirEntry {
                name: "src".to_string(),
                kind: 'd',
                size: 0,
                modified_ms: 1_700_000_000_000
            }
        );
        assert_eq!(entries[1].name, "README.md");
        assert_eq!(entries[2].kind, 'l');
    }

    #[test]
    fn listing_keeps_tabs_out_of_names_but_handles_cjk() {
        // 文件名里带空格与中文都要原样保留（只有制表符是分隔符）。
        let listing = "PATH\tC:\\Users\nf\t12\t0\t我的 文件.txt";
        let (path, entries) = parse_listing(listing).unwrap();
        assert_eq!(path, "C:\\Users");
        assert_eq!(entries[0].name, "我的 文件.txt");
    }

    #[test]
    fn rejects_listing_without_path_header() {
        assert!(matches!(
            parse_listing("f\t1\t0\tx"),
            Err(ProtocolError::Malformed(_))
        ));
    }

    #[test]
    fn parses_new_file_manager_frames() {
        assert_eq!(
            parse_server_frame(&format!("LIST_DIR:7:{}", encode_text("/tmp"))).unwrap(),
            ServerFrame::ListDir {
                req_id: 7,
                path: "/tmp".to_string()
            }
        );
        assert_eq!(
            parse_server_frame(&format!("MKDIR:7:{}", encode_text("/tmp/a"))).unwrap(),
            ServerFrame::Mkdir {
                req_id: 7,
                path: "/tmp/a".to_string()
            }
        );
        assert_eq!(
            parse_server_frame(&format!(
                "RENAME:7:{}:{}",
                encode_text("/tmp/a"),
                encode_text("/tmp/b")
            ))
            .unwrap(),
            ServerFrame::Rename {
                req_id: 7,
                from: "/tmp/a".to_string(),
                to: "/tmp/b".to_string()
            }
        );
        assert_eq!(
            parse_server_frame(&format!("DELETE:7:{}:1", encode_text("/tmp/a"))).unwrap(),
            ServerFrame::Delete {
                req_id: 7,
                path: "/tmp/a".to_string(),
                recursive: true
            }
        );
        assert_eq!(
            parse_server_frame(&format!("READ_TEXT:7:{}", encode_text("/tmp/a"))).unwrap(),
            ServerFrame::ReadText {
                req_id: 7,
                path: "/tmp/a".to_string()
            }
        );
        assert_eq!(
            parse_server_frame(&format!(
                "WRITE_TEXT:7:{}:{}",
                encode_text("/tmp/a"),
                encode_text("line1\nline2")
            ))
            .unwrap(),
            ServerFrame::WriteText {
                req_id: 7,
                path: "/tmp/a".to_string(),
                content: "line1\nline2".to_string()
            }
        );
        // 非 1 一律视为非递归，避免把 "0"/"2" 之类的值当递归删除。
        assert_eq!(
            parse_server_frame(&format!("DELETE:7:{}:0", encode_text("/tmp/a"))).unwrap(),
            ServerFrame::Delete {
                req_id: 7,
                path: "/tmp/a".to_string(),
                recursive: false
            }
        );
    }

    #[test]
    fn parses_pty_open_and_resize() {
        assert_eq!(
            parse_server_frame(&format!("PTY_OPEN:8:120:30:{}", encode_text("/bin/bash"))).unwrap(),
            ServerFrame::PtyOpen {
                req_id: 8,
                cols: 120,
                rows: 30,
                shell: "/bin/bash".to_string()
            }
        );
        // 空 shell 表示让客户端选默认值。
        assert_eq!(
            parse_server_frame(&format!("PTY_OPEN:8:80:24:{}", encode_text(""))).unwrap(),
            ServerFrame::PtyOpen {
                req_id: 8,
                cols: 80,
                rows: 24,
                shell: String::new()
            }
        );
        assert_eq!(
            parse_server_frame("PTY_RESIZE:8:100:40").unwrap(),
            ServerFrame::PtyResize {
                req_id: 8,
                cols: 100,
                rows: 40
            }
        );
        assert_eq!(
            parse_server_frame("PTY_CLOSE:8").unwrap(),
            ServerFrame::PtyClose { req_id: 8 }
        );
    }

    #[test]
    fn pty_payloads_survive_base64() {
        // 终端里的一切（ANSI 转义、Ctrl-C 的 0x03、UTF-8 中文）都必须原样往返。
        let payloads: Vec<Vec<u8>> = vec![
            b"\x1b[2J\x1b[H".to_vec(),
            vec![0x03],
            "中文输出".as_bytes().to_vec(),
            (0u8..=255).collect(),
        ];
        for payload in payloads {
            let frame = ServerFrame::PtyIn {
                req_id: 3,
                data: payload.clone(),
            };
            let encoded = frame.encode();
            let parsed = parse_server_frame(encoded.trim_end_matches('\n')).unwrap();
            assert_eq!(parsed, frame, "PTY_IN 编码/解析不互逆：{encoded:?}");
        }
    }

    #[test]
    fn rejects_malformed_frames() {
        assert!(matches!(
            parse_client_frame("REGISTER:only-two:fields"),
            Err(ProtocolError::Malformed(_))
        ));
        assert!(matches!(
            parse_client_frame("RESULT:not-a-number:AAAA"),
            Err(ProtocolError::Number(_))
        ));
        assert!(matches!(
            parse_client_frame("RESULT:1:!!!not-base64!!!"),
            Err(ProtocolError::Base64(_))
        ));
    }

    #[test]
    fn roundtrips_command_containing_colon_and_newline() {
        let command = "echo a:b\n&& ls";
        let line = format!("CMD:1:{}", encode_text(command));
        match parse_server_frame(&line).unwrap() {
            ServerFrame::Command { command: got, .. } => assert_eq!(got, command),
            other => panic!("unexpected: {other:?}"),
        }
    }
}
