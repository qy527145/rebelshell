# RebelShell

轻量级反向远程管理系统（Rust + Web）。

管理端（Server）监听**一个**固定端口，内网虚拟机（Client）主动出站连接并维持长连接；
管理员用浏览器统一操作终端、执行命令、管理文件。

```
[ 虚拟机 / Client ] --(主动出站 TCP:8080)--> [ 管理员机器 / Rust Server ]
   PTY / 命令 / 文件                                  |  ADMIN_TOKEN 鉴权 + REST + WebSocket
                                                 [ Web 浏览器界面 (React + xterm.js) ]
```

**同一个端口既接浏览器、也接虚拟机**：服务端按「首帧」分流 —— 客户端的首帧一定是
`REGISTER:…`，浏览器的一定是 `GET /` 之类的方法行，两者不会混淆。
因此内网策略只需要放行一个端口。

## 管理员能做什么

对每一台在线设备，界面顶部有三个标签：

| 标签 | 能力 |
| --- | --- |
| **终端** | xterm.js 渲染的**真 PTY 会话**。`vi` / `top` / `htop` 等全屏程序、ANSI 颜色、Tab 补全、`Ctrl-C`、窗口自适应都正常工作，回滚缓冲 10000 行 |
| **命令** | 「下发一条命令 → 收完整输出」的一次性执行，适合写脚本；另附两条按绝对路径收发文件的快捷通道 |
| **文件** | 单栏远程文件管理器：目录浏览、面包屑、上传（含拖拽）、下载、新建文件夹、重命名、删除、文本文件在线编辑 |

终端之所以能做到接近 Xshell 的体验，是因为虚拟机客户端上跑的是**真伪终端**
（Windows 用 ConPTY，Linux/macOS 用 `posix_openpt` + `fork`），而不是
「执行一条命令、收集输出」的模拟。

## 目录结构

整个仓库由根目录的 bun workspaces + turbo 统一编排：`web/`、`server/`、`client/`
各自是一个 workspace 包（后两个是 cargo 壳包，只为接入编排）。

| 路径 | 说明 |
| --- | --- |
| [package.json](package.json) | 根包：bun workspaces、全部入口脚本 |
| [turbo.json](turbo.json) | 任务图与缓存规则（前端产物落点在这里声明） |
| [dist-workspace.toml](dist-workspace.toml) | cargo-dist 发版配置（GitHub Release 打包） |
| [.env.example](.env.example) | 本机配置模板，`cp .env.example .env` 后按需改 |
| [server/](server/) | Rust 管理端：单端口监听 + 嗅探分流 + axum Web 服务 |
| [server/src/listener.rs](server/src/listener.rs) | 首帧嗅探：同一个端口区分 Web 与客户端连接 |
| [server/src/terminal.rs](server/src/terminal.rs) | 交互式终端的 WebSocket 桥（xterm.js ↔ PTY） |
| [client/](client/) | Rust 客户端（虚拟机端） |
| [client/src/pty.rs](client/src/pty.rs) | 真伪终端：ConPTY（Windows）/ pty（Unix） |
| [client/src/fsops.rs](client/src/fsops.rs) | 文件管理操作：列目录、新建、重命名、删除、读写文本 |
| [web/](web/) | React + Vite + TypeScript 管理界面 |
| [scripts/](scripts/) | 端到端冒烟测试与 WebSocket 断言 |
| [docs/protocol.md](docs/protocol.md) | 通信协议参考实现说明 |
| [docs/PRD.md](docs/PRD.md) | 原始需求说明书 |

## 快速开始

只需要 bun（前端与编排）和 cargo（两个 Rust crate）。

```bash
bun install                # 安装前端依赖、turbo 等
cp .env.example .env       # 本机配置；.env 不入库
bun run build              # 一条命令构建全部：前端 → server（内嵌前端）→ client
```

