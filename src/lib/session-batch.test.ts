import { describe, expect, it } from "vitest";

import {
  jobCounts,
  planPreview,
  rangeSelection,
  reportToCsv,
  retryTargets,
  selectedTargets,
} from "@/lib/session-batch";
import type { SessionBatchJob, SessionGroupSummary } from "@/lib/types";

function group(key: string, overrides: Partial<SessionGroupSummary> = {}): SessionGroupSummary {
  return {
    key,
    client: "workbuddy",
    variantScope: null,
    groupId: key.replace(/^.*:/, ""),
    groupVariant: "cn",
    title: `会话 ${key}`,
    projectLabel: "",
    latestActivityAt: 1,
    memberCount: 2,
    activeMemberCount: 2,
    accountNames: ["A", "B"],
    summaryStatus: "behind",
    summaryText: "",
    safeSourceMemberId: "m1",
    hasSafeSource: true,
    ...overrides,
  };
}

describe("planPreview", () => {
  it("按摘要把选中组分成可快进 / 已最新 / 无安全来源", () => {
    const groups = [
      group("k1"),
      group("k2", { summaryStatus: "latest" }),
      group("k3", { hasSafeSource: false, safeSourceMemberId: null }),
      group("k4", { summaryStatus: "diverge" }),
      group("k5"),
    ];
    const preview = planPreview(groups, new Set(["k1", "k2", "k3", "k4"]));
    expect(preview).toEqual({ total: 4, syncable: 1, alreadyLatest: 1, noSafeSource: 1, unknown: 1 });
  });

  it("只统计选中项，未选中的组不参与", () => {
    const preview = planPreview([group("k1"), group("k2")], new Set(["k2"]));
    expect(preview.total).toBe(1);
  });
});

describe("selectedTargets", () => {
  it("保持列表顺序并带上 key 与 groupId", () => {
    const targets = selectedTargets([group("k1"), group("k2"), group("k3")], new Set(["k3", "k1"]));
    expect(targets.map((item) => item.key)).toEqual(["k1", "k3"]);
    expect(targets[1].groupId).toBe("k3");
  });
});

describe("rangeSelection", () => {
  const items = ["a", "b", "c", "d", "e"];

  it("没有锚点时只切当前项", () => {
    expect([...rangeSelection(items, null, "c", new Set())]).toEqual(["c"]);
    expect([...rangeSelection(items, null, "c", new Set(["c"]))]).toEqual([]);
  });

  it("向后连选包含两端", () => {
    const next = rangeSelection(items, "b", "d", new Set(["b"]));
    expect([...next].sort()).toEqual(["b", "c", "d"]);
  });

  it("反向连选也包含两端", () => {
    const next = rangeSelection(items, "d", "b", new Set(["d"]));
    expect([...next].sort()).toEqual(["b", "c", "d"]);
  });

  it("区间内已全选时，连选整体取消", () => {
    const next = rangeSelection(items, "a", "c", new Set(["a", "b", "c"]));
    expect([...next]).toEqual([]);
  });

  it("锚点不在当前列表里时退化为单项切换", () => {
    const next = rangeSelection(items, "zzz", "c", new Set());
    expect([...next]).toEqual(["c"]);
  });
});

function job(items: SessionBatchJob["items"], overrides: Partial<SessionBatchJob> = {}): SessionBatchJob {
  return {
    jobId: "job-1",
    client: "workbuddy",
    op: "sync",
    state: "finished",
    total: items.length,
    done: items.filter((item) => item.status !== "pending" && item.status !== "notRun").length,
    current: null,
    aborted: null,
    cancelled: false,
    startedAt: 1,
    finishedAt: 2,
    items,
    ...overrides,
  };
}

function item(status: SessionBatchJob["items"][number]["status"], key: string, synced = 0) {
  return { key, groupId: key, title: key, status, synced, affected: synced, planned: 1 };
}

describe("jobCounts", () => {
  it("分别统计四类状态与成功副本数", () => {
    const counts = jobCounts(job([item("done", "a", 2), item("skipped", "b"), item("failed", "c"), item("notRun", "d")]));
    expect(counts).toEqual({ total: 4, done: 1, skipped: 1, failed: 1, notRun: 1, synced: 2 });
  });

  it("空任务返回全零而不是 NaN", () => {
    expect(jobCounts(null)).toEqual({ total: 0, done: 0, skipped: 0, failed: 0, notRun: 0, synced: 0 });
  });
});

describe("retryTargets", () => {
  it("只挑失败与未执行的项", () => {
    const targets = retryTargets(job([item("done", "a"), item("failed", "b"), item("notRun", "c"), item("skipped", "d")]));
    expect(targets.map((target) => target.groupId)).toEqual(["b", "c"]);
  });

  it("跳过项不重复提交：它们本来就是「无需执行」", () => {
    expect(retryTargets(job([item("skipped", "a")]))).toEqual([]);
  });
});

describe("reportToCsv", () => {
  it("带 UTF-8 BOM，表头固定（Excel 打开中文不乱码）", () => {
    const csv = reportToCsv(job([item("done", "会话一", 2)]));
    expect(csv.startsWith("\ufeff")).toBe(true);
    const header = csv.split("\r\n")[0];
    expect(header).toContain("会话组");
    expect(header).toContain("完成数量");
  });

  it("逗号、引号、换行都要转义", () => {
    const csv = reportToCsv(
      job([
        {
          key: "k",
          groupId: "g",
          title: 'a,b"c\nd',
          status: "failed",
          message: "首行\n第二行",
          synced: 0,
          affected: 0,
          planned: 1,
        },
      ]),
    );
    expect(csv).toContain('"a,b""c\nd"');
    expect(csv).toContain('"首行\n第二行"');
  });

  it("旧报告缺 affected 时回退 synced", () => {
    const legacy = { key: "k", groupId: "g", title: "旧报告", status: "done" as const, synced: 3, planned: 1 };
    const csv = reportToCsv(job([legacy as never]));
    expect(csv).toContain("旧报告,已完成,,,3,1");
  });

  it("公式开头的字段前置制表符，避免 Excel 当公式执行", () => {
    const csv = reportToCsv(job([{ ...item("done", "k"), title: "=SUM(A1)" }]));
    expect(csv).toContain("\t=SUM(A1)");
    expect(csv).not.toContain("\n=SUM");
  });

  it("未结束的报告不写结束时间", () => {
    const csv = reportToCsv(job([item("pending", "会话二")], { state: "running", finishedAt: null }));
    expect(csv.split("\r\n")[1].endsWith(",")).toBe(true);
  });
});
