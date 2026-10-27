import { useEffect, useMemo, useState, type ReactNode } from "react";
import { Loader2 } from "lucide-react";
import { toast } from "sonner";

import { Alert, AlertDescription } from "@/components/ui/alert";
import { Button } from "@/components/ui/button";
import {
  Dialog,
  DialogContent,
  DialogDescription,
  DialogFooter,
  DialogHeader,
  DialogTitle,
} from "@/components/ui/dialog";
import { Switch } from "@/components/ui/switch";
import { Tabs, TabsContent, TabsList, TabsTrigger } from "@/components/ui/tabs";
import { Tooltip, TooltipContent, TooltipTrigger } from "@/components/ui/tooltip";
import { buildGroups, SessionCopyTab } from "@/components/session-copy-tab";
import type { SessionLinksMeta } from "@/components/session-link-shared";
import { VscodeSessionSyncSection } from "@/components/vscode-session-sync-section";
import * as api from "@/lib/api";
import { displayName } from "@/lib/account-display";
import type {
  AccountMeta,
  SessionLinkPreviewGroup,
  SessionLinksPreview,
  SessionSyncSelection,
  VscodeSession,
  VscodeSessionCopyResult,
  VscodeSessionList,
  VscodeSessionRef,
  VscodeSessionSyncReport,
} from "@/lib/types";
import { cn } from "@/lib/utils";

/**
 * 编辑器类客户端（VS Code CodeBuddy 插件 / CodeBuddy IDE）切号弹窗的共用实现。
 *
 * 这两个弹窗原本各写一份，行级重复度 79.8%，连「复制成功但登记失败要单独告警」
 * 这类约定都靠人工同步。这里把**流程与排版**收成一份，客户端差异退化成
 * {@link SessionSwitchClient} 里的一组回调；两个 wrapper 只描述「自己是什么」，
 * 不再各自实现状态机与 toast 规则。
 *
 * 布局约定（与 WorkBuddy 切号弹窗一致）：关联会话在前、复制会话在后；两页都用
 * `forceMount`，切 tab 不卸载另一侧（否则关联勾选会被预览重拉重置）。
 */

/** 与后端 `switch_*` 同形的结果子集：两个客户端只在这几个字段上取数。 */
export interface SwitchOutcome {
  message?: string;
  restarted?: boolean;
  closedByUs?: boolean;
  sessionCopy?: VscodeSessionCopyResult;
  sessionSync?: VscodeSessionSyncReport;
}

/** 切换请求（由弹窗整理好，客户端只负责换成自己的 API 通道）。 */
export interface SwitchPayload {
  accountId: string;
  /** 要复制的会话引用；未勾选复制时为 `undefined`。 */
  refs: VscodeSessionRef[] | undefined;
  /** 要同步的关联会话；未勾选时为 `undefined`。 */
  syncSelections: SessionSyncSelection[] | undefined;
  /** 「自动关闭并重开」开关；不提供开关的客户端恒为 `true`。 */
  autoRestart: boolean;
}

/** 标题行的状态提示（三态由客户端自己判定）。 */
export interface SwitchDialogStatusNotice {
  icon: ReactNode;
  title: string;
  description: string;
  warning: boolean;
}

