import { defineConfig } from "vite";
import vue from "@vitejs/plugin-vue";

// Console dev server; API calls proxy to the Rust core service. The target is
// overridable so a browser smoke can run against the device before deploying:
//   VITE_API_TARGET=http://192.168.1.73:8930 npm run dev
const apiTarget = process.env.VITE_API_TARGET || "http://127.0.0.1:8930";

export default defineConfig({
  plugins: [vue()],
  server: {
    port: 8931,
    proxy: {
      "/api": apiTarget,
    },
  },
});
