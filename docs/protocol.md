# 通信协议参考实现

协议为面向行的文本协议，所有控制帧以 `\n` 结尾，字段以 `:` 分隔。
二进制载荷一律 Base64 编码，避免换行符破坏分帧。

## 帧一览

| 方向 | 帧 | 说明 |
| --- | --- | --- |
| C → S | `REGISTER:<UUID>:<B64(HOSTNAME)>:<OS_TYPE>` | 连接建立后第一条帧 |
| C ↔ S | `PING` / `PONG` | 双向保活，客户端每 10s 发一次 |
| S → C | `CMD:<REQ_ID>:<B64(COMMAND)>` | 下发命令 |
| C → S | `RESULT:<REQ_ID>:<B64(输出)>` | 命令执行结果（stdout+stderr） |
| S → C | `SEND_FILE:<REQ_ID>:<B64(FILENAME)>:<SIZE>:<B64(DATA)>` | 单帧下发小文件 |
| S → C | `PUT_BEGIN:<REQ_ID>:<B64(FILENAME)>:<SIZE>` | 分块下发的开始 |
| S → C | `PUT_CHUNK:<REQ_ID>:<B64(DATA)>` | 分块数据 |
| S → C | `PUT_END:<REQ_ID>` | 分块下发结束 |
| S → C | `GET_FILE:<REQ_ID>:<B64(PATH)>` | 请求提取文件 |
| C → S | `FILE_BEGIN:<REQ_ID>:<SIZE>` | 客户端开始回传文件 |
| C → S | `FILE_CHUNK:<REQ_ID>:<B64(DATA)>` | 客户端回传分块 |
| C → S | `FILE_END:<REQ_ID>:<STATUS>` | 客户端回传结束，`OK` 或 `ERR` |
| C → S | `ERROR:<REQ_ID>:<B64(MESSAGE)>` | 客户端侧错误 |
| S → C | `ACK:<REQ_ID>:<OK\|ERR>` | 服务端对 `FILE_END`/`ERROR` 的确认 |

### 交互式终端

| 方向 | 帧 | 说明 |
| --- | --- | --- |
| S → C | `PTY_OPEN:<REQ_ID>:<COLS>:<ROWS>:<B64(SHELL)>` | 打开终端；shell 为空表示用平台默认值 |
| C → S | `PTY_READY:<REQ_ID>` | 终端已在客户端侧启动，可以收发了 |
| S → C | `PTY_IN:<REQ_ID>:<B64(DATA)>` | 键盘输入（含粘贴、控制序列） |
| C → S | `PTY_OUT:<REQ_ID>:<B64(DATA)>` | 终端输出（ANSI 转义原样透传） |
| S → C | `PTY_RESIZE:<REQ_ID>:<COLS>:<ROWS>` | 窗口大小变化 |
| S → C | `PTY_CLOSE:<REQ_ID>` | 关闭终端 |
| C → S | `PTY_EXIT:<REQ_ID>:<CODE>` | 子进程退出；`CODE` 可为负（被信号杀死） |

终端是**多次往返**的请求：`PTY_OUT` 不是终止帧，同一 `REQ_ID` 会持续收到它，
直到 `PTY_EXIT` 或 `ERROR`。服务端因此把 [`Request`] 拆成发送/接收两半，
主循环才能一边下发按键、一边等输出（见 `server/src/hub.rs` 的 `RequestSender`）。

### 文件管理

| 方向 | 帧 | 说明 |
| --- | --- | --- |
| S → C | `LIST_DIR:<REQ_ID>:<B64(PATH)>` | 列目录；路径为空表示客户端的工作目录 |
| C → S | `DIR_LISTING:<REQ_ID>:<B64(LISTING)>` | 目录列表，格式见下 |
| S → C | `MKDIR:<REQ_ID>:<B64(PATH)>` | 新建目录（等价 `mkdir -p`） |
| S → C | `RENAME:<REQ_ID>:<B64(FROM)>:<B64(TO)>` | 重命名 / 移动 |
| S → C | `DELETE:<REQ_ID>:<B64(PATH)>:<1\|0>` | 删除；目录必须显式要求递归 |
| S → C | `READ_TEXT:<REQ_ID>:<B64(PATH)>` | 读文本文件（供在线编辑） |
| S → C | `WRITE_TEXT:<REQ_ID>:<B64(PATH)>:<B64(CONTENT)>` | 写文本文件（客户端先留 `.bak`） |

