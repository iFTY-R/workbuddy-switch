// @vitest-environment jsdom
import { afterEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen } from "@testing-library/react";
import userEvent from "@testing-library/user-event";

import { SessionBatchDialog } from "@/components/session-batch-dialog";
import type { BatchPlanPreview } from "@/lib/session-batch";
import type { SessionBatchJob, SessionGroupSummary } from "@/lib/types";

afterEach(() => {
  cleanup();
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
    expect(callbacks.onRetryFailed).toHaveBeenCalledWith([{ key: "k2", groupId: "g-k2" }]);
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
