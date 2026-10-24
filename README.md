# RebelShell

轻量级反向远程管理与文件传输系统（Rust + Web）。

管理端（Server）监听固定端口，内网虚拟机（Client）主动出站连接并维持长连接；
管理员通过浏览器界面统一执行命令、收发文件。

```
[ 虚拟机 / Client ] --(主动出站 TCP:9000)--> [ 管理员机器 / Rust Server ]
   执行命令 / 读写文件                              |  ADMIN_TOKEN 鉴权 + REST + WebSocket
                                              [ Web 浏览器界面 (React) ]
```

## 目录结构

整个仓库由根目录的 bun workspaces + turbo 统一编排：`web/`、`server/`、`client/`
各自是一个 workspace 包（后两个是 cargo 壳包，只为接入编排）。

| 路径 | 说明 |
| --- | --- |
| [package.json](package.json) | 根包：bun workspaces、全部入口脚本 |
| [turbo.json](turbo.json) | 任务图与缓存规则（前端产物落点在这里声明） |
| [dist-workspace.toml](dist-workspace.toml) | cargo-dist 发版配置（GitHub Release 打包） |
| [.env.example](.env.example) | 本机配置模板，`cp .env.example .env` 后按需改 |
| [server/](server/) | Rust 管理端：TCP 控制服务 + axum Web 服务 |
| [client/](client/) | Rust 客户端（虚拟机端），零第三方依赖 |
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
| `bun run dev` | 开发模式：Vite 热更新（5173）+ 服务端（8080）一起起 |
| `bun run dev:web` / `bun run dev:server` | 只起其中一个 |
| `bun run test` | 两个 crate 的单元测试 |
| `bun run lint` | `cargo clippy --all-targets -- -D warnings` + 前端 `tsc --noEmit` |
| `bun run typecheck` | `cargo check --all-targets` + 前端 `tsc --noEmit` |
| `bun run smoke` | 端到端冒烟：拉起真实二进制跑 25 项断言 |
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
ADMIN_TOKEN=my-secret-token TCP_PORT=9000 HTTP_PORT=8080 \
  ./server/target/release/rebelshell-server
```

开发期直接用 `bun run dev`（Vite 热更新 + 服务端一起起）更方便。

打开 <http://127.0.0.1:8080> ，输入 `ADMIN_TOKEN` 登录。

### 在虚拟机上运行客户端

```bash
cd client
cargo build --release            # 产物 target/release/rebelshell-client
```

```bash
# 连接地址通过参数或环境变量提供
rebelshell-client --server 192.168.1.10:9000 --uuid agent-01
# 或
REBELSHELL_SERVER=192.168.1.10:9000 REBELSHELL_UUID=agent-01 rebelshell-client

# 安装为后台常驻服务（Windows 计划任务 / Linux systemd）
rebelshell-client install --server 192.168.1.10:9000
rebelshell-client uninstall
```

不指定 `--uuid` 时，客户端会在本地生成并持久化一个 UUID
（Windows：`%APPDATA%\RebelShell\agent-id`，Linux：`~/.config/rebelshell/agent-id`），
保证重连后仍被识别为同一台机器。

## 配置项

### Server

| 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `ADMIN_TOKEN` | `rebelshell`（并在日志中告警） | 全局管理令牌，Web 端登录与所有接口鉴权均使用它 |
| `TCP_PORT` | `9000` | 客户端反向连接的控制端口 |
| `HTTP_PORT` | `8080` | Web 服务端口 |
| `BIND_ADDR` | `0.0.0.0` | 绑定地址 |
| `COMMAND_TIMEOUT_SECS` | `60` | 命令执行的等待上限 |
| `TRANSFER_TIMEOUT_SECS` | `300` | 文件传输的等待上限 |
| `PUSH_INTERVAL_SECS` | `3` | 设备列表变化检测与推送间隔 |
| `RUST_LOG` | `info,tower_http=warn` | 日志过滤，如 `RUST_LOG=rebelshell_server=debug` |

### Client

| 参数 / 环境变量 | 默认值 | 说明 |
| --- | --- | --- |
| `--server` / `REBELSHELL_SERVER` | `127.0.0.1:9000` | 管理端地址 |
| `--uuid` / `REBELSHELL_UUID` | 本地持久化生成 | 设备唯一标识 |
| `--heartbeat` / `REBELSHELL_HEARTBEAT` | `10` | PING 间隔（秒） |
| `--dir` / `REBELSHELL_DIR` | 进程当前目录 | `SEND_FILE` 的落地目录，也是相对路径的基准 |

## 功能

- **设备列表**：内存维护在线设备（UUID / 主机名 / IP / 系统类型 / 最后心跳），WebSocket 实时刷新。
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

## 开发模式

一条命令把服务端和 Vite 一起起来，`ADMIN_TOKEN` 等变量从 `.env` 读取
（`bun run dev` 内部是 `bun run --env-file=.env turbo run dev`）：

```bash
bun run dev                # 服务端 8080 + Vite 5173
```

Vite 会把 `/api` 与 `/ws` 代理到 8080，所以开发时只需访问
<http://127.0.0.1:5173>，`/api` 无跨域问题。

只想起其中一个：

```bash
bun run dev:server         # 只跑 cargo run（在 server/ 下）
bun run dev:web            # 只跑 vite
```

服务端的 `ADMIN_TOKEN` 没在 `.env` 里给就会用 `config.rs` 的默认值
（`rebelshell`，并在日志里告警）。

> 编辑 `.env` 后需要重启 `bun run dev` —— 变量是启动时注入的。
> `turbo.json` 设了 `envMode: "loose"`，环境变量原样透传给子进程；
> 这一点在 Windows 上尤其重要：cargo 链接时要读 `LIB` / `INCLUDE`，
> strict 模式会把它们清掉，表现为 `LNK1181: cannot open input file 'kernel32.lib'`。

## 测试

```bash
bun run test               # 两个 crate 的单元测试（协议编解码、设备注册表、
                           # 鉴权比较、输出编码回退等）
bun run smoke              # 端到端冒烟：拉起真实的管理端与客户端进程，覆盖
                           # 鉴权 → 注册上线 → 命令执行（含中文）→ 小/大文件双向
                           # 传输 → 静态资源与 WebSocket 握手 → 心跳保活 → 断线重连
bun run lint               # clippy -D warnings + tsc --noEmit
bun run ci                 # lint + typecheck + test + smoke，提交前跑一遍
```

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
若虚拟机上还有一层 NAT，可配合 `frp` / `socat` 做端口转发，指向管理端的 `TCP_PORT`。