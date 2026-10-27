//! RebelShell 客户端：反向连接管理端，接收并执行指令、收发文件。
//!
//! 设计目标是「极简、可移植、依赖尽量少」：
//! 单线程阻塞循环，`read` → 分派 → `write` 回应。
//! 命令执行与文件传输完全只用标准库；只有交互式终端为了拿到平台原语
//! （Windows 的 ConPTY、Unix 的 pty）才在对应平台上引入一个官方绑定。
//! 命令执行见 [`execute`]，交互式终端见 [`pty`]，文件管理见 [`fsops`]，
//! 协议分帧见本文件的 `Session`。

mod b64;
mod config;
mod envfile;
mod execute;
mod fsops;
mod pty;
mod service;

use std::collections::VecDeque;
use std::fs::{self, File, OpenOptions};
use std::io::{BufWriter, Read, Write};
use std::net::{TcpStream, ToSocketAddrs};
use std::path::{Path, PathBuf};
use std::process;
use std::time::{Duration, Instant};

use config::Config;
use pty::{Pty, ReadOutcome};

/// 单帧上限，防止对端异常时把内存吃满（分块传输的正常帧远小于此值）。
const MAX_FRAME_BYTES: usize = 256 * 1024 * 1024;
/// 读取缓冲：一次性读这么多，再按 `\n` 切帧。
const READ_CHUNK: usize = 64 * 1024;
/// 回传 / 落盘的分块大小（原始字节）。
const CHUNK_SIZE: usize = 64 * 1024;
/// 终端会话期间的 socket 读超时。
///
/// 有终端在跑时，主循环不能长时间阻塞在 socket 读上——否则终端的输出要等到
/// 下一次管理端发帧才会被转发出去。用一个很短的超时把主循环变成轮询：
/// 超时返回就顺手把终端输出抽干再继续等。15 ms 足够让按键回显感觉是「实时」的，
/// 而调用频率下 CPU 占用可以忽略。
const TERMINAL_POLL: Duration = Duration::from_millis(15);

fn main() {
    // 先补 `.env` 里缺的变量，再解析参数：真实环境变量与命令行参数的优先级不受影响。
    envfile::load();
    let args: Vec<String> = std::env::args().skip(1).collect();

    match args.first().map(String::as_str) {
        Some("install") => exit_on_err(install(&args[1..])),
        Some("uninstall") => exit_on_err(service::uninstall()),
        Some("run") => exit_on_err(run(&args[1..])),
        Some("--help") | Some("-h") | Some("help") => config::print_usage(),
        // 直接带选项启动（例如 systemd 单元里的 `ExecStart=... --server x`）。
        Some(flag) if flag.starts_with('-') => exit_on_err(run(&args)),
        Some(other) => {
            eprintln!("未知子命令 {other:?}\n");
            config::print_usage();
            process::exit(2);
        }
        None => exit_on_err(run(&[])),
    }
}

fn exit_on_err(result: Result<(), String>) {
    if let Err(err) = result {
        eprintln!("[rebelshell] 错误: {err}");
        process::exit(1);
    }
}

fn install(args: &[String]) -> Result<(), String> {
    let config = Config::from_args(args)?;
    service::install(&config)
}

/// 连接管理端并维持会话，断线后按指数退避重连。
fn run(args: &[String]) -> Result<(), String> {
    let config = Config::from_args(args)?;
    println!(
        "[rebelshell] 客户端启动：server={} uuid={} heartbeat={}s dir={}",
        config.server,
        config.uuid,
        config.heartbeat_secs,
        config.workdir.display()
    );

    let mut backoff_secs = 1u64;
    loop {
        match Session::connect(&config).and_then(|mut session| session.serve()) {
            Ok(()) => {
                eprintln!("[rebelshell] 连接已关闭，1 秒后重连");
                backoff_secs = 1;
            }
            Err(err) => {
                eprintln!("[rebelshell] 连接失败: {err}；{backoff_secs} 秒后重试");
                std::thread::sleep(Duration::from_secs(backoff_secs));
                backoff_secs = (backoff_secs * 2).min(30);
                continue;
            }
        }
        std::thread::sleep(Duration::from_secs(1));
    }
}

/// 遇到待处理的 PUT 分块上传。
struct Upload {
    req_id: String,
    path: PathBuf,
    file: BufWriter<File>,
    written: u64,
    expected: u64,
}