产物位置：
- `server/target/release/rebelshell-server[.exe]`（已内嵌前端页面）
- `client/target/release/rebelshell-client[.exe]`

### 全部脚本

| 命令 | 说明 |
| --- | --- |
| `bun run build` | 构建全部包。turbo 会自动保证「先前端、后 server」的顺序 |
| `bun run dev` | 开发模式：Vite 热更新（5173）+ 服务端 + 客户端一起起 |
| `bun run dev:web` / `dev:server` / `dev:client` | 只起其中一个 |
| `bun run test` | 两个 crate 的单元测试（含 ConPTY / pty 的真实进程测试） |
| `bun run lint` | `cargo clippy --all-targets -- -D warnings` + 前端 `tsc --noEmit` |
| `bun run typecheck` | `cargo check --all-targets` + 前端 `tsc --noEmit` |
| `bun run smoke` | 端到端冒烟：拉起真实二进制跑 46 项断言（含终端与文件管理） |
| `bun run ws-check` | WebSocket 推送内容断言（需服务端与客户端已在跑） |
| `bun run dist` | 用 cargo-dist 本地打包发布产物到 `target/distrib/` |
| `bun run dist:plan` | 只看会产出哪些发布产物，不实际构建 |
| `bun run ci` | lint + typecheck + test + smoke，提交前跑一遍 |
| `bun run clean` | 清理构建产物（保留依赖） |
| `bun run nuke` | 连依赖一起清掉 |

单个包用 `--filter`：

```bash
bun run --filter=rebelshell-client test
bun run --filter=rebelshell-web build
```

### 构建顺序为什么由 turbo 钉死

`server/` 通过 `rust-embed` 把 `server/web/dist` **编译进二进制**
（见 [server/src/assets.rs](server/src/assets.rs)），所以前端必须先构建。
`turbo.json` 里 `rebelshell-server#build` 声明了 `dependsOn: ["rebelshell-web#build"]`，
`rebelshell-web#build` 又声明了 `outputs: ["../server/web/dist/**"]` ——
跨包产物因此能被 turbo 正确缓存与恢复，删掉 `server/web/dist` 再 build
也能从缓存里还原，不会掉进「内嵌了占位页」的坑。

`server/build.rs` 仍保留兜底：目录缺失时补一个占位 `index.html`，
让「先编译服务端、后构建前端」的顺序也能成功，页面会提示先构建前端。

## 手动构建（不走 bun 编排）

想绕开 turbo、直接调 cargo 也行，但**必须自己先构建前端**：

```bash
cd web && bun install && bun run build      # 产物写入 server/web/dist
cd ../server && cargo build --release
cd ../client && cargo build --release
```

### 启动管理端

```bash
ADMIN_TOKEN=my-secret-token PORT=8080 ./server/target/release/rebelshell-server
```

开发期直接用 `bun run dev`（Vite 热更新 + 服务端 + 客户端一起起）更方便。
直接跑二进制时它也会自动读取 `.env`，所以下面这条命令与 `bun run dev` 用的是同一份配置。

打开 <http://127.0.0.1:8080> ，输入 `ADMIN_TOKEN` 登录。

### 在虚拟机上运行客户端

```bash
cd client
cargo build --release            # 产物 target/release/rebelshell-client
```

```bash
# 连接地址就是管理端那一个端口
rebelshell-client --server 192.168.1.10:8080 --uuid agent-01
# 或
REBELSHELL_SERVER=192.168.1.10:8080 REBELSHELL_UUID=agent-01 rebelshell-client

# 安装为后台常驻服务（Windows 计划任务 / Linux systemd）
rebelshell-client install --server 192.168.1.10:8080
rebelshell-client uninstall
```

不指定 `--uuid` 时，客户端会在本地生成并持久化一个 UUID
（Windows：`%APPDATA%\RebelShell\agent-id`，Linux：`~/.config/rebelshell/agent-id`），
保证重连后仍被识别为同一台机器。

