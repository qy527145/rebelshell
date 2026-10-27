#!/usr/bin/env bun
/**
 * RebelShell 端到端冒烟测试：拉起管理端与客户端，走一遍命令执行、文件往返、
 * 文件管理与交互式终端。
 *
 * 用法: bun scripts/smoke.ts     （或仓库根 `bun run smoke`）
 * 依赖: 已构建的 server/client 产物、bun。端口可经 SMOKE_PORT 覆盖。
 *
 * 为什么是 TypeScript 而不是 shell：
 * 原生 Windows PowerShell 的 PATH 里没有 sh / bash / curl / md5sum —— Git for Windows
 * 只把 `cmd\` 目录加进 PATH（那里只有 git.exe），而 sh/bash/coreutils 都在 `bin\` 与
 * `usr\bin\`。用 shell 写就意味着跑测试必须先开 Git Bash。改成 bun 之后，
 * Windows / macOS / Linux 一视同仁，只要装了 bun 就能跑。
 */

import { createHash, randomBytes } from 'node:crypto'
import { mkdtemp, mkdir, readFile, rm, writeFile } from 'node:fs/promises'
import { existsSync } from 'node:fs'
import { tmpdir } from 'node:os'
import { join, resolve } from 'node:path'

const REPO_ROOT = resolve(import.meta.dir, '..')

/** 两种命名都给到：Windows 上是 .exe，其它平台没有后缀。 */
function findBinary(...candidates: string[]): string | undefined {
  return candidates.map((p) => join(REPO_ROOT, p)).find((p) => existsSync(p))
}

const SERVER_BIN = findBinary(
  'server/target/release/rebelshell-server.exe',
  'server/target/release/rebelshell-server',
)
const CLIENT_BIN = findBinary(
  'client/target/release/rebelshell-client.exe',
  'client/target/release/rebelshell-client',
)

/**
 * 单个端口同时承载 Web 与客户端连接 —— 这正是「端口合一」要验证的点，
 * 因此下面所有断言都跑在同一个 `PORT` 上。
 */
// 用 SMOKE_PORT 而不是 PORT：bun 会自动加载仓库根的 `.env`，
// 那里写着开发用的 PORT（8288），跟着它跑就会跟开发中的服务端抢端口。
const PORT = process.env.SMOKE_PORT ?? '18080'
const TOKEN = 'test-token-e2e'
const UUID = 'test-vm-1'
const BASE = `http://127.0.0.1:${PORT}`
const WS_HEADERS = {
  Connection: 'Upgrade',
  Upgrade: 'websocket',
  'Sec-WebSocket-Version': '13',
  'Sec-WebSocket-Key': 'dGhlIHNhbXBsZSBub25jZQ==',
}

/**
 * 冒烟测试自带一整套隔离配置：bun 会自动加载仓库根的 `.env`，若把 `process.env`
 * 原样传给子进程，开发用的 PORT / ADMIN_TOKEN / REBELSHELL_* 都会漏进来 ——
 * 轻则测试连上自己的开发实例，重则端口被占用。这里剔掉正向应用配置，
 * 再显式叠加测试需要的值。
 */
function isolatedEnv(overrides: Record<string, string> = {}): Record<string, string> {
  const excluded = new Set([
    'ADMIN_TOKEN',
    'BIND_ADDR',
    'PORT',
    'COMMAND_TIMEOUT_SECS',
    'TRANSFER_TIMEOUT_SECS',
    'PUSH_INTERVAL_SECS',
  ])
  const env: Record<string, string> = {}
  for (const [key, value] of Object.entries(process.env)) {
    if (value === undefined) continue
    if (excluded.has(key) || key.startsWith('REBELSHELL_')) continue
    env[key] = value
  }
  return { ...env, ...overrides }
}

let pass = 0
let fail = 0

function ok(label: string) {
  pass += 1
  console.log(`  [PASS] ${label}`)
}
function bad(label: string) {
  fail += 1
  console.log(`  [FAIL] ${label}`)
}
function check(condition: boolean, label: string) {
  if (condition) ok(label)
  else bad(label)
}

const mkdirp = (p: string) => mkdir(p, { recursive: true })
const sleep = (ms: number) => Bun.sleep(ms)

