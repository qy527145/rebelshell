/**
 * RebelShell 管理台。
 *
 * 每台在线设备都具备三种能力，用顶部标签切换：
 * - **终端**：连到虚拟机上的真实 PTY（xterm.js 渲染，`vi`/`top`/颜色/Tab 补全都能用）
 * - **命令**：沿用「下发一条命令 → 收完整输出」的模型，适合写脚本、一次性操作
 * - **文件**：浏览远程目录、上传下载、新建/重命名/删除、在线编辑文本
 */

import {
  lazy,
  Suspense,
  useCallback,
  useEffect,
  useMemo,
  useRef,
  useState,
  type FormEvent,
  type RefObject,
} from 'react'
import {
  ApiError,
  fetchDevices,
  login,
  pullFile,
  pushFile,
  runCommand,
  type Device,
} from './api'
import { FileManager } from './FileManager'
import { clearToken, loadToken, saveToken } from './session'
import { useSocket } from './useSocket'
import './styles.css'

/**
 * 终端面板按需加载。
 *
 * xterm.js 打进主包会让首屏多背 300 多 KB（gzip 后也有近 100 KB），而登录页与
 * 设备列表根本用不到它。拆成独立 chunk 后，只有真的点开「终端」标签才下载。
 */
const TerminalPane = lazy(() =>
  import('./TerminalPane').then((module) => ({ default: module.TerminalPane })),
)

/** 右侧主区域的标签。 */
type Tab = 'terminal' | 'command' | 'files'

/** 终端里的一行：命令、回显或服务端提示。 */
interface LogEntry {
  id: number
  uuid: string
  kind: 'command' | 'stdout' | 'stderr' | 'system'
  text: string
  timestamp: number
}

let logCounter = 0

export default function App() {
  const [token, setToken] = useState<string | null>(() => loadToken())

  const handleLogout = useCallback(() => {
    clearToken()
    setToken(null)
  }, [])

  if (!token) {
    return <LoginScreen onAuthenticated={setToken} />
  }
  return <Console token={token} onLogout={handleLogout} />
}

// ---- 登录 ----

function LoginScreen({ onAuthenticated }: { onAuthenticated: (token: string) => void }) {
  const [value, setValue] = useState('')
  const [error, setError] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)

  const submit = async (event: FormEvent) => {
    event.preventDefault()
    if (!value.trim() || busy) return
    setBusy(true)
    setError(null)
    try {
      await login(value.trim())
      saveToken(value.trim())
      onAuthenticated(value.trim())
    } catch (err) {
      setError(err instanceof ApiError ? err.message : '登录失败，请检查管理端是否在运行')
    } finally {
      setBusy(false)
    }
  }

  return (
    <div className="login">
      <form className="login__card" onSubmit={submit}>
        <h1 className="login__title">RebelShell</h1>
        <p className="login__hint">请输入管理端令牌（服务端环境变量 ADMIN_TOKEN）</p>
        <input
          className="input"
          type="password"
          autoFocus
          autoComplete="current-password"
          placeholder="ADMIN_TOKEN"
          value={value}
          onChange={(event) => setValue(event.target.value)}
        />
        {error && <p className="login__error">{error}</p>}
        <button className="button button--primary" type="submit" disabled={busy || !value.trim()}>
          {busy ? '验证中…' : '登录'}
        </button>
      </form>
    </div>
  )
}

// ---- 控制台 ----