> **交互式终端会以启动客户端时的身份运行 shell**。
> 装成 Windows 计划任务（`SYSTEM`）或 systemd 服务（`root`）时，
> 管理员在网页上打开的就是一个 SYSTEM / root 的终端，权限很高 ——
> `ADMIN_TOKEN` 因此等同于这些机器的最高权限，务必只在受信网络上使用。

## 配置项

### Server

| 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `ADMIN_TOKEN` | `rebelshell`（并在日志中告警） | 全局管理令牌，Web 端登录与所有接口鉴权均使用它 |
| `PORT` | `8080` | **唯一的**监听端口：Web 界面与客户端反向连接共用 |
| `BIND_ADDR` | `0.0.0.0` | 绑定地址 |
| `COMMAND_TIMEOUT_SECS` | `60` | 命令执行的等待上限 |
| `TRANSFER_TIMEOUT_SECS` | `300` | 文件传输的等待上限 |
| `PUSH_INTERVAL_SECS` | `3` | 设备列表变化检测与推送间隔 |
| `RUST_LOG` | `info,tower_http=warn` | 日志过滤，如 `RUST_LOG=rebelshell_server=debug` |

### Client

| 参数 / 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `--server` / `REBELSHELL_SERVER` | `127.0.0.1:8080` | 管理端地址（就是管理端唯一的那个端口） |
| `--uuid` / `REBELSHELL_UUID` | 本地持久化生成 | 设备唯一标识 |
| `--heartbeat` / `REBELSHELL_HEARTBEAT` | `10` | PING 间隔（秒） |
| `--dir` / `REBELSHELL_DIR` | 进程当前目录 | 文件落地的基准目录，也是相对路径的解析基准 |
| `REBELSHELL_ENV_FILE` | 自动查找 | 指定要加载的 `.env` 路径（见下方「配置文件怎么被读到」） |

## 功能

### 终端

- **真伪终端**：Windows 走 ConPTY（`CreatePseudoConsole` + 属性链表启动进程），
  Linux / macOS 走 `posix_openpt` + `fork` + `TIOCSCTTY`。
  因此 `vi`、`top`、`htop` 这类全屏程序、回车换行、Tab 补全、`Ctrl-C`、
  `Ctrl-D`、窗口自适应（`SIGWINCH`）全部与本地终端一致。
- **渲染**：浏览器端用 xterm.js，回滚缓冲 10000 行，ANSI 16 色 + 256 色。
- **传输**：`/ws/terminal` 一条 WebSocket 双向流。输出 Base64 编码 ——
  PTY 按字节切数据，出现半个 UTF-8 字符是常态，直接塞 JSON 会坏掉。
- **可选 shell**：默认用虚拟机平台的 shell（Unix 取 `$SHELL`，
  Windows 在 `powershell.exe` / `pwsh.exe` / `cmd.exe` 里挑第一个存在的），
  也可以在界面上指定 `/bin/bash`、`/bin/zsh`、`cmd.exe` 等。
- **同时只开一个**：客户端保持「极简单线程」的设计，同一时刻只服务一个终端会话；
  新的 `PTY_OPEN` 会先收掉旧的。需要多开就多开几个浏览器标签页。

### 文件管理器

- **浏览**：`LIST_DIR` 返回解析后的绝对路径 + 条目（名称 / 类型 / 大小 / 修改时间），
  目录在前、文件在后，组内不区分大小写排序。
- **操作**：新建目录（等价 `mkdir -p`）、重命名/移动、删除（删目录必须显式递归）、
  在线编辑文本（2 MiB 上限）。
- **安全阀**：拒绝删除文件系统根；覆盖文件前先留一份同名 `.bak`；
  非空目录不勾选递归就删不掉。
- **上传下载**：沿用分块流式实现，支持把本机文件拖进列表直接上传。

### 命令与文件传输（原有能力）

