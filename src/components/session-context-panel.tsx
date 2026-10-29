import { useState } from "react";
import { Check, Info, Loader2, RefreshCw, Wrench, X } from "lucide-react";

import { Badge } from "@/components/ui/badge";
import { Button } from "@/components/ui/button";
import { Collapsible, CollapsibleContent, CollapsibleTrigger } from "@/components/ui/collapsible";
import * as api from "@/lib/api";
import type {
  SessionContextBackfillReport,
  SessionContextGcReport,
  SessionContextIntegrity,
  SessionGroupClient,
  SessionGroupMemberDetail,
  WbVariant,
} from "@/lib/types";

export interface SessionContextPanelProps {
  client: SessionGroupClient;
  groupId: string;
  variantScope?: WbVariant;
  /** 组内成员：把回执里的 memberId 映射成账号名，界面上不出现裸 id。 */
  members: SessionGroupMemberDetail[];
  disabled?: boolean;
}

/** 旁路数据的界面名：后端返回的是稳定英文键，这里只做展示翻译。 */
const sidecarLabels: Record<string, string> = {
  fileHistory: "文件备份",
  toolResults: "工具输出",
  todos: "计划清单",
  tasks: "任务清单",
  rollback: "回滚日志",
};

function sidecarText(kinds: string[]): string {
  return kinds.map((kind) => sidecarLabels[kind] ?? kind).join("、");
}

/** 留痕时间：解析不出来就说「时间未知」，不显示 `NaN`。 */
function formatWhen(ms: number): string {
  const date = new Date(ms);
  return Number.isNaN(date.getTime()) ? "时间未知" : date.toLocaleString();
}

/**
 * 会话上下文完整性面板（组维度）。
 *
 * 为什么单独做一块：内容一致（`versionStatus = latest`）只说明正文相同，文件备份、
 * 工具输出、计划清单这些小文件可能根本没跟着搬——用户看到的是「会话在、内容不对」。
 *
 * 交互刻意分两步：先「检查」只读报告每个账号缺多少悬空引用，确认后才「补齐」落盘。
 * 补齐以组内「安全来源」为源，追加式写入，重复执行不会产生第二份。
 */