/** 一个客户端的差异面：文案 + 数据通道，其余流程与排版共用。 */
export interface SessionSwitchClient {
  /** 弹窗副标题。 */
  description: string;
  /**
   * 「复制会话」空态文案。
   *
   * 客户端的安装/登录状态由调用方在构造 client 时闭包捕获（各客户端的 status 类型不同），
   * 这里只补上与本次渲染相关的会话加载状态。
   */
  emptyHint: (ctx: {
    loading: boolean;
    sourceUid: string | null;
    dataRoot: string | null | undefined;
    hasCopyable: boolean;
  }) => string;
  /** 标题行状态提示。 */
  statusNotice: (ctx: { running: boolean; autoRestart: boolean }) => SwitchDialogStatusNotice;
  /** 未登录时 tooltip 追加说明。 */
  notLoggedInHint?: string;
  /** 遮罩主文案。 */
  busyTitle: (ctx: { autoClose: boolean; copyCount: number; syncCount: number }) => string;
  /** 遮罩副文案。 */
  busyHint: (ctx: { autoClose: boolean }) => string;
  /** 未选择任何会话时底部摘要的第二行；缺省为「未选择复制或同步会话」。 */
  summarySubIdle?: (ctx: { running: boolean }) => string;
  /** 自动关闭并重开开关；不传表示该客户端不提供开关（`autoRestart` 恒为 `true`）。 */
  autoRestart?: { storageKey: string; label: string; hint: string };
  /** 列出当前客户端账号可复制的会话。**必须是稳定引用**（模块级函数），它会进 effect 依赖。 */
  loadSessions: () => Promise<VscodeSessionList>;
  /** 执行切换。 */
  switchAccount: (payload: SwitchPayload) => Promise<SwitchOutcome>;
  /** 生效方式提示（各客户端语气不同）。 */
  restartHint: (result: SwitchOutcome) => string;
  /** 关联会话预览通道；缺省走 VS Code 插件通道。 */
  fetchPreview?: (targetAccountId: string) => Promise<SessionLinksPreview>;
  /** 关联会话 tab 的未登录空态文案。 */
  loggedOutHint?: string;
}

/** tab 样式：下划线指示 + 可选计数徽标（WorkBuddy 切号弹窗同款）。 */
const TAB_TRIGGER_CLASS =
  "-mb-px h-9 flex-none rounded-none border-b-2 border-transparent px-0.5 pb-2 text-sm font-medium text-muted-foreground hover:text-foreground data-[state=active]:border-primary data-[state=active]:bg-transparent data-[state=active]:text-foreground data-[state=active]:shadow-none";

/** tab 计数徽标：0 时不显示，避免出现空的「0」。 */
function tabCount(count: number) {
  return count > 0 ? (
    <span className="ml-1 text-xs text-muted-foreground tabular-nums">{count}</span>
  ) : null;
}

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 目标账号 */
  account: AccountMeta | null;
  /** 客户端是否正在运行（决定状态提示与「自动关闭重开」的语义）。 */
  running: boolean;
  /** 客户端是否已登录（`false` 时渲染未登录提示，不请求关联预览）。 */
  loggedIn: boolean;
  client: SessionSwitchClient;
  /** 切换完成后刷新列表 */
  onDone?: () => void;
}

