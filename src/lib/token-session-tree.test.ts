import { describe, expect, it } from "vitest";

import { buildSessionTree, sessionTokenTotal } from "@/lib/token-session-tree";
import type { TokenStatsSession } from "@/lib/types";

/** 构造会话行：`total` / `uncachedInput` 未显式给出时按后端口径推导。 */
function session(overrides: Partial<TokenStatsSession> & Pick<TokenStatsSession, "key">): TokenStatsSession {
  const merged: TokenStatsSession = {
    title: null,
    project: "fixture-project",
    sessionId: overrides.key,
    kind: "main",
    total: 0,
    input: 0,
    output: 0,
    cacheRead: 0,
    cacheWrite: 0,
    uncachedInput: 0,
    records: 1,
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
    ...overrides,
    key: overrides.key,
  };
  return {
    ...merged,
    total: overrides.total ?? merged.input + merged.output + merged.cacheWrite,
    uncachedInput: overrides.uncachedInput ?? Math.max(0, merged.input - merged.cacheRead),
  };
}

/** 子代理行：`parentSessionId` 是父文件 stem，`parentKey` 才是组树依据。 */
function subagent(
  key: string,
  parentKey: string | null,
  own: number,
  overrides: Partial<TokenStatsSession> = {},
): TokenStatsSession {
  const stem = parentKey === null ? null : (parentKey.split(" · ")[1] ?? null);
  return session({
    key,
    kind: "subagent",
    input: own,
    parentSessionId: stem,
    parentKey,
    agentId: key.split(" · ")[1] ?? key,
    agentLabel: "子代理 · agent-0000",
    ...overrides,
  });
}

const sum = (values: number[]): number => values.reduce((total, value) => total + value, 0);

describe("sessionTokenTotal", () => {
  it("input 已含 cacheRead，不能把 cacheRead 再加一次", () => {
    const row = session({ key: "proj · s", input: 100, cacheRead: 80, output: 5, cacheWrite: 2 });
    expect(sessionTokenTotal(row)).toBe(107);
  });
});