- **命令下发**：Web 选择设备 → 输入命令 → TCP 下发 → 回显经 WebSocket 实时呈现。
  - Linux / macOS：`sh -c "<命令>"`
  - Windows：`powershell.exe -NoProfile -Command "<命令>"`
  - stdout 与 stderr 均被捕获，结果 Base64 编码后回传。
  - 中文 Windows 上 PowerShell 默认按本地代码页（GBK）写出，客户端会先切换
    控制台输出编码，并在解码时以 UTF-8 优先、本地代码页回退，中文回显不乱码。
- **文件下发**：网页上传文件 → 分块流式经 TCP 发送 → 客户端落盘。
- **文件提取**：输入虚拟机绝对路径 → 客户端读取 → 服务端在浏览器端触发下载。
- **保活**：客户端每 10s 发 `PING`，服务端回 `PONG`，避免被中间路由器 NAT 表超时中断。
- **多端查看**：命令回显同时经 HTTP 返回给发起方并广播给所有已打开的页面。

### 传输与稳定性细节

- 分块大小 64 KiB：大文件不会用一条超大帧长时间独占连接，心跳照常往返。
- 背压：结果投递走有界队列，浏览器下载慢时会一路把背压传回客户端，
  既不丢帧也不让服务端内存无界增长。
- 大文件下发先落临时文件再分块发送，服务端不会把整份上传读进内存。
- 提取文件时，客户端回传的分块直接转成 HTTP 响应体流式送出并立即触发下载。
- 同一 UUID 重连会顶替旧连接，且旧连接的迟到收尾不会误删新连接。
- 离线设备在列表中保留最近 50 台的历史（带最后心跳时间）。

## 单端口是怎么做到的

`server/src/listener.rs` 里的 `SniffingListener` 实现了 axum 的 `Listener` trait：
`accept()` 内部先读 8 个字节判定协议，属于 Web 的连接原样交给 axum，
属于客户端的连接转给 Hub，然后继续等下一个连接。

- 客户端首帧恒为 `REGISTER`（8 字节），浏览器首帧恒为方法行（最长 `OPTIONS ` 共 8 字节）；
- 嗅探读掉的字节通过 `PrefixedStream` **原样回灌**，下游察觉不到；
- 判不出来（空连接、TLS 握手、半个方法名）时一律按 HTTP 处理 ——
  交给 axum 走正常错误路径，比误判成客户端、把连接挂进设备表安全。

客户端本身完全不知道有这个机制：它连的还是普通 TCP，第一条帧还是 `REGISTER`。

## 开发模式

一条命令把服务端、Vite 和客户端一起起来，`ADMIN_TOKEN` 等变量从 `.env` 读取
（`bun run dev` 内部是 `bun run --env-file=.env turbo run dev`）：

```bash
bun run dev                # 服务端（.env 的 PORT，缺省 8080）+ Vite 5173 + 客户端
```

Vite 会把 `/api` 与 `/ws` 代理到 `.env` 里的 `PORT`（缺省 8080），所以开发时只需访问
<http://127.0.0.1:5173>，`/api` 无跨域问题。

只想起其中一个：

```bash
bun run dev:server         # 只跑 cargo run（在 server/ 下）
bun run dev:web            # 只跑 vite
bun run dev:client         # 只跑 cargo run（在 client/ 下）
```

### 配置文件怎么被读到

两层机制，彼此不冲突：

1. **bun 脚本**：`bun run dev` 等显式带 `--env-file=.env`（bun 自身也会自动加载
   仓库根的 `.env`），变量注入 turbo 再传给子进程。
2. **Rust 二进制**：启动时自己找 `.env` —— 先看 `REBELSHELL_ENV_FILE`，再从当前
   目录逐级向上（最多 8 层），最后看可执行文件旁边。找到即用，只补进程里**还没有**
   的变量。所以 `cargo run`、`./client/target/release/rebelshell-client` 这类
   不经 bun 的启动方式读到的也是同一份配置。

