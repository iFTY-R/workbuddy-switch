import { describe, expect, it } from "vitest";

import { describeUnifyBlockers, planUnify } from "@/lib/session-unify";
import type { SessionGroupPairPreview, SessionGroupUnifyPlan, SessionSyncVerdict } from "@/lib/types";

function preview(overrides: Partial<SessionGroupPairPreview> = {}): SessionGroupPairPreview {
  return {
    client: "workbuddy",
    variantScope: null,
    groupId: "g-1",
    sourceMemberId: "m1",
    targetMemberId: "m2",
    verdict: "diverge",
    availableModes: ["overwrite"],
    previewToken: "token-2",
    reason: "两边都有独立更新",
    recordCount: null,
    extraTargetCount: 0,
    ...overrides,
  };
}

function plan(
  targets: SessionGroupUnifyPlan["targets"],
  overrides: Partial<SessionGroupUnifyPlan> = {},
): SessionGroupUnifyPlan {
  return {
    client: "workbuddy",
    groupId: "g-1",
    sourceMemberId: "m1",
    sourceName: "主账号",
    targets,
    ...overrides,
  };
}

function target(memberId: string, accountName: string, value: SessionGroupPairPreview | null, error: string | null = null) {
  return { memberId, accountName, preview: value, error };
}

describe("planUnify", () => {
  it("内容相同的目标跳过，不产出动作也不算阻塞", () => {
    const result = planUnify(plan([target("m2", "备用账号", preview({ targetMemberId: "m2", verdict: "identical", availableModes: [] }))]));
    expect(result.actions).toEqual([]);
    expect(result.blockers).toEqual([]);
    expect(result.skippedIdentical).toBe(1);
  });

  it("判为分歧的目标用覆盖模式写入", () => {
    const result = planUnify(plan([target("m2", "备用账号", preview())]));
    expect(result.blockers).toEqual([]);
    expect(result.actions).toHaveLength(1);
    expect(result.actions[0]).toMatchObject({ targetMemberId: "m2", mode: "overwrite", previewToken: "token-2" });
  });

  it("没有可写模式时阻塞并说明原因，而不是静默忽略", () => {
    // 后端对 unknown 不给任何模式：这类目标过去会让「确认统一」变成什么都不发生。
    const unknown = preview({ verdict: "unknown" as SessionSyncVerdict, availableModes: [], previewToken: null, reason: "找不到上次同步的记录" });
    const result = planUnify(plan([target("m2", "备用账号", unknown)]));

    expect(result.actions).toEqual([]);
    expect(result.blockers).toHaveLength(1);
    expect(result.blockers[0].accountName).toBe("备用账号");
    expect(result.blockers[0].reason).toContain("找不到上次同步的记录");
  });

  it("预览属于别的组或别的来源时视为过期", () => {
    const stale = preview({ sourceMemberId: "m9" });
    const result = planUnify(plan([target("m2", "备用账号", stale)]));
    expect(result.actions).toEqual([]);
    expect(result.blockers[0].reason).toContain("已过期");
  });

  it("预览阶段就报错的目标直接带上原始错误", () => {
    const result = planUnify(plan([target("m2", "备用账号", null, "账号未登录")]));
    expect(result.blockers[0]).toEqual({ accountName: "备用账号", reason: "账号未登录" });
  });

  it("混合场景：可执行项与阻塞项同时存在时整体不允许执行", () => {
    const result = planUnify(plan([
      target("m2", "备用账号", preview()),
      target("m3", "第三账号", preview({ targetMemberId: "m3", availableModes: [], previewToken: null, verdict: "unknown" as SessionSyncVerdict })),
    ]));
    // 有阻塞时不执行任何一项：半套统一会让组里的内容更乱。
    expect(result.blockers).toHaveLength(1);
    expect(result.actions).toHaveLength(1);
  });
});

describe("describeUnifyBlockers", () => {
  it("把账号与原因一次说清", () => {
    expect(describeUnifyBlockers([{ accountName: "A", reason: "无法确认" }])).toBe("A：无法确认");
  });

  it("超过三项时收敛为「另有 N 个」", () => {
    const text = describeUnifyBlockers([
      { accountName: "A", reason: "x" },
      { accountName: "B", reason: "x" },
      { accountName: "C", reason: "x" },
      { accountName: "D", reason: "x" },
    ]);
    expect(text).toContain("另有 1 个账号同样无法写入");
  });

  it("没有阻塞时返回空串", () => {
    expect(describeUnifyBlockers([])).toBe("");
  });
});
