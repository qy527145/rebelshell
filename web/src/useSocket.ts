import { useCallback, useEffect, useRef, useState } from 'react'
import { websocketUrl, type ServerMessage } from './api'

export type ConnectionState = 'connecting' | 'open' | 'closed'

export interface UseSocketResult {
  state: ConnectionState
  lastMessage: ServerMessage | null
  /** 已重连次数，用于在界面上提示「连接不稳定」。 */
  attempts: number
}

/**
 * 维持一条到服务端的事件通道，并自动重连。
 *
 * 断线后退避重连（1s → 2s → … → 10s 封顶），页面重新可见时立即试一次，
 * 避免把休眠中的标签页一直挂在断开状态。
 */
export function useSocket(token: string | null): UseSocketResult {
  const [state, setState] = useState<ConnectionState>('closed')
  const [lastMessage, setLastMessage] = useState<ServerMessage | null>(null)
  const [attempts, setAttempts] = useState(0)

  const socketRef = useRef<WebSocket | null>(null)
  const timerRef = useRef<number | null>(null)
  const attemptsRef = useRef(0)

  const clearTimer = useCallback(() => {
    if (timerRef.current !== null) {
      window.clearTimeout(timerRef.current)
      timerRef.current = null
    }
  }, [])

  useEffect(() => {
    if (!token) {
      setState('closed')
      return
    }

    let disposed = false

    const connect = () => {
      if (disposed) return
      setState('connecting')

      const socket = new WebSocket(websocketUrl(token))
      socketRef.current = socket

      socket.onopen = () => {
        if (disposed) return
        attemptsRef.current = 0
        setAttempts(0)
        setState('open')
      }

      socket.onmessage = (event: MessageEvent<string>) => {
        if (disposed) return
        try {
          setLastMessage(JSON.parse(event.data) as ServerMessage)
        } catch {
          // 无法解析的消息直接忽略，不打断连接。
        }
      }

      socket.onclose = () => {
        if (disposed) return
        setState('closed')
        attemptsRef.current += 1
        setAttempts(attemptsRef.current)
        const delay = Math.min(1000 * 2 ** (attemptsRef.current - 1), 10_000)
        timerRef.current = window.setTimeout(connect, delay)
      }

      socket.onerror = () => {
        // onclose 会紧随其后触发，重连逻辑只需写在 onclose 里。
      }
    }

    const onVisible = () => {
      if (document.visibilityState === 'visible' && socketRef.current?.readyState !== WebSocket.OPEN) {
        clearTimer()
        connect()
      }
    }

    connect()
    document.addEventListener('visibilitychange', onVisible)

    return () => {
      disposed = true
      clearTimer()
      document.removeEventListener('visibilitychange', onVisible)
      // 主动关闭时把 onclose 置空，避免触发重连。
      const socket = socketRef.current
      if (socket) {
        socket.onclose = null
        socket.close()
      }
      socketRef.current = null
    }
  }, [token, clearTimer])

  return { state, lastMessage, attempts }
}