/// 一条进行中的交互式终端会话。
///
/// 协议上每个终端占用一个 `REQ_ID`；客户端只支持**一个**并发终端，
/// 新的 `PTY_OPEN` 会先关掉旧的。理由：多开终端意味着要把 socket 读循环
/// 变成多路复用，与「极简客户端」的目标相悖；而实际上一个人同一时刻
/// 也只会盯着一个终端看。需要多开时开多个网页标签页即可。
struct Terminal {
    req_id: String,
    pty: Box<dyn Pty>,
    cols: u16,
    rows: u16,
    /// 已经发过 `PTY_EXIT`，避免重复回传。
    exited: bool,
}

/// 一条与管理端的会话。
struct Session {
    stream: TcpStream,
    config: Config,
    /// 尚未按 `\n` 切分的残留字节。
    buffer: Vec<u8>,
    /// 已切出、待处理的行。
    lines: VecDeque<String>,
    upload: Option<Upload>,
    terminal: Option<Terminal>,
}

impl Session {
    fn connect(config: &Config) -> Result<Self, String> {
        let addr = config
            .server
            .to_socket_addrs()
            .map_err(|err| format!("无法解析地址 {}: {err}", config.server))?
            .next()
            .ok_or_else(|| format!("地址 {} 没有可用的解析结果", config.server))?;

        let stream = TcpStream::connect_timeout(&addr, Duration::from_secs(10))
            .map_err(|err| format!("连接 {addr} 失败: {err}"))?;
        // 关闭 Nagle：命令回显与心跳都是小帧，攒包只会徒增延迟。
        let _ = stream.set_nodelay(true);
        // 读取超时即心跳节拍：超时说明这段时间管理端没有下发任何东西。
        stream
            .set_read_timeout(Some(Duration::from_secs(config.heartbeat_secs)))
            .map_err(|err| format!("设置读超时失败: {err}"))?;

        let mut session = Self {
            stream,
            config: config.clone(),
            buffer: Vec::new(),
            lines: VecDeque::new(),
            upload: None,
            terminal: None,
        };
        session.register()?;
        Ok(session)
    }

    fn register(&mut self) -> Result<(), String> {
        // 主机名做 Base64，避免中文/特殊字符破坏 : 分帧。
        let frame = format!(
            "REGISTER:{}:{}:{}\n",
            self.config.uuid,
            b64::encode(config::hostname().as_bytes()),
            execute::os_type()
        );
        self.write_frame(&frame)
    }

    /// 主循环：切帧 → 分派 → 抽干终端输出 → 必要时发 PING。
    ///
    /// 有终端在跑时读超时会被压到 [`TERMINAL_POLL`]，主循环因此变成「轮询」：
    /// 每 15 ms 醒一次，把终端新产生的输出立刻转发出去。没有终端时恢复成
    /// 心跳间隔的长超时，静默期不空转。
    fn serve(&mut self) -> Result<(), String> {
        let mut chunk = vec![0u8; READ_CHUNK];
        // 上次发 PING 的时间：读超时会因终端而被缩短，不能再用「超时即心跳」。
        let mut last_ping = Instant::now();

        loop {
            while let Some(line) = self.lines.pop_front() {
                self.dispatch(&line)?;
            }

            // 终端可能在 dispatch 里刚被关掉，这里重新判断超时长度。
            let timeout = if self.terminal.is_some() {
                TERMINAL_POLL
            } else {
                Duration::from_secs(self.config.heartbeat_secs)
            };
            self.stream
                .set_read_timeout(Some(timeout))
                .map_err(|err| format!("设置读超时失败: {err}"))?;

            match self.stream.read(&mut chunk) {
                Ok(0) => return Ok(()), // 管理端关闭了连接
                Ok(read) => {
                    self.buffer.extend_from_slice(&chunk[..read]);
                    self.take_lines();
                    // 切完完整帧后仍然剩下的就是不完整帧；只有它超限才算协议异常，
                    // 否则一条正常的大帧也会被误判（每轮最多多出 READ_CHUNK）。
                    if self.buffer.len() > MAX_FRAME_BYTES {
                        return Err(format!(
                            "单帧累积 {} 字节仍未见换行，超过上限 {MAX_FRAME_BYTES}",
                            self.buffer.len()
                        ));
                    }
                }
                Err(err) if is_timeout(&err) => {
                    // 静默期已到，主动心跳保活，防止中间路由器 NAT 表超时。
                    // 终端模式下超时被压得很短，因此按实际经过时间判断，
                    // 而不是「一超时就发」。
                    if self.terminal.is_none()
                        || last_ping.elapsed() >= Duration::from_secs(self.config.heartbeat_secs)
                    {
                        self.write_frame("PING\n")?;
                        last_ping = Instant::now();
                    }
                }
                Err(err) => return Err(format!("读取失败: {err}")),
            }

            // 无论这一轮是读到帧、超时还是刚处理完输入，都要把终端输出抽干，
            // 否则 `top` 这类持续刷新的程序会看到明显的卡顿。
            self.drain_terminal()?;
        }
    }

