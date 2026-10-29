/**
 * @vitest-environment jsdom
 *
 * 切号弹窗的「迁移账号记忆」契约。
 *
 * 锁三件事，任何一条被改坏都会让用户「以为迁了、其实没迁」：
 * 1. 默认不勾选、不发预检请求——记忆是账号级文件，不能默认搬；
 * 2. 勾选后先预检，把「迁多少字」「目标已有记忆不会覆盖」讲在前面；
 * 3. 提交时 `migrateMemory` 只在勾选后出现，没勾就不传。
 */
import { afterEach, beforeEach, describe, expect, it, vi } from "vitest";
import { cleanup, render, screen, waitFor } from "@testing-library/react";
import userEvent from "@testing-library/user-event";
import { toast } from "sonner";

import { SwitchAccountDialog } from "@/components/switch-account-dialog";
import * as api from "@/lib/api";
import type { AccountMeta, MemoryPreview, SwitchResult } from "@/lib/types";

vi.mock("sonner", () => ({
  toast: { success: vi.fn(), warning: vi.fn(), error: vi.fn(), info: vi.fn() },
}));

vi.mock("@tauri-apps/api/event", () => ({
  listen: vi.fn(() => Promise.resolve(() => {})),
}));

vi.mock("@/lib/api", () => ({
  isWebui: () => false,
  asError: (error: unknown) => (error instanceof Error ? error.message : String(error)),
  listSessions: vi.fn(),
  sessionLinksPreview: vi.fn(),
  accountMemoryPreview: vi.fn(),
  switchAccount: vi.fn(),
  switchProgress: vi.fn(),
  checkAuthPermission: vi.fn(),
  revealAppInFinder: vi.fn(),
  openPermissionSettings: vi.fn(),
}));

const listSessions = vi.mocked(api.listSessions);
const linksPreview = vi.mocked(api.sessionLinksPreview);
const memoryPreview = vi.mocked(api.accountMemoryPreview);
const switchAccount = vi.mocked(api.switchAccount);

const ACCOUNT = { id: "acc-1", uid: "uid-b", nickname: "目标账号" } as AccountMeta;

const OK_RESULT: SwitchResult = { ok: true, account: "目标账号", backup: null };

function preview(overrides: Partial<MemoryPreview> = {}): MemoryPreview {
  return {
    variant: "cn",
    sourceUid: "uid-a",
    targetUid: "uid-b",
    source: {
      path: "/data/memory/uid-a_memory.md",
      bytes: 512,
      blockChars: 42,
      updatedAt: "2026-10-01T00:00:00.000Z",
      legacy: false,
    },
    target: null,
    targetHasMemory: false,
    canApply: true,
    blockedReason: null,
    ...overrides,
  };
}

function renderDialog() {
  return render(
    <SwitchAccountDialog open account={ACCOUNT} onOpenChange={() => {}} onDone={() => {}} />,
  );
}

beforeEach(() => {
  // 弹窗的高度自适应用 ResizeObserver 量内容，jsdom 没有实现；测试里只要它不报错。
  vi.stubGlobal(
    "ResizeObserver",
    class {
      observe() {}
      unobserve() {}
      disconnect() {}
    },
  );
  listSessions.mockResolvedValue({ sessions: [], current: "uid-a" });
  linksPreview.mockResolvedValue({
    supported: true,
    storeStatus: "ready",
    sourceUid: "uid-a",
    targetUid: "uid-b",
    groups: [],
  });
  switchAccount.mockResolvedValue(OK_RESULT);
  memoryPreview.mockResolvedValue(preview());
});

afterEach(() => {
  cleanup();
  vi.clearAllMocks();
});

