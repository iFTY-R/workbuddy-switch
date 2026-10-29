// @vitest-environment jsdom
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { SessionBatchDialog } from "@/components/session-batch-dialog";
import * as api from "@/lib/api";
import type { BatchPlanPreview } from "@/lib/session-batch";
import type { AccountMeta, SessionBatchJob, SessionGroupSummary } from "@/lib/types";

vi.mock("@/lib/api", () => ({
  getAccounts: vi.fn(),
  asError: (error: unknown) => (error instanceof Error ? error.message : String(error)),
}));

const getAccounts = vi.mocked(api.getAccounts);

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

beforeEach(() => {
  getAccounts.mockResolvedValue({
    accounts: [
      { id: "acc-1", uid: "u1", email: "a@x.com", nickname: "账号一" } as AccountMeta,
      { id: "acc-2", uid: "u2", email: "b@x.com", nickname: "账号二" } as AccountMeta,
    ],
  });
});

function group(key: string, title: string): SessionGroupSummary {
  return {
    key,
    client: "workbuddy",
    variantScope: null,
    groupId: `g-${key}`,
    groupVariant: "cn",
    title,
    projectLabel: "",
    latestActivityAt: 1,
    memberCount: 2,
    activeMemberCount: 2,
    accountNames: ["A", "B"],
    summaryStatus: "behind",
    summaryText: "",
    safeSourceMemberId: "m1",
    hasSafeSource: true,
  };
}

const preview: BatchPlanPreview = { total: 3, syncable: 2, alreadyLatest: 1, noSafeSource: 0, unknown: 0 };

function renderDialog(props: Partial<Parameters<typeof SessionBatchDialog>[0]> = {}) {
  const callbacks = {
    onOpenChange: vi.fn(),
    onStart: vi.fn(),
    onCancelRun: vi.fn(),
    onRetryFailed: vi.fn(),
    onDone: vi.fn(),
    onAllowRestartChange: vi.fn(),
  };
  render(
    <SessionBatchDialog
      open
      op="sync"
      groups={[group("k1", "会话一"), group("k2", "会话二")]}
      selected={new Set(["k1", "k2"])}
      preview={preview}
      job={null}
      error={null}
      starting={false}
      allowRestart={false}
      {...callbacks}
      {...props}
    />,
  );
  return callbacks;
}

function runningJob(): SessionBatchJob {
  return {
    jobId: "job-1",
    client: "workbuddy",
    op: "sync",
    state: "running",
    total: 3,
    done: 1,
    current: "g-k2",
    aborted: null,
    cancelled: false,
    startedAt: 1,
    finishedAt: null,
    items: [
      { key: "k1", groupId: "g-k1", title: "会话一", status: "done", synced: 2, planned: 2 },
      { key: "k2", groupId: "g-k2", title: "会话二", status: "pending", synced: 0, planned: 1 },
      { key: "k3", groupId: "g-k3", title: "会话三", status: "pending", synced: 0, planned: 1 },
    ],
  };
}

describe("SessionBatchDialog", () => {
  it("计划态给出估算，并说明以执行结果为准", () => {
    renderDialog();
    expect(screen.getByText("可快进")).toBeTruthy();
    expect(screen.getByText("2")).toBeTruthy();
    expect(screen.getByText(/以列表当前状态估算|执行时后端会逐组重新检查/)).toBeTruthy();
  });

  it("删除态逐条列名并给出不可撤销提示", () => {
    renderDialog({ op: "delete" });
    expect(screen.getByText(/操作不可撤销/)).toBeTruthy();
    expect(screen.getByText("· 会话一")).toBeTruthy();
    expect(screen.getByText("· 会话二")).toBeTruthy();
  });

  it("执行态显示进度，停止按钮只请求停止后续组", async () => {
    const callbacks = renderDialog({ job: runningJob() });
    expect(screen.getByText(/1\/3/)).toBeTruthy();
    expect(screen.getByText(/正在处理「会话二」/)).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "停止后续组" }));
    expect(callbacks.onCancelRun).toHaveBeenCalledTimes(1);
  });

  it("结果态按状态汇总，并只把失败项交给重试", async () => {
    const finished: SessionBatchJob = {
      ...runningJob(),
      state: "finished",
      done: 1,
      finishedAt: 5,
      items: [
        { key: "k1", groupId: "g-k1", title: "会话一", status: "done", synced: 2, planned: 2 },
        { key: "k2", groupId: "g-k2", title: "会话二", status: "failed", message: "写入失败", synced: 0, planned: 1 },
        { key: "k3", groupId: "g-k3", title: "会话三", status: "skipped", message: "没有落后的副本", synced: 0, planned: 0 },
      ],
    };
    const callbacks = renderDialog({ job: finished });
    expect(screen.getByText(/完成 1 · 跳过 1 · 失败 1/)).toBeTruthy();
    await userEvent.click(screen.getByRole("button", { name: "重试 1 项" }));
    expect(callbacks.onRetryFailed).toHaveBeenCalledWith([{ key: "k2", groupId: "g-k2" }], undefined);
  });

  it("中止原因原样展示，不吞成「失败 0 项」", () => {
    const aborted: SessionBatchJob = {
      ...runningJob(),
      state: "finished",
      aborted: "「会话二」写入待恢复，已停止后续组并保留现场",
    };
    renderDialog({ job: aborted });
    expect(screen.getByRole("alert").textContent).toContain("写入待恢复");
  });
});

