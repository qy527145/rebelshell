/**
 * 交互式终端面板：xterm.js 渲染，背后连着虚拟机上真实的 PTY。
 *
 * 之所以用 xterm.js 而不是自己画一个 `<pre>`：终端体验的关键全在**终端模拟**上 ——
 * 光标定位、`vi`/`top` 的全屏重绘、颜色、Tab 补全、`Ctrl-C`、以及程序对
 * `ESC[6n` 之类的应答。自己实现一个等于重写 xterm，没有意义。
 *
 * 这里只做三件事：把 xterm 的尺寸同步给远端 PTY、把按键送去远端、
 * 把远端输出写进 xterm。其余全部交给 xterm 自己。
 */

import { useCallback, useEffect, useRef, useState } from 'react'
import { Terminal } from '@xterm/xterm'
import { FitAddon } from '@xterm/addon-fit'
import '@xterm/xterm/css/xterm.css'
import { TerminalConnection, type TerminalState } from './terminal'

/** 终端配色：与页面深色主题一致，也让 16 色 ANSI 在深底上可读。 */
const THEME = {
  background: '#0b0d11',
  foreground: '#e6e9ef',
  cursor: '#4f9dff',
  selectionBackground: '#2f6fbf80',
  black: '#1d222b',
  red: '#ef6a6a',
  green: '#43c98a',
  yellow: '#e0b341',
  blue: '#4f9dff',
  magenta: '#c78bf0',
  cyan: '#4fd6d6',
  white: '#c9d1d9',
  brightBlack: '#6b7480',
  brightRed: '#ff8a8a',
  brightGreen: '#5fe0a0',
  brightYellow: '#f0c85a',
  brightBlue: '#6fb0ff',
  brightMagenta: '#d8a6ff',
  brightCyan: '#6ee6e6',
  brightWhite: '#eef2f7',
}

/** 可选 shell。留空 = 用虚拟机平台的默认值。 */
const SHELL_CHOICES = [
  { label: '默认', value: '' },
  { label: 'bash', value: '/bin/bash' },
  { label: 'sh', value: '/bin/sh' },
  { label: 'zsh', value: '/bin/zsh' },
  { label: 'PowerShell', value: 'powershell.exe' },
  { label: 'cmd', value: 'cmd.exe' },
]

export function TerminalPane({ token, uuid }: { token: string; uuid: string }) {
  const hostRef = useRef<HTMLDivElement | null>(null)
  const termRef = useRef<Terminal | null>(null)
  const fitRef = useRef<FitAddon | null>(null)
  const connectionRef = useRef<TerminalConnection | null>(null)

  const [state, setState] = useState<TerminalState>('connecting')
  const [shell, setShell] = useState('')
  /** 每次「重开」都递增，用来触发重新连接（无需手动拆装 effect）。 */
  const [generation, setGeneration] = useState(0)

  /** 把终端里已经显示的内容原样保留在新连接的开头，重连后不像换了台机器。 */
  const historyRef = useRef<string[]>([])

  const fit = useCallback(() => {
    const terminal = termRef.current
    const fitAddon = fitRef.current
    if (!terminal || !fitAddon) return
    try {
      fitAddon.fit()
    } catch {
      // 容器还没布局好（宽度/高度为 0）时 fit 会抛错，忽略即可。
      return
    }
    const connection = connectionRef.current
    if (connection?.isOpen) {
      connection.resize(terminal.cols, terminal.rows)
    }
  }, [])

  // 终端实例只建一次：xterm 的创建开销不小，重建会闪屏并丢掉回滚缓冲。
  useEffect(() => {
    const host = hostRef.current
    if (!host) return

    const terminal = new Terminal({
      cursorBlink: true,
      fontFamily:
        'ui-monospace, SFMono-Regular, "SF Mono", Menlo, Consolas, "Liberation Mono", monospace',
      fontSize: 13,
      lineHeight: 1.25,
      // 回滚缓冲：Xshell 那种「往上翻几百行」的体验。
      scrollback: 10_000,
      theme: THEME,
      // 远端已经处理了换行，不要再让 xterm 转换 CR/LF。
      convertEol: false,
      macOptionIsMeta: true,
    })
    const fitAddon = new FitAddon()
    terminal.loadAddon(fitAddon)
    terminal.open(host)
    termRef.current = terminal
    fitRef.current = fitAddon

    // 用户按键 → 远端。含粘贴、方向键、Ctrl-C 等所有 xterm 生成的控制序列。
    const disposable = terminal.onData((data) => {
      connectionRef.current?.input(data)
    })

    // 尺寸变化 → 远端。防抖一下：拖动窗口时不需要每像素都发一帧。
    let timer: number | null = null
    const observer = new ResizeObserver(() => {
      if (timer !== null) window.clearTimeout(timer)
      timer = window.setTimeout(fit, 60)
    })
    observer.observe(host)

    // 首帧先 fit 一次，让 open 消息带上正确的列数。
    window.setTimeout(fit, 0)

    return () => {
      if (timer !== null) window.clearTimeout(timer)
      observer.disconnect()
      disposable.dispose()
      terminal.dispose()
      termRef.current = null
      fitRef.current = null
    }
  }, [fit])

  // 每次 generation 变化都重连一条终端。切设备时 uuid 变化也会走到这里。
  useEffect(() => {
    const terminal = termRef.current
    if (!terminal) return

    // 复位：清屏并把光标放回左上角。
    terminal.reset()
    for (const chunk of historyRef.current) terminal.write(chunk)
    if (historyRef.current.length === 0) {
      terminal.write(`\x1b[2m正在打开 ${uuid} 上的交互式终端…\x1b[0m\r\n`)
    }

    const connection = new TerminalConnection(
      token,
      uuid,
      { cols: terminal.cols, rows: terminal.rows, shell },
      {
        onStateChange: setState,
        onOutput: (text) => {
          terminal.write(text)
          // 只留最近若干块，重连时回放用；避免把整场会话都堆在内存里。
          historyRef.current.push(text)
          if (historyRef.current.length > 200) historyRef.current.shift()
        },
        onError: (message) => {
          terminal.write(`\r\n\x1b[31m[打开终端失败] ${message}\x1b[0m\r\n`)
        },
        onExit: (code) => {
          terminal.write(`\r\n\x1b[2m[会话已结束，退出码 ${code}]\x1b[0m\r\n`)
        },
      },
    )
    connectionRef.current = connection
    connection.connect()

    return () => {
      connection.close()
      connectionRef.current = null
    }
  }, [token, uuid, shell, generation])

  const label =
    state === 'open' ? '已连接' : state === 'connecting' ? '连接中…' : '已断开'

  return (
    <section className="pane">
      <header className="pane__header">
        <span className="pane__title">终端</span>
        <span className={`chip chip--${state === 'open' ? 'ok' : 'off'}`}>{label}</span>
        <div className="pane__spacer" />
        <label className="pane__field">
          Shell
          <select
            className="input input--compact"
            value={shell}
            onChange={(event) => setShell(event.target.value)}
          >
            {SHELL_CHOICES.map((choice) => (
              <option key={choice.value} value={choice.value}>
                {choice.label}
              </option>
            ))}
          </select>
        </label>
        <button
          className="button button--ghost"
          onClick={() => {
            historyRef.current = []
            setGeneration((value) => value + 1)
          }}
          title="清空并重新打开一个终端会话"
        >
          重开
        </button>
      </header>
      <div className="terminal-host" ref={hostRef} />
    </section>
  )
}