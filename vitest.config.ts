import { defineConfig } from "vitest/config";
import path from "node:path";

// 独立于 vite.config.ts：那份是异步函数式 defineConfig，且带 Tauri 专用的
// server / base 配置，测试用不上。这里只保留测试真正需要的两件事：
// @ 别名（与 vite.config.ts 保持一致）与测试文件匹配规则。
export default defineConfig({
  resolve: {
    alias: {
      "@": path.resolve(__dirname, "./src"),
    },
  },
  test: {
    // 默认 node 环境：多数用例是纯逻辑模块，启动最快。
    // 组件测试（*.test.tsx）在文件头用 `@vitest-environment jsdom` 单文件声明 DOM 环境，
    // 不把整个套件拖进 jsdom。
    environment: "node",
    include: ["src/**/*.test.ts", "src/**/*.test.tsx"],
  },
});
