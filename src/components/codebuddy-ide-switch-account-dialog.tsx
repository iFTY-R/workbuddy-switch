import { CircleCheck, RefreshCw } from "lucide-react";

import { SessionSwitchDialog, type SessionSwitchClient } from "@/components/session-switch-dialog";
import * as api from "@/lib/api";
import type { AccountMeta, CodeBuddyCnIdeStatus, WbVariant } from "@/lib/types";
import { variantUsesIntlCodebuddyIde } from "@/lib/variant";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 目标账号 */
  account: AccountMeta | null;
  /**
   * 当前档位：决定会话列表 / 关联预览 / 切换走哪条通道。
   *
   * 国内版（`CodeBuddy CN.app`）与国际版（`CodeBuddy.app`）共用同一套会话存储与后端语义，
   * 因此组件只做这一处分流，两个 tab、摘要与空态保持一份。
   */
  variant: WbVariant;
  /** CodeBuddy IDE 状态（用于渲染空态与运行中提示）。 */
  ideStatus?: CodeBuddyCnIdeStatus | null;
  /** 切换完成后刷新列表 */
  onDone?: () => void;
}

/**
 * CodeBuddy IDE 客户端在切号弹窗里的差异面。
 *
 * 与 VS Code 的差异：切换固定走「关闭并重开 IDE」（`restart = true`，没有自动重开开关）、
 * 状态提示只有两态、国内版/国际版按 `variant` 分流三条 API 通道。
 */
function buildClient(
  status: CodeBuddyCnIdeStatus | null | undefined,
  intl: boolean,
): SessionSwitchClient {
  return {
    description:
      "将把所选账号写入 CodeBuddy IDE；可选把当前账号的会话复制过去，并把关联会话的新内容同步过去。",
    statusNotice: ({ running }) =>
      running
        ? {
            icon: <RefreshCw className="size-4" />,
            title: "将自动关闭并重开 CodeBuddy IDE",
            description:
              "将先关闭 CodeBuddy IDE（未保存内容由 IDE 自身提示保护），写入凭证后自动重新打开。",
            warning: false,
          }
        : {
            icon: <CircleCheck className="size-4" />,
            title: "CodeBuddy IDE 未运行，可直接切换",
            description: "写入凭证后自动打开 CodeBuddy IDE，即为目标账号。",
            warning: false,
          },
    notLoggedInHint:
      "未检测到 CodeBuddy IDE 登录态，将按新登录写入；切换后打开 IDE 即登录为目标账号。",
    busyTitle: ({ copyCount, syncCount }) =>
      copyCount > 0
        ? "正在关闭 CodeBuddy IDE 并复制会话…"
        : syncCount > 0
          ? "正在关闭 CodeBuddy IDE 并同步会话…"
          : "正在关闭 CodeBuddy IDE 并写入凭证…",
    busyHint: () => "若 IDE 弹出保存提示请先处理（最长等待 60 秒）",
    // 国际版不再有独立确认框，不勾选时由底部摘要承担「将重启 IDE」的说明。
    // 国内版文案保持原句，不在这里改。
    summarySubIdle: intl
      ? ({ running }) =>
          running ? "未选择复制或同步会话，确认后将关闭并重启 IDE" : "未选择复制或同步会话，确认后将打开 IDE"
      : undefined,
    emptyHint: ({ loading, sourceUid, dataRoot, hasCopyable }) =>
      emptyStateHint(status, loading, sourceUid, dataRoot, hasCopyable),
    loadSessions: intl ? api.listCodebuddyIntlIdeSessions : api.listCodebuddyIdeSessions,
    switchAccount: ({ accountId, refs, syncSelections }) =>
      intl
        ? api.switchCodebuddyIdeAccount(accountId, true, refs, syncSelections)
        : api.switchCodebuddyCnIdeAccount(accountId, true, refs, syncSelections),
    restartHint: (res) => res.message || "已重启 CodeBuddy IDE",
    // 两个 IDE 共用展示件，只有预览通道按档位分流（稳定引用：模块级函数）。
    fetchPreview: intl ? api.codebuddyIntlIdeSessionLinksPreview : api.codebuddyIdeSessionLinksPreview,
    loggedOutHint: "未检测到 CodeBuddy IDE 当前登录账号，请先在 CodeBuddy IDE 中登录后再切换。",
  };
}

/**
 * CodeBuddy IDE 会话切换弹窗：可勾选「当前 IDE 账号」的会话复制到目标账号。
 *
 * 国内版与国际版共用本组件（差异只有三处 API 通道，按 `variant` 分流），与 VS Code 插件弹窗
 * 同形（关联会话 / 复制会话两个 tab），差异集中在 {@link buildClient}。
 */
export function CodebuddyIdeSwitchAccountDialog({ open, onOpenChange, account, variant, ideStatus, onDone }: Props) {
  const intl = variantUsesIntlCodebuddyIde(variant);
  return (
    <SessionSwitchDialog
      open={open}
      onOpenChange={onOpenChange}
      account={account}
      running={ideStatus?.running === true}
      loggedIn={ideStatus?.loggedIn !== false}
      client={buildClient(ideStatus, intl)}
      onDone={onDone}
    />
  );
}

/** 统一空态文案：区分未装 IDE / 未找到数据目录 / 未登录 / 无会话。 */
function emptyStateHint(
  status: CodeBuddyCnIdeStatus | null | undefined,
  loading: boolean,
  sourceUid: string | null,
  dataRoot: string | null | undefined,
  hasCopyable: boolean,
): string {
  if (loading) return "正在加载会话…";
  if (status && !status.installed) return "未检测到 CodeBuddy IDE，请先安装并登录";
  if (dataRoot === null) return "未找到 CodeBuddy IDE 数据目录，请先打开 IDE 并登录一次";
  if (!sourceUid) return "未检测到 CodeBuddy IDE 当前登录账号，请先在 IDE 中登录";
  if (!hasCopyable) return "当前账号暂无可复制的会话（无含正文的历史）";
  return "将当前账号勾选的会话复制给目标账号（沿用会话 id，目标已有同 id 时自动改用新 id）";
}
