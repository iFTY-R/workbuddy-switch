// @vitest-environment jsdom
import { cleanup, render, screen, within } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";

import { TooltipProvider } from "@/components/ui/tooltip";
import * as api from "@/lib/api";
import { screenshotDemoResponse } from "@/lib/screenshot-demo";
import { buildSessionTree } from "@/lib/token-session-tree";
import type { TokenStatistics, TokenStatsSource } from "@/lib/types";
import TokenStatsPage from "@/pages/TokenStatsPage";

vi.mock("@/lib/api", () => ({
  getTokenStatistics: vi.fn(),
  asError: (error: unknown) => (error instanceof Error ? error.message : String(error)),
}));

const getTokenStatistics = vi.mocked(api.getTokenStatistics);
const SOURCE_STORAGE_KEY = "wb-switch:token-stats:source";

// recharts 的 ResponsiveContainer 依赖 ResizeObserver；jsdom 没有实现，
// 补一个空实现即可（图表尺寸不影响本文件要验证的 DOM 结构）。
class ResizeObserverStub {
  observe() {}
  unobserve() {}
  disconnect() {}
}
globalThis.ResizeObserver ??= ResizeObserverStub as unknown as typeof ResizeObserver;

const compact = new Intl.NumberFormat("en-US", {
  notation: "compact",
  compactDisplay: "short",
  maximumFractionDigits: 1,
});
const compactToken = (value: number) => compact.format(value).replace(/([kmb])$/i, (unit) => unit.toUpperCase());
const exactToken = (value: number) => new Intl.NumberFormat("en-US").format(value);

function demoStatistics(): TokenStatistics {
  return screenshotDemoResponse("get_token_statistics") as TokenStatistics;
}

/** 无子代理数据的来源（真实后端里 codebuddy-cli / codebuddy-ide 就是这个形状）。 */
function sourceWithoutSubagents(): TokenStatistics {
  const daily = Array.from({ length: 2 }, (_, index) => ({
    key: new Date(Date.now() - (1 - index) * 86_400_000).toISOString().slice(0, 10),
    total: 1_000,
    input: 900,
    output: 100,
    cacheRead: 700,
    cacheWrite: 0,
    uncachedInput: 200,
    records: 3,
    cacheHitRate: 700 / 900,
    mainTotal: 1_000,
    subagentTotal: 0,
  }));
  const source: TokenStatsSource = {
    source: "codebuddy-cli",
    summary: {
      total: 2_000,
      input: 1_800,
      output: 200,
      cacheRead: 1_400,
      cacheWrite: 0,
      uncachedInput: 400,
      records: 6,
      cacheHitRate: 1_400 / 1_800,
      mainTotal: 2_000,
      subagentTotal: 0,
    },
    models: [],
    projects: [],
    sessions: [
      {
        key: "cli-project · session-a",
        title: "只有主会话的来源",
        project: "cli-project",
        sessionId: "session-a",
        kind: "main",
        total: 2_000,
        input: 1_800,
        output: 200,
        cacheRead: 1_400,
        cacheWrite: 0,
        uncachedInput: 400,
        records: 6,
        cacheHitRate: null,
        parentSessionId: null,
        parentKey: null,
        agentId: null,
        agentType: null,
        agentLabel: null,
        subagentTotal: 0,
        subagentSessionCount: 0,
        subagentFileCount: 0,
        subagentEmptyCount: 0,
      },
    ],
    daily,
    hours: [],
    filesScanned: 1,
    dedupeHits: 0,
    parseErrors: 0,
  };
  return { generatedAt: Date.now(), rangeDays: null, sources: [source] };
}

