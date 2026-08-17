import { defineConfig } from "vite";
import react from "@vitejs/plugin-react";
import tailwindcss from "@tailwindcss/vite";

// @ts-expect-error process is a nodejs global
const host = process.env.TAURI_DEV_HOST;

// Allow overriding the dev server port for parallel-track worktrees.
// Pair with a Tauri config overlay (e.g. tauri.conf.tools.json) so devUrl matches.
// @ts-expect-error process is a nodejs global
const devPort = Number(process.env.LAUNCHPAD_VITE_PORT) || 1420;
const hmrPort = devPort + 1;

// https://vite.dev/config/
export default defineConfig(async () => ({
  plugins: [tailwindcss(), react()],

  // Auto-stamp every build with today's date for debug panel EOL calculation
  define: {
    'import.meta.env.VITE_BUILD_DATE': JSON.stringify(new Date().toISOString().split('T')[0]),
  },

  // Vite options tailored for Tauri development and only applied in `tauri dev` or `tauri build`
  //
  // 1. prevent Vite from obscuring rust errors
  clearScreen: false,
  // 2. tauri expects a fixed port, fail if that port is not available
  server: {
    port: devPort,
    strictPort: true,
    host: host || false,
    hmr: host
      ? {
        protocol: "ws",
        host,
        port: hmrPort,
      }
      : undefined,
    watch: {
      // 3. tell Vite to ignore watching `src-tauri`
      ignored: ["**/src-tauri/**"],
    },
  },
}));
