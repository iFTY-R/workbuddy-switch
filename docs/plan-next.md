# 后续方案评审与实施规划

> 时点：2026-10-29　范围：会话上下文一致性 + 批量操作 + 结构性还债
> 前置：P0（退役目录自愈）、P1（旁路补全）、P2（scope/备份/守卫）、P3-10（上下文接入界面）已完成并提交。
>
> **进度**：Phase 0 完成（5 个提交建立可回滚基线）；Phase 1 完成（`account_memory` + 切号弹窗开关 +
> `docs/verified-assumptions.md`）；Phase 3 完成（`notCarried` 单一来源 + GC 留痕）；
> Phase 2 **代码完成但未真机执行**——放大写入的第一次必须有人在场，冒烟六步仍是前置。

## 1. 评审目标与范围

### 1.1 目标

1. 判断「现在这套方案」是否值得继续加功能，还是该先收口；
2. 把剩余待办（P3-11、D1、V1–V6、冒烟、审计遗留）变成一份有顺序、有验收、可执行的计划；
3. 明确哪些事**不做**，避免继续摊薄维护成本。

### 1.2 范围内 / 范围外

| 在范围内 | 不在范围内 |
|---|---|
| 提交切分、真机冒烟、未验证假设收口 | 新客户端档位（JetBrains / 其它 IDE） |
| 账号记忆迁移（P3-11） | 界面视觉改版、路由重构 |
| 批量补副本（D1） | 服务端多用户 / 远程访问 |
| 上下文「未搬运内容」的显式告知 | 云端同步协议变更 |
| 巨型页面拆分、tray 错误可见化 | 三套内核抽 trait（见 §8 取舍） |

### 1.3 评审依据（本轮全部为可复核证据，不是推断）

| 证据类型 | 具体 |
|---|---|
| 真机数据 | `~/.workbuddy`：33 会话 / 11 个 file-history 目录 / `projects/*/<sid>/tool-results` 4.6MB / `workspace/sessions` 6.2GB |
| 客户端源码 | `app.asar`：`visibleRows`、`statusesForConversationState`、`toSpace`、`rowMode`、`PATHS`、`writeTasks/writeTodos`、`mergeConfigDir` |
| 本轮改动 | `git diff`：12 文件 +555/−77（含 7 条新前端测试、5 条新 Rust 测试） |
| 门禁 | core 805 passed（4 条沙箱禁 `reg.exe`）、server 10、clippy `-D warnings`、tsc 0 错、vitest 106 |

## 2. 现状结论

### 2.1 已完成项与支撑证据

| 项 | 关键实现 | 证据/门禁 |
|---|---|---|
| 副本可见性归一 | `copy_visibility_override`（清 `project_id`/`group_*`、`transport=local`、`status→completed`、`buddy_snapshot_id→NULL`） | 与 `visibleRows` / `toSpace` 逐条对齐 |
| 崩溃自愈 | `retired_candidates` 单一命名源 + 预览期批量还原 | 5 条新用例，含从 `links_preview_at` 入口驱动 |
| 上下文补全 | file-history / tool-results / todos / tasks(目录) / rollback | 复制与 sync 共用 `copy_session_sidecars` |
| 上下文可观测 | `ContextIntegrity` + 组详情面板（检查/补齐/明细/GC） | 7 条 jsdom 用例 |
| 批量引擎 | job 锁判活、一次关/重开、错误分级、检查点 | 21 条用例 |

### 2.2 一句话判断

**方案方向已由证据支撑，问题从「设计对不对」转为「验证够不够、成本控不控得住」。**
继续加功能前，必须先完成两件事：把 555 行改动切成可回滚的提交；在真实客户端上跑一次冒烟。

## 3. 优点（为什么这套方向值得保留）

1. **写入模型是「可恢复」的，不是「尽量不出错」**：阶段化 operation + 备份 + `needsRecovery` 门禁 + job 锁判活，任何一步失败都留下可解释的现场。
2. **对齐的是客户端真实判定链，不是文档**：清 `project_id` 是因为 `toSpace()` 见 projectId 即判 project 空间；`status→completed` 是因为 `statusesForConversationState('idle')` 只认 4 个值。
3. **上下文从「猜」变成「量」**：`ContextIntegrity` 给出 `referenced/present/missing`，讨论「缺不缺」时有数字，不靠印象。
4. **测试范式被纠偏**：P0 的教训固化成注释——夹具必须用真实目录名，用例从入口驱动，而不是复述实现。
5. **前后端契约统一**：`{jobId}`、camelCase、HTTP 与 invoke 同形，新增能力（上下文三件套）一次接线两处可用。

## 4. 风险与不足