    /// 把终端当前可读的输出全部转发给管理端。
    ///
    /// 一次调用最多转发 [`MAX_PTY_BURST`] 帧，避免某个疯狂刷屏的程序
    /// （例如 `yes`）把 socket 写队列灌满、把心跳挤掉。
    fn drain_terminal(&mut self) -> Result<(), String> {
        const MAX_PTY_BURST: usize = 64;

        for _ in 0..MAX_PTY_BURST {
            let Some(terminal) = self.terminal.as_mut() else {
                return Ok(());
            };
            if terminal.exited {
                return Ok(());
            }

            match terminal.pty.read() {
                Ok(ReadOutcome::Data(data)) => {
                    if data.is_empty() {
                        continue;
                    }
                    let frame = format!(
                        "PTY_OUT:{}:{}\n",
                        terminal.req_id,
                        b64::encode(&data)
                    );
                    self.reply(frame)?;
                }
                Ok(ReadOutcome::Idle) => return Ok(()),
                Ok(ReadOutcome::Closed) => {
                    return self.finish_terminal();
                }
                Err(err) => {
                    // 读失败（PTY 被拆掉等）也按会话结束处理，并且要让管理端知道。
                    eprintln!("[rebelshell] 读取终端输出失败: {err}");
                    return self.finish_terminal();
                }
            }
        }
        Ok(())
    }

    /// 子进程结束后收尾：回 `PTY_EXIT` 并释放 PTY。
    fn finish_terminal(&mut self) -> Result<(), String> {
        let Some(mut terminal) = self.terminal.take() else {
            return Ok(());
        };
        if terminal.exited {
            return Ok(());
        }
        terminal.exited = true;

        // 可能还有最后一点输出没取出来（子进程退出前打印的收尾信息）。
        loop {
            match terminal.pty.read() {
                Ok(ReadOutcome::Data(data)) if !data.is_empty() => {
                    self.reply(format!("PTY_OUT:{}:{}\n", terminal.req_id, b64::encode(&data)))?;
                }
                Ok(ReadOutcome::Data(_)) | Ok(ReadOutcome::Idle) => break,
                Ok(ReadOutcome::Closed) | Err(_) => break,
            }
        }

        let code = terminal.pty.wait().ok().flatten().unwrap_or(0);
        terminal.pty.kill();
        eprintln!("[rebelshell] 终端 #{0} 已结束（退出码 {code}）", terminal.req_id);
        self.reply(format!("PTY_EXIT:{}:{code}\n", terminal.req_id))
    }

    /// 把缓冲区里所有完整的行移到待处理队列。
    fn take_lines(&mut self) {
        while let Some(pos) = self.buffer.iter().position(|byte| *byte == b'\n') {
            let mut line: Vec<u8> = self.buffer.drain(..=pos).collect();
            line.pop(); // 丢掉 '\n'
            if line.last() == Some(&b'\r') {
                line.pop();
            }
            match String::from_utf8(line) {
                Ok(line) if !line.is_empty() => self.lines.push_back(line),
                Ok(_) => {}
                Err(err) => eprintln!("[rebelshell] 丢弃非 UTF-8 帧: {err}"),
            }
        }
    }

    fn write_frame(&mut self, frame: &str) -> Result<(), String> {
        self.stream
            .write_all(frame.as_bytes())
            .map_err(|err| format!("写帧失败: {err}"))?;
        self.stream
            .flush()
            .map_err(|err| format!("flush 失败: {err}"))
    }

    fn reply(&mut self, frame: String) -> Result<(), String> {
        self.write_frame(&frame)
    }

    /// 把执行结果 / 错误按 `RESULT:req:...` 回传。
    fn reply_result(&mut self, req_id: &str, text: &str) -> Result<(), String> {
        self.reply(format!(
            "RESULT:{req_id}:{}\n",
            b64::encode(text.as_bytes())
        ))
    }

    fn reply_error(&mut self, req_id: &str, text: &str) -> Result<(), String> {
        self.reply(format!("ERROR:{req_id}:{}\n", b64::encode(text.as_bytes())))
    }