beforeEach(() => {
  window.localStorage.clear();
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("TokenStatsPage 主/子代理展示", () => {
  it("总览卡常驻「含子代理」徽标，并随四档范围联动拆分行", async () => {
    const stats = demoStatistics();
    getTokenStatistics.mockResolvedValue(stats);
    const user = userEvent.setup();
    render(<TooltipProvider><TokenStatsPage /></TooltipProvider>);

    expect(await screen.findByText("含子代理")).toBeTruthy();
    const overview = screen.getByRole("region", { name: "Token 总览" });
    // 默认「今日」档：拆分行存在且两数之和等于总 Token。
    expect(overview.textContent).toContain("主会话");
    expect(overview.textContent).toContain("子代理");
    expect(overview.textContent).toContain("今日");

    const todayText = overview.textContent ?? "";
    await user.click(screen.getByRole("tab", { name: "总计" }));
    const totalText = screen.getByRole("region", { name: "Token 总览" }).textContent ?? "";
    // 四档联动：今日与总计的拆分行文案必须不同，且总计含 source.summary 的两个数。
    expect(totalText).not.toBe(todayText);
    const summary = stats.sources[0].summary;
    expect(totalText).toContain(compactToken(summary.mainTotal));
    expect(totalText).toContain(compactToken(summary.subagentTotal));
    expect(summary.mainTotal + summary.subagentTotal).toBe(summary.total);
  });

  it("会话排行先组树再截断：父行合计含子代理，展开后可见子行与降级标签", async () => {
    const stats = demoStatistics();
    getTokenStatistics.mockResolvedValue(stats);
    const user = userEvent.setup();
    render(<TooltipProvider><TokenStatsPage /></TooltipProvider>);

    const ranking = await screen.findByRole("region", { name: "消耗最高的会话" });
    const source = stats.sources[0];
    const roots = buildSessionTree(source.sessions);
    const top = roots[0];
    const mainRows = source.sessions.filter((row) => row.kind === "main").length;
    const subagentRows = source.sessions.length - mainRows;

    // 排行标题按 kind 区分计数。
    expect(ranking.textContent).toContain(`共 ${mainRows} 个主会话 · ${subagentRows} 个子代理`);
    // 父行 token 用 aggregate（含子行），不是 session.total（仅自身）。
    expect(top.aggregate).not.toBe(top.own);
    expect(within(ranking).getByLabelText(`${exactToken(top.aggregate)} Token`)).toBeTruthy();

    const toggle = within(ranking).getByRole("button", {
      name: /展开「完善 Token 统计仪表盘与本地用量分析」的子代理/,
    });
    expect(toggle.getAttribute("aria-expanded")).toBe("false");
    // 默认收起：子行不可见。
    expect(within(ranking).queryByLabelText("Explore · 查旧版检查项实现")).toBeNull();

    await user.click(toggle);
    expect(
      within(ranking)
        .getByRole("button", { name: /收起「完善 Token 统计仪表盘与本地用量分析」的子代理/ })
        .getAttribute("aria-expanded"),
    ).toBe("true");
    // 第 1 级标签与第 3 级降级标签都要渲染，且 agentType 为 null 时不能渲染 "null"。
    expect(within(ranking).getByLabelText("Explore · 查旧版检查项实现")).toBeTruthy();
    expect(within(ranking).getByLabelText("子代理 · agent-9c1f4a2b")).toBeTruthy();
    expect(ranking.textContent).not.toContain("null");
    // 演示数据没有空子代理，不显示灰字。
    expect(ranking.textContent).not.toContain("无用量记录");
  });

  it("「展开全部」一次展开所有带子行的父行", async () => {
    getTokenStatistics.mockResolvedValue(demoStatistics());
    const user = userEvent.setup();
    render(<TooltipProvider><TokenStatsPage /></TooltipProvider>);

    const ranking = await screen.findByRole("region", { name: "消耗最高的会话" });
    await user.click(within(ranking).getByRole("button", { name: "展开全部" }));
    expect(within(ranking).getByLabelText("Explore · 查旧版检查项实现")).toBeTruthy();
    expect(within(ranking).getByLabelText("design-strategist · 产出游戏概念文档")).toBeTruthy();
    await user.click(within(ranking).getByRole("button", { name: "收起全部" }));
    expect(within(ranking).queryByLabelText("Explore · 查旧版检查项实现")).toBeNull();
  });

  it("无子代理数据的来源不渲染徽标与拆分行", async () => {
    window.localStorage.setItem(SOURCE_STORAGE_KEY, "codebuddy-cli");
    getTokenStatistics.mockResolvedValue(sourceWithoutSubagents());
    render(<TooltipProvider><TokenStatsPage /></TooltipProvider>);

    const ranking = await screen.findByRole("region", { name: "消耗最高的会话" });
    expect(screen.queryByText("含子代理")).toBeNull();
    const overview = screen.getByRole("region", { name: "Token 总览" });
    expect(overview.textContent).not.toContain("主会话");
    expect(ranking.textContent).toContain("共 1 个主会话 · 0 个子代理");
    expect(ranking.textContent).toContain("只有主会话的来源");
  });
});