/** 编辑器类客户端切号弹窗（VS Code 插件 / CodeBuddy IDE 共用一套流程）。 */
export function SessionSwitchDialog({
  open,
  onOpenChange,
  account,
  running,
  loggedIn,
  client,
  onDone,
}: Props) {
  const [sessions, setSessions] = useState<VscodeSession[]>([]);
  const [sourceUid, setSourceUid] = useState<string | null>(null);
  /** 数据根目录：`null` = 未找到（与「有目录但无会话」区分）；`undefined` = 后端未返回该字段。 */
  const [dataRoot, setDataRoot] = useState<string | null | undefined>(undefined);
  const [loadingSessions, setLoadingSessions] = useState(false);
  const [copyEnabled, setCopyEnabled] = useState(false);
  const [selected, setSelected] = useState<Set<string>>(new Set());
  /** 折叠的工作区分组（默认全部展开，会话较少）。 */
  const [collapsed, setCollapsed] = useState<Set<string>>(new Set());
  /** 自动关闭并重开：提供开关的客户端默认开启，并持久化到 localStorage。 */
  const [autoRestart, setAutoRestartState] = useState<boolean>(() => {
    const storageKey = client.autoRestart?.storageKey;
    if (!storageKey) return true;
    try {
      return localStorage.getItem(storageKey) !== "0";
    } catch {
      return true;
    }
  });
  const [busy, setBusy] = useState(false);
  const [error, setError] = useState("");
  /** 当前 tab：与 WorkBuddy 切换弹窗同一顺序与默认值（关联会话在前）。 */
  const [tab, setTab] = useState<"links" | "copy">("links");
  /** 关联会话区块上报的状态（tab 徽标 / 能力不可用时隐藏 tab）。 */
  const [linksMeta, setLinksMeta] = useState<SessionLinksMeta | null>(null);
  /** 勾选提交给后端的同步选择（绑定预览凭据，执行前后端会重新校验）。 */
  const [syncSelections, setSyncSelections] = useState<SessionSyncSelection[]>([]);
  /** 已勾选的关联会话（结果反馈里回显会话名）。 */
  const [syncGroups, setSyncGroups] = useState<SessionLinkPreviewGroup[]>([]);

  function setAutoRestart(next: boolean) {
    setAutoRestartState(next);
    const storageKey = client.autoRestart?.storageKey;
    if (!storageKey) return;
    try {
      localStorage.setItem(storageKey, next ? "1" : "0");
    } catch {
      /* 存储不可用时静默 */
    }
  }

  // 打开时加载「当前客户端账号」可复制的会话。`client.loadSessions` 是稳定引用
  // （模块级函数 / 按档位二选一），换数据源即重新加载，与逐弹窗手写 effect 等价。
  useEffect(() => {
    if (!open || !account) return;
    setCopyEnabled(false);
    setSelected(new Set());
    setCollapsed(new Set());
    setDataRoot(undefined);
    setError("");
    setLoadingSessions(true);
    let cancelled = false;
    client
      .loadSessions()
      .then((res) => {
        if (cancelled) return;
        setSessions(res.sessions);
        setSourceUid(res.sourceUid);
        setDataRoot(res.dataRoot);
      })
      .catch(() => {
        if (cancelled) return;
        setSessions([]);
        setSourceUid(null);
        setDataRoot(undefined);
      })
      .finally(() => {
        if (!cancelled) setLoadingSessions(false);
      });
    // 弹窗关闭/账号切换/数据源变化后到达的旧结果必须丢弃，不能写进新一轮状态。
    return () => {
      cancelled = true;
    };
  }, [open, account, client.loadSessions]);

  const groups = useMemo(() => buildGroups(sessions), [sessions]);
  const sessionById = useMemo(() => {
    const map = new Map<string, VscodeSession>();
    for (const session of sessions) map.set(session.id, session);
    return map;
  }, [sessions]);

  function toggleSession(id: string) {
    setSelected((prev) => {
      const next = new Set(prev);
      if (next.has(id)) next.delete(id);
      else next.add(id);
      return next;
    });
  }

  function toggleGroup(ids: string[]) {
    setSelected((prev) => {
      const next = new Set(prev);
      const allOn = ids.length > 0 && ids.every((id) => next.has(id));
      if (allOn) ids.forEach((id) => next.delete(id));
      else ids.forEach((id) => next.add(id));
      return next;
    });
  }

  function toggleCollapsed(key: string) {
    setCollapsed((prev) => {
      const next = new Set(prev);
      if (next.has(key)) next.delete(key);
      else next.add(key);
      return next;
    });
  }

  async function doSwitch() {
    if (!account) return;
    setBusy(true);
    setError("");
    try {
      const refs: VscodeSessionRef[] | undefined = copyEnabled
        ? [...selected]
            .map((id) => {
              const session = sessionById.get(id);
              return session
                ? { workspaceHash: session.workspaceHash, conversationId: session.id }
                : null;
            })
            .filter((ref): ref is VscodeSessionRef => ref !== null)
        : undefined;

      // 勾选绑定预览凭据；执行前后端会重新校验，版本变化则跳过该项。
      const res = await client.switchAccount({
        accountId: account.id,
        refs,
        syncSelections: syncSelections.length > 0 ? syncSelections : undefined,
        autoRestart,
      });
      notifySwitchOutcome({
        nickname: displayName(account),
        res,
        restartHint: client.restartHint(res),
        syncGroups,
        requestedSync: syncSelections.length > 0,
      });
      onOpenChange(false);
      onDone?.();
    } catch (e) {
      setError(api.asError(e));
    } finally {
      setBusy(false);
    }
  }

  const copyCount = copyEnabled ? selected.size : 0;
  const hasCopyable = groups.length > 0;
  const syncCount = syncSelections.length;
  /** 覆盖项数量：底部摘要据此提示风险。 */
  const overwriteCount = syncSelections.filter((item) => item.mode === "overwrite").length;
  /** 关联会话 tab 是否可用：区块不可用（能力判定不通过）时回落到仅复制会话。 */
  const linksAvailable = linksMeta?.available ?? true;
  const summary = switchSummary({
    copyCount,
    syncCount,
    overwriteCount,
    idleText: client.summarySubIdle?.({ running }) ?? "未选择复制或同步会话",
  });
  /** 未登录：按新登录写入，仅影响提示文案。 */
  const notLoggedIn = !loggedIn;
  /** 本次会由后端关闭并重开客户端（仅在客户端正在运行且开关打开时）。 */
  const autoClose = running && autoRestart;
  const statusNotice = client.statusNotice({ running, autoRestart });
  const emptyHint = client.emptyHint({
    loading: loadingSessions,
    sourceUid,
    dataRoot,
    hasCopyable,
  });

  // 关联 tab 不可用（能力判定不通过）时回落到复制会话，避免停在空 tab。
  useEffect(() => {
    if (!linksAvailable && tab === "links") setTab("copy");
  }, [linksAvailable, tab]);

  // 「复制会话」tab 内容：勾选即意图，提交结果由底部摘要兜底确认。
  const copyTabContent = (
    <SessionCopyTab
      loading={loadingSessions}
      hint={emptyHint}
      hasCopyable={hasCopyable}
      enabled={copyEnabled}
      onEnabledChange={setCopyEnabled}
      groups={groups}
      selected={selected}
      onToggleSession={toggleSession}
      onToggleGroup={toggleGroup}
      collapsed={collapsed}
      onToggleCollapsed={toggleCollapsed}
    />
  );

  return (
    <Dialog open={open} onOpenChange={onOpenChange}>
      <DialogContent
        showCloseButton={!busy}
        className="flex max-h-[min(90vh,calc(100vh-2rem))] min-w-0 flex-col overflow-hidden"
        tabIndex={-1}
        onOpenAutoFocus={(event) => {
          // Radix 默认把焦点交给第一个可聚焦元素（状态图标 / 开关），其 Tooltip 会因
          // focus 常驻在弹窗上；改为聚焦弹窗容器本身。
          event.preventDefault();
          const container = event.target as HTMLElement | null;
          window.requestAnimationFrame(() => container?.focus());
        }}
      >
        <DialogHeader className="shrink-0">
          <div className="flex items-center gap-3">
            <DialogTitle className="min-w-0 flex-1">
              切换到「{account ? displayName(account) : "该账号"}」
            </DialogTitle>
            {/* 提示收进图标 tooltip、开关与标题同行；mr-6 为右上角关闭按钮留空位。 */}
            <div className="mr-6 flex shrink-0 items-center gap-2">
              <Tooltip>
                <TooltipTrigger asChild>
                  {/* 仅悬停出提示：不加 tabIndex，避免点击/自动聚焦后 Tooltip 常驻不消失。 */}
                  <span
                    className={cn(
                      "flex size-5 cursor-help items-center justify-center",
                      statusNotice.warning
                        ? "text-amber-600 dark:text-amber-400"
                        : "text-muted-foreground",
                    )}
                    aria-label={statusNotice.title}
                  >
                    {statusNotice.icon}
                  </span>
                </TooltipTrigger>
                <TooltipContent side="bottom" align="end" className="max-w-xs space-y-1">
                  <div className="font-medium">{statusNotice.title}</div>
                  <p className="text-muted-foreground">{statusNotice.description}</p>
                  {notLoggedIn && client.notLoggedInHint && (
                    <p className="text-muted-foreground">{client.notLoggedInHint}</p>
                  )}
                </TooltipContent>
              </Tooltip>
              {client.autoRestart && (
                <Tooltip>
                  <TooltipTrigger asChild>
                    <span className="inline-flex">
                      <Switch
                        checked={autoRestart}
                        onCheckedChange={setAutoRestart}
                        disabled={busy}
                        aria-label={client.autoRestart.label}
                      />
                    </span>
                  </TooltipTrigger>
                  <TooltipContent side="bottom" align="end" className="max-w-xs space-y-1">
                    <div className="font-medium">{client.autoRestart.label}</div>
                    <p className="text-muted-foreground">{client.autoRestart.hint}</p>
                  </TooltipContent>
                </Tooltip>
              )}
            </div>
          </div>
          <DialogDescription>{client.description}</DialogDescription>
        </DialogHeader>

        {busy && (
          <div className="absolute inset-0 z-50 flex flex-col items-center justify-center gap-3 rounded-lg bg-background/85 backdrop-blur-sm">
            <Loader2 className="size-8 animate-spin text-primary" />
            <p className="text-sm font-medium">
              {client.busyTitle({ autoClose, copyCount, syncCount })}
            </p>
            <p className="max-w-xs text-center text-xs text-muted-foreground">
              {client.busyHint({ autoClose })}
            </p>
          </div>
        )}

        <div className="min-h-0 space-y-3 overflow-x-hidden overflow-y-auto">
          {linksAvailable ? (
            <Tabs
              value={tab}
              onValueChange={(value) => setTab(value as "links" | "copy")}
              className="flex min-h-0 flex-col gap-3 overflow-hidden"
            >
              <TabsList className="h-auto w-full shrink-0 justify-start gap-5 rounded-none border-b border-border bg-transparent p-0">
                <TabsTrigger value="links" className={TAB_TRIGGER_CLASS}>
                  关联会话
                  {tabCount(linksMeta?.groupCount ?? 0)}
                </TabsTrigger>
                <TabsTrigger value="copy" className={TAB_TRIGGER_CLASS}>
                  复制会话
                  {tabCount(copyCount)}
                </TabsTrigger>
              </TabsList>
              {/* forceMount：切换 tab 不得卸载另一侧，否则关联勾选会被预览重拉重置。 */}
              <TabsContent
                value="copy"
                forceMount
                className="min-h-0 space-y-3 overflow-y-auto data-[state=inactive]:hidden data-[state=active]:animate-in data-[state=active]:fade-in-0 data-[state=active]:duration-150"
              >
                {copyTabContent}
              </TabsContent>
              <TabsContent
                value="links"
                forceMount
                className="min-h-0 overflow-y-auto data-[state=inactive]:hidden data-[state=active]:animate-in data-[state=active]:fade-in-0 data-[state=active]:duration-150"
              >
                <VscodeSessionSyncSection
                  open={open}
                  account={account}
                  loggedIn={!notLoggedIn}
                  disabled={busy}
                  fetchPreview={client.fetchPreview}
                  loggedOutHint={client.loggedOutHint}
                  onChange={(state) => {
                    setSyncSelections(state.selections);
                    setSyncGroups(state.groups);
                  }}
                  onMetaChange={setLinksMeta}
                />
              </TabsContent>
            </Tabs>
          ) : (
            <div className="min-h-0 space-y-3 overflow-y-auto">{copyTabContent}</div>
          )}

          {error && (
            <Alert variant="destructive" className="min-w-0 break-all">
              <AlertDescription className="min-w-0 break-all">{error}</AlertDescription>
            </Alert>
          )}
        </div>

        <DialogFooter className="shrink-0 sm:justify-between">
          <div className="min-w-0 space-y-0.5">
            <div className="text-sm font-medium">{summary.main}</div>
            {summary.sub && <div className="text-xs text-muted-foreground">{summary.sub}</div>}
          </div>
          <div className="flex shrink-0 gap-2">
            <Button variant="outline" onClick={() => onOpenChange(false)} disabled={busy}>
              取消
            </Button>
            {/* 勾了复制却没选会话时仍然拦住；只要还有同步项可执行就允许确认。 */}
            <Button
              onClick={doSwitch}
              disabled={busy || (copyEnabled && copyCount === 0 && syncCount === 0)}
            >
              {busy ? "切换中…" : "确认切换"}
            </Button>
          </div>
        </DialogFooter>
      </DialogContent>
    </Dialog>
  );
}

