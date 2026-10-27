/**
 * @vitest-environment jsdom
 *
 * 切号弹窗的冒烟测试：锁住 #8 合并后**必须成立**的几条契约。
 *
 * 这里测的不是像素，而是「两个客户端共用同一套流程」这件事本身：
 * 勾选 → 摘要 → 提交载荷 → 结果提示 → 错误不关窗。任何一个客户端改坏了，
 * 或有人把 `SessionSwitchDialog` 改回各写一份，这些断言都会失败。
 */
import { afterEach, beforeAll, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { toast } from "sonner";

import {
  SessionSwitchDialog,
  type SessionSwitchClient,
  type SwitchPayload,
} from "@/components/session-switch-dialog";
import { TooltipProvider } from "@/components/ui/tooltip";
import type {
  AccountMeta,
  SessionLinksPreview,
  VscodeSessionCopyResult,
  VscodeSessionList,
} from "@/lib/types";

vi.mock("sonner", () => ({
  toast: { success: vi.fn(), warning: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

const ACCOUNT = { id: "acc-1", uid: "uid-b", nickname: "目标账号" } as AccountMeta;

const EMPTY_PREVIEW: SessionLinksPreview = {
  supported: true,
  storeStatus: "ready",
  sourceUid: "uid-a",
  targetUid: "uid-b",
  groups: [],
};

const ONE_SESSION: VscodeSessionList = {
  sourceUid: "uid-a",
  dataRoot: "/tmp/ext",
  sessions: [
    {
      id: "c1",
      workspaceHash: "hash-1",
      title: "会话一",
      updatedAt: 1,
      type: "craft",
      hasHistory: true,
    },
  ],
};

/** 一个最小可用客户端：只有 loadSessions / switchAccount 参与断言，其余照默认填。 */
function makeClient(overrides: Partial<SessionSwitchClient> = {}): SessionSwitchClient {
  return {
    description: "测试用说明",
    emptyHint: ({ loading, hasCopyable }) =>
      loading ? "正在加载会话…" : hasCopyable ? "可复制" : "暂无可复制的会话",
    statusNotice: () => ({ icon: null, title: "状态提示", description: "说明", warning: false }),
    busyTitle: () => "正在切换…",
    busyHint: () => "请勿关闭窗口",
    loadSessions: async () => ONE_SESSION,
    switchAccount: async () => ({ message: "已切换" }),
    restartHint: () => "已切换",
    fetchPreview: async () => EMPTY_PREVIEW,
    ...overrides,
  };
}

function renderDialog(client: SessionSwitchClient, props: Record<string, unknown> = {}) {
  const onOpenChange = vi.fn();
  const onDone = vi.fn();
  render(
    // 应用层在 App.tsx 里包了 TooltipProvider，测试要补上同一层上下文。
    <TooltipProvider>
      <SessionSwitchDialog
        open
        onOpenChange={onOpenChange}
        account={ACCOUNT}
        running={false}
        loggedIn
        client={client}
        onDone={onDone}
        {...props}
      />
    </TooltipProvider>,
  );
  return { onOpenChange, onDone };
}

beforeAll(() => {
  // Radix 的 Tooltip / DismissableLayer 在 jsdom 下会用到这两个 API，缺了会抛错。
  const globalWithStubs = globalThis as unknown as {
    ResizeObserver?: new () => { observe(): void; unobserve(): void; disconnect(): void };
    DOMRect?: unknown;
  };
  globalWithStubs.ResizeObserver ??= class {
    observe() {}
    unobserve() {}
    disconnect() {}
  };
  if (!("matchMedia" in window)) {
    Object.defineProperty(window, "matchMedia", {
      writable: true,
      value: (query: string) => ({
        matches: false,
        media: query,
        onchange: null,
        addEventListener() {},
        removeEventListener() {},
        addListener() {},
        removeListener() {},
        dispatchEvent: () => false,
      }),
    });
  }
});

// vitest 未开 globals，RTL 的自动清理不会注册：显式清理，避免上一个用例的弹窗
// 残留在 DOM 里导致「找到多个同名元素」。
afterEach(() => {
  cleanup();
});

beforeEach(() => {
  vi.mocked(toast.success).mockClear();
  vi.mocked(toast.warning).mockClear();
  vi.mocked(toast.error).mockClear();
});

describe("SessionSwitchDialog", () => {
  it("渲染标题、两个 tab 与空闲态摘要", async () => {
    renderDialog(makeClient());

    expect(await screen.findByText("切换到「目标账号」")).toBeTruthy();
    expect(screen.getByRole("tab", { name: /关联会话/ })).toBeTruthy();
    expect(screen.getByRole("tab", { name: /复制会话/ })).toBeTruthy();
    expect(screen.getByText("本次仅切换账号")).toBeTruthy();
    expect(screen.getByText("未选择复制或同步会话")).toBeTruthy();
    // 没勾复制时确认按钮可用（只切号）。
    expect((screen.getByRole("button", { name: "确认切换" }) as HTMLButtonElement).disabled).toBe(
      false,
    );
  });

  it("勾选整组会话后摘要更新，提交载荷带上 refs 并关闭弹窗", async () => {
    const user = userEvent.setup();
    const switchAccount = vi.fn(async (_payload: SwitchPayload) => ({ message: "已切换" }));
    const client = makeClient({ switchAccount });
    const { onOpenChange, onDone } = renderDialog(client);

    await user.click(await screen.findByRole("tab", { name: /复制会话/ }));
    await user.click(await screen.findByLabelText("复制会话到目标账号"));
    // 组头复选框：点一下即全选该工作区的会话。
    await user.click(await screen.findByLabelText("选择工作区 #1"));

    await waitFor(() => expect(screen.getByText("将复制 1 个会话")).toBeTruthy());

    await user.click(screen.getByRole("button", { name: "确认切换" }));

    await waitFor(() => expect(switchAccount).toHaveBeenCalledTimes(1));
    const payload = switchAccount.mock.calls[0][0];
    expect(payload.accountId).toBe("acc-1");
    expect(payload.refs).toEqual([{ workspaceHash: "hash-1", conversationId: "c1" }]);
    // 没有关联会话被勾选时，不该带 syncSelections。
    expect(payload.syncSelections).toBeUndefined();
    expect(toast.success).toHaveBeenCalledTimes(1);
    expect(onOpenChange).toHaveBeenCalledWith(false);
    expect(onDone).toHaveBeenCalledTimes(1);
  });

  it("切换报错时显示错误条、弹窗不关闭、不提示成功", async () => {
    const user = userEvent.setup();
    const client = makeClient({
      switchAccount: async () => {
        throw new Error("写入失败：磁盘只读");
      },
    });
    const { onOpenChange } = renderDialog(client);

    await user.click(screen.getByRole("button", { name: "确认切换" }));

    expect(await screen.findByText("写入失败：磁盘只读")).toBeTruthy();
    expect(toast.success).not.toHaveBeenCalled();
    expect(onOpenChange).not.toHaveBeenCalled();
  });

  it("复制了但未建立关联时单独告警，不当成完整成功", async () => {
    const user = userEvent.setup();
    const client = makeClient({
      switchAccount: async () => ({
        message: "已切换",
        sessionCopy: {
          sourceUid: "uid-a",
          targetUid: "uid-b",
          copied: [{ workspaceHash: "hash-1", oldId: "c1", newId: "c9", messages: 2 }],
          errors: [],
          linkErrors: [{ conversationId: "c1", error: "关联表不可写" }],
        } satisfies VscodeSessionCopyResult,
      }),
    });
    renderDialog(client);

    await user.click(screen.getByRole("button", { name: "确认切换" }));

    await waitFor(() => expect(toast.warning).toHaveBeenCalledTimes(1));
    const [title, options] = vi.mocked(toast.warning).mock.calls[0];
    expect(title).toBe("部分会话已复制但未建立关联");
    expect(String((options as { description?: string }).description)).toContain("关联表不可写");
  });

  it("不提供 autoRestart 的客户端不渲染自动重开开关", async () => {
    const { unmount } = render(
      <TooltipProvider>
        <SessionSwitchDialog
          open
          onOpenChange={vi.fn()}
          account={ACCOUNT}
          running
          loggedIn
          client={makeClient()}
        />
      </TooltipProvider>,
    );
    // 「复制会话」tab 里也有一个开关（forceMount 常驻），所以按标签精确判定。
    expect(screen.queryByLabelText("自动关闭并重开")).toBeNull();
    unmount();

    renderDialog(
      makeClient({
        autoRestart: { storageKey: "test.autoRestart", label: "自动关闭并重开", hint: "说明" },
      }),
      { running: true },
    );
    expect(await screen.findByLabelText("自动关闭并重开")).toBeTruthy();
  });
});
