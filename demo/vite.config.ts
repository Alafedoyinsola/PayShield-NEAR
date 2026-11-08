import react from '@vitejs/plugin-react'
import { defineConfig } from 'vite'

export default defineConfig({
  plugins: [react()],
  server: {
    proxy: {
      '/near-rpc': {
        target: 'https://rpc.testnet.near.org',
        changeOrigin: true,
        rewrite: () => '/',
      },
    },
  },
})