function Console({ token, onLogout }: { token: string; onLogout: () => void }) {
  const [devices, setDevices] = useState<Device[]>([])
  const [selected, setSelected] = useState<string | null>(null)
  const [tab, setTab] = useState<Tab>('terminal')
  /** 终端标签是否被打开过。一旦打开就不再卸载，避免切回来时会话被重开。 */
  const [terminalMounted, setTerminalMounted] = useState(false)
  const [logs, setLogs] = useState<LogEntry[]>([])
  const [error, setError] = useState<string | null>(null)
  const [now, setNow] = useState(() => Date.now())

  const { state: socketState, lastMessage, attempts } = useSocket(token)
  const autoScrollRef = useRef(true)
  const logEndRef = useRef<HTMLDivElement | null>(null)

  const appendLog = useCallback((entry: Omit<LogEntry, 'id' | 'timestamp'>) => {
    logCounter += 1
    setLogs((previous) => [
      ...previous.slice(-999),
      { ...entry, id: logCounter, timestamp: Date.now() },
    ])
  }, [])

  // 「最后心跳」需要跟着时间走，因此每秒重渲染一次。
  useEffect(() => {
    const timer = window.setInterval(() => setNow(Date.now()), 1000)
    return () => window.clearInterval(timer)
  }, [])

  // 首屏先拉一次设备列表，WebSocket 尚未连上时也有内容可看。
  useEffect(() => {
    let cancelled = false
    fetchDevices(token)
      .then((response) => {
        if (!cancelled) setDevices(response.devices)
      })
      .catch((err: unknown) => {
        if (cancelled) return
        if (err instanceof ApiError && err.status === 401) {
          onLogout()
          return
        }
        setError(err instanceof ApiError ? err.message : '加载设备列表失败')
      })
    return () => {
      cancelled = true
    }
  }, [token, onLogout])

  useEffect(() => {
    if (!lastMessage) return
    switch (lastMessage.type) {
      case 'devices':
        setDevices(lastMessage.devices)
        break
      case 'output':
        appendLog({
          uuid: lastMessage.uuid,
          kind: lastMessage.stream,
          text: lastMessage.text,
        })
        break
      case 'done':
        appendLog({
          uuid: lastMessage.uuid,
          kind: lastMessage.ok ? 'system' : 'stderr',
          text: lastMessage.message,
        })
        break
      case 'ping':
        break
    }
  }, [lastMessage, appendLog])

  // 只有用户停在底部时才自动滚动，避免打断向上翻阅。
  useEffect(() => {
    if (autoScrollRef.current) {
      logEndRef.current?.scrollIntoView({ block: 'end' })
    }
  }, [logs])

  // 设备下线时清掉选中项，否则表单会对着幽灵设备发指令。
  useEffect(() => {
    if (selected && !devices.some((device) => device.uuid === selected && device.online)) {
      setSelected(null)
    }
  }, [devices, selected])

  // 默认落在「终端」上，因此首次选中设备时就把终端挂起来（懒加载的那一段）。
  useEffect(() => {
    if (tab === 'terminal') setTerminalMounted(true)
  }, [tab])

  const current = useMemo(
    () => devices.find((device) => device.uuid === selected) ?? null,
    [devices, selected],
  )

  const handleCommand = useCallback(
    async (command: string) => {
      if (!selected) return
      setError(null)
      appendLog({ uuid: selected, kind: 'command', text: `$ ${command}` })
      try {
        const response = await runCommand(token, selected, command)
        // 输出也会经 WebSocket 广播；为了避免重复显示，这里只在 HTTP 返回
        // 与广播都可能缺失时兜底——即仅在 WS 断开时补一条。
        if (socketState !== 'open' && response.output) {
          appendLog({ uuid: response.uuid, kind: 'stdout', text: response.output })
        }
      } catch (err) {
        if (err instanceof ApiError && err.status === 401) {
          onLogout()
          return
        }
        const message = err instanceof ApiError ? err.message : '命令下发失败'
        appendLog({ uuid: selected, kind: 'stderr', text: message })
      }
    },
    [appendLog, onLogout, selected, socketState, token],
  )

  const handlePush = useCallback(
    async (file: File, path: string) => {
      if (!selected) return
      setError(null)
      appendLog({
        uuid: selected,
        kind: 'command',
        text: `> 下发 ${file.name}（${formatBytes(file.size)}）${path ? ` → ${path}` : ''}`,
      })
      try {
        const response = await pushFile(token, selected, file, path)
        appendLog({ uuid: response.uuid, kind: 'system', text: response.message })
        // 局域网内下发很快，给一句明确的完成提示，避免界面看起来「没反应」。
        appendLog({
          uuid: response.uuid,
          kind: 'system',
          text: `下发完成：${response.name}（${formatBytes(response.size)}）`,
        })
      } catch (err) {
        const message = err instanceof ApiError ? err.message : '文件下发失败'
        appendLog({ uuid: selected, kind: 'stderr', text: message })
      }
    },
    [appendLog, selected, token],
  )

  const handlePull = useCallback(
    async (path: string) => {
      if (!selected) return
      setError(null)
      appendLog({ uuid: selected, kind: 'command', text: `< 提取 ${path}` })
      try {
        await pullFile(token, selected, path)
        appendLog({ uuid: selected, kind: 'system', text: `已开始下载 ${path}` })
      } catch (err) {
        const message = err instanceof ApiError ? err.message : '文件提取失败'
        appendLog({ uuid: selected, kind: 'stderr', text: message })
      }
    },
    [appendLog, selected, token],
  )

  const online = devices.filter((device) => device.online).length

  return (
    <div className="app">
      <aside className="sidebar">
        <header className="sidebar__header">
          <div>
            <h1 className="sidebar__title">RebelShell</h1>
            <p className="sidebar__subtitle">
              在线 {online} / 共 {devices.length}
            </p>
          </div>
          <button className="button button--ghost" onClick={onLogout} title="清除本地令牌">
            退出
          </button>
        </header>

        <ul className="device-list">
          {devices.length === 0 && (
            <li className="device-list__empty">暂无设备。请在虚拟机上启动 rebelshell-client。</li>
          )}
          {devices.map((device) => (
            <li key={device.uuid}>
              <button
                className={`device${device.uuid === selected ? ' device--active' : ''}${
                  device.online ? '' : ' device--offline'
                }`}
                disabled={!device.online}
                onClick={() => setSelected(device.uuid)}
              >
                <span className={`dot dot--${device.online ? 'online' : 'offline'}`} />
                <span className="device__body">
                  <span className="device__host">
                    {device.hostname}
                    <span className="device__os">{device.os}</span>
                  </span>
                  <span className="device__meta">
                    {device.ip} · {device.uuid}
                  </span>
                  <span className="device__meta">
                    {device.online ? '最后心跳 ' : '离线于 '}
                    {formatAge(now - device.last_seen_ms)}
                  </span>
                </span>
              </button>
            </li>
          ))}
        </ul>

        <footer className="sidebar__footer">
          <span className={`dot dot--${socketState === 'open' ? 'online' : 'offline'}`} />
          {socketState === 'open'
            ? '实时通道已连接'
            : socketState === 'connecting'
              ? '正在连接实时通道…'
              : `实时通道已断开（第 ${attempts} 次重试）`}
        </footer>
      </aside>

      <main className="main">
        {error && (
          <div className="banner banner--error">
            {error}
            <button className="banner__close" onClick={() => setError(null)}>
              ×
            </button>
          </div>
        )}
        {current ? (
          <>
            <DeviceHeader device={current} now={now} />
            <nav className="tabs">
              {(
                [
                  ['terminal', '终端'],
                  ['command', '命令'],
                  ['files', '文件'],
                ] as Array<[Tab, string]>
              ).map(([key, label]) => (
                <button
                  key={key}
                  className={`tab${tab === key ? ' tab--active' : ''}`}
                  onClick={() => setTab(key)}
                >
                  {label}
                </button>
              ))}
            </nav>

            {/* 终端一旦打开过就一直挂载（用 display 隐藏），这样切标签不会重开会话、
                也不会丢回滚缓冲。但从未打开过时不挂载 —— 否则懒加载就白做了。 */}
            <div className={`tab-panel${tab === 'terminal' ? '' : ' tab-panel--hidden'}`}>
              {terminalMounted && (
                <Suspense fallback={<div className="placeholder">正在加载终端组件…</div>}>
                  {/* key 绑 uuid：换设备时强制重建 xterm 实例与连接，
                      否则会把上一台机器的回滚缓冲带到新设备上。 */}
                  <TerminalPane key={current.uuid} token={token} uuid={current.uuid} />
                </Suspense>
              )}
            </div>

            {tab === 'command' && (
              <div className="tab-panel tab-panel--command">
                <Echo
                  logs={logs}
                  logEndRef={logEndRef}
                  onAutoScrollChange={(value) => {
                    autoScrollRef.current = value
                  }}
                  onClear={() => setLogs([])}
                />
                <CommandBar onSubmit={handleCommand} />
                <QuickFiles onPush={handlePush} onPull={handlePull} />
              </div>
            )}

            {tab === 'files' && (
              <div className="tab-panel">
                <FileManager
                  token={token}
                  uuid={current.uuid}
                  onNotify={(kind, text) =>
                    appendLog({
                      uuid: current.uuid,
                      kind: kind === 'ok' ? 'system' : 'stderr',
                      text,
                    })
                  }
                  onLogout={onLogout}
                />
              </div>
            )}
          </>
        ) : (
          <div className="placeholder">
            <p>从左侧选择一台在线设备开始。</p>
          </div>
        )}
      </main>
    </div>
  )
}