    fn dispatch(&mut self, line: &str) -> Result<(), String> {
        let (tag, rest) = match line.split_once(':') {
            Some((tag, rest)) => (tag, rest),
            None => (line, ""),
        };

        match tag {
            "PING" => self.reply("PONG\n".to_string()),
            "PONG" => Ok(()),
            "CMD" => self.on_cmd(rest),
            "SEND_FILE" => self.on_send_file(rest),
            "PUT_BEGIN" => self.on_put_begin(rest),
            "PUT_CHUNK" => self.on_put_chunk(rest),
            "PUT_END" => self.on_put_end(rest),
            "GET_FILE" => self.on_get_file(rest),
            // ---- 交互式终端 ----
            "PTY_OPEN" => self.on_pty_open(rest),
            "PTY_IN" => self.on_pty_in(rest),
            "PTY_RESIZE" => self.on_pty_resize(rest),
            "PTY_CLOSE" => self.on_pty_close(rest),
            // ---- 文件管理 ----
            "LIST_DIR" => self.on_list_dir(rest),
            "MKDIR" => self.on_mkdir(rest),
            "RENAME" => self.on_rename(rest),
            "DELETE" => self.on_delete(rest),
            "READ_TEXT" => self.on_read_text(rest),
            "WRITE_TEXT" => self.on_write_text(rest),
            "ACK" => {
                println!("[rebelshell] 管理端确认: {rest}");
                Ok(())
            }
            other => {
                eprintln!("[rebelshell] 未知帧 {other:?}，已忽略");
                Ok(())
            }
        }
    }

    // ---- 命令 ----

    /// `CMD:<REQ_ID>:<B64(COMMAND)>`
    fn on_cmd(&mut self, rest: &str) -> Result<(), String> {
        let (req_id, encoded) = split_pair(rest, "CMD")?;
        let command = decode_field(encoded, "命令")?;

        println!("[rebelshell] 执行 #{req_id}: {command}");
        let output = execute::run(&command);
        self.reply_result(req_id, &output)
    }

    // ---- 下发文件 ----

    /// `SEND_FILE:<REQ_ID>:<B64(FILENAME)>:<SIZE>:<B64(DATA)>`
    fn on_send_file(&mut self, rest: &str) -> Result<(), String> {
        let fields: Vec<&str> = rest.splitn(4, ':').collect();
        if fields.len() != 4 {
            return Err(format!("SEND_FILE 字段数错误: {}", fields.len()));
        }
        let (req_id, name, size, data) = (fields[0], fields[1], fields[2], fields[3]);

        let name = decode_field(name, "文件名")?;
        let expected: usize = size
            .parse()
            .map_err(|_| format!("SEND_FILE 文件大小非法: {size:?}"))?;
        let bytes = b64::decode(data).map_err(|err| format!("SEND_FILE 数据解码失败: {err}"))?;

        if bytes.len() != expected {
            self.reply_error(
                req_id,
                &format!("数据长度不符：声明 {expected} 字节，实际 {}", bytes.len()),
            )?;
            return Ok(());
        }

        let path = self.resolve(&name);
        match write_whole_file(&path, &bytes) {
            Ok(()) => {
                println!(
                    "[rebelshell] 已写入 {} ({} 字节)",
                    path.display(),
                    bytes.len()
                );
                self.reply_result(req_id, &format!("stored {} ({} bytes)", name, bytes.len()))
            }
            Err(err) => self.reply_error(req_id, &err),
        }
    }

    /// `PUT_BEGIN:<REQ_ID>:<B64(FILENAME)>:<SIZE>`
    fn on_put_begin(&mut self, rest: &str) -> Result<(), String> {
        let fields: Vec<&str> = rest.splitn(3, ':').collect();
        if fields.len() != 3 {
            return Err(format!("PUT_BEGIN 字段数错误: {}", fields.len()));
        }
        let (req_id, name, size) = (fields[0], fields[1], fields[2]);
        let name = decode_field(name, "文件名")?;
        let expected: u64 = size
            .parse()
            .map_err(|_| format!("PUT_BEGIN 文件大小非法: {size:?}"))?;

        // 同一时刻只允许一个上传；新的开始帧会终止上一个未完成的上传。
        if let Some(prev) = self.upload.take() {
            self.reply_error(
                &prev.req_id,
                &format!("上传被新的 PUT_BEGIN 中断，已接收 {} 字节", prev.written),
            )?;
        }

        let path = self.resolve(&name);
        if let Some(parent) = path.parent() {
            if !parent.as_os_str().is_empty() {
                fs::create_dir_all(parent)
                    .map_err(|err| format!("创建目录 {} 失败: {err}", parent.display()))?;
            }
        }
        let file = OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .open(&path)
            .map_err(|err| format!("创建 {} 失败: {err}", path.display()))?;

        self.upload = Some(Upload {
            req_id: req_id.to_string(),
            path,
            file: BufWriter::new(file),
            written: 0,
            expected,
        });
        Ok(())
    }