/** 带令牌的 GET，返回解析后的 JSON 文本（保留原文以便断言子串）。 */
async function apiGet(path: string): Promise<string> {
  const response = await fetch(`${BASE}${path}`, {
    headers: { Authorization: `Bearer ${TOKEN}` },
  })
  return response.text()
}

async function statusOf(path: string, init?: RequestInit): Promise<number> {
  try {
    const response = await fetch(`${BASE}${path}`, init)
    return response.status
  } catch {
    return -1
  }
}

/** POST JSON：走 body 而不是 argv，非 ASCII 内容不会被代码页破坏。 */
async function postJson(path: string, body: unknown): Promise<Response> {
  return fetch(`${BASE}${path}`, {
    method: 'POST',
    headers: { Authorization: `Bearer ${TOKEN}`, 'Content-Type': 'application/json' },
    body: JSON.stringify(body),
  })
}

/** 轮询直到条件成立，返回是否在时限内成立。 */
async function waitFor(
  probe: () => Promise<boolean>,
  { attempts = 50, intervalMs = 200 } = {},
): Promise<boolean> {
  for (let i = 0; i < attempts; i += 1) {
    if (await probe()) return true
    await sleep(intervalMs)
  }
  return false
}

if (!SERVER_BIN || !CLIENT_BIN) {
  console.error('缺少构建产物，请先构建 server 与 client（`bun run build`）。')
  process.exit(1)
}

/**
 * 走一遍交互式终端的完整链路：WebSocket 握手 → open → 收到 ready →
 * 写入一条命令 → 从终端输出里读到回显 → resize → 关闭。
 *
 * 这是唯一能验证「真 PTY 起来了」的测试：客户端上跑的是 ConPTY/pty，
 * 没有真终端的话 shell 根本不会产生提示符与回显。
 */
async function probeTerminal(): Promise<{
  ready: boolean
  echoed: boolean
  resized: boolean
  seen: string
}> {
  const marker = `rebelshell-term-${Math.floor(Math.random() * 1e6)}`
  const url = `ws://127.0.0.1:${PORT}/ws/terminal?token=${encodeURIComponent(TOKEN)}&uuid=${encodeURIComponent(UUID)}`

  const socket = new WebSocket(url)
  let output = ''
  let ready = false
  let resized = false
  let finished = false

  const done = new Promise<void>((resolve) => {
    const finish = () => {
      if (finished) return
      finished = true
      resolve()
    }

    socket.addEventListener('open', () => {
      socket.send(JSON.stringify({ type: 'open', cols: 90, rows: 26 }))
    })

    socket.addEventListener('message', (event) => {
      let message: { type: string; data?: string; message?: string }
      try {
        message = JSON.parse(String(event.data)) as typeof message
      } catch {
        return
      }

      if (message.type === 'ready') {
        ready = true
        // ready 之后立刻 resize：验证尺寸调整不会打断会话。
        socket.send(JSON.stringify({ type: 'resize', cols: 100, rows: 30 }))
        resized = true
        // 给 shell 一点时间把提示符画完，再送出探针命令。
        // （不算多余：ConPTY 在提示符之前会先发一串初始化序列。）
        setTimeout(() => {
          socket.send(
            JSON.stringify({
              type: 'input',
              // Base64 of `echo <marker>\r\n`
              data: btoa(`echo ${marker}\r\n`),
            }),
          )
        }, 300)
      }

      if (message.type === 'output' && message.data) {
        // 输出是 Base64 的字节流，可能切在多字节字符中间 —— 这里只找 ASCII 标记，
        // 因此直接按 latin1 解码就够。
        output += atob(message.data)
        if (output.includes(marker)) finish()
      }

      if (message.type === 'error' || message.type === 'exit') finish()
    })

    socket.addEventListener('error', () => finish())
    socket.addEventListener('close', () => finish())
    // 兜底：任何一步没来也要让测试结束，而不是挂住整个冒烟。
    setTimeout(finish, 25_000)
  })

  await done
  socket.close()

  return { ready, echoed: output.includes(marker), resized, seen: output.slice(-200) }
}

