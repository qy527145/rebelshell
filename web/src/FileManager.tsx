/**
 * 文件管理器：单栏的远程目录浏览器 + 现有的上传 / 下载能力。
 *
 * 交互尽量贴近资源管理器：
 * - 双击目录进入，双击文件下载；
 * - 单击选中，然后在上方操作栏做重命名 / 删除 / 编辑；
 * - 面包屑可以点回任意上一层；
 * - 支持把本机文件直接拖进列表上传。
 *
 * 「当前目录」由服务端返回的绝对路径维护（客户端的 `LIST_DIR` 会回带解析后的
 * 路径），因此不依赖前端自己拼路径 —— Windows 的反斜杠、Linux 的正斜杠、
 * 以及符号链接都会由客户端那边统一成真实路径。
 */

import { useCallback, useEffect, useMemo, useRef, useState } from 'react'
import {
  ApiError,
  deletePath,
  listDir,
  makeDir,
  pullFile,
  pushFile,
  readText,
  renamePath,
  writeText,
  type DirEntry,
} from './api'

interface Props {
  token: string
  uuid: string
  onNotify: (kind: 'ok' | 'err', text: string) => void
  onLogout: () => void
}

/** 在线编辑器里打开的文件。 */
interface Editing {
  path: string
  content: string
  /** 原始内容，用来判断是否有未保存改动。 */
  original: string
  dirty: boolean
}