    /// `PUT_CHUNK:<REQ_ID>:<B64(DATA)>`
    fn on_put_chunk(&mut self, rest: &str) -> Result<(), String> {
        let (req_id, encoded) = split_pair(rest, "PUT_CHUNK")?;
        let bytes = b64::decode(encoded).map_err(|err| format!("PUT_CHUNK 解码失败: {err}"))?;

        let Some(upload) = self.upload.as_mut() else {
            return Err("收到 PUT_CHUNK 但没有进行中的上传".to_string());
        };
        if upload.req_id != req_id {
            return Err(format!(
                "PUT_CHUNK 请求号 {req_id} 与进行中的上传 {} 不匹配",
                upload.req_id
            ));
        }
        upload
            .file
            .write_all(&bytes)
            .map_err(|err| format!("写入 {} 失败: {err}", upload.path.display()))?;
        upload.written += bytes.len() as u64;
        Ok(())
    }

    /// `PUT_END:<REQ_ID>`
    fn on_put_end(&mut self, rest: &str) -> Result<(), String> {
        let req_id = rest.split(':').next().unwrap_or("");
        let Some(mut upload) = self.upload.take() else {
            return Err("收到 PUT_END 但没有进行中的上传".to_string());
        };
        if upload.req_id != req_id {
            return Err(format!(
                "PUT_END 请求号 {req_id} 与进行中的上传 {} 不匹配",
                upload.req_id
            ));
        }

        let result = upload
            .file
            .flush()
            .map_err(|err| format!("flush {} 失败: {err}", upload.path.display()));

        match result {
            Ok(()) if upload.written == upload.expected => {
                println!(
                    "[rebelshell] 已写入 {} ({} 字节)",
                    upload.path.display(),
                    upload.written
                );
                self.reply_result(
                    &upload.req_id,
                    &format!(
                        "stored {} ({} bytes)",
                        upload.path.display(),
                        upload.written
                    ),
                )
            }
            Ok(()) => self.reply_error(
                &upload.req_id,
                &format!(
                    "分块总长不符：声明 {} 字节，实际 {}",
                    upload.expected, upload.written
                ),
            ),
            Err(err) => self.reply_error(&upload.req_id, &err),
        }
    }

    // ---- 提取文件 ----

    /// `GET_FILE:<REQ_ID>:<B64(PATH)>`
    fn on_get_file(&mut self, rest: &str) -> Result<(), String> {
        let (req_id, encoded) = split_pair(rest, "GET_FILE")?;
        let requested = decode_field(encoded, "路径")?;
        let path = self.resolve(&requested);

        let mut file = match File::open(&path) {
            Ok(file) => file,
            Err(err) => {
                return self.reply_error(req_id, &format!("打开 {} 失败: {err}", path.display()))
            }
        };
        let size = match file.metadata() {
            Ok(meta) => meta.len(),
            Err(err) => return self.reply_error(req_id, &format!("读取元信息失败: {err}")),
        };

        println!("[rebelshell] 回传 {} ({} 字节)", path.display(), size);
        self.reply(format!("FILE_BEGIN:{req_id}:{size}\n"))?;

        // 边读边发，不把整个文件读进内存。
        let mut buffer = vec![0u8; CHUNK_SIZE];
        let mut sent = 0u64;
        loop {
            let read = match file.read(&mut buffer) {
                Ok(0) => break,
                Ok(read) => read,
                Err(err) => {
                    // 已经发出 FILE_BEGIN，只能用 FILE_END:ERR 收尾。
                    eprintln!("[rebelshell] 读取 {} 中断: {err}", path.display());
                    return self.reply(format!("FILE_END:{req_id}:ERR\n"));
                }
            };
            sent += read as u64;
            self.reply(format!(
                "FILE_CHUNK:{req_id}:{}\n",
                b64::encode(&buffer[..read])
            ))?;
        }

        if sent != size {
            eprintln!("[rebelshell] 文件在传输中被修改：声明 {size} 字节，实发 {sent}");
            return self.reply(format!("FILE_END:{req_id}:ERR\n"));
        }
        self.reply(format!("FILE_END:{req_id}:OK\n"))
    }