const WORKDIR = await mkdtemp(join(tmpdir(), 'rebelshell-smoke-'))
// 两侧目录必须分开：admin 放待下发的源文件，agent 是客户端的工作目录，
// 否则下发前后同名文件会互相覆盖，比对就失去意义。
const ADMIN_DIR = join(WORKDIR, 'admin')
const AGENT_DIR = join(WORKDIR, 'agent')
const SERVER_LOG = join(WORKDIR, 'server.log')
const CLIENT_LOG = join(WORKDIR, 'client.log')
await mkdirp(ADMIN_DIR)
await mkdirp(AGENT_DIR)

let server: Bun.Subprocess | undefined
let client: Bun.Subprocess | undefined

/** 收集子进程日志：出错时要连同日志一起打出来，否则只有一堆 FAIL 无从下手。 */
async function drain(stream: ReadableStream<Uint8Array>, chunks: Buffer[]) {
  for await (const chunk of stream) chunks.push(Buffer.from(chunk))
}

const serverChunks: Buffer[] = []
const clientChunks: Buffer[] = []

async function cleanup() {
  // 先杀客户端再杀服务端：反过来服务端会先记一笔客户端离线。
  client?.kill()
  server?.kill()
  // 给子进程一点时间真正退出，否则 Windows 上句柄没释放，临时目录删不掉。
  await sleep(200)
  await rm(WORKDIR, { recursive: true, force: true }).catch(() => {})
}

function tail(chunks: Buffer[], lines: number): string {
  return Buffer.concat(chunks).toString('utf8').split('\n').slice(-lines).join('\n')
}

console.log('== 准备 ==')
console.log(`  server: ${SERVER_BIN}`)
console.log(`  client: ${CLIENT_BIN}`)
console.log(`  临时目录: ${WORKDIR}`)
console.log(`  单端口: ${PORT}（Web 与客户端共用）`)