describe("SessionBatchDialog 上限", () => {
  it("超过 50 组时提示并禁用开始按钮（与后端上限一致）", () => {
    renderDialog({ preview: { total: 51, syncable: 51, alreadyLatest: 0, noSafeSource: 0, unknown: 0 } });
    expect(screen.getByRole("alert").textContent).toContain("一次最多处理 50 个会话组");
    expect(screen.getByRole("button", { name: "开始批量同步" })).toHaveProperty("disabled", true);
  });
});

describe("SessionBatchDialog 中断态", () => {
  it("中断的报告给出解释而不是空结果", () => {
    renderDialog({ job: { ...runningJob(), state: "interrupted", finishedAt: 9 } });
    expect(screen.getByRole("status").textContent).toContain("没有跑完");
  });

  it("记录关闭过的客户端与是否重开", () => {
    renderDialog({
      job: {
        ...runningJob(),
        state: "finished",
        finishedAt: 9,
        lifecycle: { closed: ["WorkBuddy 国内版"], keptClosed: false, reopenErrors: [] },
      },
    });
    expect(screen.getByText(/本次关闭过 WorkBuddy 国内版/)).toBeTruthy();
    expect(screen.getByText(/已自动重开/)).toBeTruthy();
  });

  it("中止后刻意不重开要说清楚", () => {
    renderDialog({
      job: {
        ...runningJob(),
        state: "finished",
        finishedAt: 9,
        lifecycle: { closed: ["WorkBuddy 国际版"], keptClosed: true, reopenErrors: [] },
      },
    });
    expect(screen.getByText(/为保留恢复现场，未自动重开/)).toBeTruthy();
  });
});

describe("SessionBatchDialog 补副本", () => {
  it("没选目标账号时不能开始：不选不等于全选", async () => {
    renderDialog({ op: "copy" });

    const start = await screen.findByRole("button", { name: "开始补副本" });
    expect(start).toHaveProperty("disabled", true);
    // 账号列表要真的拉出来，而不是让用户盲选。
    expect(await screen.findByText("账号一")).toBeTruthy();
    expect(getAccounts).toHaveBeenCalledTimes(1);
  });

  it("勾选账号后把目标 id 交给上层，并预估最多新建多少份", async () => {
    const callbacks = renderDialog({ op: "copy" });

    await userEvent.click(await screen.findByRole("checkbox", { name: "补到 账号一" }));
    // 3 个组 × 1 个账号：预估要给用户一个「会写出多少东西」的量级。
    expect(screen.getByText(/最多会新建 3 份副本/)).toBeTruthy();

    await userEvent.click(screen.getByRole("button", { name: "开始补副本" }));
    expect(callbacks.onStart).toHaveBeenCalledWith(["acc-1"]);
  });

  it("组数 × 账号数超过上限时提示并禁用，不等到后端拒绝", async () => {
    renderDialog({ op: "copy", preview: { total: 60, syncable: 60, alreadyLatest: 0, noSafeSource: 0, unknown: 0 } });

    await userEvent.click(await screen.findByRole("checkbox", { name: "补到 账号一" }));
    await userEvent.click(screen.getByRole("checkbox", { name: "补到 账号二" }));

    expect(await screen.findByText(/超过单批上限 100 份/)).toBeTruthy();
    await waitFor(() =>
      expect(screen.getByRole("button", { name: "开始补副本" })).toHaveProperty("disabled", true),
    );
  });

  it("账号列表拉不到时给出可读错误，而不是空列表", async () => {
    getAccounts.mockRejectedValueOnce(new Error("账号库读取失败"));
    renderDialog({ op: "copy" });
    expect((await screen.findByRole("alert")).textContent).toContain("账号库读取失败");
  });
});

describe("SessionBatchDialog 补副本重试", () => {
  it("重试要把同一批目标账号带上，否则后端会因空选择拒绝", async () => {
    const finished: SessionBatchJob = {
      jobId: "job-copy",
      client: "workbuddy",
      op: "copy",
      state: "finished",
      total: 2,
      done: 1,
      current: null,
      aborted: null,
      cancelled: false,
      startedAt: 1,
      finishedAt: 2,
      targetAccountIds: ["acc-1"],
      items: [
        { key: "k1", groupId: "g-k1", title: "会话一", status: "done", synced: 1, affected: 1, planned: 1 },
        { key: "k2", groupId: "g-k2", title: "会话二", status: "failed", message: "写入失败", synced: 0, affected: 0, planned: 1 },
      ],
    };
    const callbacks = renderDialog({ op: "copy", job: finished });

    // 结果态看不到账号勾选（计划区已收起）：重试必须以报告里落盘的目标账号为准，
    // 否则「弹窗重开后再重试」会因为空选择被后端拒绝。
    await userEvent.click(screen.getByRole("button", { name: "重试 1 项" }));

    expect(callbacks.onRetryFailed).toHaveBeenCalledWith([{ key: "k2", groupId: "g-k2" }], ["acc-1"]);
  });
});