export function SessionContextPanel({ client, groupId, variantScope, members, disabled }: SessionContextPanelProps) {
  const [checking, setChecking] = useState(false);
  const [applying, setApplying] = useState(false);
  const [report, setReport] = useState<SessionContextBackfillReport | null>(null);
  const [gcReport, setGcReport] = useState<SessionContextGcReport | null>(null);
  const [gcBusy, setGcBusy] = useState(false);
  const [error, setError] = useState<string | null>(null);
  /** 单个副本的明细：一次只展开一个，避免整块变成表格。 */
  const [detailId, setDetailId] = useState<string | null>(null);
  const [integrity, setIntegrity] = useState<Record<string, SessionContextIntegrity>>({});
  const [detailBusy, setDetailBusy] = useState<string | null>(null);

  const busy = disabled || checking || applying;
  const nameOf = (memberId: string) =>
    members.find((member) => member.memberId === memberId)?.accountName ?? memberId;

  async function run(apply: boolean) {
    if (apply) setApplying(true);
    else setChecking(true);
    setError(null);
    try {
      const next = await api.sessionContextBackfill({
        client,
        groupIds: [groupId],
        ...(variantScope ? { variantScope } : {}),
        ...(apply ? { apply: true } : {}),
      });
      setReport(next);
    } catch (caught) {
      setError(api.asError(caught));
    } finally {
      setApplying(false);
      setChecking(false);
    }
  }

  async function runGc(apply: boolean) {
    setGcBusy(true);
    setError(null);
    try {
      setGcReport(await api.sessionContextGc({ client, ...(apply ? { apply: true } : {}) }));
    } catch (caught) {
      setError(api.asError(caught));
    } finally {
      setGcBusy(false);
    }
  }

  /** 展开某个副本的明细：按需单条拉取，不在开面板时并发打 N 个请求。 */
  async function toggleDetail(memberId: string, sessionId: string) {
    if (detailId === memberId) {
      setDetailId(null);
      return;
    }
    setDetailId(memberId);
    if (integrity[memberId] || detailBusy) return;
    setDetailBusy(memberId);
    try {
      const value = await api.sessionContextIntegrity({ client, sessionId });
      setIntegrity((current) => ({ ...current, [memberId]: value }));
    } catch (caught) {
      setError(api.asError(caught));
      setDetailId(null);
    } finally {
      setDetailBusy(null);
    }
  }

  const targets = report?.groups.find((group) => group.groupId === groupId)?.targets ?? [];
  const missingTotal = targets.reduce((sum, target) => sum + (target.missing ?? 0), 0);
  const canApply = report !== null && !report.applied && missingTotal > 0;

  return <section aria-label="会话上下文" className="mt-3 rounded-xl border border-border/70 p-3">
    <div className="flex flex-wrap items-center justify-between gap-2">
      <div className="min-w-0">
        <h3 className="text-xs font-semibold">会话上下文</h3>
        <p className="mt-0.5 text-[11px] leading-4 text-muted-foreground">正文之外的文件备份、工具输出与计划清单。内容一致不代表这些也搬过去了。</p>
      </div>
      <div className="flex shrink-0 items-center gap-1">
        <Button variant="outline" size="sm" className="h-7 text-xs" disabled={busy} onClick={() => void run(false)}>
          {checking ? <Loader2 className="size-3.5 animate-spin" /> : <RefreshCw className="size-3.5" />}
          {report ? "重新检查" : "检查上下文"}
        </Button>
        {canApply && <Button size="sm" className="h-7 text-xs" disabled={busy} onClick={() => void run(true)}>
          {applying ? <Loader2 className="size-3.5 animate-spin" /> : null}补齐上下文
        </Button>}
      </div>
    </div>

    {error && <p role="alert" className="mt-2 break-words rounded-lg bg-destructive/10 p-2 text-[11px] leading-4 text-destructive">{error}</p>}

    {report === null && !error && <p className="mt-2 text-[11px] leading-4 text-muted-foreground">检查只读取信息，不会写入任何文件。</p>}

    {report && <div className="mt-2 space-y-1.5">
      {targets.length === 0 && <p className="text-[11px] leading-4 text-muted-foreground">这个会话组没有需要检查的其他账号。</p>}
      {targets.map((target) => {
        const copied = target.copied ?? [];
        const warnings = target.warnings ?? [];
        const open = detailId === target.memberId;
        const detail = integrity[target.memberId];
        return <div key={target.memberId} className="rounded-lg bg-muted/40 px-2 py-1.5">
          <div className="flex flex-wrap items-center justify-between gap-1.5">
            <span className="min-w-0 break-words text-[11px] font-medium [overflow-wrap:anywhere]">{nameOf(target.memberId)}</span>
            <span className="flex shrink-0 items-center gap-1">
              {report.applied
                ? <Badge variant={copied.length > 0 ? "success" : "outline"} className="text-[10px]">{copied.length > 0 ? `已补 ${sidecarText(copied)}` : "无需补齐"}</Badge>
                : target.missing == null
                  ? <Badge variant="outline" className="text-[10px]">无法自检</Badge>
                  : target.missing === 0
                    ? <Badge variant="success" className="text-[10px]">引用完整</Badge>
                    : <Badge variant="warning" className="text-[10px]">{target.missing} 个引用失效</Badge>}
              <Button variant="ghost" size="sm" className="h-5 px-1 text-[10px] text-muted-foreground" aria-label={`${nameOf(target.memberId)} 的上下文明细`} aria-expanded={open} disabled={detailBusy === target.memberId} onClick={() => void toggleDetail(target.memberId, target.sessionId)}>
                {detailBusy === target.memberId ? <Loader2 className="size-3 animate-spin" /> : null}{open ? "收起" : "明细"}
              </Button>
            </span>
          </div>
          {warnings.length > 0 && <ul className="mt-1 space-y-0.5">{warnings.map((warning, index) => <li key={index} className="break-words text-[10px] leading-4 text-amber-700 dark:text-amber-300">{warning}</li>)}</ul>}
          {open && detail && <div className="mt-1.5 rounded-md bg-background/70 p-1.5">
            <p className="text-[10px] text-muted-foreground">文件备份引用：{detail.present}/{detail.referenced}{detail.missing > 0 ? `，失效 ${detail.missing}` : ""}{detail.pruned > 0 ? `，${detail.pruned} 个已被新版本取代` : ""}</p>
            {/* 目录整份不在、正文却有引用：多数是客户端按保留策略清理过，不是复制丢了。
                不解释这一点，用户会以为功能坏了。 */}
            {!detail.fileHistoryDir && detail.referenced > 0 && <p className="mt-0.5 break-words text-[10px] leading-4 text-muted-foreground">客户端已不再保留这个会话的备份目录（通常是清理旧版本），正文里的引用因此指向不存在的文件。</p>}
            <ul className="mt-1 grid grid-cols-2 gap-x-2 gap-y-0.5">
              {[
                { label: "文件备份", ok: detail.fileHistoryDir },
                { label: "工具输出", ok: detail.toolResults },
                { label: "计划清单", ok: detail.todos },
                { label: "任务清单", ok: detail.tasks },
                { label: "回滚日志", ok: detail.rollback },
                { label: "上下文用量", ok: detail.usage },
              ].map((flag) => <li key={flag.label} className={`flex items-center gap-1 text-[10px] leading-4 ${flag.ok ? "text-muted-foreground" : "text-amber-700 dark:text-amber-300"}`}>
                {flag.ok ? <Check className="size-2.5 shrink-0 text-brand" aria-hidden="true" /> : <X className="size-2.5 shrink-0" aria-hidden="true" />}
                {flag.label}{flag.ok ? "" : "缺失"}
              </li>)}
            </ul>
            {detail.missing > 0 && detail.missingSamples.length > 0 && <p className="mt-1 break-all text-[10px] leading-4 text-muted-foreground">缺样：{detail.missingSamples.slice(0, 3).join("、")}</p>}
          </div>}
        </div>;
      })}
      {report.applied && <p className="flex items-center gap-1 text-[11px] leading-4 text-muted-foreground"><Check className="size-3 shrink-0 text-brand" />本次共补 {report.copiedSidecars} 个条目；重复执行不会写第二份。</p>}
      {!report.applied && missingTotal > 0 && <p className="text-[11px] leading-4 text-muted-foreground">补齐以组内可验证的安全来源为源，只补目标缺的文件，不覆盖已有内容。</p>}
      {report.warnings.length > 0 && <ul className="space-y-0.5">{report.warnings.map((warning, index) => <li key={index} className="break-words text-[10px] leading-4 text-amber-700 dark:text-amber-300">{warning}</li>)}</ul>}
      {/* 「补了什么」之外必须说清「没补什么」：不搬的那几类体积是正文的几十倍，
          用户翻不到变更记录时会以为补齐没生效。清单由后端 NOT_CARRIED 单一来源驱动。 */}
      {report.notCarried && report.notCarried.count > 0 && <Collapsible>
        <CollapsibleTrigger asChild>
          <Button variant="ghost" size="sm" className="h-5 gap-1 px-0 text-[10px] text-muted-foreground hover:bg-transparent hover:text-foreground">
            <Info className="size-2.5" />不会搬运的内容（{report.notCarried.count} 类）
          </Button>
        </CollapsibleTrigger>
        <CollapsibleContent className="pt-1">
          <ul className="space-y-0.5">
            {report.notCarried.items.map((item) => <li key={item.name} className="break-words text-[10px] leading-4 text-muted-foreground">
              <span className="font-medium text-foreground/80">{item.name}</span>：{item.reason}
            </li>)}
          </ul>
        </CollapsibleContent>
      </Collapsible>}
    </div>}

    <Collapsible className="mt-2">
      <CollapsibleTrigger asChild>
        <Button variant="ghost" size="sm" className="h-6 gap-1 px-0 text-[11px] text-muted-foreground hover:bg-transparent hover:text-foreground">
          <Wrench className="size-3" />维护：清理无主文件历史
        </Button>
      </CollapsibleTrigger>
      <CollapsibleContent className="space-y-1.5 pt-1.5">
        <p className="flex items-start gap-1 text-[10px] leading-4 text-muted-foreground"><Info className="mt-0.5 size-3 shrink-0" />影响范围是当前客户端的全部会话，不只这个组；只清「没有任何会话或关联引用、且超过 7 天」的目录。</p>
        <div className="flex flex-wrap items-center gap-1.5">
          <Button variant="outline" size="sm" className="h-7 text-xs" disabled={gcBusy} onClick={() => void runGc(false)}>{gcBusy ? <Loader2 className="size-3.5 animate-spin" /> : null}扫描候选</Button>
          {gcReport && !gcReport.applied && gcReport.orphans.length > 0 && <Button variant="destructive" size="sm" className="h-7 text-xs" disabled={gcBusy} onClick={() => void runGc(true)}>删除 {gcReport.orphans.length} 个</Button>}
        </div>
        {gcReport && <p className="text-[10px] leading-4 text-muted-foreground">
          {gcReport.applied ? `已删除 ${gcReport.removed} 个目录。` : gcReport.orphans.length === 0 ? "没有可回收的目录。" : `扫描 ${gcReport.scanned} 个目录，发现 ${gcReport.orphans.length} 个可回收：${gcReport.orphans.slice(0, 3).map((orphan) => `${orphan.sessionId.slice(0, 8)}…（${orphan.ageDays} 天）`).join("、")}${gcReport.orphans.length > 3 ? " 等" : ""}`}
        </p>}
        {/* 上次清理：删除不可逆，干跑时把「上次真删了什么」摆出来，用户才知道现在看到的是哪一次的结果。 */}
        {gcReport && !gcReport.applied && gcReport.lastRun && <p className="text-[10px] leading-4 text-muted-foreground">
          上次清理：{formatWhen(gcReport.lastRun.at)}，删除 {gcReport.lastRun.removed} 个目录。
        </p>}
      </CollapsibleContent>
    </Collapsible>
  </section>;
}