try {
  // ---- 启动管理端 ----
  console.log('\n== 启动管理端 ==')
  server = Bun.spawn([SERVER_BIN], {
    cwd: REPO_ROOT,
    env: isolatedEnv({
      ADMIN_TOKEN: TOKEN,
      PORT,
      BIND_ADDR: '127.0.0.1',
    }),
    stdout: 'pipe',
    stderr: 'pipe',
  })
  void drain(server.stdout as ReadableStream<Uint8Array>, serverChunks)
  void drain(server.stderr as ReadableStream<Uint8Array>, serverChunks)

  const healthy = await waitFor(async () => {
    try {
      const response = await fetch(`${BASE}/api/health`)
      return response.ok
    } catch {
      return false
    }
  })

  if (healthy) {
    ok(`管理端已就绪（${BASE}）`)
  } else {
    bad('管理端未能启动')
    console.log(tail(serverChunks, 40))
    await cleanup()
    process.exit(1)
  }

  // ---- 鉴权 ----
  console.log('\n== 鉴权 ==')
  let code = await statusOf('/api/devices')
  check(code === 401, `无令牌访问 /api/devices 返回 401（实际 ${code}）`)

  code = await statusOf('/api/devices', { headers: { Authorization: 'Bearer wrong' } })
  check(code === 401, `错误令牌返回 401（实际 ${code}）`)

  code = await statusOf('/api/devices', { headers: { Authorization: `Bearer ${TOKEN}` } })
  check(code === 200, `正确令牌返回 200（实际 ${code}）`)

  const login = await postJson('/api/login', { token: TOKEN })
  check(login.status === 200, `POST /api/login 令牌正确返回 200（实际 ${login.status}）`)

  // ---- 启动客户端 ----
  console.log('\n== 启动客户端 ==')
  client = Bun.spawn(
    [CLIENT_BIN, '--server', `127.0.0.1:${PORT}`, '--uuid', UUID, '--heartbeat', '2', '--dir', AGENT_DIR],
    { cwd: AGENT_DIR, env: isolatedEnv(), stdout: 'pipe', stderr: 'pipe' },
  )
  void drain(client.stdout as ReadableStream<Uint8Array>, clientChunks)
  void drain(client.stderr as ReadableStream<Uint8Array>, clientChunks)

  let devices = ''
  const online = await waitFor(async () => {
    devices = await apiGet('/api/devices')
    return devices.includes('"online":true')
  })
  check(online, '客户端注册后出现在在线设备列表')
  console.log(`  设备快照: ${devices}`)

  // ---- 命令执行 ----
  console.log('\n== 命令执行 ==')
  const marker = `rebelshell-e2e-${Math.floor(Math.random() * 32768)}`
  let resp = await (await postJson('/api/command', { uuid: UUID, command: `echo ${marker}` })).text()
  check(resp.includes(marker), `命令回显包含标记 ${marker}`)

  // 中文与特殊字符不应破坏帧（客户端在中文 Windows 上回传的应是可读文本）。
  resp = await (
    await postJson('/api/command', { uuid: UUID, command: 'echo 中文:冒号 "引号"' })
  ).text()
  check(resp.includes('中文:冒号'), `含中文与冒号的命令可正确回显：${resp}`)

  // 失败命令也应返回输出而不是接口报错。
  resp = await (
    await postJson('/api/command', { uuid: UUID, command: 'this-command-does-not-exist-e2e' })
  ).text()
  check(resp.includes('"output"'), '不存在的命令仍返回 output 字段')

  // 离线设备应被拒绝。
  const offlineResponse = await postJson('/api/command', {
    uuid: 'no-such-device',
    command: 'echo hi',
  })
  check(
    offlineResponse.status === 404,
    `给离线设备下发命令返回 404（实际 ${offlineResponse.status}）`,
  )

  /** 上传一个本地文件到客户端。FormData 等价于 curl 的 `-F file=@path`。 */
  async function pushFile(name: string, source: string): Promise<string> {
    const form = new FormData()
    form.set('uuid', UUID)
    form.set('path', name)
    form.set('file', new Blob([await readFile(source)]), name)
    const response = await fetch(`${BASE}/api/files/push`, {
      method: 'POST',
      headers: { Authorization: `Bearer ${TOKEN}` },
      body: form,
    })
    return response.text()
  }

  const md5 = (buffer: Buffer) => createHash('md5').update(buffer).digest('hex')

  // ---- 文件下发：小文件（单帧 SEND_FILE）----
  console.log('\n== 文件下发：小文件 ==')
  const smallPath = join(ADMIN_DIR, 'small.txt')
  await writeFile(smallPath, 'hello-from-admin\n')
  resp = await pushFile('small.txt', smallPath)
  check(resp.includes('stored'), `小文件下发返回 stored：${resp}`)

  const landedSmall = join(AGENT_DIR, 'small.txt')
  if (existsSync(landedSmall)) {
    const content = await readFile(landedSmall, 'utf8')
    check(content.includes('hello-from-admin'), '小文件已落到客户端目录且内容正确')
  } else {
    bad(`小文件未落到 ${landedSmall}`)
  }

  // ---- 文件下发：大文件（PUT_BEGIN/PUT_CHUNK/PUT_END）----
  console.log('\n== 文件下发：大文件（分块） ==')
  const bigPath = join(ADMIN_DIR, 'big.bin')
  await writeFile(bigPath, randomBytes(307200)) // 300 KiB，跨 5 个 64 KiB 分块
  resp = await pushFile('big.bin', bigPath)
  check(resp.includes('stored'), `大文件下发返回 stored：${resp}`)

  const landedBig = join(AGENT_DIR, 'big.bin')
  if (existsSync(landedBig)) {
    const source = md5(await readFile(bigPath))
    const received = md5(await readFile(landedBig))
    check(source === received, '分块下发的文件与源文件逐字节一致')
    console.log(`  源: ${source}`)
    console.log(`  收: ${received}`)
  } else {
    bad('大文件未落盘')
  }

  /** 从客户端提取一个文件。等价于 curl 的 `-o`。 */
  async function pullFile(name: string) {
    return fetch(`${BASE}/api/files/pull?uuid=${UUID}&path=${encodeURIComponent(name)}`, {
      headers: { Authorization: `Bearer ${TOKEN}` },
    })
  }

  // ---- 文件提取：小文件 ----
  console.log('\n== 文件提取：小文件 ==')
  const pulledSmall = await pullFile('small.txt')
  const pulledSmallBody = pulledSmall.ok ? await pulledSmall.text() : ''
  check(
    pulledSmallBody.includes('hello-from-admin'),
    '提取回的文件内容正确',
  )

  // ---- 文件提取：大文件（FILE_BEGIN/FILE_CHUNK/FILE_END）----
  console.log('\n== 文件提取：大文件（分块回传） ==')
  const pulledBig = await pullFile('big.bin')
  if (pulledBig.ok) {
    const source = md5(await readFile(landedBig))
    const pulled = md5(Buffer.from(await pulledBig.arrayBuffer()))
    check(source === pulled, '分块提取的文件与原文件逐字节一致')
  } else {
    bad('分块提取失败')
  }

  // 提取不存在的路径应报错而非返回空文件。
  const missing = await pullFile('definitely/not/here.log')
  check(missing.status === 502, `提取不存在的路径返回 502（实际 ${missing.status}）`)

  // ---- 文件管理：列目录 / 新建 / 重命名 / 读写文本 / 删除 ----
  console.log('\n== 文件管理 ==')

  const fsGet = async (path: string, params: Record<string, string>) => {
    const query = new URLSearchParams({ uuid: UUID, ...params })
    return fetch(`${BASE}${path}?${query}`, {
      headers: { Authorization: `Bearer ${TOKEN}` },
    })
  }

  const fsPost = async (path: string, body: Record<string, unknown>) =>
    postJson(`/api/fs/${path}`, { uuid: UUID, ...body })

  // 列工作目录：路径必须被解析成绝对路径回带回来，前端据此维护 cwd。
  const listing = await fsGet('/api/fs/list', { path: '' })
  check(listing.status === 200, `列目录返回 200（实际 ${listing.status}）`)
  const listed = (await listing.json()) as {
    path: string
    entries: Array<{ name: string; kind: string }>
  }
  check(
    typeof listed.path === 'string' && listed.path.length > 0,
    `列目录回带解析后的绝对路径：${listed.path}`,
  )
  check(
    listed.entries.some((entry) => entry.name === 'small.txt' && entry.kind === 'f'),
    '目录列表里能看到刚下发的小文件',
  )

  // 新建目录（含中间层级，等价 mkdir -p）。
  const mkdir = await fsPost('mkdir', { path: 'nested/deep' })
  check(mkdir.status === 200, `新建嵌套目录返回 200（实际 ${mkdir.status}）`)
  check(existsSync(join(AGENT_DIR, 'nested', 'deep')), '新建的嵌套目录已落到客户端')

  // 重命名：只给新名字，应当落在同一目录里。
  const rename = await fsPost('rename', { from: 'small.txt', to: 'renamed.txt' })
  check(rename.status === 200, `重命名返回 200（实际 ${rename.status}）`)
  check(
    existsSync(join(AGENT_DIR, 'renamed.txt')) && !existsSync(join(AGENT_DIR, 'small.txt')),
    '重命名后新旧文件名都符合预期',
  )

  // 在线编辑：写回文本应原样落盘，并留下 .bak 备份。
  const edited = 'edited-by-smoke\n第二行\n'
  const write = await fsPost('write', { path: 'renamed.txt', content: edited })
  check(write.status === 200, `写入文本返回 200（实际 ${write.status}）`)
  check(
    (await readFile(join(AGENT_DIR, 'renamed.txt'), 'utf8')) === edited,
    '写入的内容原样落盘（含中文）',
  )
  check(existsSync(join(AGENT_DIR, 'renamed.txt.bak')), '覆盖前留下了 .bak 备份')

  const read = await fsGet('/api/fs/read', { path: 'renamed.txt' })
  const readBody = (await read.json()) as { content: string }
  check(readBody.content === edited, '读回的文本与写入一致')

  // 300 KiB 的二进制文件小于在线编辑的 2 MiB 上限，应当能读回（内容是乱码也无所谓，
// 客户端只保证字节不丢）。这条同时也验证了「不是只支持纯文本文件」。
  const bigText = await fsGet('/api/fs/read', { path: 'big.bin' })
  check(bigText.status === 200, `上限内的较大文件可读回（实际 ${bigText.status}）`)

  // 把目录当文本读应被拒绝，而不是把目录条目当内容塞回来。
  const readDir = await fsGet('/api/fs/read', { path: 'nested' })
  check(readDir.status === 502, `把目录当文本读返回 502（实际 ${readDir.status}）`)

  // 删除文件；删非空目录时必须显式递归。
  const removeFile = await fsPost('delete', { path: 'renamed.txt' })
  check(removeFile.status === 200, `删除文件返回 200（实际 ${removeFile.status}）`)
  check(!existsSync(join(AGENT_DIR, 'renamed.txt')), '文件已被删除')

  const removeNonRecursive = await fsPost('delete', { path: 'nested' })
  check(
    removeNonRecursive.status === 502,
    `非递归删除非空目录被拒绝（实际 ${removeNonRecursive.status}）`,
  )

  const removeRecursive = await fsPost('delete', { path: 'nested', recursive: true })
  check(removeRecursive.status === 200, `递归删除目录返回 200（实际 ${removeRecursive.status}）`)
  check(!existsSync(join(AGENT_DIR, 'nested')), '目录及其内容已被递归删除')

  // ---- 交互式终端 ----
  console.log('\n== 交互式终端 ==')
  const terminal = await probeTerminal()
  check(terminal.ready, '打开终端后客户端立即回 ready（不必等 shell 吐字）')
  check(terminal.echoed, `在终端里执行命令能看到回显：${JSON.stringify(terminal.seen)}`)
  check(terminal.resized, '终端 resize 不报错且会话继续可用')

  // ---- 静态资源与 WebSocket 鉴权 ----
  console.log('\n== 静态资源与 WebSocket ==')
  const root = await fetch(`${BASE}/`)
  check(root.status === 200, `根路径返回前端页面（实际 ${root.status}）`)
  check((await root.text()).includes('RebelShell'), '页面内容包含应用标识')

  // 带完整握手头才能走到鉴权分支；否则 axum 会因为缺少 Upgrade 头先返回 400。
  code = await statusOf('/ws', { headers: WS_HEADERS })
  check(code === 401, `无令牌的 WebSocket 握手被拒绝（实际 ${code}）`)

  code = await statusOf('/ws?token=wrong', { headers: WS_HEADERS })
  check(code === 401, `错误令牌的 WebSocket 握手被拒绝（实际 ${code}）`)

  // 令牌正确时应完成握手（101 Switching Protocols）。
  code = await statusOf(`/ws?token=${TOKEN}`, { headers: WS_HEADERS })
  check(code === 101, `正确令牌的 WebSocket 握手成功（实际 ${code}）`)

  // ---- 心跳保活 ----
  console.log('\n== 心跳保活 ==')
  await sleep(5000)
  devices = await apiGet('/api/devices')
  check(devices.includes('"online":true'), '静默 5 秒（心跳间隔 2 秒）后设备仍在线')

  // ---- 重连后仍为同一设备 ----
  console.log('\n== 断线重连 ==')
  client.kill()
  await client.exited

  const wentOffline = await waitFor(
    async () => (await apiGet('/api/devices')).includes('"online":false'),
    { attempts: 40, intervalMs: 250 },
  )
  check(wentOffline, '客户端退出后设备被标记为离线（历史仍保留）')

  client = Bun.spawn(
    [CLIENT_BIN, '--server', `127.0.0.1:${PORT}`, '--uuid', UUID, '--heartbeat', '2', '--dir', AGENT_DIR],
    { cwd: AGENT_DIR, env: isolatedEnv(), stdout: 'pipe', stderr: 'pipe' },
  )
  void drain(client.stdout as ReadableStream<Uint8Array>, clientChunks)
  void drain(client.stderr as ReadableStream<Uint8Array>, clientChunks)

  const reonline = await waitFor(
    async () => (await apiGet('/api/devices')).includes('"online":true'),
    { attempts: 40, intervalMs: 250 },
  )
  check(reonline, '同一 UUID 重连后恢复为在线')

  console.log('\n== 结果 ==')
  console.log(`  通过: ${pass}  失败: ${fail}`)
  if (fail > 0) {
    console.log('\n--- 服务端日志（末 40 行）---')
    console.log(tail(serverChunks, 40))
    console.log('--- 客户端日志（末 20 行）---')
    console.log(tail(clientChunks, 20))
  }
} finally {
  await cleanup()
}

process.exit(fail > 0 ? 1 : 0)