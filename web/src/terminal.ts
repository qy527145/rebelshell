/**
 * 交互式终端的客户端封装：把 `/ws/terminal` 的消息协议包成一个小对象。
 *
 * 与命令回显不同，终端是一条**双向字节流**：用户在 xterm.js 里敲的每个键要立刻
 * 送到虚拟机，虚拟机上 `vi` / `top` 画的每一帧也要立刻画回来。因此这里不做任何
 * 缓冲或节流 —— xterm.js 的 `onData` 一到就发。
 *
 * 服务端消息里的终端输出一律 Base64：PTY 按字节切数据，出现半个 UTF-8 字符是
 * 常态，直接当 JSON 字符串传会编码失败或变成替换字符。
 */

/** 从 Base64 还原成字节；`TextDecoder` 用流式模式承接被切断的多字节字符。 */
function decodeBase64(data: string): Uint8Array {
  const binary = atob(data)
  const bytes = new Uint8Array(binary.length)
  for (let index = 0; index < binary.length; index += 1) {
    bytes[index] = binary.charCodeAt(index)
  }
  return bytes
}

/** 字节转 Base64；按键输入可能是中文粘贴，必须先按 UTF-8 编码。 */
function encodeBase64(text: string): string {
  const bytes = new TextEncoder().encode(text)
  let binary = ''
  for (const byte of bytes) binary += String.fromCharCode(byte)
  return btoa(binary)
}

export type TerminalMessage =
  | { type: 'ready' }
  | { type: 'output'; data: string }
  | { type: 'exit'; code: number }
  | { type: 'error'; message: string }

export interface TerminalCallbacks {
  /** 终端已在虚拟机侧就绪。 */
  onReady?: () => void
  /** 终端输出（已解码为文本，交给 xterm.js 渲染）。 */
  onOutput?: (text: string) => void
  /** 终端结束。 */
  onExit?: (code: number) => void
  /** 打开失败或中途出错。 */
  onError?: (message: string) => void
  /** 连接状态变化，供界面显示。 */
  onStateChange?: (state: TerminalState) => void
}

export type TerminalState = 'connecting' | 'open' | 'closed'

export interface TerminalOptions {
  cols: number
  rows: number
  /** 留空表示用虚拟机平台的默认 shell。 */
  shell?: string
}

/**
 * 一条终端连接。生命周期：`connect()` → `onReady` → 双向收发 → `close()`。
 *
 * 之所以自己管 WebSocket 而不是复用 `useSocket`：那个 hook 是给「服务端单向推送」
 * 设计的（收到消息就 setState），而终端需要把 xterm.js 的按键也写回去，
 * 还得处理「output 里夹着被切断的多字节字符」这类流式细节。
 */
export class TerminalConnection {
  private socket: WebSocket | null = null
  private callbacks: TerminalCallbacks
  private options: TerminalOptions
  private decoder = new TextDecoder('utf-8')
  private closed = false

  constructor(
    private token: string,
    private uuid: string,
    options: TerminalOptions,
    callbacks: TerminalCallbacks = {},
  ) {
    this.options = options
    this.callbacks = callbacks
  }

  connect(): void {
    if (this.closed) return
    this.callbacks.onStateChange?.('connecting')

    const protocol = location.protocol === 'https:' ? 'wss:' : 'ws:'
    const query = new URLSearchParams({ token: this.token, uuid: this.uuid })
    const socket = new WebSocket(`${protocol}//${location.host}/ws/terminal?${query}`)
    this.socket = socket

    socket.onopen = () => {
      if (this.closed) return
      this.callbacks.onStateChange?.('open')
      // 第一条必须是 open：服务端靠它知道要开多大的终端、用什么 shell。
      this.send({
        type: 'open',
        cols: this.options.cols,
        rows: this.options.rows,
        shell: this.options.shell ?? '',
      })
    }

    socket.onmessage = (event: MessageEvent<string>) => {
      if (this.closed) return
      let message: TerminalMessage
      try {
        message = JSON.parse(event.data) as TerminalMessage
      } catch {
        // 无法解析的消息直接忽略，不打断终端。
        return
      }
      switch (message.type) {
        case 'ready':
          this.callbacks.onReady?.()
          break
        case 'output':
          // 流式解码：一次 output 可能刚好切在多字节字符中间，
          // 用 stream 模式让 TextDecoder 把余下的字节留到下一片。
          this.callbacks.onOutput?.(
            this.decoder.decode(decodeBase64(message.data), { stream: true }),
          )
          break
        case 'exit':
          this.closed = true
          this.callbacks.onExit?.(message.code)
          this.callbacks.onStateChange?.('closed')
          break
        case 'error':
          this.closed = true
          this.callbacks.onError?.(message.message)
          this.callbacks.onStateChange?.('closed')
          break
      }
    }

    socket.onclose = () => {
      if (this.closed) return
      this.closed = true
      // 冲掉解码器里残留的半个字符，否则最后几个字会丢。
      const tail = this.decoder.decode()
      if (tail) this.callbacks.onOutput?.(tail)
      this.callbacks.onStateChange?.('closed')
    }

    socket.onerror = () => {
      // onclose 紧随其后，状态与提示都在那里收尾。
    }
  }

  /** 发送按键 / 粘贴内容。 */
  input(data: string): void {
    if (this.closed || !data) return
    this.send({ type: 'input', data: encodeBase64(data) })
  }

  /** 通知虚拟机调整 PTY 大小（被控程序会收到 SIGWINCH 重新排版）。 */
  resize(cols: number, rows: number): void {
    if (this.closed) return
    this.options = { ...this.options, cols, rows }
    this.send({ type: 'resize', cols, rows })
  }

  /** 主动关闭。幂等。 */
  close(): void {
    if (this.closed) return
    this.send({ type: 'close' })
    this.closed = true
    this.socket?.close()
    this.socket = null
  }

  /** 连接是否仍然有效。 */
  get isOpen(): boolean {
    return !this.closed && this.socket?.readyState === WebSocket.OPEN
  }

  private send(payload: Record<string, unknown>): void {
    const socket = this.socket
    if (!socket || socket.readyState !== WebSocket.OPEN) return
    socket.send(JSON.stringify(payload))
  }
}