describe("切号弹窗的账号记忆迁移", () => {
  it("默认不勾选，也不发预检请求", async () => {
    renderDialog();
    const checkbox = await screen.findByRole("checkbox", { name: "迁移账号记忆" });
    expect(checkbox.getAttribute("aria-checked")).toBe("false");
    expect(memoryPreview).not.toHaveBeenCalled();
  });

  it("勾选后预检并说明将迁移多少字", async () => {
    const user = userEvent.setup();
    renderDialog();
    await user.click(await screen.findByRole("checkbox", { name: "迁移账号记忆" }));

    await waitFor(() => expect(memoryPreview).toHaveBeenCalledWith("acc-1"));
    expect(await screen.findByText(/将迁移约 42 字记忆/)).toBeTruthy();
  });

  it("目标已有记忆时明确说明不会覆盖", async () => {
    memoryPreview.mockResolvedValue(
      preview({
        target: {
          path: "/data/memory/uid-b_memory.md",
          bytes: 128,
          blockChars: 7,
          updatedAt: null,
          legacy: false,
        },
        targetHasMemory: true,
        canApply: false,
        blockedReason: "目标账号已有记忆，需要显式选择覆盖",
      }),
    );
    const user = userEvent.setup();
    renderDialog();
    await user.click(await screen.findByRole("checkbox", { name: "迁移账号记忆" }));

    // 静默替换等于替用户删掉目标账号的记忆，所以提示必须写明「不覆盖」。
    expect(await screen.findByText(/已有 7 字记忆，本次不会覆盖/)).toBeTruthy();
  });

  it("目标已有记忆时才出现覆盖选项，勾上才传 memoryOverwrite", async () => {
    memoryPreview.mockResolvedValue(
      preview({
        target: {
          path: "/data/memory/uid-b_memory.md",
          bytes: 128,
          blockChars: 7,
          updatedAt: null,
          legacy: false,
        },
        targetHasMemory: true,
        canApply: false,
        blockedReason: "目标账号已有记忆，需要显式选择覆盖",
      }),
    );
    const user = userEvent.setup();
    renderDialog();

    // 目标为空时不该出现这个选项：没东西可覆盖，不制造多余的风险开关。
    expect(screen.queryByRole("checkbox", { name: "覆盖目标账号已有的记忆" })).toBeNull();

    await user.click(await screen.findByRole("checkbox", { name: "迁移账号记忆" }));
    const overwrite = await screen.findByRole("checkbox", { name: "覆盖目标账号已有的记忆" });
    await user.click(overwrite);
    expect(await screen.findByText(/将替换目标账号现有的 7 字记忆/)).toBeTruthy();

    await user.click(screen.getByRole("button", { name: "确认切换" }));
    await waitFor(() => expect(switchAccount).toHaveBeenCalledTimes(1));
    expect(switchAccount.mock.calls[0][0]).toMatchObject({
      migrateMemory: true,
      memoryOverwrite: true,
    });
  });

  it("勾选后提交带上 migrateMemory", async () => {
    const user = userEvent.setup();
    renderDialog();
    await user.click(await screen.findByRole("checkbox", { name: "迁移账号记忆" }));
    await user.click(screen.getByRole("button", { name: "确认切换" }));

    await waitFor(() => expect(switchAccount).toHaveBeenCalledTimes(1));
    expect(switchAccount.mock.calls[0][0]).toMatchObject({ migrateMemory: true });
  });

  it("没勾选时不传 migrateMemory", async () => {
    const user = userEvent.setup();
    renderDialog();
    await user.click(await screen.findByRole("button", { name: "确认切换" }));

    await waitFor(() => expect(switchAccount).toHaveBeenCalledTimes(1));
    expect(switchAccount.mock.calls[0][0].migrateMemory).toBeUndefined();
  });

  it("后端跳过迁移时单独提示原因，不并进成功文案", async () => {
    switchAccount.mockResolvedValue({
      ...OK_RESULT,
      memoryMigration: { status: "skipped", reason: "目标账号已有记忆，未覆盖；如确认替换请选择覆盖" },
    });
    const user = userEvent.setup();
    renderDialog();
    await user.click(await screen.findByRole("checkbox", { name: "迁移账号记忆" }));
    await user.click(screen.getByRole("button", { name: "确认切换" }));

    await waitFor(() =>
      expect(toast.warning).toHaveBeenCalledWith(
        "账号记忆未迁移",
        expect.objectContaining({ description: expect.stringContaining("已有记忆") }),
      ),
    );
  });
});
