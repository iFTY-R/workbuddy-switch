import { CircleCheck, RefreshCw, TriangleAlert } from "lucide-react";

import { SessionSwitchDialog, type SessionSwitchClient } from "@/components/session-switch-dialog";
import * as api from "@/lib/api";
import type { AccountMeta, VscodeExtStatus } from "@/lib/types";

interface Props {
  open: boolean;
  onOpenChange: (open: boolean) => void;
  /** 目标账号 */
  account: AccountMeta | null;
  /** VS Code 扩展状态（用于渲染空态与运行中提示）。 */
  vscodeExtStatus?: VscodeExtStatus | null;
  /** 切换完成后刷新列表 */
  onDone?: () => void;
}

/** 自动关闭并重开 VS Code 的开关持久化 key（缺省开启，与 `wb-switch.compact` 同风格）。 */
const AUTO_RESTART_KEY = "wb-switch.vscodeExt.autoRestart";

/**
 * VS Code 扩展客户端在切号弹窗里的差异面。
 *
 * 流程、排版、结果提示全部来自 {@link SessionSwitchDialog}；这里只声明「VS Code 是什么样」：
 * 有一个「自动关闭并重开」开关，状态提示分三态（运行中 + 开/关、未运行），空态要区分
 * 「没装 VS Code / 没装扩展 / 没数据目录 / 未登录 / 无可复制会话」。
 */
function buildClient(status: VscodeExtStatus | null | undefined): SessionSwitchClient {
  return {
    description:
      "将把所选账号写入 VS Code CodeBuddy 插件；可选把当前账号的会话复制过去，并把关联会话的新内容同步过去。",
    autoRestart: {
      storageKey: AUTO_RESTART_KEY,
      label: "自动关闭并重开 VS Code",
      hint: "VS Code 运行时先自动关闭编辑器，写入凭证后再重新打开",
    },
    statusNotice: ({ running, autoRestart }) =>
      running
        ? autoRestart
          ? {
              icon: <RefreshCw className="size-4" />,
              title: "将自动关闭并重开 VS Code",
              description:
                "将先关闭 VS Code（未保存内容由 VS Code 自身提示/热退出保护），写入凭证后自动重新打开。",
              warning: false,
            }
          : {
              icon: <TriangleAlert className="size-4" />,
              title: "请先完全退出 VS Code",
              description:
                "已关闭「自动关闭并重开」。检测到 VS Code 正在运行，运行中写入会被覆盖且不会生效，请完全退出后重试。",
              warning: true,
            }
        : {
            icon: <CircleCheck className="size-4" />,
            title: "VS Code 未运行，可直接切换",
            description: "写入凭证后打开 VS Code，插件即为目标账号。",
            warning: false,
          },
    notLoggedInHint:
      "未检测到 VS Code CodeBuddy 插件登录态，将按新会话写入；切换后打开 VS Code 即登录为目标账号。",
    busyTitle: ({ autoClose, copyCount, syncCount }) =>
      autoClose
        ? "正在关闭 VS Code 并写入凭证…"
        : copyCount > 0
          ? "正在切换并复制会话…"
          : syncCount > 0
            ? "正在切换并同步会话…"
            : "正在切换账号…",
    busyHint: ({ autoClose }) =>
      autoClose ? "若 VS Code 弹出保存提示请先处理（最多等待 60 秒）" : "正在处理中，请勿关闭窗口",
    emptyHint: ({ loading, sourceUid, dataRoot, hasCopyable }) =>
      emptyStateHint(status, loading, sourceUid, dataRoot, hasCopyable),
    loadSessions: api.listVscodeSessions,
    switchAccount: ({ accountId, refs, syncSelections, autoRestart }) =>
      api.switchVscodeExtAccount(accountId, autoRestart, refs, syncSelections),
    // 生效方式提示：重载窗口读不到外部写入，不再提示；区分「已重开 / 重开失败 / 本来没运行」。
    // 重开失败时前端拿不到原因，用后端 message（含具体错误）兜底。
    restartHint: (res) =>
      res.restarted
        ? "已切换并重新打开 VS Code"
        : res.closedByUs
          ? res.message || "已切换，但自动重新打开 VS Code 失败，请手动打开"
          : "已切换；请打开 VS Code 生效",
  };
}

/** VS Code 扩展会话切换弹窗：可勾选「当前扩展账号」的会话复制到目标账号。 */
export function VscodeSwitchAccountDialog({ open, onOpenChange, account, vscodeExtStatus, onDone }: Props) {
  return (
    <SessionSwitchDialog
      open={open}
      onOpenChange={onOpenChange}
      account={account}
      running={vscodeExtStatus?.running === true}
      loggedIn={vscodeExtStatus?.loggedIn !== false}
      client={buildClient(vscodeExtStatus)}
      onDone={onDone}
    />
  );
}

/** 统一空态文案：区分未装 VS Code / 未装扩展 / 未找到数据目录 / 未登录 / 无会话。 */
function emptyStateHint(
  status: VscodeExtStatus | null | undefined,
  loading: boolean,
  sourceUid: string | null,
  dataRoot: string | null | undefined,
  hasCopyable: boolean,
): string {
  if (loading) return "正在加载会话…";
  if (status && !status.installed) return "未检测到 VS Code，请先安装并登录 CodeBuddy 插件";
  if (status && !status.extensionInstalled) return "未安装 VS Code CodeBuddy 插件，请先在 VS Code 中安装并登录";
  if (dataRoot === null) return "未找到 VS Code CodeBuddy 插件数据目录，请先打开 VS Code 并登录插件";
  if (!sourceUid) return "未检测到 VS Code CodeBuddy 插件当前登录账号，请先在 VS Code 中登录";
  if (!hasCopyable) return "当前账号暂无可复制的会话（无含正文的历史）";
  return "将当前账号勾选的会话以新 id 复制给目标账号（加法，不影响源账号）";
}
