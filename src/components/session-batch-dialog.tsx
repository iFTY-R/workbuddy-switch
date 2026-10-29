import { AlertTriangle, CheckCircle2, CircleSlash, Loader2, XCircle } from "lucide-react";
import { useEffect, useState } from "react";

import { Button } from "@/components/ui/button";
import { Checkbox } from "@/components/ui/checkbox";
import { Dialog, DialogContent, DialogDescription, DialogFooter, DialogHeader, DialogTitle } from "@/components/ui/dialog";
import { Switch } from "@/components/ui/switch";
import * as api from "@/lib/api";
import { MAX_BATCH_COPIES, MAX_BATCH_GROUPS, MAX_COPY_TARGETS, retryTargets, type BatchPlanPreview } from "@/lib/session-batch";
import { displayName } from "@/lib/account-display";
import type { AccountMeta, SessionBatchItem, SessionBatchJob, SessionBatchOp, SessionBatchTarget, SessionGroupSummary } from "@/lib/types";

const STATUS_LABEL: Record<SessionBatchItem["status"], string> = {
  pending: "等待中",
  done: "已完成",
  skipped: "已跳过",
  failed: "失败",
  notRun: "未执行",
};

const STATUS_CLASS: Record<SessionBatchItem["status"], string> = {
  pending: "text-muted-foreground",
  done: "text-brand",
  skipped: "text-muted-foreground",
  failed: "text-destructive",
  notRun: "text-amber-600 dark:text-amber-500",
};

function StatusIcon({ status }: { status: SessionBatchItem["status"] }) {
  if (status === "done") return <CheckCircle2 className="size-3.5" />;
  if (status === "failed") return <XCircle className="size-3.5" />;
  if (status === "notRun") return <AlertTriangle className="size-3.5" />;
  if (status === "skipped") return <CircleSlash className="size-3.5" />;
  return <Loader2 className="size-3.5 animate-spin" />;
}

/**
 * 批量操作的唯一入口弹窗，三种形态共用一个 Dialog：
 *
 * - 计划：说明这批会做什么、会跳过什么，删除还要逐条列名；
 * - 执行：进度 + 取消（只在组与组之间生效）；
 * - 结果：逐项状态、可重试项。
 *
 * 把三态放在一个弹窗里，是为了让「计划里承诺的」与「结果里发生的」在同一位置对照，
 * 而不是弹窗关掉后只留一条 toast。
 */