function DeviceHeader({ device, now }: { device: Device; now: number }) {
  return (
    <header className="device-header">
      <h2 className="device-header__name">{device.hostname}</h2>
      <dl className="device-header__facts">
        <div>
          <dt>系统</dt>
          <dd>{device.os}</dd>
        </div>
        <div>
          <dt>IP</dt>
          <dd>{device.ip}</dd>
        </div>
        <div>
          <dt>UUID</dt>
          <dd className="mono">{device.uuid}</dd>
        </div>
        <div>
          <dt>最后心跳</dt>
          <dd>{formatAge(now - device.last_seen_ms)}</dd>
        </div>
      </dl>
    </header>
  )
}

/** 命令回显区。名字叫 Echo 是为了与「终端」标签区分：这里只是一段日志流。 */
function Echo({
  logs,
  logEndRef,
  onAutoScrollChange,
  onClear,
}: {
  logs: LogEntry[]
  logEndRef: RefObject<HTMLDivElement | null>
  onAutoScrollChange: (value: boolean) => void
  onClear: () => void
}) {
  const containerRef = useRef<HTMLDivElement | null>(null)

  const handleScroll = () => {
    const element = containerRef.current
    if (!element) return
    const distance = element.scrollHeight - element.scrollTop - element.clientHeight
    onAutoScrollChange(distance < 40)
  }

  return (
    <section className="terminal">
      <header className="terminal__header">
        <span>回显</span>
        <button className="button button--ghost" onClick={onClear} disabled={logs.length === 0}>
          清空
        </button>
      </header>
      <div className="terminal__body" ref={containerRef} onScroll={handleScroll}>
        {logs.length === 0 && <p className="terminal__empty">尚无输出。</p>}
        {logs.map((entry) => (
          <pre key={entry.id} className={`line line--${entry.kind}`}>
            <span className="line__time">{formatTime(entry.timestamp)}</span>
            {entry.text.replace(/\n$/, '')}
          </pre>
        ))}
        <div ref={logEndRef} />
      </div>
    </section>
  )
}