| # | 风险 | 证据 | 影响 | 等级 |
|---|---|---|---|---|
| R1 | 555 行改动无提交边界 | `git status` 12 改 2 新增，全在工作区 | 出问题只能整体回退，无法定位/回滚单项 | **高** |
| R2 | 关键路径未在真机验证 | `docs/smoke-batch.md` 六步未跑；P0 自愈、侧车补全也无真机验证 | 单测全绿但真实客户端行为未知（P0 就是这么漏的） | **高** |
| R3 | 未验证假设仍影响「副本能否打开」 | V1 `buddy_snapshot_id`、V3 `agent_dirty`、V6 `plugin_context_json`/`expert_id` 全量继承 | 跨账号副本可能缺插件/专家上下文 | 中 |
| R4 | 上下文能力发现性差 | 仅在组详情内，无集中入口；无「本次未搬运」提示 | 用户以为「同步了=完整」，实际缺 6.2G 快照 | 中 |
| R5 | 批量引擎只支持 WorkBuddy，`scope` 今日恒 `None` | `session_batch::start` 直接拒绝非 WorkBuddy | 扩到 IDE 时要重做生命周期语义 | 中 |
| R6 | GC 账号级、不可逆 | 仅 7 天宽限 + 干跑；无操作审计 | 误删只能靠备份，用户无感知 | 中 |
| R7 | tray 22 处 `let _ =` 吞错 | `src-tauri/src/tray.rs` | 托盘失败静默，用户只看到「点了没反应」 | 中 |
| R8 | 巨型页面 | `SettingsPage` 2135 / `CreditStatsPage` 1976 / `TokenStatsPage` 1718 行 | 每次改动都在超大文件里定位，回归面大 | 中 |
| R9 | `vendor-patched` 是构建层 hack | `Cargo.toml` patch + `vendor-patched/indexmap` | 工具链升级后可能静默失效或需重打补丁 | 低 |

## 5. 改进建议（逐条对应）

| 对应 | 建议 | 成本 |
|---|---|---|
| R1 | 按「P0 / P1 / P2 / P3-10」切 4 个提交，各自独立可编译可测；提交信息写清证据 | 0.5h |
| R2 | 先跑冒烟六步，再看是否补一条「覆盖被打断后预览自愈」的手工步骤 | 0.5d |
| R3 | V3 用「置 NULL → 启动客户端 → 观察 `agent_dirty` 是否回写/上传」验证；V6 用「带插件上下文的会话复制到无该插件的账号」验证 | 0.5d |
| R4 | 在补齐结果里增加 `notCarried` 固定清单（快照/文件树/变更索引），并在面板上常驻一行说明 | 2h |
| R5 | `scope` 保留透传（已做），并在 `start` 的拒绝文案里写明「支持 IDE 需先抽象生命周期」 | 已含 |
| R6 | GC 应用后写一条 `audit-log`（复用既有 audit-log 模块），面板显示「上次清理时间/条数」 | 3h |
| R7 | tray 的 `let _ =` 分类：可忽略的留下并注释；影响用户操作的改为「托盘气泡/日志」上报 | 0.5d |
| R8 | 只拆正在改的两个页面（Settings、Sessions），按「数据 hook / 子面板」切，不做全量重构 | 1.5d |
| R9 | 在 `Cargo.toml` 注释里加「工具链升级后先跑 `cargo check`，失败再考虑换 patch 方式」；不主动迁移 | 0 |

## 6. 本轮新核实的事实（直接改设计）

| 项 | 结论 | 依据 | 对设计的影响 |
|---|---|---|---|
| V5 账号记忆落点 | `~/.workbuddy/memory/<uid>_memory.md`（另有 `.bak`；旧拼写 `memery/<uid>_memery.md`） | 真机两账号各一份，254B | 迁移 = 复制单文件，**必须重写内嵌 `uid`** |
| V5 记忆文件格式 | Markdown 头 + `RAW_JSON_START {... "uid": ...} RAW_JSON_END` | 直读文件 | 只改 JSON 里的 `uid`，不动正文 |
| V5 身份目录 | `user-<uid>-personal/`（BOOTSTRAP/IDENTITY/SOUL/USER.md + workspace-state.json） | 真机 | **默认不迁**：属人格设定，迁过去等于改目标账号的身份 |
| V3 `agent_dirty` | 本地无 `agent_dirty = 1` 写入点，只有云端运行时状态回写与 upsert | `app.asar` | 副本置 NULL **不阻断上传**；风险降为「低」，仍需一次实机确认 |
| V4 `credit_json` | 只有建表/迁移命中，无读取路径 | `app.asar` | 复制它无害，但不进账号统计；该项可**关闭** |
| D1 现状 | `copy_linked_sessions` 是 VS Code 专用「单账号多会话」，不是跨组批量 | `session_groups.rs:765` | D1 是新增能力，需按 job 引擎重做，不能复用 |

## 7. 后续规划

