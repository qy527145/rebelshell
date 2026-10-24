#!/usr/bin/env bun
/**
 * WebSocket 推送的端到端校验。
 *
 * 冒烟脚本只能验证握手状态码，这里进一步断言实际收到的消息序列：
 * 连接即收到设备快照 → 下发命令后收到 output 与 done。
 *
 * 用法: bun scripts/ws-check.ts [http://127.0.0.1:18080] [token] [uuid]
 */

const base = process.argv[2] ?? 'http://127.0.0.1:18080'
const token = process.argv[3] ?? 'test-token-e2e'
const uuid = process.argv[4] ?? 'test-vm-1'

const wsUrl = base.replace(/^http/, 'ws') + `/ws?token=${encodeURIComponent(token)}`

type Message =
  | { type: 'devices'; devices: Array<{ uuid: string; online: boolean }>; server_time_ms: number }
  | { type: 'output'; uuid: string; command: string; stream: string; text: string }
  | { type: 'done'; uuid: string; command: string; ok: boolean; message: string }
  | { type: 'ping' }

const seen: Message[] = []
let failures = 0

function check(ok: boolean, label: string) {
  console.log(`  [${ok ? 'PASS' : 'FAIL'}] ${label}`)
  if (!ok) failures += 1
}

const socket = new WebSocket(wsUrl)

const devicesPromise = new Promise<void>((resolve, reject) => {
  const timer = setTimeout(() => reject(new Error('等待设备快照超时（5s）')), 5_000)
  socket.addEventListener('message', (event) => {
    const message = JSON.parse(String(event.data)) as Message
    seen.push(message)
    if (message.type === 'devices') {
      clearTimeout(timer)
      resolve()
    }
  })
  socket.addEventListener('error', () => reject(new Error('WebSocket 出错')))
})

await devicesPromise

const snapshot = seen.find((message) => message.type === 'devices') as Extract<Message, { type: 'devices' }>
check(Array.isArray(snapshot.devices), '连接后立即收到设备列表快照')
check(
  snapshot.devices.some((device) => device.uuid === uuid && device.online),
  `快照中包含在线设备 ${uuid}`,
)
check(typeof snapshot.server_time_ms === 'number', '快照带 server_time_ms')

// 通过 REST 下发一条命令，断言推送里出现 output 与 done。
const marker = `ws-check-${Math.floor(Math.random() * 1e6)}`
const response = await fetch(`${base}/api/command`, {
  method: 'POST',
  headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
  body: JSON.stringify({ uuid, command: `echo ${marker}` }),
})
check(response.ok, `REST 命令下发成功（HTTP ${response.status}）`)

const deadline = Date.now() + 5_000
while (Date.now() < deadline) {
  const done = seen.find(
    (message) => message.type === 'done' && message.command.includes(marker),
  )
  if (done) break
  await Bun.sleep(50)
}

const output = seen.find(
  (message): message is Extract<Message, { type: 'output' }> =>
    message.type === 'output' && message.command.includes(marker),
)
check(output !== undefined, 'WebSocket 收到命令回显（output）')
check(output?.text.includes(marker) ?? false, `回显内容包含 ${marker}`)
check(output?.stream === 'stdout', `回显标记为 stdout（实际 ${output?.stream}）`)

const done = seen.find(
  (message): message is Extract<Message, { type: 'done' }> =>
    message.type === 'done' && message.command.includes(marker),
)
check(done?.ok === true, 'WebSocket 收到命令完成事件（done.ok=true）')

// 错误路径：给离线设备发命令应收到 done.ok=false，而不是静默失败。
const failing = await fetch(`${base}/api/command`, {
  method: 'POST',
  headers: { Authorization: `Bearer ${token}`, 'Content-Type': 'application/json' },
  body: JSON.stringify({ uuid: 'no-such-device-ws', command: 'echo hi' }),
})
check(failing.status === 404, `给离线设备发命令返回 404（实际 ${failing.status}）`)

socket.close()

console.log()
console.log(`  WebSocket 校验结果：失败 ${failures} 项`)
if (failures > 0) process.exit(1)
console.log('  全部通过')