function CommandBar({ onSubmit }: { onSubmit: (command: string) => void }) {
  const [value, setValue] = useState('')
  const [history, setHistory] = useState<string[]>([])
  const [cursor, setCursor] = useState<number | null>(null)

  const submit = () => {
    const command = value.trim()
    if (!command) return
    onSubmit(command)
    setHistory((previous) => [command, ...previous].slice(0, 50))
    setCursor(null)
    setValue('')
  }

  const navigate = (delta: number) => {
    if (history.length === 0) return
    const next = cursor === null ? 0 : Math.min(Math.max(cursor + delta, 0), history.length - 1)
    setCursor(next)
    setValue(history[next] ?? '')
  }

  return (
    <form
      className="command-bar"
      onSubmit={(event) => {
        event.preventDefault()
        submit()
      }}
    >
      <span className="command-bar__prompt">$</span>
      <input
        className="input command-bar__input"
        placeholder="输入命令后回车（↑/↓ 浏览历史）"
        value={value}
        spellCheck={false}
        autoComplete="off"
        onChange={(event) => setValue(event.target.value)}
        onKeyDown={(event) => {
          if (event.key === 'ArrowUp') {
            event.preventDefault()
            navigate(1)
          } else if (event.key === 'ArrowDown') {
            event.preventDefault()
            navigate(-1)
          }
        }}
      />
      <button className="button button--primary" type="submit" disabled={!value.trim()}>
        执行
      </button>
    </form>
  )
}

