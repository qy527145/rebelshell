import { defineConfig } from 'vite'
import react from '@vitejs/plugin-react'

// 产物直接落到 server/web/dist，由 rust-embed 编译进服务端二进制。
export default defineConfig({
  plugins: [react()],
  build: {
    outDir: '../server/web/dist',
    emptyOutDir: true,
  },
  server: {
    port: 5173,
    // 开发期把接口与 WebSocket 代理到本地服务端，避免跨域与重复配置地址。
    proxy: {
      '/api': 'http://127.0.0.1:8080',
      '/ws': { target: 'ws://127.0.0.1:8080', ws: true },
    },
  },
})