describe("buildSessionTree", () => {
  const parentA = session({ key: "proj · a", input: 1_000, output: 100, cacheWrite: 10, subagentTotal: 800 });
  const childA1 = subagent("proj · agent-a1", "proj · a", 500);
  const childA2 = subagent("proj · agent-a2", "proj · a", 300);
  const parentB = session({ key: "proj · b", input: 40, subagentTotal: 700 });
  const childB1 = subagent("proj · agent-b1", "proj · b", 700);
  const orphan = subagent("proj · agent-orphan", null, 90);
  const all = [parentA, childA1, childA2, parentB, childB1, orphan];

  it("总量守恒：根节点 aggregate 之和等于全部行 token 之和", () => {
    const roots = buildSessionTree(all);
    expect(sum(roots.map((node) => node.aggregate))).toBe(sum(all.map(sessionTokenTotal)));
    // 2 个父行 + 1 个孤儿根行。
    expect(roots).toHaveLength(3);
  });

  it("父行自洽：own + Σ children.aggregate === aggregate", () => {
    const roots = buildSessionTree(all);
    for (const root of roots) {
      expect(root.own + sum(root.children.map((child) => child.aggregate))).toBe(root.aggregate);
    }
    const rootA = roots.find((node) => node.session.key === "proj · a");
    expect(rootA?.own).toBe(1_110);
    expect(rootA?.aggregate).toBe(1_910);
  });

  it("子行 aggregate 之和等于后端给的权威值 subagentTotal", () => {
    const roots = buildSessionTree(all);
    for (const root of roots) {
      expect(sum(root.children.map((child) => child.aggregate))).toBe(root.session.subagentTotal);
    }
  });

  it("children 按 aggregate 降序", () => {
    const roots = buildSessionTree(all);
    const rootA = roots.find((node) => node.session.key === "proj · a");
    expect(rootA?.children.map((child) => child.session.key)).toEqual(["proj · agent-a1", "proj · agent-a2"]);
  });

  it("孤儿提升为根节点且 aggregate 正确", () => {
    const roots = buildSessionTree([parentA, childA1, orphan]);
    const promoted = roots.find((node) => node.session.key === "proj · agent-orphan");
    expect(promoted?.session.kind).toBe("subagent");
    expect(promoted?.children).toEqual([]);
    expect(promoted?.aggregate).toBe(90);
  });

  it("parentKey 指向不存在的父行时按孤儿提升", () => {
    const dangling = subagent("proj · agent-dangling", "proj · missing", 70);
    const roots = buildSessionTree([parentA, dangling]);
    // 按 aggregate 降序：父行 1 910 在前，孤儿 70 在后。
    expect(roots.map((node) => node.session.key)).toEqual(["proj · a", "proj · agent-dangling"]);
    expect(roots[1].aggregate).toBe(70);
    expect(roots[1].children).toEqual([]);
  });

  it("按 parentKey 挂载而不是 parentSessionId（父 key 带同名冲突后缀）", () => {
    const first = session({ key: "proj · dup", input: 10 });
    const second = session({ key: "proj · dup · 2", input: 20, subagentTotal: 60 });
    // parentSessionId 是父文件 stem `dup`，与父行 key `proj · dup · 2` 不同。
    const child = subagent("proj · agent-c1", "proj · dup · 2", 60, { parentSessionId: "dup" });
    const roots = buildSessionTree([first, second, child]);
    const target = roots.find((node) => node.session.key === "proj · dup · 2");
    const other = roots.find((node) => node.session.key === "proj · dup");
    expect(target?.children.map((node) => node.session.key)).toEqual(["proj · agent-c1"]);
    expect(other?.children).toEqual([]);
    expect(roots).toHaveLength(2);
  });

  it("自环防御：parentKey === key 的行不被挂到自己下面且不丢 token", () => {
    const self = subagent("proj · agent-self", "proj · agent-self", 55);
    const roots = buildSessionTree([parentA, self]);
    const promoted = roots.find((node) => node.session.key === "proj · agent-self");
    expect(promoted?.children).toEqual([]);
    expect(promoted?.aggregate).toBe(55);
    expect(sum(roots.map((node) => node.aggregate))).toBe(sum([parentA, self].map(sessionTokenTotal)));
  });

  it("同一 key 重复出现时只挂载一次，其余按孤儿保留", () => {
    const parent = session({ key: "proj · p", input: 5, subagentTotal: 30 });
    const first = subagent("proj · agent-dup", "proj · p", 30);
    const second = subagent("proj · agent-dup", "proj · p", 30);
    const roots = buildSessionTree([parent, first, second]);
    const root = roots.find((node) => node.session.key === "proj · p");
    expect(root?.children).toHaveLength(1);
    // 重复行提升为根节点，token 不丢。
    expect(roots.filter((node) => node.session.key === "proj · agent-dup")).toHaveLength(1);
    expect(sum(roots.map((node) => node.aggregate))).toBe(sum([parent, first, second].map(sessionTokenTotal)));
  });

  it("同 aggregate 时按 key 升序，不依赖后端顺序", () => {
    const later = session({ key: "proj · zeta", input: 100 });
    const earlier = session({ key: "proj · alpha", input: 100 });
    const roots = buildSessionTree([later, earlier]);
    expect(roots.map((node) => node.session.key)).toEqual(["proj · alpha", "proj · zeta"]);
  });

  it("先组树再 slice：截断只作用于父行，子行不会被切开", () => {
    const parent = session({ key: "proj · busy", input: 10, subagentTotal: 220 });
    const children = Array.from({ length: 22 }, (_, index) =>
      subagent(`proj · agent-${String(index).padStart(2, "0")}`, "proj · busy", 10),
    );
    const roots = buildSessionTree([parent, ...children]).slice(0, 10);
    expect(roots).toHaveLength(1);
    expect(roots[0].children).toHaveLength(22);
    expect(roots[0].aggregate).toBe(230);
  });

  it("空输入返回空森林", () => {
    expect(buildSessionTree([])).toEqual([]);
  });
});
