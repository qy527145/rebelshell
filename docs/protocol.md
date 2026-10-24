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

## 客户端状态机

客户端为单线程阻塞循环：`read_line` → 按 `REQ_ID` 分派 → `write_all` 回应。
不启动交互式 Shell 进程，每条命令独立进程、收集输出后即返回。

## 服务端连接模型

每个客户端连接由两个 tokio 任务持有：

- **reader**：负责分帧、`REGISTER` 注册、`PING`→`PONG` 自动应答、
  把 `RESULT` / `FILE_*` / `ERROR` 路由到等待中的请求。
- **writer**：消费 per-connection 的 `mpsc` 通道，串行写出，
  因此命令、分块、下载数据之间不会交错。

请求结果通过 `oneshot` 通道回给 HTTP 处理函数；HTTP 侧只读取一个完成信号 +
共享缓冲，不阻塞其它设备。