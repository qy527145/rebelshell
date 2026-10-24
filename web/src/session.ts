/** 令牌在浏览器端的持久化。 */

const STORAGE_KEY = 'rebelshell.token'

export function loadToken(): string | null {
  try {
    const token = window.localStorage.getItem(STORAGE_KEY)
    return token && token.length > 0 ? token : null
  } catch {
    // 隐私模式下 localStorage 可能不可用，退化为「每次都重新登录」。
    return null
  }
}

export function saveToken(token: string): void {
  try {
    window.localStorage.setItem(STORAGE_KEY, token)
  } catch {
    // 存不下也不影响本次会话使用。
  }
}

export function clearToken(): void {
  try {
    window.localStorage.removeItem(STORAGE_KEY)
  } catch {
    // 忽略。
  }
}