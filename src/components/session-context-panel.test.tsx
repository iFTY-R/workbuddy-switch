// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { SessionContextPanel } from "@/components/session-context-panel";
import * as api from "@/lib/api";
import type { SessionContextBackfillReport, SessionContextGcReport, SessionGroupMemberDetail } from "@/lib/types";

vi.mock("@/lib/api", () => ({
  sessionContextBackfill: vi.fn(),
  sessionContextGc: vi.fn(),
  sessionContextIntegrity: vi.fn(),
  asError: (error: unknown) => (error instanceof Error ? error.message : String(error)),
}));

const backfill = vi.mocked(api.sessionContextBackfill);
const gc = vi.mocked(api.sessionContextGc);
const integrity = vi.mocked(api.sessionContextIntegrity);

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

beforeEach(() => {
  backfill.mockReset();
  gc.mockReset();
  integrity.mockReset();
});

function member(memberId: string, accountName: string, sessionId: string): SessionGroupMemberDetail {
  return {
    memberId,
    accountId: `acc-${memberId}`,
    uid: `uid-${memberId}`,
    sessionId,
    accountName,
    variant: "cn",
    linkState: "active",
    versionStatus: "latest",
    title: "标题",
    projectLabel: "",
    updatedAt: 1,
    recordCount: 2,
    contentState: "ready",
    reason: "",
    canBeSource: true,
  };
}

const members = [member("m1", "主账号", "s-1"), member("m2", "备用账号", "s-2")];

function dryRun(missing: number): SessionContextBackfillReport {
  return {
    applied: false,
    groups: [{ groupId: "g-1", sourceSessionId: "s-1", targets: [{ memberId: "m2", sessionId: "s-2", missing }] }],
    copiedSidecars: 0,
    warnings: [],
    notCarried: {
      count: 2,
      items: [
        { name: "workspace/sessions", reason: "工作区快照，实测单机 6.2G；打开会话时按需重建" },
        { name: "artifact-index", reason: "产物索引；按需重新生成" },
      ],
    },
  };
}

function applied(copied: string[]): SessionContextBackfillReport {
  return {
    applied: true,
    groups: [{ groupId: "g-1", sourceSessionId: "s-1", targets: [{ memberId: "m2", sessionId: "s-2", copied, warnings: [] }] }],
    copiedSidecars: copied.length,
    warnings: [],
  };
}

function renderPanel() {
  return render(<SessionContextPanel client="workbuddy" groupId="g-1" members={members} />);
}