    /// 相对路径以客户端工作目录为基准，绝对路径原样使用。
    fn resolve(&self, name: &str) -> PathBuf {
        let path = Path::new(name);
        if path.is_absolute() {
            path.to_path_buf()
        } else {
            self.config.workdir.join(path)
        }
    }

    // ---- 交互式终端 ----

    /// `PTY_OPEN:<REQ_ID>:<COLS>:<ROWS>:<B64(SHELL)>`
    fn on_pty_open(&mut self, rest: &str) -> Result<(), String> {
        let fields: Vec<&str> = rest.splitn(4, ':').collect();
        if fields.len() != 4 {
            return Err(format!("PTY_OPEN 字段数错误: {}", fields.len()));
        }
        let (req_id, cols, rows, shell) = (fields[0], fields[1], fields[2], fields[3]);
        let cols: u16 = cols
            .parse()
            .map_err(|_| format!("PTY_OPEN 列数非法: {cols:?}"))?;
        let rows: u16 = rows
            .parse()
            .map_err(|_| format!("PTY_OPEN 行数非法: {rows:?}"))?;
        // shell 允许为空（用平台默认值），因此不能因为解码出空串就报错。
        let shell = if shell.is_empty() {
            String::new()
        } else {
            decode_field(shell, "shell")?
        };

        // 同一时刻只支持一个终端；旧的先收尾，免得留下没人认领的 PTY 进程。
        if self.terminal.is_some() {
            self.finish_terminal()?;
        }

        match pty::spawn(&shell, cols, rows) {
            Ok(handle) => {
                eprintln!("[rebelshell] 终端 #{req_id} 已启动（{cols}x{rows}）");
                self.terminal = Some(Terminal {
                    req_id: req_id.to_string(),
                    pty: handle,
                    cols,
                    rows,
                    exited: false,
                });
                // 主动报一声「起来了」：管理端据此告诉浏览器可以开始收发了，
                // 而不是让前端靠「收到第一段输出」去猜（空闲的 shell 可能几百毫秒
                // 都不吐一个字）。
                self.reply(format!("PTY_READY:{req_id}\n"))
            }
            Err(err) => {
                // 启动失败要走 ERROR 帧，让网页上的终端显示原因而不是空屏。
                self.reply_error(
                    req_id,
                    &format!("无法启动交互式终端（{cols}x{rows}）: {err}"),
                )
            }
        }
    }

    /// `PTY_IN:<REQ_ID>:<B64(DATA)>`
    fn on_pty_in(&mut self, rest: &str) -> Result<(), String> {
        let (req_id, encoded) = split_pair(rest, "PTY_IN")?;
        let Some(terminal) = self.terminal.as_mut() else {
            // 终端已经结束（例如 shell 被 exit 掉了），静默忽略按键，
            // 而不是让整个会话因为一次迟到输入而断开。
            return Ok(());
        };
        if terminal.req_id != req_id {
            return Ok(());
        }
        let data = b64::decode(encoded).map_err(|err| format!("PTY_IN 解码失败: {err}"))?;
        if let Err(err) = terminal.pty.write(&data) {
            eprintln!("[rebelshell] 写入终端失败: {err}");
            return self.finish_terminal();
        }
        Ok(())
    }

    /// `PTY_RESIZE:<REQ_ID>:<COLS>:<ROWS>`
    fn on_pty_resize(&mut self, rest: &str) -> Result<(), String> {
        let fields: Vec<&str> = rest.splitn(3, ':').collect();
        if fields.len() != 3 {
            return Err(format!("PTY_RESIZE 字段数错误: {}", fields.len()));
        }
        let cols: u16 = fields[1]
            .parse()
            .map_err(|_| format!("PTY_RESIZE 列数非法: {:?}", fields[1]))?;
        let rows: u16 = fields[2]
            .parse()
            .map_err(|_| format!("PTY_RESIZE 行数非法: {:?}", fields[2]))?;

        if let Some(terminal) = self.terminal.as_mut() {
            if terminal.req_id != fields[0] {
                return Ok(());
            }
            terminal.cols = cols;
            terminal.rows = rows;
            if let Err(err) = terminal.pty.resize(cols, rows) {
                // 尺寸调整失败不影响会话继续（只是显示会挤在一起）。
                eprintln!("[rebelshell] 调整终端大小失败: {err}");
            }
        }
        Ok(())
    }

