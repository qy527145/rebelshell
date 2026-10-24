/**
 * 后端接口的类型定义与调用封装。
 *
 * 所有请求都带上 `Authorization: Bearer <token>`；WebSocket 因为浏览器无法设置
 * 自定义头，改用 `?token=` 查询参数（服务端两种都接受）。
 */

export interface Device {
  uuid: string
  hostname: string
  ip: string
  os: string
  /** 最近一次心跳的 Unix 毫秒时间戳。 */
  last_seen_ms: number
  online: boolean
}

export interface DevicesResponse {
  devices: Device[]
  server_time_ms: number
}

export interface CommandResponse {
  uuid: string
  command: string
  output: string
}

export interface PushResponse {
  uuid: string
  name: string
  size: number
  message: string
}

/** 服务端通过 WebSocket 推送的消息。 */
export type ServerMessage =
  | { type: 'devices'; devices: Device[]; server_time_ms: number }
  | { type: 'output'; uuid: string; command: string; stream: 'stdout' | 'stderr' | 'system'; text: string }
  | { type: 'done'; uuid: string; command: string; ok: boolean; message: string }
  | { type: 'ping' }

/** 携带后端返回的可读错误信息。 */
export class ApiError extends Error {
  readonly status: number

  constructor(status: number, message: string) {
    super(message)
    this.name = 'ApiError'
    this.status = status
  }
}

/** 从响应体里提取错误文案；后端所有失败路径都返回 `{ error: string }`。 */
async function errorFrom(response: Response): Promise<ApiError> {
  const text = await response.text().catch(() => '')
  if (text) {
    try {
      const parsed: unknown = JSON.parse(text)
      if (parsed && typeof parsed === 'object' && 'error' in parsed) {
        const { error } = parsed as { error?: unknown }
        if (typeof error === 'string') return new ApiError(response.status, error)
      }
    } catch {
      // 不是 JSON，直接用原文。
    }
    return new ApiError(response.status, text)
  }
  return new ApiError(response.status, `请求失败（HTTP ${response.status}）`)
}

async function request<T>(path: string, token: string, init: RequestInit = {}): Promise<T> {
  const response = await fetch(path, {
    ...init,
    headers: { ...init.headers, Authorization: `Bearer ${token}` },
  })
  if (!response.ok) throw await errorFrom(response)

  if (response.status === 204) return undefined as T
  const text = await response.text()
  if (!text) return undefined as T
  return JSON.parse(text) as T
}

/** 校验令牌是否可用（`POST /api/login`）。 */
export async function login(token: string): Promise<void> {
  const response = await fetch('/api/login', {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ token }),
  })
  if (!response.ok) throw await errorFrom(response)
}

export function fetchDevices(token: string): Promise<DevicesResponse> {
  return request<DevicesResponse>('/api/devices', token)
}

export function runCommand(token: string, uuid: string, command: string): Promise<CommandResponse> {
  return request<CommandResponse>('/api/command', token, {
    method: 'POST',
    headers: { 'Content-Type': 'application/json' },
    body: JSON.stringify({ uuid, command }),
  })
}

/** 下发文件到虚拟机（管理端 → 虚拟机）。 */
export async function pushFile(
  token: string,
  uuid: string,
  file: File,
  path: string,
): Promise<PushResponse> {
  const form = new FormData()
  form.append('uuid', uuid)
  if (path.trim()) form.append('path', path.trim())
  form.append('file', file, file.name)
  // 不设 Content-Type：交给浏览器带上 multipart 的 boundary。
  return request<PushResponse>('/api/files/push', token, { method: 'POST', body: form })
}

/**
 * 从虚拟机提取文件（虚拟机 → 管理端）。
 *
 * 服务端把客户端回传的分块直接流式写入响应体，因此这里拿到 blob 后
 * 用临时链接触发浏览器下载。
 */
export async function pullFile(token: string, uuid: string, path: string): Promise<void> {
  const url = `/api/files/pull?uuid=${encodeURIComponent(uuid)}&path=${encodeURIComponent(path)}`
  const response = await fetch(url, { headers: { Authorization: `Bearer ${token}` } })
  if (!response.ok) throw await errorFrom(response)

  const blob = await response.blob()
  const filename = filenameFrom(response.headers.get('Content-Disposition')) ?? basename(path)
  triggerDownload(blob, filename)
}

function filenameFrom(disposition: string | null): string | null {
  if (!disposition) return null
  const match = /filename="([^"]*)"/.exec(disposition)
  return match?.[1] ?? null
}

function basename(path: string): string {
  const normalized = path.replace(/[\\/]+$/, '')
  const index = Math.max(normalized.lastIndexOf('/'), normalized.lastIndexOf('\\'))
  return index >= 0 ? normalized.slice(index + 1) : normalized
}

function triggerDownload(blob: Blob, filename: string): void {
  const url = URL.createObjectURL(blob)
  const anchor = document.createElement('a')
  anchor.href = url
  anchor.download = filename
  document.body.append(anchor)
  anchor.click()
  anchor.remove()
  // 立即回收会让部分浏览器来不及开始下载，延后一拍更稳。
  setTimeout(() => URL.revokeObjectURL(url), 10_000)
}

/** 拼出 WebSocket 地址；`?token=` 让服务端完成握手鉴权。 */
export function websocketUrl(token: string): string {
  const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
  return `${protocol}//${location.host}/ws?token=${encodeURIComponent(token)}`
}