import { CopyPlus, Link2, Trash2, X } from "lucide-react";

import { Button } from "@/components/ui/button";

/**
 * 列表级批量操作条：只在有选中项时出现。
 *
 * 「全选当前页」与「全选筛选结果」分开给：列表是 8 条一页、筛选在客户端做，
 * 只留一个「全选」会让用户不知道自己选了多少页之外的东西。
 */
export function SessionBatchBar({
  selectedCount,
  hiddenCount,
  pageAllSelected,
  allSelected,
  filteredCount,
  busy,
  onTogglePage,
  onSelectAll,
  onClear,
  onSync,
  onCopy,
  onDelete,
}: {
  selectedCount: number;
  /** 选中项里不在当前筛选结果内的数量（换筛选后仍保留选择）。 */
  hiddenCount: number;
  pageAllSelected: boolean;
  allSelected: boolean;
  filteredCount: number;
  busy: boolean;
  onTogglePage: () => void;
  onSelectAll: () => void;
  onClear: () => void;
  onSync: () => void;
  /** 补副本：把选中的组复制成新会话补到指定账号（放大写入，需先选目标账号）。 */
  onCopy: () => void;
  onDelete: () => void;
}) {
  return (
    <div
      role="toolbar"
      aria-label="批量操作"
      className="mb-3 flex flex-wrap items-center justify-between gap-2 rounded-lg border border-brand/40 bg-brand/5 px-3 py-2"
    >
      <div className="flex min-w-0 flex-wrap items-center gap-2 text-sm">
        <span className="font-medium">已选 {selectedCount} 个会话组</span>
        {hiddenCount > 0 && (
          <span className="text-xs text-muted-foreground">其中 {hiddenCount} 个不在当前筛选内</span>
        )}
        <span className="mx-1 h-4 w-px bg-border" aria-hidden="true" />
        <Button variant="ghost" size="sm" className="h-7 px-2 text-xs" disabled={busy} onClick={onTogglePage}>
          {pageAllSelected ? "取消本页" : "全选本页"}
        </Button>
        <Button
          variant="ghost"
          size="sm"
          className="h-7 px-2 text-xs"
          disabled={busy || allSelected || filteredCount === 0}
          onClick={onSelectAll}
        >
          全选筛选结果（{filteredCount}）
        </Button>
        <Button variant="ghost" size="sm" className="h-7 px-2 text-xs text-muted-foreground" disabled={busy} onClick={onClear}>
          <X className="size-3.5" />清空
        </Button>
      </div>
      <div className="flex shrink-0 items-center gap-2">
        <Button size="sm" className="h-8" disabled={busy} onClick={onSync}>
          <Link2 className="size-4" />批量同步
        </Button>
        <Button variant="outline" size="sm" className="h-8" disabled={busy} onClick={onCopy}>
          <CopyPlus className="size-4" />补副本
        </Button>
        <Button variant="outline" size="sm" className="h-8 text-destructive" disabled={busy} onClick={onDelete}>
          <Trash2 className="size-4" />批量删除关联
        </Button>
      </div>
    </div>
  );
}