    /// `PTY_CLOSE:<REQ_ID>`
    fn on_pty_close(&mut self, rest: &str) -> Result<(), String> {
        let req_id = rest.split(':').next().unwrap_or("");
        match self.terminal.as_ref() {
            Some(terminal) if terminal.req_id == req_id => {
                // 管理端主动关闭时不回 PTY_EXIT：对方已经走了，
                // 回帧只会让服务端记下一条没有等待者的结果。
                let _ = self.terminal.take();
                Ok(())
            }
            _ => Ok(()),
        }
    }

    // ---- 文件管理 ----

    /// `LIST_DIR:<REQ_ID>:<B64(PATH)>`
    fn on_list_dir(&mut self, rest: &str) -> Result<(), String> {
        let (req_id, encoded) = split_pair(rest, "LIST_DIR")?;
        let requested = decode_field(encoded, "路径")?;
        // 空路径 = 客户端工作目录，方便前端一上来就有内容可看。
        let path = if requested.trim().is_empty() {
            self.config.workdir.clone()
        } else {
            self.resolve(&requested)
        };

        match fsops::list_dir(&path) {
            Ok(listing) => self.reply(format!(
                "DIR_LISTING:{req_id}:{}\n",
                b64::encode(listing.as_bytes())
            )),
            Err(err) => self.reply_error(req_id, &err),
        }
    }

    /// `MKDIR:<REQ_ID>:<B64(PATH)>`
    fn on_mkdir(&mut self, rest: &str) -> Result<(), String> {
        let (req_id, encoded) = split_pair(rest, "MKDIR")?;
        let requested = decode_field(encoded, "路径")?;
        let path = self.resolve(&requested);
        match fsops::make_dir(&path) {
            Ok(message) => self.reply_result(req_id, &message),
            Err(err) => self.reply_error(req_id, &err),
        }
    }

    /// `RENAME:<REQ_ID>:<B64(FROM)>:<B64(TO)>`
    fn on_rename(&mut self, rest: &str) -> Result<(), String> {
        let fields: Vec<&str> = rest.splitn(3, ':').collect();
        if fields.len() != 3 {
            return Err(format!("RENAME 字段数错误: {}", fields.len()));
        }
        let from = decode_field(fields[1], "源路径")?;
        let to = decode_field(fields[2], "目标路径")?;
        // 目标若是纯名字（不含分隔符）则视作「同目录下改名」，
        // 这样前端只需要提交新名字，不必自己拼路径。
        let from = self.resolve(&from);
        let to = if to.contains('/') || to.contains('\\') {
            self.resolve(&to)
        } else {
            from.parent().unwrap_or(Path::new(".")).join(&to)
        };

        match fsops::rename(&from, &to) {
            Ok(message) => self.reply_result(fields[0], &message),
            Err(err) => self.reply_error(fields[0], &err),
        }
    }

    /// `DELETE:<REQ_ID>:<B64(PATH)>:<1|0>`
    fn on_delete(&mut self, rest: &str) -> Result<(), String> {
        let fields: Vec<&str> = rest.splitn(3, ':').collect();
        if fields.len() != 3 {
            return Err(format!("DELETE 字段数错误: {}", fields.len()));
        }
        let requested = decode_field(fields[1], "路径")?;
        let recursive = fields[2] == "1";
        let path = self.resolve(&requested);

        match fsops::delete(&path, recursive) {
            Ok(message) => self.reply_result(fields[0], &message),
            Err(err) => self.reply_error(fields[0], &err),
        }
    }

    /// `READ_TEXT:<REQ_ID>:<B64(PATH)>`
    fn on_read_text(&mut self, rest: &str) -> Result<(), String> {
        let (req_id, encoded) = split_pair(rest, "READ_TEXT")?;
        let requested = decode_field(encoded, "路径")?;
        let path = self.resolve(&requested);
        match fsops::read_text(&path) {
            Ok(content) => self.reply_result(req_id, &content),
            Err(err) => self.reply_error(req_id, &err),
        }
    }

