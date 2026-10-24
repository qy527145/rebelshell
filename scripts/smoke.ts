#!/usr/bin/env bun
/**
 * RebelShell 端到端冒烟测试：拉起管理端与客户端，走一遍命令执行与文件往返。
 *
 * 用法: bun scripts/smoke.ts     （或仓库根 `bun run smoke`）
 * 依赖: 已构建的 server/client 产物、bun。端口可经 TCP_PORT / HTTP_PORT 覆盖。
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

const TCP_PORT = process.env.TCP_PORT ?? '19000'
const HTTP_PORT = process.env.HTTP_PORT ?? '18080'
const TOKEN = 'test-token-e2e'
const UUID = 'test-vm-1'
const BASE = `http://127.0.0.1:${HTTP_PORT}`
const WS_HEADERS = {
  Connection: 'Upgrade',
  Upgrade: 'websocket',
  'Sec-WebSocket-Version': '13',
  'Sec-WebSocket-Key': 'dGhlIHNhbXBsZSBub25jZQ==',
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

try {
  // ---- 启动管理端 ----
  console.log('\n== 启动管理端 ==')
  server = Bun.spawn([SERVER_BIN], {
    cwd: REPO_ROOT,
    env: {
      ...process.env,
      ADMIN_TOKEN: TOKEN,
      TCP_PORT,
      HTTP_PORT,
      BIND_ADDR: '127.0.0.1',
    },
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
    [CLIENT_BIN, '--server', `127.0.0.1:${TCP_PORT}`, '--uuid', UUID, '--heartbeat', '2', '--dir', AGENT_DIR],
    { cwd: AGENT_DIR, stdout: 'pipe', stderr: 'pipe' },
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
    [CLIENT_BIN, '--server', `127.0.0.1:${TCP_PORT}`, '--uuid', UUID, '--heartbeat', '2', '--dir', AGENT_DIR],
    { cwd: AGENT_DIR, stdout: 'pipe', stderr: 'pipe' },
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