### Phase 0　提交收口 + 真机冒烟（最高优先）

> **状态**：提交部分**已完成**（5 个提交：P0 / P1 / P2 / P3-10 / 规划文档）；真机冒烟**未执行**。

- **目标**：把已完成的 4 组改动变成可回滚、可发布的基线，并用真机确认没坏。
- **交付物**：4 个提交（P0 / P1 / P2 / P3-10）；`docs/smoke-batch.md` 六步执行记录（新增第 7 步：覆盖中断自愈）；冒烟发现的问题清单。
- **依赖**：2 个已保存账号、≥3 个关联组、可关闭的 WorkBuddy。
- **验收**：4 个提交各自 `cargo test -p wb-switch-core --lib` + `npm run build` 通过；冒烟六步（+第 7 步）全过，或每个失败都有明确归因。
- **风险**：冒烟需要真账号，可能暴露 R3 类问题 → 直接转入 Phase 1。

### Phase 1　假设收口 + 账号记忆迁移（P3-11）

> **状态**：**代码完成**（`account_memory.rs` 9 条单测 + 预检接口 + 弹窗勾选与覆盖二级选项 + 7 条前端用例），
> `docs/verified-assumptions.md` 已落地。**真机双向验证未执行**（见该文档末节）。
> 与计划的偏差：验收里写的「正则替换 RAW_JSON 内的 uid」改为**解析 JSON 后重写**——
> 正则会在 `memoryBlock` 里出现 `"uid"` 字样时误伤，解析失败则中止并保留原文件。

- **目标**：把「未验证」的假设变成结论；交付一个默认关闭的记忆迁移开关。
- **交付物**：
  1. `docs/verified-assumptions.md`：V1/V3/V4/V6 逐条结论（成立/不成立/不适用）；
  2. `modules/account_memory.rs`：`preview_memory_migration(src_uid,dst_uid)` + `apply_memory_migration(...,overwrite)`；
  3. 切换弹窗勾选项「同时迁移账号记忆（不含身份设定）」，默认不勾；
  4. 覆盖前备份到 `~/.wb-switch/backups/memory/<uid>-<ts>.md`。
- **关键设计**（来自 V5 证据）：
  - 只处理 `memory/<uid>_memory.md`，兼容 `memery/<uid>_memery.md`；
  - 写入时重写 `RAW_JSON` 里的 `uid` 与 `updatedAt`，同步写 `.bak`；
  - 目标已有非空 `memoryBlock` 且未勾「覆盖」→ 拒绝并提示，绝不静默覆盖；
  - **不触碰** `user-<uid>-personal/`（IDENTITY/SOUL/USER）。
- **依赖**：Phase 0 的冒烟结论（切换流程本身要先可信）。
- **验收**：单测覆盖「目标为空 / 目标非空未授权 / 目标非空已授权 / 旧拼写存在 / 内嵌 uid 被改写」；真机切换后目标账号记忆可见、源账号记忆不变。
- **风险**：记忆文件格式由客户端私有，字段变动会让重写失效 → 写入用「正则替换 RAW_JSON 内的 `"uid"` 值」，失败即中止并保留原文件。

### Phase 2　批量补副本（D1）

> **状态**：**代码完成**（`BatchOp::Copy` + 计划/执行双路径 + 路由/命令 + 前端「补副本」入口）。
> **但刻意未在真机上跑过**：它是放大写入，第一次执行应当在有人看着的情况下做，见文末验收。
> 与计划的偏差：目标账号改为**显式多选**（不选 ≠ 全选），并新增两道闸门
> `MAX_COPY_TARGETS=20`（单组账号数）与 `MAX_BATCH_COPIES=100`（组数 × 账号数）。

- **目标**：让「一个源账号的会话批量复制到 N 个目标账号」复用批量引擎的生命周期与报告。
- **设计要点**：任务单元从 `group` 变成 `(sourceMemberId, targetAccountId)`；计划阶段用 `detail.addTargets` 现算；执行阶段走 `copy_sessions_cross` 单条内核，但生命周期只进出一次；报告复用 `BatchItem`，`affected` 记「新建副本数」。
- **交付物**：`session_batch::start_copy`、`BatchOp::Copy`、3 条 HTTP/命令、前端批量栏加「补副本」入口、**上限仍是 50**（复制比同步重，建议先按 20 试）。
- **依赖**：Phase 1 完成（复制内核的真机可信度）；`add_member` 的兼容性校验（`validate_target_compatibility`）抽成可复用判定。
- **验收**：批内某组失败不影响其它组；中途取消后报告 `notRun` 与 `interruptedMidWrite` 正确；50 组上限、重复提交不产生第二份副本。
- **风险**：复制是**放大**操作（N 组 × M 账号），一次误点会写出大量副本 → 必须默认「源 = 安全来源」且执行前给出「将新建 N 份副本」的确认数字。

