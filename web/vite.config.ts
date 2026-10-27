import { resolve } from 'node:path'
import { defineConfig, loadEnv } from 'vite'
import react from '@vitejs/plugin-react'

// 产物直接落到 server/web/dist，由 rust-embed 编译进服务端二进制。
export default defineConfig(({ mode }) => {
  // 代理目标跟随 `.env` 里的 PORT：bun 跑脚本时已经把它注入进程环境，
  // 直接从 `cd web` 单独起 vite 时则退回读取仓库根的 .env。
  // 只用服务端配置，不注入前端包 —— 带 VITE_ 前缀的变量 Vite 自己会处理。
  const fileEnv = loadEnv(mode, resolve(import.meta.dirname, '..'), '')
  const port = process.env.PORT || fileEnv.PORT || '8080'
  const target = `http://127.0.0.1:${port}`

  return {
    plugins: [react()],
    build: {
      outDir: '../server/web/dist',
      emptyOutDir: true,
    },
    server: {
      // 显式绑 IPv4：默认的 localhost 在 macOS 上只解析到 ::1，
      // 于是 README 里的 http://127.0.0.1:5173 打不开，curl 也连不上。
      host: '127.0.0.1',
      port: 5173,
      // 开发期把接口与 WebSocket 代理到本地服务端，避免跨域与重复配置地址。
      proxy: {
        '/api': target,
        '/ws': { target: target.replace(/^http/, 'ws'), ws: true },
      },
    },
  }
})