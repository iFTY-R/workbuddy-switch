import type { TokenStatsSession } from "@/lib/types";

/**
 * 会话树节点：父行聚合、子行明细，一个视图同时回答「这次任务总共花了多少」
 * 与「其中哪个子代理最贵」。
 */
export interface TokenSessionNode {
  /** 原始行；父行的 `session.subagentTotal` 是后端给的权威子行合计。 */
  session: TokenStatsSession;
  /** 该行自身的 token（input + output + cacheWrite），不含子行。 */
  own: number;
  /** 子行节点，按 `aggregate` 降序。 */
  children: TokenSessionNode[];
  /** `own + Σ children.aggregate`。 */
  aggregate: number;
}

/**
 * 会话 token 口径：`input` 已包含 cache reads，因此**不能**再加一次 `cacheRead`。
 * 与后端 `usage_total()`、请求明细行、总览卡完全一致。
 */
export function sessionTokenTotal(session: TokenStatsSession): number {
  return session.input + session.output + session.cacheWrite;
}

/**
 * 把后端返回的扁平 `sessions` 组成为「父行聚合 + 子行挂载」的森林。
 *
 * 组树规则：
 * 1. 只有 `kind === "main"` 的行可以做父行；子代理不会成为别人的父行，
 *    因此结构上最多两层，天然不会成环。
 * 2. 子行按 `parentKey` 挂载 —— **绝不能用 `parentSessionId`**：那是父**文件
 *    stem**，而父行 `key` 可能带同名冲突后缀（` · 2`），按 stem 匹配会漏配。
 * 3. 孤儿（`parentKey === null`、或指向不存在的父行、或 `parentKey === key`
 *    的自环）一律**提升为根节点**，不丢 token、不报错。
 * 4. 每个子行最多挂载一次（`Set` 记录已挂载的 key）：同一 key 重复出现时
 *    后续行按孤儿处理并保留，宁可多一行也不丢数据。
 *
 * 排序：根节点与每一层 `children` 都按 `aggregate` 降序，同值时按 `key` 升序
 * 保证稳定 —— **不依赖后端顺序**（后端仍按 `total` 降序，子代理可能排在父行
 * 前面）。
 *
 * **本函数不做任何截断。** `RANKING_LIMIT` 必须由调用方拿到完整树之后再对
 * 根数组 `slice`：先截断再组树会把某个父会话的子行切掉一半，出现「父行合计
 * 91.5M 但只列出 1 个子代理」的错误。
 *
 * 复杂度：O(n log n) 时间（排序主导）、O(n) 空间。
 */
export function buildSessionTree(sessions: TokenStatsSession[]): TokenSessionNode[] {
  const nodes: TokenSessionNode[] = sessions.map((session) => ({
    session,
    own: sessionTokenTotal(session),
    children: [],
    aggregate: 0,
  }));

  // 父行索引：同名冲突时取第一条，保证 parentKey 唯一指向。
  const parents = new Map<string, TokenSessionNode>();
  for (const node of nodes) {
    if (node.session.kind !== "main") continue;
    if (!parents.has(node.session.key)) parents.set(node.session.key, node);
  }

  const roots: TokenSessionNode[] = [];
  const mounted = new Set<string>();
  for (const node of nodes) {
    const { kind, key, parentKey } = node.session;
    // 主会话恒为根；子代理只有在 parentKey 指向一个已存在的父行时才挂载。
    const parent = kind === "main" ? null : parentKey === null || parentKey === key ? null : parents.get(parentKey);
    if (parent === undefined || parent === null || mounted.has(key)) {
      roots.push(node);
      continue;
    }
    parent.children.push(node);
    mounted.add(key);
  }

  // 自底向上累加。结构上最多两层，递归深度恒定；父行只来自主会话，故无环。
  const finalize = (node: TokenSessionNode): number => {
    let total = node.own;
    for (const child of node.children) total += finalize(child);
    node.aggregate = total;
    return total;
  };
  for (const root of roots) finalize(root);

  const byAggregateDesc = (left: TokenSessionNode, right: TokenSessionNode): number =>
    right.aggregate - left.aggregate || left.session.key.localeCompare(right.session.key);
  roots.sort(byAggregateDesc);
  for (const node of nodes) node.children.sort(byAggregateDesc);

  return roots;
}