### Phase 3　上下文可见性收口

> **状态**：**代码完成**（`session::NOT_CARRIED` 单一来源 + 回填报告 `notCarried` + GC 留痕
> `gc-audit.jsonl` 与「上次清理」展示，前端 2 条新用例）。原计划的「一键跳转差异说明」并入
> 面板内的折叠清单——跳转要新增路由与页面，收益不抵成本。

- **目标**：让「补了什么 / 没补什么」在界面上说清楚。
- **交付物**：`notCarried` 字段（固定四项：文件快照、文件树清单、变更索引、产物索引）+ 面板常驻说明 + 一键跳转「查看差异说明」；GC 应用后写 audit-log 并在面板展示上次清理。
- **依赖**：Phase 0 的 UI 已上线。
- **验收**：面板文案与 `copy_session_sidecars` 的白名单**同源**（同一常量驱动，不允许两处各写一份）。

### Phase 4　结构性还债（按需，不阻塞发布）

- **目标**：只处理「正在被改」的部分。
- **交付物**：`SettingsPage` 拆数据 hook + 子面板；`SessionsPage` 拆选择态 hook；tray 的 `let _ =` 分类处理（可忽略加注释、影响操作的走日志/气泡）。
- **验收**：拆分后页面行数下降 ≥40%，`vitest`、`tsc`、`npm run build` 全绿，无行为变化（用现有用例作回归基线）。

### Phase 5　发布

- **交付物**：版本号 + 四位 tag、`build-exe.yml` 出 Windows NSIS、CHANGELOG 写明「副本可见性归一 / 上下文补全 / 崩溃自愈 / 批量引擎」。
- **验收**：干净环境安装包可启动、旧数据目录升级后首次启动无恢复提示、`cleanupState` 无残留。

## 8. 优先级排序与取舍

| 顺序 | 项 | 理由 |
|---|---|---|
| 1 | Phase 0 提交+冒烟 | 没有可回滚基线和真机结论，后面所有工作都在沙地上 |
| 2 | Phase 1 假设收口 + 记忆迁移 | 成本低、直接消除 R3；记忆迁移是用户可感知的价值 |
| 3 | Phase 2 批量补副本 | 价值最高但风险最大（放大写入），必须排在验证之后 |
| 4 | Phase 3 可见性收口 | 成本低，避免「同步了=完整」的误解 |
| 5 | Phase 4 还债 | 不阻塞发布，随改动顺手做 |
| 6 | Phase 5 发布 | 前面都过了再发 |

**明确不做（并说明理由）**

| 项 | 理由 |
|---|---|
| #10 `group_id` 完整改名 | 152 处引用、零功能收益，改名期间任何一次合并冲突都可能写坏关联表 |
| #7 三套内核抽 trait | 三套内核（WorkBuddy / IDE / 插件）的失败语义不同，抽象会掩盖差异；等第三个客户端真正需要时再抽 |
| #14 `vendor-patched` 迁移 | 只在旧工具链上报错，属于构建层兼容；保持现状并在注释里写明触发条件即可 |

## 9. 关键约束与依赖

| 约束 | 影响 |
|---|---|
| 写客户端数据前必须关闭客户端（目标账号当前登录时） | 批量/复制都需要用户在场并授权；不能做成后台任务 |
| 国际版数据根不同构 | 跨档能力需先跑 `session_copy_supported_at` 探测，失败要明确报错而不是半成品 |
| 单向不可逆操作（覆盖、GC） | 必须「备份 + 干跑 + 二次确认」三件套，缺一不可 |
| 客户端私有格式 | 会话正文/记忆文件格式变动会打破假设 → 每次客户端升版要跑一次 `session_doctor` |
| `vendor/agent-companion` 被构建脚本改写 | 提交前必须 `git status` 确认未混入（本轮已踩过） |
| 沙箱禁 `reg.exe` | 4 条 `windows_registry_probe_*` 恒失败，CI 与本地门禁需明确豁免说明 |

## 10. 验收标准总表（DoD）

| 维度 | 标准 |
|---|---|
| 功能 | Phase 1/2/3 各自的验收项全过；记忆迁移默认关闭且可预览 |
| 数据 | 副本 `missing ⊆ 源 missing`；GC 后无「会话行在但 file-history 缺失」的新增；备份无遗留 `cleanupPending` |
| 回归 | core lib 全绿（除 4 条沙箱项）、server 10、clippy `-D warnings`、tsc 0 错、vitest 全绿、`npm run build` |
| 可观测 | 每次写操作都能在报告里回答「写了什么、跳过了什么、为什么」 |
| 文档 | `smoke-batch.md` 有执行记录；`verified-assumptions.md` 每条假设有结论与证据；CHANGELOG 面向用户描述 |
