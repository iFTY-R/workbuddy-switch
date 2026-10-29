import type { SessionBatchJob, SessionBatchTarget, SessionGroupSummary } from "@/lib/types";

/** 与后端 `session_batch::MAX_BATCH_GROUPS` 对齐：超过这个数要求先筛选。 */
export const MAX_BATCH_GROUPS = 50;

/** 与后端 `session_batch::MAX_COPY_TARGETS` 对齐：一个组最多补到这么多个账号。 */
export const MAX_COPY_TARGETS = 20;

/** 与后端 `session_batch::MAX_BATCH_COPIES` 对齐：组数 × 账号数的总量上限。 */
export const MAX_BATCH_COPIES = 100;

/** 计划阶段能在本地算出来的粗判定（不做任何读写，只按列表已有的摘要分组）。 */
export interface BatchPlanPreview {
  total: number;
  /** 有安全来源且存在落后副本：会真正执行。 */
  syncable: number;
  /** 没有落后副本：执行时会被跳过。 */
  alreadyLatest: number;
  /** 没有可确认的安全来源：执行时会被跳过。 */
  noSafeSource: number;
  /** 内容缺失 / 状态无法确认：执行时大概率被跳过。 */
  unknown: number;
}

/**
 * 由列表摘要推出「这批里大概能同步几个」。
 *
 * 只用于计划弹窗的预期说明：真正的判定在执行时由后端逐组重做（内容随时会变），
 * 所以这里允许和最终结果存在差异，界面上必须说清「以执行结果为准」。
 */
export function planPreview(groups: SessionGroupSummary[], keys: Set<string>): BatchPlanPreview {
  const preview: BatchPlanPreview = { total: 0, syncable: 0, alreadyLatest: 0, noSafeSource: 0, unknown: 0 };
  for (const group of groups) {
    if (!keys.has(group.key)) continue;
    preview.total += 1;
    if (!group.hasSafeSource) preview.noSafeSource += 1;
    else if (group.summaryStatus === "latest") preview.alreadyLatest += 1;
    else if (group.summaryStatus === "behind") preview.syncable += 1;
    else preview.unknown += 1;
  }
  return preview;
}

/** 选中键 → 后端要的目标列表（key 用于结果回填，groupId 用于执行）。 */
export function selectedTargets(groups: SessionGroupSummary[], keys: Set<string>): SessionBatchTarget[] {
  return groups.filter((group) => keys.has(group.key)).map((group) => ({ key: group.key, groupId: group.groupId }));
}

/**
 * 只重试失败的与因中止/取消而未执行的项。
 *
 * 已经成功或计划阶段就跳过的项不再重复提交：同步是幂等的，但重复提交会白跑一遍
 * 复核与备份，还会把「已是最新」的组再探一次。
 */
export function retryTargets(job: SessionBatchJob): SessionBatchTarget[] {
  return job.items
    .filter((item) => item.status === "failed" || item.status === "notRun")
    .map((item) => ({ key: item.key, groupId: item.groupId }));
}

export interface BatchJobCounts {
  total: number;
  done: number;
  skipped: number;
  failed: number;
  notRun: number;
  synced: number;
  /**
   * 「完成但没做全」的组里未完成的子目标数之和（补副本按账号计）。
   *
   * 单列出来是因为这些组 `status` 仍是 `done`：只报「完成 N 个」会让用户以为没有遗留，
   * 而实际上还有账号没补上。
   */
  partialTargets: number;
}

export function jobCounts(job: SessionBatchJob | null): BatchJobCounts {
  const counts: BatchJobCounts = { total: 0, done: 0, skipped: 0, failed: 0, notRun: 0, synced: 0, partialTargets: 0 };
  if (!job) return counts;
  counts.total = job.items.length;
  for (const item of job.items) {
    if (item.status === "done") {
      counts.done += 1;
      // 旧报告只有 `synced`，新报告两者同值：优先读新字段。
      counts.synced += item.affected ?? item.synced;
      counts.partialTargets += item.failedTargets ?? 0;
    } else if (item.status === "skipped") counts.skipped += 1;
    else if (item.status === "failed") counts.failed += 1;
    else if (item.status === "notRun") counts.notRun += 1;
  }
  return counts;
}

/**
 * 连选：`anchor` 到 `target` 之间（含两端）按「点谁就取谁的反态」整段设置。
 *
 * 传 `items` 而不是用 Set 自身顺序，是因为列表可能被排序/筛选过；区间必须按
 * 屏幕上看到的顺序取，否则用户按 Shift 时会选到看不见的组。
 */
export function rangeSelection(
  items: string[],
  anchor: string | null,
  target: string,
  selected: Set<string>,
): Set<string> {
  const next = new Set(selected);
  const targetIndex = items.indexOf(target);
  if (targetIndex < 0) return next;
  const anchorIndex = anchor ? items.indexOf(anchor) : -1;
  if (anchorIndex < 0) {
    if (next.has(target)) next.delete(target);
    else next.add(target);
    return next;
  }
  const [start, end] = anchorIndex <= targetIndex ? [anchorIndex, targetIndex] : [targetIndex, anchorIndex];
  const shouldSelect = !next.has(target);
  for (let index = start; index <= end; index += 1) {
    if (shouldSelect) next.add(items[index]);
    else next.delete(items[index]);
  }
  return next;
}

/**
 * CSV 字段转义：含逗号、引号、换行时加引号并把内部引号翻倍。
 *
 * 另外挡住公式注入：Excel 会把 `= + - @` 开头的单元格当公式执行。
 * 用前置制表符而不是 `'`——前者在 Excel 里不显示，后者会把撇号一起显示出来。
 */
function csvCell(value: string | number): string {
  const text = String(value);
  const safe = /^[=+\-@\t\r]/.test(text) ? `\t${text}` : text;
  return /[",\n\r]/.test(safe) ? `"${safe.replace(/"/g, '""')}"` : safe;
}

/**
 * 把一次批量报告导成 CSV。
 *
 * 带 UTF-8 BOM：Excel 打开中文列名会乱码，BOM 是唯一不依赖用户设置的办法。
 * 列里保留“计划/完成”两个数量，便于回看“当时以为要做多少、实际做了多少”。
 */
export function reportToCsv(job: SessionBatchJob): string {
  const header = ["会话组", "状态", "原因", "说明", "完成数量", "计划数量", "未完成数量", "开始时间", "结束时间"];
  const started = new Date(job.startedAt).toISOString();
  const finished = job.finishedAt ? new Date(job.finishedAt).toISOString() : "";
  const rows = job.items.map((item) => [
    item.title || item.groupId,
    STATUS_TEXT[item.status] ?? item.status,
    item.reasonCode ?? "",
    item.message ?? "",
    item.affected ?? item.synced,
    item.planned,
    // 补副本的「部分未完成」要进导出：只看「完成数量」会漏掉没补上的账号。
    item.failedTargets ?? 0,
    started,
    finished,
  ]);
  const lines = [header, ...rows].map((row) => row.map(csvCell).join(","));
  return `\ufeff${lines.join("\r\n")}\r\n`;
}

const STATUS_TEXT: Record<string, string> = {
  pending: "等待中",
  done: "已完成",
  skipped: "已跳过",
  failed: "失败",
  notRun: "未执行",
};