    /// `WRITE_TEXT:<REQ_ID>:<B64(PATH)>:<B64(CONTENT)>`
    fn on_write_text(&mut self, rest: &str) -> Result<(), String> {
        let fields: Vec<&str> = rest.splitn(3, ':').collect();
        if fields.len() != 3 {
            return Err(format!("WRITE_TEXT 字段数错误: {}", fields.len()));
        }
        let requested = decode_field(fields[1], "路径")?;
        let content = decode_field(fields[2], "内容")?;
        let path = self.resolve(&requested);
        match fsops::write_text(&path, &content) {
            Ok(message) => self.reply_result(fields[0], &message),
            Err(err) => self.reply_error(fields[0], &err),
        }
    }
}

/// 判断读超时（不同平台错误类型不同，统一按 kind 与文案匹配）。
fn is_timeout(err: &std::io::Error) -> bool {
    matches!(
        err.kind(),
        std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
    )
}

/// 按第一个 `:` 拆成 (请求号, 载荷)。
fn split_pair<'a>(rest: &'a str, frame: &str) -> Result<(&'a str, &'a str), String> {
    rest.split_once(':')
        .ok_or_else(|| format!("{frame} 缺少字段: {rest:?}"))
}

fn decode_field(encoded: &str, what: &str) -> Result<String, String> {
    let bytes = b64::decode(encoded).map_err(|err| format!("{what} Base64 解码失败: {err}"))?;
    String::from_utf8(bytes).map_err(|err| format!("{what} 不是合法的 UTF-8: {err}"))
}

fn write_whole_file(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        if !parent.as_os_str().is_empty() {
            fs::create_dir_all(parent)
                .map_err(|err| format!("创建目录 {} 失败: {err}", parent.display()))?;
        }
    }
    fs::write(path, bytes).map_err(|err| format!("写入 {} 失败: {err}", path.display()))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn session() -> Session {
        // 仅用于测试纯函数式的分派逻辑，不实际连接网络。
        // 已 bind + listen 的 socket 其 backlog 足以让 connect 立即成功，无需 accept。
        let listener = std::net::TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpStream::connect(addr).unwrap();
        Session {
            stream: client,
            config: Config {
                server: addr.to_string(),
                uuid: "test".to_string(),
                heartbeat_secs: 10,
                workdir: std::env::temp_dir(),
            },
            buffer: Vec::new(),
            lines: VecDeque::new(),
            upload: None,
            terminal: None,
        }
    }

    #[test]
    fn splits_lines_and_keeps_partial() {
        let mut session = session();
        session.buffer = b"PONG\nFILE_END:3:OK\npartial".to_vec();
        session.take_lines();
        assert_eq!(session.lines.len(), 2);
        assert_eq!(session.lines[0], "PONG");
        assert_eq!(session.lines[1], "FILE_END:3:OK");
        assert_eq!(session.buffer, b"partial");
    }

    #[test]
    fn strips_carriage_return() {
        let mut session = session();
        session.buffer = b"PONG\r\n".to_vec();
        session.take_lines();
        assert_eq!(session.lines.pop_front().unwrap(), "PONG");
    }

    #[test]
    fn resolves_relative_to_workdir() {
        let session = session();
        let expected = session.config.workdir.join("payload.ps1");
        assert_eq!(session.resolve("payload.ps1"), expected);
    }

    #[test]
    fn keeps_absolute_path() {
        let session = session();
        let abs = if cfg!(windows) {
            r"C:\temp\x.log"
        } else {
            "/var/log/x.log"
        };
        assert_eq!(session.resolve(abs), PathBuf::from(abs));
    }

    #[test]
    fn rejects_short_send_file() {
        let mut session = session();
        assert!(session.on_send_file("1:abc").is_err());
    }

    #[test]
    fn ignores_keyboard_input_without_terminal() {
        // 终端已结束时的迟到按键不该让会话断开，只应被忽略。
        let mut session = session();
        assert!(session.on_pty_in("7:AAAA").is_ok());
        assert!(session.on_pty_resize("7:80:24").is_ok());
        assert!(session.on_pty_close("7").is_ok());
    }

    #[test]
    fn rename_with_bare_name_stays_in_same_directory() {
        // 前端只提交新名字时，目标应该落在源文件所在目录里，而不是工作目录根部。
        let session = session();
        let source = session.config.workdir.join("keep").join("old.txt");
        let target = source.parent().unwrap().join("new.txt");
        assert_eq!(target, session.config.workdir.join("keep").join("new.txt"));
    }

    #[test]
    fn empty_list_dir_path_falls_back_to_workdir() {
        // 空路径表示「工作目录」，客户端要能自己兜底而不是把它当相对路径拼。
        let session = session();
        let workdir = session.config.workdir.clone();
        assert_eq!(session.resolve(""), workdir.join(""));
    }
}