除 `LIST_DIR` 外，这些请求的回应都是通用的 `RESULT` / `ERROR`。

#### 目录列表格式

`LISTING` 解码后是一个制表符分隔的文本块：

```
PATH\t<解析后的绝对路径>
<kind>\t<size>\t<mtime_ms>\t<name>
...
```

- `kind`：`d` 目录 / `f` 文件 / `l` 符号链接；
- 条目顺序由客户端排好（目录在前、文件在后，组内不区分大小写按名排序），
  服务端不再重排；
- 之所以不用 JSON：客户端刻意保持「标准库优先」，拼字符串比引一个 JSON 序列化器便宜得多。

## 关键约定

- **一律 Base64**：主机名、命令、文件名、路径、文件数据全部 Base64，
  因此这些字段里出现 `:`、`\n`、中文都不需要额外转义；只有 UUID、OS 类型、
  纯数字大小、`REQ_ID`、状态字保持明文。
- **REQ_ID**：服务端生成的单调递增十进制整数，客户端必须在所有响应帧中原样回带。
- **长度前缀替代方案**：服务端实际按 `\n` 分帧（缓冲扫描），
  读取大帧时设 `MAX_FRAME` 上限（128 MiB）防止内存被撑爆；
  更大的文件走 `PUT_CHUNK` / `FILE_CHUNK` 分块路径。
- **分块大小**：服务端固定 64 KiB 原始数据 / 块，编码后约 87 KiB，
  保证心跳帧不会被长时间的单一 IO 阻塞太久。
- **终端输出分片**：`PTY_OUT` 每片最多 16 KiB，避免某个疯狂刷屏的程序
  （例如 `yes`）用一条巨帧把 socket 写队列灌满、把心跳挤掉。

## 前端与终端之间（WebSocket）

浏览器侧不是这套行协议，而是 WebSocket 上的 JSON（见 `server/src/terminal.rs`）：

| 方向 | 消息 |
| --- | --- |
| 浏览器 → 服务端 | `{"type":"open","cols":120,"rows":30,"shell":""}` |
| 浏览器 → 服务端 | `{"type":"input","data":"<Base64>"}` |
| 浏览器 → 服务端 | `{"type":"resize","cols":120,"rows":30}` |
| 浏览器 → 服务端 | `{"type":"close"}` |
| 服务端 → 浏览器 | `{"type":"ready"}` |
| 服务端 → 浏览器 | `{"type":"output","data":"<Base64>"}` |
| 服务端 → 浏览器 | `{"type":"exit","code":0}` / `{"type":"error","message":"…"}` |

终端输出在这里**仍然**是 Base64，理由与行协议一致：PTY 按字节切数据，
出现半个 UTF-8 字符是常态。前端用 `TextDecoder` 的流式模式跨消息拼接。

命令回显走另一条通道（`/ws`），是纯推送的 JSON，没有这层 Base64。

## 时序

### 命令执行

```
S -> C  CMD:7:<B64("uname -a")>
C -> S  RESULT:7:<B64("Linux vm 6.8.0 ...")>
```

### 下发文件（小文件单帧）

```
S -> C  SEND_FILE:8:<B64("payload.ps1")>:20481:<B64(...)>
C -> S  RESULT:8:<B64("stored payload.ps1 (20481 bytes)")>
```

### 下发文件（大文件分块）

```
S -> C  PUT_BEGIN:9:<B64("dump.bin")>:10485760
S -> C  PUT_CHUNK:9:<B64(chunk0)>
S -> C  PUT_CHUNK:9:<B64(chunk1)>
...
S -> C  PUT_END:9
C -> S  RESULT:9:<B64("stored dump.bin (10485760 bytes)")>
```

### 提取文件

```
S -> C  GET_FILE:10:<B64("/var/log/syslog")>
C -> S  FILE_BEGIN:10:52310
C -> S  FILE_CHUNK:10:<B64(chunk0)>
C -> S  FILE_END:10:OK
S -> C  ACK:10:OK
```

失败路径：`FILE_END:10:ERR`（读取失败）或 `ERROR:10:<B64(原因)>`。

### 交互式终端