/**
 * 底部摘要：第一行说明本次动作，第二行补充风险/缺口。
 *
 * 两个客户端只有「什么都没选」时的文案不同（IDE 国际版要说明会重启/打开 IDE），
 * 由 `idleText` 传入。
 */
function switchSummary(params: {
  copyCount: number;
  syncCount: number;
  overwriteCount: number;
  idleText: string;
}): { main: string; sub: string | null } {
  const { copyCount, syncCount, overwriteCount, idleText } = params;
  const main =
    copyCount > 0 && syncCount > 0
      ? `将复制 ${copyCount} 个、同步 ${syncCount} 个关联会话`
      : copyCount > 0
        ? `将复制 ${copyCount} 个会话`
        : syncCount > 0
          ? `将同步 ${syncCount} 个关联会话`
          : "本次仅切换账号";
  const sub =
    overwriteCount > 0
      ? `其中 ${overwriteCount} 个会替换目标账号的完整内容`
      : copyCount === 0 && syncCount === 0
        ? idleText
        : copyCount === 0
          ? "未选择复制会话"
          : syncCount === 0
            ? "未选择同步会话"
            : null;
  return { main, sub };
}

/**
 * 切换结果提示：成功/部分失败/已复制未关联/同步跳过或失败，四类必须分别可见。
 *
 * 规则集中在这里，避免两个弹窗各自演化出不同的告警口径。
 */