/**
 * 「命令」标签下的按路径收发文件。
 *
 * 完整目录浏览在「文件」标签里；这里保留两条按绝对路径操作的快捷通道 ——
 * 一条命令就能把日志文件抓回来，不必先在目录树里找到它。
 */
function QuickFiles({
  onPush,
  onPull,
}: {
  onPush: (file: File, path: string) => Promise<void>
  onPull: (path: string) => Promise<void>
}) {
  const [path, setPath] = useState('')
  const [target, setTarget] = useState('')
  const [busy, setBusy] = useState<'push' | 'pull' | null>(null)
  const fileRef = useRef<HTMLInputElement | null>(null)

  const push = async () => {
    const file = fileRef.current?.files?.[0]
    if (!file) return
    setBusy('push')
    try {
      await onPush(file, target)
      if (fileRef.current) fileRef.current.value = ''
      setTarget('')
    } finally {
      setBusy(null)
    }
  }

  const pull = async () => {
    const value = path.trim()
    if (!value) return
    setBusy('pull')
    try {
      await onPull(value)
    } finally {
      setBusy(null)
    }
  }

  return (
    <section className="file-panel">
      <div className="file-panel__group">
        <h3 className="file-panel__title">下发文件（本机 → 虚拟机）</h3>
        <div className="file-panel__row">
          <input
            className="input"
            type="file"
            ref={fileRef}
            onChange={(event) => {
              const file = event.target.files?.[0]
              if (file && !target) setTarget(file.name)
            }}
          />
          <input
            className="input"
            placeholder="目标路径（留空则用文件名）"
            value={target}
            onChange={(event) => setTarget(event.target.value)}
          />
          <button className="button" onClick={push} disabled={busy !== null}>
            {busy === 'push' ? '发送中…' : '下发'}
          </button>
        </div>
      </div>

      <div className="file-panel__group">
        <h3 className="file-panel__title">提取文件（虚拟机 → 本机）</h3>
        <div className="file-panel__row">
          <input
            className="input"
            placeholder="虚拟机上的绝对路径，如 C:\\logs\\app.log 或 /var/log/syslog"
            value={path}
            onChange={(event) => setPath(event.target.value)}
            onKeyDown={(event) => {
              if (event.key === 'Enter') pull()
            }}
          />
          <button className="button" onClick={pull} disabled={busy !== null || !path.trim()}>
            {busy === 'pull' ? '接收中…' : '下载'}
          </button>
        </div>
      </div>
    </section>
  )
}

// ---- 格式化工具 ----

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  const units = ['KiB', 'MiB', 'GiB']
  let value = bytes / 1024
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return `${value.toFixed(1)} ${units[unit]}`
}

/** 「3 秒前」这类相对时间；服务端与浏览器时钟可能不一致，因此负数也兜住。 */
function formatAge(milliseconds: number): string {
  const seconds = Math.max(0, Math.round(milliseconds / 1000))
  if (seconds < 60) return `${seconds} 秒前`
  const minutes = Math.floor(seconds / 60)
  if (minutes < 60) return `${minutes} 分钟前`
  const hours = Math.floor(minutes / 60)
  if (hours < 24) return `${hours} 小时前`
  return `${Math.floor(hours / 24)} 天前`
}

function formatTime(timestamp: number): string {
  return new Date(timestamp).toLocaleTimeString('zh-CN', { hour12: false })
}