export function FileManager({ token, uuid, onNotify, onLogout }: Props) {
  /** 当前目录的绝对路径；空串表示「还没加载」。 */
  const [cwd, setCwd] = useState('')
  const [entries, setEntries] = useState<DirEntry[]>([])
  const [selected, setSelected] = useState<string | null>(null)
  const [busy, setBusy] = useState(false)
  const [loading, setLoading] = useState(false)
  const [error, setError] = useState<string | null>(null)
  const [editing, setEditing] = useState<Editing | null>(null)
  const [dragging, setDragging] = useState(false)
  const uploadRef = useRef<HTMLInputElement | null>(null)

  /** 统一处理「令牌过期 → 退回登录」。 */
  const handleError = useCallback(
    (err: unknown, fallback: string) => {
      if (err instanceof ApiError && err.status === 401) {
        onLogout()
        return
      }
      const message = err instanceof ApiError ? err.message : fallback
      setError(message)
    },
    [onLogout],
  )

  const load = useCallback(
    async (path: string) => {
      setLoading(true)
      setError(null)
      try {
        const response = await listDir(token, uuid, path)
        setCwd(response.path)
        setEntries(response.entries)
        setSelected(null)
      } catch (err) {
        handleError(err, '列目录失败')
      } finally {
        setLoading(false)
      }
    },
    [token, uuid, handleError],
  )

  // 首次进入（或换设备）时先列工作目录，页面一打开就有内容。
  useEffect(() => {
    setCwd('')
    void load('')
  }, [load])

  const entry = useMemo(
    () => entries.find((item) => item.name === selected) ?? null,
    [entries, selected],
  )

  /** 当前目录 + 名字 = 绝对路径。服务端返回的 cwd 已是绝对路径。 */
  const fullPath = useCallback(
    (name: string) => {
      if (!cwd) return name
      const separator = cwd.includes('\\') && !cwd.includes('/') ? '\\' : '/'
      return cwd.endsWith(separator) ? `${cwd}${name}` : `${cwd}${separator}${name}`
    },
    [cwd],
  )

  const parentPath = useMemo(() => {
    if (!cwd) return ''
    const normalized = cwd.replace(/[\\/]+$/, '')
    const index = Math.max(normalized.lastIndexOf('/'), normalized.lastIndexOf('\\'))
    if (index <= 0) return normalized.slice(0, index + 1)
    return normalized.slice(0, index)
  }, [cwd])

  /** 面包屑：把路径逐段拆开，每段都能点。 */
  const crumbs = useMemo(() => {
    if (!cwd) return []
    const separator = cwd.includes('\\') && !cwd.includes('/') ? '\\' : '/'
    const parts = cwd.replace(/[\\/]+$/, '').split(/[\\/]/)
    const out: Array<{ label: string; path: string }> = []
    let accumulated = ''
    parts.forEach((part, index) => {
      if (index === 0 && separator === '/') {
        // Unix 的根：保留开头的斜杠。
        accumulated = '/'
        return
      }
      if (index === 0 && part.endsWith(':')) {
        // Windows 的盘符：`C:` 后面要跟分隔符才是完整路径。
        accumulated = `${part}${separator}`
        out.push({ label: part, path: accumulated })
        return
      }
      accumulated =
        accumulated.endsWith(separator) || accumulated === ''
          ? `${accumulated}${part}`
          : `${accumulated}${separator}${part}`
      out.push({ label: part, path: accumulated })
    })
    return out
  }, [cwd])

  const download = useCallback(
    async (item: DirEntry) => {
      setError(null)
      try {
        await pullFile(token, uuid, fullPath(item.name))
        onNotify('ok', `已开始下载 ${item.name}`)
      } catch (err) {
        handleError(err, '下载失败')
      }
    },
    [token, uuid, fullPath, onNotify, handleError],
  )

  /** 双击：目录进入、文件下载 —— 资源管理器的常规行为。 */
  const open = useCallback(
    (item: DirEntry) => {
      if (item.kind === 'd') {
        void load(fullPath(item.name))
        return
      }
      void download(item)
    },
    [fullPath, load, download],
  )

  const upload = useCallback(
    async (file: File) => {
      setError(null)
      const target = fullPath(file.name)
      onNotify('ok', `正在上传 ${file.name} → ${target}`)
      try {
        const response = await pushFile(token, uuid, file, target)
        onNotify('ok', response.message)
        await load(cwd)
      } catch (err) {
        handleError(err, '上传失败')
      }
    },
    [token, uuid, fullPath, onNotify, load, cwd, handleError],
  )

  const createFolder = useCallback(async () => {
    const name = window.prompt('新建文件夹的名称', '新建文件夹')
    if (!name) return
    setBusy(true)
    setError(null)
    try {
      const response = await makeDir(token, uuid, fullPath(name))
      onNotify('ok', response.message)
      await load(cwd)
    } catch (err) {
      handleError(err, '新建文件夹失败')
    } finally {
      setBusy(false)
    }
  }, [token, uuid, fullPath, onNotify, load, cwd, handleError])

  const rename = useCallback(async () => {
    if (!entry) return
    const name = window.prompt('新的名称', entry.name)
    if (!name || name === entry.name) return
    setBusy(true)
    setError(null)
    try {
      const response = await renamePath(token, uuid, fullPath(entry.name), name)
      onNotify('ok', response.message)
      await load(cwd)
    } catch (err) {
      handleError(err, '重命名失败')
    } finally {
      setBusy(false)
    }
  }, [entry, token, uuid, fullPath, onNotify, load, cwd, handleError])

  const remove = useCallback(async () => {
    if (!entry) return
    const isDir = entry.kind === 'd'
    const hint = isDir
      ? `确定删除目录 ${entry.name} 及其全部内容？此操作不可撤销。`
      : `确定删除文件 ${entry.name}？此操作不可撤销。`
    if (!window.confirm(hint)) return

    setBusy(true)
    setError(null)
    try {
      // 目录走递归删除；客户端那边只有在显式 recursive 时才会删整棵树。
      const response = await deletePath(token, uuid, fullPath(entry.name), isDir)
      onNotify('ok', response.message)
      await load(cwd)
    } catch (err) {
      handleError(err, '删除失败')
    } finally {
      setBusy(false)
    }
  }, [entry, token, uuid, fullPath, onNotify, load, cwd, handleError])

  const edit = useCallback(async () => {
    if (!entry || entry.kind === 'd') return
    setBusy(true)
    setError(null)
    try {
      const path = fullPath(entry.name)
      const response = await readText(token, uuid, path)
      setEditing({
        path: response.path,
        content: response.content,
        original: response.content,
        dirty: false,
      })
    } catch (err) {
      handleError(err, '读取文件失败')
    } finally {
      setBusy(false)
    }
  }, [entry, token, uuid, fullPath, handleError])

  const save = useCallback(async () => {
    if (!editing) return
    setBusy(true)
    setError(null)
    try {
      const response = await writeText(token, uuid, editing.path, editing.content)
      onNotify('ok', response.message)
      setEditing({ ...editing, original: editing.content, dirty: false })
    } catch (err) {
      handleError(err, '保存失败')
    } finally {
      setBusy(false)
    }
  }, [editing, token, uuid, onNotify, handleError])

  return (
    <section className="pane">
      <header className="pane__header">
        <span className="pane__title">文件</span>
        <div className="pane__spacer" />
        <button
          className="button button--ghost"
          onClick={() => void load(parentPath)}
          disabled={!cwd || loading}
          title="返回上一层"
        >
          上级
        </button>
        <button
          className="button button--ghost"
          onClick={() => void load(cwd)}
          disabled={loading}
          title="重新列目录"
        >
          刷新
        </button>
      </header>

      <div className="file-toolbar">
        <nav className="crumbs" title={cwd}>
          {crumbs.length === 0 && <span className="crumbs__empty">（未加载）</span>}
          {crumbs.map((crumb, index) => (
            <span key={`${crumb.path}-${index}`} className="crumbs__item">
              {index > 0 && <span className="crumbs__sep">›</span>}
              <button
                className="crumbs__link"
                onClick={() => void load(crumb.path)}
                disabled={loading}
              >
                {crumb.label}
              </button>
            </span>
          ))}
        </nav>

        <div className="file-actions">
          <button className="button" onClick={createFolder} disabled={busy || !cwd}>
            新建文件夹
          </button>
          <button className="button" onClick={() => uploadRef.current?.click()} disabled={busy || !cwd}>
            上传
          </button>
          <button
            className="button"
            onClick={() => entry && void download(entry)}
            disabled={busy || !entry || entry.kind === 'd'}
            title={entry?.kind === 'd' ? '目录不能直接下载' : '下载到本机'}
          >
            下载
          </button>
          <button
            className="button"
            onClick={edit}
            disabled={busy || !entry || entry.kind === 'd'}
            title={entry?.kind === 'd' ? '目录不能编辑' : '在线编辑文本内容'}
          >
            编辑
          </button>
          <button className="button" onClick={rename} disabled={busy || !entry}>
            重命名
          </button>
          <button className="button button--danger" onClick={remove} disabled={busy || !entry}>
            删除
          </button>
        </div>
      </div>

      {error && (
        <div className="banner banner--error">
          {error}
          <button className="banner__close" onClick={() => setError(null)}>
            ×
          </button>
        </div>
      )}

      <div
        className={`file-list${dragging ? ' file-list--drop' : ''}`}
        onDragOver={(event) => {
          event.preventDefault()
          setDragging(true)
        }}
        onDragLeave={() => setDragging(false)}
        onDrop={(event) => {
          event.preventDefault()
          setDragging(false)
          const file = event.dataTransfer.files?.[0]
          if (file) void upload(file)
        }}
      >
        <table className="file-table">
          <thead>
            <tr>
              <th className="file-table__name">名称</th>
              <th className="file-table__size">大小</th>
              <th className="file-table__time">修改时间</th>
            </tr>
          </thead>
          <tbody>
            {loading && entries.length === 0 && (
              <tr>
                <td colSpan={3} className="file-table__empty">
                  正在读取目录…
                </td>
              </tr>
            )}
            {!loading && entries.length === 0 && (
              <tr>
                <td colSpan={3} className="file-table__empty">
                  这个目录是空的。可以把文件拖进来上传。
                </td>
              </tr>
            )}
            {entries.map((item) => (
              <tr
                key={item.name}
                className={`file-row${item.name === selected ? ' file-row--active' : ''}`}
                onClick={() => setSelected(item.name)}
                onDoubleClick={() => open(item)}
              >
                <td className="file-table__name">
                  <span className={`file-icon file-icon--${item.kind}`}>
                    {item.kind === 'd' ? '📁' : item.kind === 'l' ? '🔗' : '📄'}
                  </span>
                  <span className="file-name" title={item.name}>
                    {item.name}
                  </span>
                </td>
                <td className="file-table__size">{item.kind === 'd' ? '—' : formatBytes(item.size)}</td>
                <td className="file-table__time">{formatTime(item.modified_ms)}</td>
              </tr>
            ))}
          </tbody>
        </table>
      </div>

      <input
        ref={uploadRef}
        type="file"
        hidden
        onChange={(event) => {
          const file = event.target.files?.[0]
          if (file) void upload(file)
          // 清空，否则同一个文件第二次选不会触发 change。
          if (uploadRef.current) uploadRef.current.value = ''
        }}
      />

      {editing && (
        <div className="editor">
          <header className="editor__header">
            <span className="editor__path mono" title={editing.path}>
              {editing.path}
            </span>
            {editing.dirty && <span className="chip chip--warn">未保存</span>}
            <div className="pane__spacer" />
            <button className="button" onClick={save} disabled={busy || !editing.dirty}>
              保存
            </button>
            <button
              className="button button--ghost"
              onClick={() => {
                if (!editing.dirty || window.confirm('有未保存的改动，确定关闭？')) {
                  setEditing(null)
                }
              }}
            >
              关闭
            </button>
          </header>
          <textarea
            className="editor__body mono"
            spellCheck={false}
            value={editing.content}
            onChange={(event) =>
              setEditing({
                ...editing,
                content: event.target.value,
                dirty: event.target.value !== editing.original,
              })
            }
          />
          <p className="editor__hint">
            保存时客户端会先把原文件备份为同名 <code>.bak</code>，误覆盖还可以找回来。
          </p>
        </div>
      )}
    </section>
  )
}

// ---- 格式化 ----

function formatBytes(bytes: number): string {
  if (bytes < 1024) return `${bytes} B`
  const units = ['KiB', 'MiB', 'GiB', 'TiB']
  let value = bytes / 1024
  let unit = 0
  while (value >= 1024 && unit < units.length - 1) {
    value /= 1024
    unit += 1
  }
  return `${value.toFixed(1)} ${units[unit]}`
}

function formatTime(ms: number): string {
  if (!ms) return '—'
  const date = new Date(ms)
  // 用固定格式而不是 toLocaleString：跨浏览器/地区的输出差异会让列宽跳动。
  const pad = (value: number) => value.toString().padStart(2, '0')
  return (
    `${date.getFullYear()}-${pad(date.getMonth() + 1)}-${pad(date.getDate())} ` +
    `${pad(date.getHours())}:${pad(date.getMinutes())}`
  )
}