优先级自高到低：命令行参数 → 已存在的环境变量 → `.env` → 代码默认值。
找不到 `.env` 属正常情况（发布到虚拟机上的裸二进制就是如此），静默跳过。

服务端的 `ADMIN_TOKEN` 没在 `.env` 里给就会用 `config.rs` 的默认值
（`rebelshell`，并在日志里告警）。

> 编辑 `.env` 后需要重启 `bun run dev` —— 变量是启动时注入的。
> `turbo.json` 设了 `envMode: "loose"`，环境变量原样透传给子进程；
> 这一点在 Windows 上尤其重要：cargo 链接时要读 `LIB` / `INCLUDE`，
> strict 模式会把它们清掉，表现为 `LNK1181: cannot open input file 'kernel32.lib'`。

## 测试

```bash
bun run test               # 两个 crate 的单元测试（协议编解码、设备注册表、鉴权比较、
                           # 输出编码回退、目录列表、以及会真的起 shell 的 PTY 测试）
bun run smoke              # 端到端冒烟：拉起真实的管理端与客户端进程，覆盖
                           # 单端口分流 → 鉴权 → 注册上线 → 命令执行（含中文）→
                           # 小/大文件双向传输 → 文件管理全流程 → 交互式终端
                           # （WebSocket 握手 → open → ready → 回显 → resize）→
                           # 静态资源与 WebSocket 握手 → 心跳保活 → 断线重连
bun run lint               # clippy -D warnings + tsc --noEmit
bun run ci                 # lint + typecheck + test + smoke，提交前跑一遍
```

`client/src/pty.rs` 里的终端测试会**真的启动一个 shell** 并与之交互：写入
`echo <标记>`、从终端读到回显、调整尺寸、`exit` 后确认能检测到会话结束。
这比 mock 掉 PTY 有意义得多 —— ConPTY 最容易错的地方（管道继承标志、
`STARTUPINFOEXW` 的 `cb` 取值、属性链表的 `lpValue` 形态、以及必须显式把
三个标准句柄置为 `INVALID_HANDLE_VALUE`）都只有真起进程才会暴露。

`bun run smoke` 会自动先构建两个二进制（`dependsOn` 里声明了），
不需要手动 `cargo build`。

冒烟测试是 TypeScript（[scripts/smoke.ts](scripts/smoke.ts)），不是 shell 脚本 ——
原生 Windows PowerShell 的 PATH 里没有 `sh` / `bash` / `curl` / `md5sum`
（Git for Windows 只把 `cmd\` 加进 PATH，那里只有 `git.exe`），
用 shell 写就意味着跑测试必须先开 Git Bash。改成 bun 之后三个平台一致。

## 全平台可跑：不要在脚本里依赖 shell

`bun run` 用的是 Bun 自己的内建 shell，**它会去 PATH 上找 `sh` / `bash` / `rm`
这类外部程序**。在原生 PowerShell 里这些都不存在，于是 `"dev": "sh -c '...'"`
会直接报 `bun: command not found: sh`。

所以所有 `package.json` 脚本都只用两类东西：

1. **PATH 上真实存在的可执行文件**：`cargo`、`bun`、`vite`、`tsc`
   （`node_modules/.bin` 会被 bun 自动加进 PATH）
2. **Bun 内建 shell 的操作**：`echo`、`mkdir -p`、`rm -rf`、`cd`、`&&`

> 注意 Bun 内建 shell **不支持** `${VAR:-default}` 这种带默认值的展开 ——
> 它会原样输出字面量。需要默认值时让程序自己兜底
> （服务端的 `ADMIN_TOKEN` 就是这么做的，见 [server/src/config.rs](server/src/config.rs)），
> 或者写进 `.env`。

## 发版（cargo-dist）

配置在 [dist-workspace.toml](dist-workspace.toml)。`server/` 与 `client/` 是两个
独立的 crate（不是 cargo workspace），所以在 `workspace.members` 里显式列出，
各自打包成一个 App。

目标平台两个：
- `x86_64-unknown-linux-musl` —— 静态链接，丢到任意发行版的内网虚拟机上都能跑
- `x86_64-pc-windows-msvc` —— 管理员机器与 Windows 虚拟机

每个 App 会产出 `.zip` / `.tar.xz`、`shell` 与 `powershell` 安装器、
`sha256` 校验和，以及一个源码包。

### 本地打包

```bash
bun run dist:plan          # 只看会产出什么（不构建）
bun run dist               # 产出到 target/distrib/
```

`bun run dist` 会先构建前端 —— **这是必须的**：`server` 用 `rust-embed`
把 `server/web/dist` 编译进二进制，dist 自己不管前端，不先构建就会
发布出内嵌占位页的服务端。这个顺序由 `turbo.json` 的
`//#dist:release` → `rebelshell-web#build` 保证。