function notifySwitchOutcome(params: {
  nickname: string;
  res: SwitchOutcome;
  restartHint: string;
  syncGroups: SessionLinkPreviewGroup[];
  /** 用户是否勾选了要同步的关联会话：未勾选时不该提示「没有收到同步结果」。 */
  requestedSync: boolean;
}) {
  const { nickname, res, restartHint, syncGroups, requestedSync } = params;
  const copied = res.sessionCopy?.copied.length ?? 0;
  const errors = res.sessionCopy?.errors ?? [];
  const linkErrors = res.sessionCopy?.linkErrors ?? [];
  const syncReport = res.sessionSync;
  const syncedItems = syncReport?.synced ?? [];
  const skippedItems = syncReport?.skipped ?? [];
  const syncErrors = syncReport?.errors ?? [];
  const copiedHint = copied > 0 ? `已复制 ${copied} 个会话` : null;
  const syncedHint = syncedItems.length > 0 ? `已同步 ${syncedItems.length} 个会话` : null;

  if (errors.length > 0) {
    toast.warning(`已切换至「${nickname}」，但部分会话未复制`, {
      description: [
        `已复制 ${copied} 个会话`,
        `${errors.length} 个失败：${errors.map((e) => e.error).join("；")}`,
        restartHint,
      ].join("；"),
    });
  } else {
    toast.success(`已切换至「${nickname}」`, {
      description: [copiedHint, syncedHint, restartHint].filter(Boolean).join("；"),
    });
  }
  // 复制成功但登记失败：复制本身有效，必须提示「未建立关联」而不是当成完整成功。
  if (linkErrors.length > 0) {
    toast.warning("部分会话已复制但未建立关联", {
      description: [
        ...linkErrors.map((item) => `${item.conversationId ?? "会话"}：${item.error}`),
        "未建立关联的会话不会出现在「关联会话」列表里。",
      ].join("；"),
    });
  }
  // 同步结果：成功、跳过、失败都要能看到，不能只显示成功数。
  const groupLabel = (groupId: string) =>
    syncGroups.find((group) => group.groupId === groupId)?.title || groupId;
  const syncIssues = [
    ...syncErrors.map(
      (item) => `${item.groupId ? groupLabel(item.groupId) : "全部会话"}：${item.error}`,
    ),
    // 预览过期/前置条件变化一律跳过并给出原因，绝不显示成已同步。
    ...skippedItems.map((item) => `${groupLabel(item.groupId)}：${item.message}`),
  ];
  if (syncIssues.length > 0) {
    if (syncErrors.length > 0) {
      toast.error("部分关联会话没有同步（失败或已跳过）", { description: syncIssues.join("；") });
    } else {
      toast.warning("有关联会话没有同步（已跳过）", { description: syncIssues.join("；") });
    }
  } else if (requestedSync && !syncReport) {
    toast.warning("关联会话未同步", {
      description: "没有收到同步结果：当前版本可能不支持同步关联会话；账号已切换，但会话没有同步。",
    });
  }
}