```
S -> C  PTY_OPEN:11:120:30:<B64("/bin/bash")>
C -> S  PTY_READY:11
C -> S  PTY_OUT:11:<B64("\x1b[?2004h")>          # shell 的初始化序列
S -> C  PTY_IN:11:<B64("ls\r")>
C -> S  PTY_OUT:11:<B64("file1  file2\r\n")>
S -> C  PTY_RESIZE:11:100:40
C -> S  PTY_OUT:11:<B64("...重新排版后的画面...")>
S -> C  PTY_CLOSE:11
C -> S  PTY_EXIT:11:0
```

注意 `PTY_OUT` 会成批出现，且**每一帧都可能切在多字节字符或转义序列中间** ——
接收方必须按字节流拼接后再解释，不能逐帧当作完整文本。

### 文件管理

```
S -> C  LIST_DIR:12:<B64("/home/vm")>
C -> S  DIR_LISTING:12:<B64("PATH\t/home/vm\nd\t0\t1700000000000\tsrc\nf\t1024\t1700000001000\tREADME.md\n")>

S -> C  MKDIR:13:<B64("/home/vm/logs")>
C -> S  RESULT:13:<B64("已创建目录 /home/vm/logs")>

S -> C  RENAME:14:<B64("/home/vm/a.txt")>:<B64("b.txt")>
C -> S  RESULT:14:<B64("已重命名为 /home/vm/b.txt")>

S -> C  DELETE:15:<B64("/home/vm/logs")>:1
C -> S  RESULT:15:<B64("已递归删除 /home/vm/logs")>

S -> C  READ_TEXT:16:<B64("/home/vm/README.md")>
C -> S  RESULT:16:<B64("# 标题\n正文\n")>

S -> C  WRITE_TEXT:17:<B64("/home/vm/README.md")>:<B64("# 新标题\n")>
C -> S  RESULT:17:<B64("已保存 /home/vm/README.md（13 字节），原文件备份为 /home/vm/README.md.bak")>
```

`RENAME` 的目标若只是个名字（不含路径分隔符），客户端会把它解释成
「同目录下改名」，服务端因此不必自己拼路径。

## 客户端状态机

客户端主线程是单线程循环：`read_line` → 按 `REQ_ID` 分派 → `write_all` 回应。

- **命令 / 文件管理**：每条请求独立处理、立即回应，不保留跨请求状态。
  `READ_TEXT` / `WRITE_TEXT` 会带上/回传整个文件内容（受 2 MiB 上限约束），
  大文件走 `GET_FILE` / `SEND_FILE` 的分块路径。
- **交互式终端**：唯一的跨请求状态（`Session::terminal`）。有终端在跑时 socket
  读超时被压到 15 ms，主循环变成轮询 —— 每轮把终端新产生的输出抽干转发，
  再去等下一批输入。这样 `top` 这类持续刷新的程序不会卡顿，
  而空闲时（没有终端）仍旧回到心跳间隔的长超时，不空转。
- 同一时刻**只支持一个**终端会话；新的 `PTY_OPEN` 会先收掉旧的。

## 服务端连接模型

每个客户端连接由两个 tokio 任务持有：

- **reader**：负责分帧、`REGISTER` 注册、`PING`→`PONG` 自动应答、
  把 `RESULT` / `FILE_*` / `PTY_*` / `DIR_LISTING` / `ERROR` 路由到等待中的请求。
- **writer**：消费 per-connection 的 `mpsc` 通道，串行写出，
  因此命令、分块、下载数据、终端输入之间不会交错。

请求结果通过有界 `mpsc` 回给 HTTP / WebSocket 处理方；HTTP 侧只读取一个完成信号
+ 共享缓冲，不阻塞其它设备。交互式终端需要「同一请求上并发收发」，因此
`Request` 提供了 `split()`：发送半边下发按键、接收半边等输出，
两边各借各的，主循环就是一个普通的双分支 `select!`。

## 单端口分流

服务端只监听一个端口，靠**首帧嗅探**区分两类连接：

- 客户端首帧恒为 `REGISTER`（8 字节）；
- 浏览器首帧恒为 HTTP 方法行（最长的 `OPTIONS ` 也是 8 字节）。

读满 8 字节即可判定，判定后把这 8 字节**原样回灌**给下游
（见 `server/src/listener.rs` 的 `PrefixedStream`），因此 axum 与
`serve_connection` 看到的都是完整字节流。判定不清时按 HTTP 处理。