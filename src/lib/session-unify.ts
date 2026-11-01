import type { SessionGroupUnifyPlan, SessionSyncMode } from "@/lib/types";

/** 「以此为准」统一时要执行的一项：目标 + 已校验的预览 + 写入模式。 */
export interface UnifyAction {
  targetMemberId: string;
  accountName: string;
  previewToken: string;
  mode: SessionSyncMode;
}

/**
 * 无法参与统一的目标。有任意一项时**不能**执行，且必须告诉用户原因。
 *
 * 之前这里是「只要有一项不满足就 return」——没有提示、没有日志，用户点了确认
 * 看到的是「什么都没发生」，与「执行了但没生效」无法区分。
 */
export interface UnifyBlocker {
  accountName: string;
  reason: string;
}

/**
 * 把统一计划拆成「可执行项」与「阻塞项」。
 *
 * 判定与后端一致：
 * - 预览缺失 / 失败 → 阻塞；
 * - 预览与当前组不匹配（对象过期）→ 阻塞；
 * - 判为 `identical` 的目标本来就无需写入，跳过且不算阻塞；
 * - 其余目标必须有可用模式（`fastForward` / `overwrite` / `unifyOverwrite`），
 *   没有就是「无法确认」，同样阻塞。
 *
 * 纯函数：不碰 IO，便于用固定夹具覆盖「全部可执行 / 部分阻塞」两类分支。
 */
export function planUnify(plan: SessionGroupUnifyPlan): {
  actions: UnifyAction[];
  blockers: UnifyBlocker[];
  skippedIdentical: number;
} {
  const actions: UnifyAction[] = [];
  const blockers: UnifyBlocker[] = [];
  let skippedIdentical = 0;

  for (const target of plan.targets) {
    if (target.error) {
      blockers.push({ accountName: target.accountName, reason: target.error });
      continue;
    }
    const preview = target.preview;
    if (!preview) {
      blockers.push({ accountName: target.accountName, reason: "没有拿到检查结果" });
      continue;
    }
    if (
      preview.client !== plan.client
      || preview.groupId !== plan.groupId
      || preview.sourceMemberId !== plan.sourceMemberId
      || preview.targetMemberId !== target.memberId
    ) {
      blockers.push({ accountName: target.accountName, reason: "检查结果已过期，请重新检查" });
      continue;
    }
    if (preview.verdict === "identical") {
      skippedIdentical += 1;
      continue;
    }
    // 顺序与「以此为准」的语义一致：能快进就快进，冲突才覆盖，来源更旧才用整组覆盖。
    const mode = (["fastForward", "overwrite", "unifyOverwrite"] as SessionSyncMode[])
      .find((candidate) => preview.availableModes.includes(candidate));
    if (!mode || !preview.previewToken) {
      blockers.push({
        accountName: target.accountName,
        reason: preview.reason || "该账号的内容无法确认，不能写入；请重新检查或先在客户端确认这份会话",
      });
      continue;
    }
    actions.push({
      targetMemberId: target.memberId,
      accountName: target.accountName,
      previewToken: preview.previewToken,
      mode,
    });
  }

  return { actions, blockers, skippedIdentical };
}

/** 阻塞原因的汇总文案：一次把「谁、为什么」说清楚，不让用户逐个猜。 */
export function describeUnifyBlockers(blockers: UnifyBlocker[]): string {
  if (blockers.length === 0) return "";
  if (blockers.length <= 3) {
    return blockers.map((blocker) => `${blocker.accountName}：${blocker.reason}`).join("；");
  }
  const head = blockers.slice(0, 3).map((blocker) => `${blocker.accountName}：${blocker.reason}`).join("；");
  return `${head}；另有 ${blockers.length - 3} 个账号同样无法写入`;
}