### 在 GitHub 上发版

推一个形如 `v0.1.0` 的 tag 即可：

```bash
git tag v0.1.0 && git push origin v0.1.0
```

`.github/workflows/release.yml` 由 `dist generate` 生成（**不要手改**），
它会先跑 [.github/workflows/build-setup.yml](.github/workflows/build-setup.yml)
里声明的步骤（装 bun → `bun install` → 构建前端），再调 `dist build`。
`build-setup.yml` 是手写的，改它就等于改 CI 的前置步骤。

每个平台一个 App 独立版本号；tag 用 `rebelshell-server-v0.1.0` 这种形式可以只发其中一个。

> **发版前必做**：把 `server/Cargo.toml` 与 `client/Cargo.toml` 里的
> `repository` 从 `https://github.com/OWNER/rebelshell` 改成真实仓库地址，
> 否则 Release 链接会指向不存在的地方。改完在仓库根跑一次 `dist generate`
> 重新生成 workflow。

### 关于 dist 版本

`dist-workspace.toml` 里的 `cargo-dist-version` 必须与本机/CI 实际运行的
dist 版本**完全一致**，否则 `dist build` 会直接拒绝执行：

```
× You're running dist 0.32.0, but 'cargo-dist-version = 0.33.1' is set
```

当前 pin 在 `0.32.0`。本地装同版本：

```bash
cargo install cargo-dist --version 0.32.0 --locked
# 或者用 npm 包（会下载对应的预编译二进制）
bun add -d @axodotdev/dist@0.32.0 && bun pm trust --all
```

升级 dist 时改两处并重新生成：`cargo-dist-version` 与
`dist generate`（后者会更新 release.yml 里的下载链接）。

### 本地 `dist build` 需要至少一次提交

cargo-dist 会从 git 归档源码包，**仓库里一个 commit 都没有时会失败**：

```
fatal: not a valid object name: HEAD
× failed to generate a source tarball for your project
```

全新初始化的仓库先提交一次，再 `bun run dist`。CI 上不存在这个问题。

## 鉴权模型

- 所有 `/api/**` 与 `/ws` 请求必须携带 `Authorization: Bearer <ADMIN_TOKEN>`；
  未认证请求返回 `401`，WebSocket 直接以 `401` 拒绝握手。
- 浏览器端把令牌存在 `localStorage`，刷新后仍然有效；「退出登录」会清除。
- 令牌为明文比对（常量时间比较），仅建议在受信网络或叠加 TLS 反向代理后使用。

## 局域网 / 多网段部署提示

管理端所在机器需要能被虚拟机的**出站**流量访问，即管理端要监听在
虚拟机能路由到的地址上（通常是管理端的内网网卡地址，而非仅 `127.0.0.1`）。
若虚拟机上还有一层 NAT，可配合 `frp` / `socat` 做端口转发，指向管理端的 `PORT`。

因为 Web 与客户端共用一个端口，内网策略只需放行这一个端口；
转发时也不必再区分「控制端口」和「Web 端口」。