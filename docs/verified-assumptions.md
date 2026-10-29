# 客户端假设核对表（V1–V6）

本文件把「我们对 WorkBuddy 客户端行为的假设」逐条落成可复核的结论。
每条都标注**证据来源**与**当前处理**，避免下次再凭印象讨论。

- 证据来源只有两类：本机真实数据 `~/.workbuddy`，或客户端产物
  `D:/SoftWare/AI_Tools/WorkBuddy/resources/app.asar` 的字符串反查。
- 版本基线：客户端 v5.8.3，数据根 `~/.workbuddy`。
- 结论分三档：**已证实**（有直接证据）、**已推翻**（证据与假设相反）、
  **未证实**（找不到证据，按保守方式处理）。

---

## V1 `buddy_snapshots` 是否带账号维度

| 项 | 内容 |
|---|---|
| 假设 | 快照表可能按账号隔离，跨档复制会悬空 |
| 结论 | **已推翻**：不带账号维度 |
| 证据 | `PRAGMA table_info(buddy_snapshots)` = `snapshot_id, application_id, template_id, schema_version, template_version, snapshot_format_version, config_json, config_digest, created_at`；**没有 user_id**。本机 `select count(*)` = 0 |
| 处理 | 复制时 `buddy_snapshot_id` 置 NULL。理由不是「怕悬空」，而是这张表本身没有账号归属，继承一个 id 只会让副本指向一个与目标账号无关的本地快照 |

## V2 客户端遇到 `file-history/<id>/` 缺失会怎样

| 项 | 内容 |
|---|---|
| 假设 | 缺目录时客户端能容忍，只是「无法还原文件」 |
| 结论 | **未证实**（未做真机对照实验） |
| 证据 | 客户端用 `PathMapper` 拼 `file-history/<sessionId>/<backupFileName>`；找不到文件时的降级路径未在 asar 中定位到 |
| 处理 | 不去赌它能容忍：复制/同步一律把 `file-history/<id>/` 整目录搬过去（追加式，已存在即跳过）。`context_integrity` 把「正文引用了但文件不存在」量成 `missing` 数字 |

## V3 `agent_dirty*` 置 NULL 会不会触发重传

| 项 | 内容 |
|---|---|
| 假设 | 置 NULL 可能让客户端认为「需要上传」 |
| 结论 | **已证实无本地写入点**，重传行为未证实 |
| 证据 | asar 中 upsert 语句为 `agent_dirty = excluded.agent_dirty`，只出现在云端回写路径；**没有** `agent_dirty = 1` 这类本地写入 |
| 处理 | 复制时置 NULL。副本在目标账号下确实没有同步进度，沿用源账号的旧值等于替客户端断言一个不成立的进度。风险降为低：即便触发重传，也只是多一次上传，不会覆盖内容 |

## V4 `credit_json` 是否进账号统计

| 项 | 内容 |
|---|---|
| 假设 | 只有建表/迁移语句命中，没有读取路径（**该假设曾被当成「已关闭」**） |
| 结论 | **已推翻**：有读取路径，且语义是「这笔会话花了多少钱」 |
| 证据 | `SqliteConversationUsagePort.persistUsage(conversationId, requestId, usage)`：<br>`credit_json` 的键是 **requestId**（不是账号），值为累加金额；<br>另有 `loadCredit(sessionId)` 直接 `SELECT credit_json` 读回 |
| 处理 | 复制 `session_usage` 行时 `used`/`size` 照搬、`credit_json` **置 NULL**。副本在目标账号下还没产生消费，照搬会让它显示源账号付过的钱 |

## V5 账号记忆落在哪里、怎么认身份

| 项 | 内容 |
|---|---|
| 假设 | 记忆是按账号存的文件，改文件名即可迁移 |
| 结论 | **部分推翻**：文件名只是其中一半，**正文里还有一段 JSON 带 uid** |
| 证据 | `~/.workbuddy/memory/<uid>_memory.md` 与 `<uid>_memory.md.bak`；<br>正文形如 `# User Memory Profile` + `## Memory Block` + `<!-- RAW_JSON_START { "uid": ..., "memoryBlock": ..., "updatedAt": ... } RAW_JSON_END -->`；<br>旧版本拼写 `memery/<uid>_memery.md`（本机已空） |
| 处理 | `account_memory` 只改写内嵌 `uid` 与 `updatedAt`，其余原样保留；<br>旧拼写只读不写；<br>**不迁** `user-<uid>-personal/`（`IDENTITY.md` / `SOUL.md` / `USER.md`）——那是人格设定，迁过去等于把目标账号改造成另一个人 |

