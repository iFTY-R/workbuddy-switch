import { useCallback, useEffect, useRef, useState } from "react";

import * as api from "@/lib/api";
import type { SessionBatchJob, SessionBatchOp, SessionBatchTarget, SessionGroupClient, WbVariant } from "@/lib/types";

/** 轮询间隔：批量以「组」为粒度推进，600ms 足够跟手，也不会把 IPC 打满。 */
const POLL_INTERVAL = 600;

/**
 * 按动作分派到对应的启动接口。
 *
 * 用 `switch` + `never` 而不是 if 链：if 链的兜底分支必须是某个具体动作，新增第四个动作
 * 却忘了写分支时，会**静默**走到那个兜底（若兜底是 delete，等于误删）。这里的 `default`
 * 断言穷尽性，漏分支直接编译失败。
 *
 * 三个动作的入参也不同（补副本要多带目标账号），所以显式分派：少传字段应当由后端拒绝，
 * 而不是在这里被默认值悄悄补成别的语义。
 */
function startJob(args: StartBatchArgs): Promise<{ jobId: string }> {
  switch (args.op) {
    case "sync":
      return api.startSessionBatchSync({
        client: args.client,
        groups: args.groups,
        variantScope: args.variantScope,
        allowRestart: args.allowRestart,
      });
    case "copy":
      return api.startSessionBatchCopy({
        client: args.client,
        groups: args.groups,
        targetAccountIds: args.targetAccountIds ?? [],
        variantScope: args.variantScope,
        allowRestart: args.allowRestart,
      });
    case "delete":
      return api.startSessionBatchDelete({
        client: args.client,
        groups: args.groups,
        variantScope: args.variantScope,
      });
    default: {
      // 走到这里说明 SessionBatchOp 加了新值但没写分支：让编译器先报错。
      const unreachable: never = args.op;
      return Promise.reject(new Error(`未知的批量动作：${String(unreachable)}`));
    }
  }
}

export interface StartBatchArgs {
  op: SessionBatchOp;
  client: SessionGroupClient;
  groups: SessionBatchTarget[];
  variantScope?: WbVariant;
  allowRestart?: boolean;
  /** 补副本专用：要补到的账号 id。缺失时后端会直接拒绝（不选 ≠ 全选）。 */
  targetAccountIds?: string[];
}

/**
 * 批量任务的进度轮询。
 *
 * 之所以自己轮询而不是等一个长请求：桌面端走 `invoke`、webui 走 HTTP，长任务绑在请求上
 * 会因断连/刷新丢结果；后端把状态放在 job 里，前端随时可以重连继续看。
 *
 * 组件卸载或任务换代后，晚到的响应一律丢弃（`jobId.current` 比对），不会写回新一轮状态。
 */
export function useSessionBatch() {
  const [job, setJob] = useState<SessionBatchJob | null>(null);
  const [error, setError] = useState<string | null>(null);
  const [starting, setStarting] = useState(false);
  const timer = useRef<ReturnType<typeof setTimeout> | null>(null);
  const jobId = useRef<string | null>(null);
  const alive = useRef(true);

  const stop = useCallback(() => {
    if (timer.current) {
      clearTimeout(timer.current);
      timer.current = null;
    }
  }, []);

  useEffect(() => {
    alive.current = true;
    return () => {
      alive.current = false;
      if (timer.current) clearTimeout(timer.current);
    };
  }, []);

  const poll = useCallback(function pollInner(id: string) {
    stop();
    timer.current = setTimeout(async () => {
      try {
        const next = await api.sessionBatchProgress(id);
        if (!alive.current || jobId.current !== id) return;
        setJob(next);
        if (next.state === "finished" || next.state === "interrupted") {
          jobId.current = null;
          stop();
          return;
        }
        pollInner(id);
      } catch (caught) {
        if (!alive.current || jobId.current !== id) return;
        // 任务可能已被清理（进程重启/过期）：明确报错，而不是让进度条永远停在那。
        setError(api.asError(caught));
        jobId.current = null;
        stop();
      }
    }, POLL_INTERVAL);
  }, [stop]);

  const start = useCallback(
    async (args: StartBatchArgs) => {
      setStarting(true);
      setError(null);
      setJob(null);
      stop();
      jobId.current = null;
      try {
        const created = await startJob(args);
        jobId.current = created.jobId;
        poll(created.jobId);
        return true;
      } catch (caught) {
        setError(api.asError(caught));
        return false;
      } finally {
        setStarting(false);
      }
    },
    [poll, stop],
  );

  const cancel = useCallback(async () => {
    const id = jobId.current;
    if (!id) return;
    try {
      await api.sessionBatchCancel(id);
    } catch (caught) {
      setError(api.asError(caught));
    }
  }, []);

  const reset = useCallback(() => {
    stop();
    jobId.current = null;
    setJob(null);
    setError(null);
    setStarting(false);
  }, [stop]);

  /** 打开一份历史报告（来自 `sessionBatchReports`），不发起轮询。 */
  const adopt = useCallback((report: SessionBatchJob) => {
    stop();
    jobId.current = null;
    setError(null);
    setJob(report);
  }, [stop]);

  // interrupted 也是"已结束"：它只是上一个进程留下的终态，不该继续轮询或挡住操作。
  const running = starting || (job !== null && job.state !== "finished" && job.state !== "interrupted");
  return { job, error, starting, running, start, cancel, reset, adopt };
}