export function SessionBatchDialog({
  open,
  onOpenChange,
  op,
  groups,
  selected,
  preview,
  job,
  error,
  starting,
  allowRestart,
  onAllowRestartChange,
  onStart,
  onCancelRun,
  onRetryFailed,
  onExport,
  onDone,
}: {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  op: SessionBatchOp;
  groups: SessionGroupSummary[];
  selected: Set<string>;
  preview: BatchPlanPreview;
  job: SessionBatchJob | null;
  error: string | null;
  starting: boolean;
  allowRestart: boolean;
  onAllowRestartChange: (value: boolean) => void;
  /** 开始执行；补副本要把选中的目标账号一起带上（后端拒绝空选择）。 */
  onStart: (targetAccountIds?: string[]) => void;
  onCancelRun: () => void;
  onRetryFailed: (targets: SessionBatchTarget[]) => void;
  /** 导出本次报告（CSV）；不传则不显示导出按钮。 */
  onExport?: (job: SessionBatchJob) => void;
  onDone: () => void;
}) {
  // interrupted 是终态（上一个进程留下的），与 finished 一样按结果展示。
  const running = starting || (job !== null && job.state !== "finished" && job.state !== "interrupted");
  const selectedGroups = groups.filter((group) => selected.has(group.key));
  const retry = job ? retryTargets(job) : [];
  const progress = job && job.total > 0 ? Math.round((job.done / job.total) * 100) : 0;
  const overLimit = !job && preview.total > MAX_BATCH_GROUPS;
  const currentTitle = job?.current
    ? job.items.find((item) => item.groupId === job.current)?.title || job.current
    : null;

  // ── 补副本：目标账号 ────────────────────────────────────────────────
  /** 目标账号列表：只在补副本形态下按需拉取，其它形态不多打一次接口。 */
  const [accounts, setAccounts] = useState<AccountMeta[]>([]);
  const [accountsError, setAccountsError] = useState<string | null>(null);
  const [targets, setTargets] = useState<Set<string>>(new Set());
  const copyMode = op === "copy" && !job;

  useEffect(() => {
    if (!open || op !== "copy") return;
    let cancelled = false;
    setAccountsError(null);
    api.getAccounts()
      .then((res) => { if (!cancelled) setAccounts(res.accounts); })
      .catch((caught) => { if (!cancelled) setAccountsError(api.asError(caught)); });
    return () => { cancelled = true; };
  }, [open, op]);

  // 每次打开都从「没选」开始：目标账号决定会写出多少份副本，不能沿用上次的选择。
  useEffect(() => {
    if (!open) setTargets(new Set());
  }, [open]);

  const copies = preview.total * targets.size;
  const copiesOverLimit = copyMode && copies > MAX_BATCH_COPIES;
  const targetsOverLimit = copyMode && targets.size > MAX_COPY_TARGETS;

  function toggleTarget(id: string, on: boolean) {
    setTargets((current) => {
      const next = new Set(current);
      if (on) next.add(id);
      else next.delete(id);
      return next;
    });
  }

  /** 「允许关闭并重开」开关：同步与补副本共用（删除不需要写客户端，不展示）。 */
  const restartSwitch = (
    <label className="flex items-start gap-3 rounded-lg border border-border p-3">
      <Switch checked={allowRestart} onCheckedChange={onAllowRestartChange} aria-label="允许关闭并重开 WorkBuddy" />
      <span className="min-w-0 space-y-1">
        <span className="block text-sm font-medium">允许关闭并重开 WorkBuddy</span>
        <span className="block text-xs leading-5 text-muted-foreground">
          写入目标正是该档当前登录账号、且客户端在运行时，需要先关闭客户端（整批只关闭一次，完成后自动重开）。
          不勾选时这些组会被跳过，其余组照常执行。
        </span>
      </span>
    </label>
  );

  return (
    <Dialog open={open} onOpenChange={(next) => { if (!next && !running) onOpenChange(false); }}>
      <DialogContent showCloseButton={!running} className="flex max-h-[min(88vh,calc(100vh-2rem))] flex-col gap-4 overflow-hidden">
        <DialogHeader>
          <DialogTitle>
            {job ? (running ? "正在处理" : "批量结果") : op === "sync" ? "批量同步会话" : op === "copy" ? "批量补副本" : "批量删除关联"}
          </DialogTitle>
          <DialogDescription>
            {job
              ? running ? "任务在后端逐组串行执行，关闭本窗口不会中断。" : "以下是逐项的最终状态。"
              : op === "sync"
                ? "只做安全快进：不会覆盖目标账号里更新的内容。"
                : op === "copy"
                  ? "把每个选中组的可确认来源复制成新会话补到所选账号；已经是组成员的账号不会重复复制。"
                  : "只解除关联，账号里的会话内容不会被删除。"}
          </DialogDescription>
        </DialogHeader>

        {error && (
          <p role="alert" className="rounded-lg border border-destructive/30 p-3 text-sm text-destructive">{error}</p>
        )}

        {!job && (
          <div className="min-h-0 overflow-y-auto">
            {op === "sync" ? (
              <div className="space-y-3">
                <dl className="grid grid-cols-2 gap-2 text-sm sm:grid-cols-4">
                  <Stat label="已选" value={preview.total} />
                  <Stat label="可快进" value={preview.syncable} tone="brand" />
                  <Stat label="已是最新" value={preview.alreadyLatest} />
                  <Stat label="无安全来源" value={preview.noSafeSource} tone="warn" />
                </dl>
                <p className="text-xs leading-5 text-muted-foreground">
                  以上按列表当前状态估算；执行时后端会逐组重新检查，内容变过的组会被跳过并记入结果。
                </p>
                {overLimit && (
                  <p role="alert" className="rounded-lg border border-amber-500/30 p-3 text-sm text-amber-700 dark:text-amber-400">
                    一次最多处理 {MAX_BATCH_GROUPS} 个会话组（当前 {preview.total} 个）：请先按状态筛选，或分批执行。
                  </p>
                )}
                {restartSwitch}
              </div>
            ) : op === "copy" ? (
              <div className="space-y-3">
                <div className="space-y-1.5">
                  <div className="flex items-center justify-between">
                    <span className="text-sm font-medium">补到哪些账号</span>
                    <span className="text-xs text-muted-foreground">已选 {targets.size} 个</span>
                  </div>
                  {accountsError ? (
                    <p role="alert" className="rounded-lg border border-destructive/30 p-3 text-sm text-destructive">{accountsError}</p>
                  ) : accounts.length === 0 ? (
                    <p className="text-xs leading-5 text-muted-foreground">正在加载账号…</p>
                  ) : (
                    <ul className="max-h-48 space-y-0.5 overflow-y-auto rounded-lg border border-border p-1.5">
                      {accounts.map((account) => (
                        <li key={account.id}>
                          <label className="flex cursor-pointer items-center gap-2.5 rounded-md px-1.5 py-1 text-sm hover:bg-muted/60">
                            <Checkbox
                              checked={targets.has(account.id)}
                              onCheckedChange={(on) => toggleTarget(account.id, Boolean(on))}
                              aria-label={`补到 ${displayName(account)}`}
                            />
                            <span className="min-w-0 truncate">{displayName(account)}</span>
                            {account.email && (
                              <span className="ml-auto shrink-0 truncate text-xs text-muted-foreground">{account.email}</span>
                            )}
                          </label>
                        </li>
                      ))}
                    </ul>
                  )}
                </div>
                <p className="text-xs leading-5 text-muted-foreground">
                  最多会新建 {copies} 份副本（{preview.total} 个组 × {targets.size} 个账号）。实际数量按每个组能补的账号算，可能更少；
                  与该组不兼容、或已经是组成员的账号会被跳过并记入结果。
                </p>
                {overLimit && (
                  <p role="alert" className="rounded-lg border border-amber-500/30 p-3 text-sm text-amber-700 dark:text-amber-400">
                    一次最多处理 {MAX_BATCH_GROUPS} 个会话组（当前 {preview.total} 个）：请先筛选或分批执行。
                  </p>
                )}
                {copiesOverLimit && (
                  <p role="alert" className="rounded-lg border border-amber-500/30 p-3 text-sm text-amber-700 dark:text-amber-400">
                    本次将新建 {copies} 份副本，超过单批上限 {MAX_BATCH_COPIES} 份：请减少会话组或目标账号。
                  </p>
                )}
                {targetsOverLimit && (
                  <p role="alert" className="rounded-lg border border-amber-500/30 p-3 text-sm text-amber-700 dark:text-amber-400">
                    一次最多补到 {MAX_COPY_TARGETS} 个账号（当前 {targets.size} 个）。
                  </p>
                )}
                {restartSwitch}
              </div>
            ) : (
              <div className="space-y-3">
                <p className="flex items-start gap-2 rounded-lg border border-destructive/30 bg-destructive/5 p-3 text-sm text-destructive">
                  <AlertTriangle className="mt-0.5 size-4 shrink-0" />
                  <span>将解除 {selectedGroups.length} 个会话组的关联，操作不可撤销；账号里的会话内容不受影响。</span>
                </p>
                <ul className="max-h-56 space-y-1 overflow-y-auto text-sm">
                  {selectedGroups.slice(0, 30).map((group) => (
                    <li key={group.key} className="truncate text-muted-foreground" title={group.title}>· {group.title}</li>
                  ))}
                  {selectedGroups.length > 30 && (
                    <li className="text-xs text-muted-foreground">…另有 {selectedGroups.length - 30} 个</li>
                  )}
                </ul>
              </div>
            )}
          </div>
        )}

        {job && running && (
          <div className="space-y-3">
            <div className="h-2 overflow-hidden rounded-full bg-muted" role="progressbar" aria-valuenow={progress} aria-valuemin={0} aria-valuemax={100}>
              <div className="h-full rounded-full bg-brand transition-[width]" style={{ width: `${progress}%` }} />
            </div>
            <p className="flex items-center gap-2 text-sm text-muted-foreground">
              <Loader2 className="size-4 animate-spin" />
              {job.state === "planning"
                ? "正在检查选中的会话组…"
                : `${job.done}/${job.total}${currentTitle ? ` · 正在处理「${currentTitle}」` : ""}`}
            </p>
          </div>
        )}

        {job && !running && (
          <div className="flex min-h-0 flex-col gap-3">
            {job.aborted && (
              <p role="alert" className="rounded-lg border border-destructive/30 bg-destructive/5 p-3 text-sm text-destructive">
                {job.aborted}
              </p>
            )}
            {job.state === "interrupted" && (
              <p role="status" className="rounded-lg border border-amber-500/30 bg-amber-500/5 p-3 text-sm text-amber-700 dark:text-amber-400">
                这次批量没有跑完（进程退出或被关闭），下面的状态来自最后一次落盘的记录，可能略旧于实际。
              </p>
            )}
            {job.cancelled && (
              <p className="rounded-lg border border-amber-500/30 p-3 text-sm text-amber-700 dark:text-amber-400">
                已按请求停止：正在写入的那一组会照常完成，其余标记为未执行。
              </p>
            )}
            {job.lifecycle && job.lifecycle.closed.length > 0 && (
              <p className="text-xs leading-5 text-muted-foreground">
                本次关闭过 {job.lifecycle.closed.join("、")}：
                {job.lifecycle.keptClosed
                  ? "为保留恢复现场，未自动重开"
                  : job.lifecycle.reopenErrors.length > 0
                    ? `重开失败（${job.lifecycle.reopenErrors.join("；")}）`
                    : "已自动重开"}
              </p>
            )}
            <ul className="min-h-0 flex-1 space-y-1 overflow-y-auto rounded-lg border border-border p-2">
              {job.items.map((item) => (
                <li key={item.key} className="rounded-md px-2 py-1.5 text-sm hover:bg-accent/40">
                  <div className="flex min-w-0 items-center justify-between gap-2">
                    <span className="truncate" title={item.title || item.groupId}>{item.title || item.groupId}</span>
                    <span className={`flex shrink-0 items-center gap-1 text-xs ${STATUS_CLASS[item.status]}`}>
                      <StatusIcon status={item.status} />
                      {STATUS_LABEL[item.status]}
                    </span>
                  </div>
                  {(item.message || item.status === "done") && (
                    <p className="mt-0.5 truncate text-xs text-muted-foreground" title={item.message}>
                      {item.message ?? (op === "copy" ? `新建 ${item.affected} 份副本` : `完成 ${item.synced} 项`)}
                    </p>
                  )}
                </li>
              ))}
            </ul>
          </div>
        )}

        <DialogFooter className="shrink-0 sm:justify-between">
          <div className="text-xs text-muted-foreground">
            {job
              ? `完成 ${job.items.filter((item) => item.status === "done").length} · 跳过 ${job.items.filter((item) => item.status === "skipped").length} · 失败 ${job.items.filter((item) => item.status === "failed").length} · 未执行 ${job.items.filter((item) => item.status === "notRun").length}`
              : `共 ${preview.total} 个会话组`}
          </div>
          <div className="flex shrink-0 gap-2">
            {running ? (
              <Button variant="outline" onClick={onCancelRun} disabled={starting}>停止后续组</Button>
            ) : job ? (
              <>
                {onExport && (
                  <Button variant="ghost" onClick={() => onExport(job)}>导出报告</Button>
                )}
                {retry.length > 0 && (
                  <Button variant="outline" onClick={() => onRetryFailed(retry)}>重试 {retry.length} 项</Button>
                )}
                <Button onClick={onDone}>完成</Button>
              </>
            ) : (
              <>
                <Button variant="outline" onClick={() => onOpenChange(false)}>取消</Button>
                <Button
                  className={op === "delete" ? "bg-destructive text-white hover:bg-destructive/90" : undefined}
                  onClick={() => onStart(copyMode ? [...targets] : undefined)}
                  disabled={
                    starting
                    || preview.total === 0
                    || overLimit
                    // 补副本：目标账号决定写出多少份新会话，没选就绝不能开始。
                    || (copyMode && (targets.size === 0 || copiesOverLimit || targetsOverLimit))
                  }
                >
                  {starting ? <Loader2 className="animate-spin" /> : null}
                  {op === "sync" ? "开始批量同步" : op === "copy" ? "开始补副本" : "确认删除关联"}
                </Button>
              </>
            )}
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

function Stat({ label, value, tone }: { label: string; value: number; tone?: "brand" | "warn" }) {
  return (
    <div className="rounded-lg border border-border p-2">
      <dt className="text-xs text-muted-foreground">{label}</dt>
      <dd className={`text-lg font-semibold tabular-nums ${tone === "brand" ? "text-brand" : tone === "warn" ? "text-amber-600 dark:text-amber-500" : ""}`}>
        {value}
      </dd>
    </div>
  );
}