describe("SessionContextPanel", () => {
  it("初始只提示可以检查，不自动发请求", () => {
    renderPanel();
    expect(screen.getByRole("button", { name: /检查上下文/ })).toBeTruthy();
    expect(screen.getByText("检查只读取信息，不会写入任何文件。")).toBeTruthy();
    expect(backfill).not.toHaveBeenCalled();
  });

  it("检查后按账号显示悬空引用数，并出现补齐入口", async () => {
    backfill.mockResolvedValueOnce(dryRun(3));
    renderPanel();

    await userEvent.click(screen.getByRole("button", { name: /检查上下文/ }));

    // 请求必须带组 id 且明确是干跑（不带 apply）。
    expect(backfill).toHaveBeenCalledWith({ client: "workbuddy", groupIds: ["g-1"] });
    expect(await screen.findByText("3 个引用失效")).toBeTruthy();
    expect(screen.getByText("备用账号")).toBeTruthy();
    expect(screen.getByRole("button", { name: /补齐上下文/ })).toBeTruthy();
  });

  it("引用完整时不提供补齐按钮", async () => {
    backfill.mockResolvedValueOnce(dryRun(0));
    renderPanel();

    await userEvent.click(screen.getByRole("button", { name: /检查上下文/ }));

    expect(await screen.findByText("引用完整")).toBeTruthy();
    expect(screen.queryByRole("button", { name: /补齐上下文/ })).toBeNull();
  });

  it("补齐后展示实际搬运的种类与条目数", async () => {
    backfill.mockResolvedValueOnce(dryRun(2)).mockResolvedValueOnce(applied(["toolResults", "tasks"]));
    renderPanel();

    await userEvent.click(screen.getByRole("button", { name: /检查上下文/ }));
    await userEvent.click(await screen.findByRole("button", { name: /补齐上下文/ }));

    expect(backfill).toHaveBeenLastCalledWith({ client: "workbuddy", groupIds: ["g-1"], apply: true });
    expect(await screen.findByText("已补 工具输出、任务清单")).toBeTruthy();
    expect(screen.getByText(/本次共补 2 个条目/)).toBeTruthy();
  });

  it("明细按需拉单条自检，标出缺哪一类旁路数据", async () => {
    backfill.mockResolvedValueOnce(dryRun(2));
    integrity.mockResolvedValueOnce({
      sessionId: "s-2",
      referenced: 3,
      present: 1,
      missing: 2,
      pruned: 0,
      missingSamples: ["h2@v1", "h3@v1"],
      prunedSamples: [],
      fileHistoryDir: true,
      toolResults: false,
      todos: true,
      tasks: false,
      rollback: false,
      usage: true,
    });
    renderPanel();

    await userEvent.click(screen.getByRole("button", { name: /检查上下文/ }));
    await userEvent.click(await screen.findByRole("button", { name: "备用账号 的上下文明细" }));

    expect(integrity).toHaveBeenCalledWith({ client: "workbuddy", sessionId: "s-2" });
    expect(await screen.findByText("文件备份引用：1/3，失效 2")).toBeTruthy();
    expect(screen.getByText("工具输出缺失")).toBeTruthy();
    // 计划清单是存在的，不该被标成缺失。
    expect(screen.queryByText("计划清单缺失")).toBeNull();
    expect(screen.getByText(/缺样：h2@v1、h3@v1/)).toBeTruthy();

    // 再点一次只收起，不重复请求。
    await userEvent.click(screen.getByRole("button", { name: "备用账号 的上下文明细" }));
    expect(screen.queryByText("文件备份引用：1/3，失效 2")).toBeNull();
    expect(integrity).toHaveBeenCalledTimes(1);
  });

  it("失败时给出可读错误，不把界面留在加载态", async () => {
    backfill.mockRejectedValueOnce(new Error("该客户端不支持会话上下文回填"));
    renderPanel();

    await userEvent.click(screen.getByRole("button", { name: /检查上下文/ }));

    expect(await screen.findByRole("alert")).toHaveProperty("textContent", "该客户端不支持会话上下文回填");
    // 出错后按钮必须恢复可用，否则用户只能刷新页面。
    await waitFor(() => expect((screen.getByRole("button", { name: /检查上下文/ }) as HTMLButtonElement).disabled).toBe(false));
  });

  it("清理入口默认干跑，发现候选后才出现删除按钮", async () => {
    const dry: SessionContextGcReport = { scanned: 4, orphans: [{ sessionId: "aaaabbbbcccc", path: "/x", ageDays: 9 }], removed: 0, applied: false };
    gc.mockResolvedValueOnce(dry);
    renderPanel();

    await userEvent.click(screen.getByRole("button", { name: /维护：清理无主文件历史/ }));
    await userEvent.click(screen.getByRole("button", { name: /扫描候选/ }));

    expect(gc).toHaveBeenCalledWith({ client: "workbuddy" });
    expect(await screen.findByText(/发现 1 个可回收/)).toBeTruthy();
    expect(screen.getByRole("button", { name: /删除 1 个/ })).toBeTruthy();
  });

  it("检查后说清「没补什么」，清单来自后端而不是前端自写", async () => {
    backfill.mockResolvedValueOnce(dryRun(1));
    renderPanel();

    await userEvent.click(screen.getByRole("button", { name: /检查上下文/ }));
    // 折叠标题先给出条目数，展开后才逐项列出——「补了什么」旁边必须有「没补什么」。
    await userEvent.click(await screen.findByRole("button", { name: /不会搬运的内容（2 类）/ }));

    expect(await screen.findByText(/打开会话时按需重建/)).toBeTruthy();
    expect(screen.getByText(/按需重新生成/)).toBeTruthy();
  });

  it("干跑时显示上次真实清理的时间，删除后不重复展示", async () => {
    const at = Date.UTC(2026, 9, 20, 3, 0, 0);
    gc.mockResolvedValueOnce({
      scanned: 0,
      orphans: [],
      removed: 0,
      applied: false,
      lastRun: { at, removed: 3, sessionIds: ["a", "b", "c"] },
    });
    renderPanel();

    await userEvent.click(screen.getByRole("button", { name: /维护：清理无主文件历史/ }));
    await userEvent.click(screen.getByRole("button", { name: /扫描候选/ }));

    // 删除不可逆：干跑时也要能看到「上次删了几个」，否则用户不知道现在这堆候选从哪来。
    expect(await screen.findByText(/上次清理：.*删除 3 个目录/)).toBeTruthy();
  });
});
