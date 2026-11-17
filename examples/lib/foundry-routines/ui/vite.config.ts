import { defineConfig } from 'vite';
import react from '@vitejs/plugin-react';

export default defineConfig({
  plugins: [react()],
  server: {
    host: '127.0.0.1',
    proxy: {
      '/api': {
        target: 'http://127.0.0.1:8780',
        changeOrigin: true,
        configure(proxy) {
          proxy.on('proxyReq', proxyRequest => {
            proxyRequest.setHeader('Origin', 'http://127.0.0.1:8780');
          });
        },
      },
    },
  },
});