## V6 跨账号列是否还有未处理项

| 项 | 内容 |
|---|---|
| 假设 | 只处理 `project_id` / `status` / `transport` / `agent_*` 就够 |
| 结论 | **未证实**：至少 4 列仍全量继承 |
| 证据 | `sessions` 表迁移清单里有 `plugin_context_json`、`expert_id`、`expert_marketplace`、`permission_mode`、`context_window` |
| 处理 | 暂不动。理由：这些列不影响「会话是否可见」，而改动它们会改变会话的运行时行为（例如清掉 `permission_mode` 会让副本重新询问权限）。等真机验证「目标账号未安装对应插件/专家时打开副本会怎样」再决定 |

---

## 已核实但容易记错的事实

| 事实 | 证据 |
|---|---|
| 会话可见性 = `transport='local' AND deleted_at IS NULL AND (user_id=? OR user_id='')` | asar `visibleRows()` |
| 空间归属 = `isPlayground` 优先 → `project_id` 非空即 project → 其余按 local/assistant | asar `toSpace()` |
| 状态分组 = `idle/active/completed/archived`、`error/interrupted`、`failed` | asar `statusesForConversationState()` |
| `deleted_at` 本地删除用真实时间戳（1.7e12 量级），`-1` 是云端哨兵 | 本机 `sessions` 分布 |
| 上下文旁路不止三处 | `file-history/<sid>/`、`todos/<sid>.json`、`tasks/<sid>/<taskId>.json`（目录！）、`projects/<ws>/<sid>/tool-results/`、`<sid>.file-rollback.ndjson`、`session_usage` 行 |
| `tool-results` 在正文里是**绝对路径**引用，正文的会话 id 被整体替换后路径随之指向新 id | 单会话实测 143 处 |
| **`file-history` 的备份会被客户端清理**：目录里只留同一文件的较新版本，正文却保留每一步的引用 | 实测 `47d0fc70` 正文引用 199 个、目录里只有 38 个；同哈希只见到 `@v9`，`@v1..@v8` 都不在 |
| 因此「正文引用数 − 实存数」**不能直接当缺失数** | 修正后全量体检：missing 1896 → 1638，另 258 个属「被新版本取代」，broken 22 → 21 |
| 有些会话**整份** `file-history/<id>/` 都不在（正文仍有上百个引用） | 33 个会话只有 11 个有备份目录，两个账号都是如此 → 是保留策略，不是复制丢了 |
| 体积大到不该跟副本走的目录 | `workspace/sessions/`（6.2G）、`file-tree-manifests`（87M）、`changes-index` / `changes-detail`（40M）、`artifact-index` |

---

## 仍需真机验证的项

1. **V2**：删掉副本的 `file-history/<id>/` 后打开会话，客户端表现如何（报错 / 静默降级）。
2. **V3**：副本置 NULL 的 `agent_dirty*` 是否在某次启动后被重新写成 1。
3. **V6**：目标账号未装对应插件/专家时，打开带 `plugin_context_json` 的副本是否报缺 MCP。
4. 批量操作的「关一次、开一次」是否真的成立（`docs/smoke-batch.md` 六步）。
5. 记忆迁移后，目标账号的 `memory/<uid>_memory.md` 是否被客户端正常读取（而不是被判为「别人的记忆」后清空）。

## 当前基线（2026-10-29，`session_doctor` 只读体检）

```
data root: C:\Users\Rostiute\.workbuddy
sessions=33 broken=21 missingReferences=1638 prunedReferences=258
withoutFileHistoryDir=22 withoutUsage=22
```

口径说明：`broken` 只数**真缺**（连任何版本都不在）的会话；`prunedReferences`
是被同文件新版本取代的引用，属客户端正常清理。验收复制能力时用
「**副本的 missing ⊆ 源的 missing**」，而不是「副本 missing == 0」——
源会话自己就有上千条指向已清理文件的引用。
