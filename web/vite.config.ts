import { defineConfig } from "vite";
import preact from "@preact/preset-vite";
import tailwindcss from "@tailwindcss/vite";

// `npm run dev` proxies the API to a dev box (`dev/up.sh tugport-ui 100`); TUGPORT_API overrides it.
const api = process.env.TUGPORT_API ?? "http://localhost:28584";

export default defineConfig({
  plugins: [preact(), tailwindcss()],
  build: { outDir: "dist", emptyOutDir: true },
  server: { proxy: { "/api": { target: api, ws: true } } },
});
