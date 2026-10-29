//! WorkBuddy 关联会话的批量操作引擎：计划 → 串行执行 → 进度 / 取消 → 报告落盘。
//!
//! 与单组操作的差别集中在这五处：
//!
//! 1. **先计划再执行**：计划阶段逐组读详情，得出「可执行 / 无安全来源 / 已经最新 / 读不到」，
//!    并算出本次需要关闭的档位集合；执行阶段才取预览凭据。凭据靠 binding 校验
//!    （`session_link::verify_preview`），写入后即失效，因此**不能**在计划阶段批量预生成。
//! 2. **生命周期只进出一次**：整批共用一把客户端操作锁；需要关闭的档位在批次开始前一次性
//!    关闭、结束后一次性重开（`WorkbuddyLifecycle`）。绝不允许每组各关一次。
//! 3. **错误分两类**：`Lifecycle`（拿锁失败 / 关闭客户端失败 / 写入待恢复）中止整批，剩余
//!    标记 `notRun`；`Item`（组消失、复核后不再符合）只影响该组，继续下一组。
//! 4. **进度不依赖调用方连接**：job 状态由后台线程维护，前端轮询 `progress`。桌面端走
//!    `invoke`、webui 走 HTTP，长任务绑在请求上会因断连/刷新丢结果（`api_switch` 的运行
//!    标志曾因此永久卡死）。
//! 5. **只做安全同步**：批量只执行 fastForward；覆盖目标已更新的内容必须逐组手动确认。

use serde::Serialize;
use serde_json::{json, Value};
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use crate::modules::session::{self, SessionPaths};
use crate::modules::session_backup;
use crate::modules::session_groups::{
    self, reopen_workbuddy_variants, running_target_needs_restart, workbuddy_variant_label,
    SessionClient,
};
use crate::modules::session_link;
use crate::modules::variant::WbVariant;
use crate::modules::{config, lock_or_recover, process, switch};

/// 一次批量的组数上限：再多应该先筛选，而不是把界面当批处理终端用。
pub const MAX_BATCH_GROUPS: usize = 50;
/// 保留的历史批量报告份数（落盘，供「上次批量做到哪」追溯）。
const KEEP_BATCH_REPORTS: usize = 20;
/// 判定「这份 running 报告是上一个进程留下的」的宽限期。
/// 只按时间不够——同机可能同时跑桌面端与 webui，两者都能发批量；再叠加「本进程内存里
/// 没有这个 job」才认定中断，避免把另一个进程正在跑的任务标成 interrupted。
const INTERRUPT_GRACE_MS: i64 = 90_000;

/// 单项状态。状态全部由后端写入，前端只展示、不推演。
pub const ITEM_PENDING: &str = "pending";
pub const ITEM_DONE: &str = "done";
pub const ITEM_SKIPPED: &str = "skipped";
pub const ITEM_FAILED: &str = "failed";
pub const ITEM_NOT_RUN: &str = "notRun";

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatchOp {
    /// 安全同步：只做 fastForward。
    Sync,
    /// 删除关联：只解除管理关系，不动账号里的会话内容。
    Delete,
}

impl BatchOp {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Sync => "sync",
            Self::Delete => "delete",
        }
    }
}

#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct BatchItem {
    /// 前端选择键（`client:scope:groupId`），用于结果回填。
    pub key: String,
    pub group_id: String,
    pub title: String,
    pub status: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub reason_code: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub message: Option<String>,
    /// 本组实际完成的数量（Sync 为同步成功的副本数，Delete 为解除的关联数）。
    /// 保留一版用于兼容旧前端/旧报告。
    pub synced: usize,
    /// 与 `synced` 同值：`synced` 只对同步说得通，`affected` 是动作无关的通用名。
    /// 双写一版后前端只读 `affected`，下个版本再删除 `synced`。
    pub affected: usize,
    /// 计划阶段看到的目标数（Sync 为落后副本数，Delete 为成员数）。
    pub planned: usize,
}

impl BatchItem {
    fn new(key: String, group_id: &str, title: String) -> Self {
        Self {
            key,
            group_id: group_id.to_string(),
            title,
            status: ITEM_PENDING.to_string(),
            reason_code: None,
            message: None,
            synced: 0,
            affected: 0,
            planned: 0,
        }
    }

    fn skipped(mut self, code: &str, message: impl Into<String>) -> Self {
        self.status = ITEM_SKIPPED.to_string();
        self.reason_code = Some(code.to_string());
        self.message = Some(message.into());
        self
    }

    fn failed(mut self, code: &str, message: impl Into<String>) -> Self {
        self.status = ITEM_FAILED.to_string();
        self.reason_code = Some(code.to_string());
        self.message = Some(message.into());
        self
    }

    fn done(mut self, count: usize) -> Self {
        self.status = ITEM_DONE.to_string();
        self.synced = count;
        self.affected = count;
        self
    }
}

/// 计划阶段的一项：条目本身 + 需要为它关闭的档位。
struct PlannedGroup {
    item: BatchItem,
    restart_variants: Vec<WbVariant>,
}

struct BatchJob {
    id: String,
    client: SessionClient,
    /// 档位范围：计划与执行都必须沿用它，否则「在 cn 页面上操作」会波及同 id 的其它档位。
    scope: Option<WbVariant>,
    op: BatchOp,
    allow_restart: bool,
    state: String,
    done: usize,
    current: Option<String>,
    aborted: Option<String>,
    cancelled: bool,
    cancel: Arc<AtomicBool>,
    started_at: i64,
    finished_at: Option<i64>,
    items: Vec<BatchItem>,
    /// 本次关闭过的档位（人类可读标签）；没关过就是空。
    closed_variants: Vec<String>,
    /// 中止后刻意不重开（保留现场），必须显式记录，否则「没重开也没报错」会被当成 bug。
    kept_closed: bool,
    /// 重开失败的说明；写入已完成，只作告警。
    reopen_errors: Vec<String>,
}

impl BatchJob {
    fn snapshot(&self) -> Value {
        json!({
            "jobId": self.id,
            "client": self.client.as_str(),
            "op": self.op.as_str(),
            "state": self.state,
            "total": self.items.len(),
            "done": self.done,
            "current": self.current,
            "aborted": self.aborted,
            "cancelled": self.cancelled,
            "startedAt": self.started_at,
            "finishedAt": self.finished_at,
            "lifecycle": {
                "closed": self.closed_variants,
                "keptClosed": self.kept_closed,
                "reopenErrors": self.reopen_errors,
            },
            "items": self.items,
        })
    }

    fn is_finished(&self) -> bool {
        self.state == "finished"
    }
}

static JOBS: Mutex<Option<HashMap<String, BatchJob>>> = Mutex::new(None);

fn with_jobs<T>(f: impl FnOnce(&mut HashMap<String, BatchJob>) -> T) -> T {
    let mut guard = lock_or_recover(&JOBS);
    let jobs = guard.get_or_insert_with(HashMap::new);
    f(jobs)
}

/// 取后台线程开工所需的参数（避免线程长期持锁）。
/// 后台线程开工所需的参数：客户端、档位范围、动作、`(选择键, 组 id)` 列表、
/// 是否允许关闭客户端。
type JobParams = (
    SessionClient,
    Option<WbVariant>,
    BatchOp,
    Vec<(String, String)>,
    bool,
);

fn job_params(job_id: &str) -> Option<JobParams> {
    with_jobs(|jobs| {
        let job = jobs.get(job_id)?;
        Some((
            job.client,
            job.scope,
            job.op,
            job.items
                .iter()
                .map(|item| (item.key.clone(), item.group_id.clone()))
                .collect(),
            job.allow_restart,
        ))
    })
}

/// 起一个同步批次。
///
/// `allow_restart` 为真才允许为「目标账号正是该档当前登录」的写入关闭并重开对应档位；
/// 为假时这类组会被跳过并说明原因——关闭用户正在用的客户端必须是用户按的。
pub fn start_sync(
    client: SessionClient,
    scope: Option<WbVariant>,
    group_keys: &[(String, String)],
    allow_restart: bool,
) -> Result<String, String> {
    start(client, scope, BatchOp::Sync, group_keys, allow_restart)
}

/// 起一个删除关联批次：只解除关联，不删除会话内容。
pub fn start_delete(
    client: SessionClient,
    scope: Option<WbVariant>,
    group_keys: &[(String, String)],
) -> Result<String, String> {
    start(client, scope, BatchOp::Delete, group_keys, false)
}

fn start(
    client: SessionClient,
    scope: Option<WbVariant>,
    op: BatchOp,
    group_keys: &[(String, String)],
    allow_restart: bool,
) -> Result<String, String> {
    // 本期只开放 WorkBuddy：其余客户端要各自处理编辑器/IDE 的运行门禁，
    // 混进同一个引擎会让「只关闭一次」的语义失真。明确拒绝好过做半套。
    if client != SessionClient::Workbuddy {
        return Err("批量操作当前只支持 WorkBuddy 会话".to_string());
    }
    let items = normalize_items(group_keys, client, scope)?;
    let id = uuid::Uuid::new_v4().to_string();
    let cancel = Arc::new(AtomicBool::new(false));
    let job = BatchJob {
        id: id.clone(),
        client,
        scope,
        op,
        allow_restart,
        state: "queued".to_string(),
        done: 0,
        current: None,
        aborted: None,
        cancelled: false,
        cancel: cancel.clone(),
        started_at: config::now_ms(),
        finished_at: None,
        items,
        closed_variants: Vec::new(),
        kept_closed: false,
        reopen_errors: Vec::new(),
    };
    with_jobs(|jobs| {
        prune_jobs(jobs);
        jobs.insert(id.clone(), job);
    });
    let job_id = id.clone();
    std::thread::spawn(move || {
        // 后台线程 panic 不能把 job 永远留在 running：调用方只会看到不再变化的进度。
        let outcome = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| run_job(&job_id)));
        if outcome.is_err() {
            finish_job(&job_id, Some("批量执行异常中止".to_string()), false);
        }
    });
    Ok(id)
}

fn prune_jobs(jobs: &mut HashMap<String, BatchJob>) {
    let mut finished: Vec<(i64, String)> = jobs
        .iter()
        .filter(|(_, job)| job.is_finished())
        .map(|(id, job)| (job.started_at, id.clone()))
        .collect();
    if finished.len() < KEEP_BATCH_REPORTS {
        return;
    }
    finished.sort_by_key(|(started_at, _)| *started_at);
    let remove = finished.len() - KEEP_BATCH_REPORTS + 1;
    for (_, id) in finished.into_iter().take(remove) {
        jobs.remove(&id);
    }
}

/// 前端传 `(key, groupId)`：key 用于回填选择态，groupId 用于执行。去重且保序。
fn normalize_items(
    group_keys: &[(String, String)],
    client: SessionClient,
    scope: Option<WbVariant>,
) -> Result<Vec<BatchItem>, String> {
    if group_keys.is_empty() {
        return Err("请先选择要处理的会话组".to_string());
    }
    let mut seen = std::collections::HashSet::new();
    let mut items = Vec::new();
    for (key, group_id) in group_keys {
        let group_id = group_id.trim();
        if group_id.is_empty() {
            return Err("会话组标识为空，请刷新后重试".to_string());
        }
        if !seen.insert(group_id.to_string()) {
            continue;
        }
        let key = if key.trim().is_empty() {
            format!(
                "{}:{}:{}",
                client.as_str(),
                scope.map(WbVariant::as_str).unwrap_or("all"),
                group_id
            )
        } else {
            key.clone()
        };
        items.push(BatchItem::new(key, group_id, String::new()));
    }
    if items.len() > MAX_BATCH_GROUPS {
        return Err(format!(
            "一次最多处理 {MAX_BATCH_GROUPS} 个会话组，请先筛选后再试"
        ));
    }
    Ok(items)
}

fn run_job(job_id: &str) {
    let Some((client, scope, op, targets, allow_restart)) = job_params(job_id) else {
        return;
    };
    let paths = client.store_paths();

    // 先拿「本 job 的存活锁」，再写报告。
    // 报告一旦存在就说明锁曾被持有；扫描端据此判断持有者是否已死（锁由系统在进程退出时释放），
    // 比 pid / mtime 都可靠：僵尸 PID 会被复用，单个大会话项也可能超过任何时间宽限期。
    let _job_lock = match session_link::try_lock_file(&job_lock_path(&paths, job_id)) {
        Ok(lock) => Some(lock),
        Err(session_link::LockError::Busy) => {
            finish_job(job_id, Some("该批量已在其它进程中执行".to_string()), false);
            return;
        }
        // 无法建锁（只读盘/网络盘）：降级为无锁执行，扫描端回落到时间宽限。
        Err(session_link::LockError::Unavailable(_)) => None,
    };

    set_state(job_id, "planning", None);

    // 先按「用户选了什么」铺满条目：计划阶段若被取消，未计划的组要能落成 notRun，
    // 而不是从报告里整条消失（那会让「选了 20 个、报告只有 3 个」无从解释）。
    let mut items: Vec<BatchItem> = targets
        .iter()
        .map(|(key, group_id)| BatchItem::new(key.clone(), group_id, String::new()))
        .collect();
    let mut restart_variants: Vec<WbVariant> = Vec::new();
    // 计划阶段一开始就落盘：计划要逐组读详情，可能是整批里最慢的一段。
    checkpoint(job_id);

    // ── 计划阶段：只读，不产生任何写副作用 ────────────────────────────────
    for (index, (_, group_id)) in targets.iter().enumerate() {
        if cancel_requested(job_id) {
            break;
        }
        let planned = plan_group(client, scope, group_id, op, allow_restart);
        for variant in &planned.restart_variants {
            if !restart_variants.contains(variant) {
                restart_variants.push(*variant);
            }
        }
        items[index] = planned.item;
    }
    publish_items(job_id, items.clone(), "running");
    checkpoint(job_id);
    if cancel_requested(job_id) {
        mark_remaining(&mut items, 0);
        publish_items(job_id, items, "finished");
        finish_job(job_id, None, true);
        return;
    }

    // ── 生命周期：整批只拿一次锁、只关一次客户端 ──────────────────────────
    let lock = match session_link::try_acquire_client_ops_lock(&paths) {
        Ok(lock) => lock,
        Err(error) => {
            mark_remaining(&mut items, 0);
            publish_items(job_id, items, "finished");
            finish_job(
                job_id,
                Some(format!("其它会话操作正在进行，本次未开始：{error}")),
                false,
            );
            return;
        }
    };
    let mut lifecycle = match WorkbuddyLifecycle::start(restart_variants, allow_restart) {
        Ok(lifecycle) => lifecycle,
        Err(error) => {
            mark_remaining(&mut items, 0);
            publish_items(job_id, items, "finished");
            drop(lock);
            finish_job(job_id, Some(error), false);
            return;
        }
    };

    // ── 执行阶段：逐组现场取凭据；串行，组间可取消 ────────────────────────
    let mut aborted: Option<String> = None;
    for index in 0..items.len() {
        if items[index].status != ITEM_PENDING {
            continue;
        }
        if cancel_requested(job_id) {
            mark_remaining(&mut items, index);
            break;
        }
        set_state(job_id, "running", Some(items[index].group_id.clone()));
        // 先落一次「正在处理第 N 组」：进程死在这一项中途时，扫描端才能把它标成
        // 「可能已部分写入」，而不是和没碰过的组一样显示 pending。
        checkpoint(job_id);
        let item = items[index].clone();
        match execute_item(client, scope, op, &item.group_id) {
            Ok(ItemOutcome::Done(count)) => items[index] = item.done(count),
            Ok(ItemOutcome::Skipped(code, message)) => items[index] = item.skipped(code, message),
            Ok(ItemOutcome::Failed(code, message, fatal)) => {
                let title = display_title(&items[index]);
                items[index] = item.failed(code, message);
                if fatal {
                    aborted = Some(format!("「{title}」写入待恢复，已停止后续组并保留现场"));
                }
            }
            Err(error) => items[index] = item.failed("itemFailed", error),
        }
        publish_items(job_id, items.clone(), "running");
        checkpoint(job_id);
        if aborted.is_some() {
            mark_remaining(&mut items, index + 1);
            break;
        }
    }

    // ── 收尾：存在待恢复写入时不重开客户端（重开会让未恢复的写入被覆盖） ──
    if aborted.is_none() {
        for variant in lifecycle.closed_variants() {
            match session::recover_pending_session_operations(variant) {
                Ok(recovery) if !switch::recovery_blocks_startup(&recovery) => {}
                Ok(recovery) => {
                    aborted = Some(format!(
                        "{}仍有待恢复的会话写入（{}），已暂停重新打开客户端",
                        workbuddy_variant_label(variant),
                        switch::recovery_blocking_detail(&recovery)
                    ));
                    break;
                }
                Err(error) => {
                    aborted = Some(format!(
                        "{}无法检查会话恢复状态（{error}），已暂停重新打开客户端",
                        workbuddy_variant_label(variant)
                    ));
                    break;
                }
            }
        }
    }
    if aborted.is_some() {
        lifecycle.keep_closed();
    }

    // 先把「关过谁 / 是否故意没重开 / 重开失败」记进报告，再真正重开：
    // 重开失败只作告警，写入已经完成，不该把整批判成失败。
    let closed_labels = lifecycle.closed_labels();
    let kept_closed = lifecycle.kept_closed();
    let reopen_errors = lifecycle.finish();
    set_lifecycle(job_id, closed_labels, kept_closed, reopen_errors);

    let cancelled = cancel_requested(job_id);
    publish_items(job_id, items.clone(), "finished");
    drop(lifecycle); // Drop 兜底重开，panic / 提前 return 也不会把客户端留在关闭状态
    drop(lock);
    finish_job(job_id, aborted, cancelled);
}

enum ItemOutcome {
    Done(usize),
    Skipped(&'static str, String),
    /// (原因码, 可读消息, 是否中止整批)
    Failed(&'static str, String, bool),
}

fn execute_item(
    client: SessionClient,
    scope: Option<WbVariant>,
    op: BatchOp,
    group_id: &str,
) -> Result<ItemOutcome, String> {
    match op {
        BatchOp::Sync => {
            let report = session_groups::sync_safe_batch(client, scope, group_id, false)?;
            Ok(summarize_sync_report(&report))
        }
        BatchOp::Delete => {
            let report = session_groups::delete_group(client, scope, group_id)?;
            let removed = report.get("removed").and_then(Value::as_u64).unwrap_or(0) as usize;
            Ok(ItemOutcome::Done(removed))
        }
    }
}

/// 单组同步报告 → 结论。`needsRecovery` 是唯一的中止信号。
fn summarize_sync_report(report: &Value) -> ItemOutcome {
    if report
        .get("needsRecovery")
        .and_then(Value::as_bool)
        .unwrap_or(false)
    {
        return ItemOutcome::Failed(
            "needsRecovery",
            "本次写入需要恢复，已保留现场".to_string(),
            true,
        );
    }
    let errors = report
        .get("errors")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();
    if let Some(first) = errors.first() {
        let message = first
            .get("error")
            .and_then(Value::as_str)
            .unwrap_or("同步失败")
            .to_string();
        return ItemOutcome::Failed("itemFailed", message, false);
    }
    let synced = report
        .get("synced")
        .and_then(Value::as_array)
        .map_or(0, Vec::len);
    if synced > 0 {
        return ItemOutcome::Done(synced);
    }
    ItemOutcome::Skipped(
        "recheckNotFastForward",
        "重新检查后不再属于安全快进范围".to_string(),
    )
}

/// 计划阶段：读详情并判定这一组「能不能做」。只读。
fn plan_group(
    client: SessionClient,
    scope: Option<WbVariant>,
    group_id: &str,
    op: BatchOp,
    allow_restart: bool,
) -> PlannedGroup {
    let key = format!(
        "{}:{}:{}",
        client.as_str(),
        scope.map(WbVariant::as_str).unwrap_or("all"),
        group_id
    );
    match session_groups::detail(client, scope, group_id) {
        Ok(detail) => {
            plan_from_detail(key, group_id, &detail, op, allow_restart, |variant, uid| {
                running_target_needs_restart(variant, uid)
            })
        }
        Err(error) => PlannedGroup {
            item: BatchItem::new(key, group_id, String::new()).failed("detailUnavailable", error),
            restart_variants: Vec::new(),
        },
    }
}

/// 纯判定：`detail` JSON + 「目标是否正在使用」探针 → 这一组该做什么。
///
/// 与 IO 分离后，判定分支（无安全源 / 已最新 / 目标在用未授权 / 需要关客户端 /
/// 可快进）可以用固定夹具直接覆盖，不再依赖真实存储。
fn plan_from_detail(
    key: String,
    group_id: &str,
    detail: &Value,
    op: BatchOp,
    allow_restart: bool,
    is_target_busy: impl Fn(WbVariant, &str) -> bool,
) -> PlannedGroup {
    let title = detail
        .get("title")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    let members = detail
        .get("members")
        .and_then(Value::as_array)
        .map(Vec::as_slice)
        .unwrap_or_default();

    if op == BatchOp::Delete {
        let mut item = BatchItem::new(key, group_id, title);
        item.planned = members.len();
        return PlannedGroup {
            item,
            restart_variants: Vec::new(),
        };
    }

    let source = detail
        .get("safeSourceMemberId")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .to_string();
    if source.is_empty() {
        return PlannedGroup {
            item: BatchItem::new(key, group_id, title)
                .skipped("noSafeSource", "该组没有可确认的安全同步来源"),
            restart_variants: Vec::new(),
        };
    }
    let behind: Vec<&Value> = members
        .iter()
        .filter(|member| {
            member.get("memberId").and_then(Value::as_str) != Some(source.as_str())
                && member.get("versionStatus").and_then(Value::as_str) == Some("behind")
        })
        .collect();
    if behind.is_empty() {
        return PlannedGroup {
            item: BatchItem::new(key, group_id, title)
                .skipped("alreadyLatest", "没有落后的副本，无需同步"),
            restart_variants: Vec::new(),
        };
    }
    // 成员缺 `variant` 时用组级档位回退：旧存储只有组级 variant，
    // 直接 `parse(None)` 会一律落到国内版，可能误关错的客户端。
    let group_variant = WbVariant::parse(detail.get("groupVariant").and_then(Value::as_str));
    let mut restart_variants: Vec<WbVariant> = Vec::new();
    for member in &behind {
        let variant = member
            .get("variant")
            .and_then(Value::as_str)
            .map(|raw| WbVariant::parse(Some(raw)))
            .unwrap_or(group_variant);
        let Some(uid) = member.get("uid").and_then(Value::as_str) else {
            continue;
        };
        if is_target_busy(variant, uid) && !restart_variants.contains(&variant) {
            restart_variants.push(variant);
        }
    }
    if !allow_restart && !restart_variants.is_empty() {
        // 这些组的目标账号正是该档当前登录、且客户端在运行：不授权关闭就只能跳过。
        // 执行时才报错会让用户以为「批量坏了」，而真正的原因是缺一次授权。
        return PlannedGroup {
            item: BatchItem::new(key, group_id, title).skipped(
                "appRunning",
                "目标账号正在 WorkBuddy 中使用：勾选「允许关闭并重开」后可同步",
            ),
            restart_variants,
        };
    }
    let mut item = BatchItem::new(key, group_id, title);
    item.planned = behind.len();
    PlannedGroup {
        item,
        restart_variants,
    }
}

/// 关闭整批需要关闭的档位，并保证离开作用域时重开。
struct WorkbuddyLifecycle {
    closed: Vec<WbVariant>,
    keep_closed: bool,
}

impl WorkbuddyLifecycle {
    fn start(variants: Vec<WbVariant>, allow_restart: bool) -> Result<Self, String> {
        let mut lifecycle = Self {
            closed: Vec::new(),
            keep_closed: false,
        };
        if variants.is_empty() || !allow_restart {
            return Ok(lifecycle);
        }
        for variant in variants {
            if let Err(error) = process::close_workbuddy(variant, 20) {
                let reopen_errors = reopen_workbuddy_variants(&lifecycle.closed);
                lifecycle.closed.clear();
                let suffix = if reopen_errors.is_empty() {
                    String::new()
                } else {
                    format!(
                        "；此前关闭的客户端重新打开失败：{}",
                        reopen_errors.join("；")
                    )
                };
                return Err(format!(
                    "关闭 {}失败：{error}{suffix}",
                    workbuddy_variant_label(variant)
                ));
            }
            lifecycle.closed.push(variant);
        }
        Ok(lifecycle)
    }

    fn closed_variants(&self) -> Vec<WbVariant> {
        self.closed.clone()
    }

    fn closed_labels(&self) -> Vec<String> {
        self.closed
            .iter()
            .map(|variant| workbuddy_variant_label(*variant).to_string())
            .collect()
    }

    fn kept_closed(&self) -> bool {
        self.keep_closed
    }

    fn keep_closed(&mut self) {
        self.keep_closed = true;
    }

    /// 显式收尾：重开并交回失败原因；清空 closed 后 Drop 不会再重开一次。
    fn finish(&mut self) -> Vec<String> {
        if self.closed.is_empty() {
            return Vec::new();
        }
        let closed = std::mem::take(&mut self.closed);
        if self.keep_closed {
            return Vec::new();
        }
        reopen_workbuddy_variants(&closed)
    }
}

impl Drop for WorkbuddyLifecycle {
    fn drop(&mut self) {
        if self.closed.is_empty() {
            return;
        }
        let closed = std::mem::take(&mut self.closed);
        if self.keep_closed {
            return;
        }
        let errors = reopen_workbuddy_variants(&closed);
        if !errors.is_empty() {
            // 重开失败不影响写入结果，但必须留痕：用户会以为客户端是自己崩的。
            eprintln!("[batch] 重新打开客户端失败：{}", errors.join("；"));
        }
    }
}

fn display_title(item: &BatchItem) -> String {
    if item.title.trim().is_empty() {
        item.group_id.clone()
    } else {
        item.title.clone()
    }
}

fn mark_remaining(items: &mut [BatchItem], from: usize) {
    for item in items.iter_mut().skip(from) {
        if item.status == ITEM_PENDING {
            item.status = ITEM_NOT_RUN.to_string();
        }
    }
}

fn cancel_requested(job_id: &str) -> bool {
    with_jobs(|jobs| {
        jobs.get(job_id)
            .map(|job| job.cancel.load(Ordering::SeqCst))
            .unwrap_or(false)
    })
}

fn set_state(job_id: &str, state: &str, current: Option<String>) {
    with_jobs(|jobs| {
        if let Some(job) = jobs.get_mut(job_id) {
            job.state = state.to_string();
            job.current = current;
        }
    });
}

fn publish_items(job_id: &str, items: Vec<BatchItem>, state: &str) {
    with_jobs(|jobs| {
        if let Some(job) = jobs.get_mut(job_id) {
            job.done = items
                .iter()
                .filter(|item| item.status != ITEM_PENDING && item.status != ITEM_NOT_RUN)
                .count();
            job.items = items;
            job.state = state.to_string();
            if state == "finished" {
                job.current = None;
            }
        }
    });
}

/// 把当前内存态落盘：不改变状态，只为「进程被强杀后仍能回答做到哪」。
fn checkpoint(job_id: &str) {
    let snapshot = with_jobs(|jobs| jobs.get(job_id).map(BatchJob::snapshot));
    if let Some(snapshot) = snapshot {
        persist_report(job_id, &snapshot);
    }
}

fn set_lifecycle(job_id: &str, closed: Vec<String>, kept_closed: bool, reopen_errors: Vec<String>) {
    with_jobs(|jobs| {
        if let Some(job) = jobs.get_mut(job_id) {
            job.closed_variants = closed;
            job.kept_closed = kept_closed;
            job.reopen_errors = reopen_errors;
        }
    });
}

fn finish_job(job_id: &str, aborted: Option<String>, cancelled: bool) {
    let snapshot = with_jobs(|jobs| {
        let job = jobs.get_mut(job_id)?;
        job.state = "finished".to_string();
        job.current = None;
        job.aborted = aborted.clone();
        job.cancelled = cancelled;
        job.finished_at = Some(config::now_ms());
        Some(job.snapshot())
    });
    if let Some(snapshot) = snapshot {
        persist_report(job_id, &snapshot);
        // 保留策略只在收尾做一次：检查点每项都扫目录纯属浪费。
        prune_reports(&batch_dir(&SessionPaths::for_variant(WbVariant::Cn)));
    }
}

fn batch_dir(paths: &SessionPaths) -> std::path::PathBuf {
    paths.session_links_dir().join("batches")
}

/// 每个 job 一把存活锁：持有者活着 → Busy；进程退出 → 系统释放，扫描端即可接手。
fn job_lock_path(paths: &SessionPaths, job_id: &str) -> std::path::PathBuf {
    batch_dir(paths).join(format!("batch-{job_id}.lock"))
}

/// 报告落盘：进程重启后仍能回答「上次批量做到哪、哪些失败」。
fn persist_report(job_id: &str, snapshot: &Value) {
    let paths = SessionPaths::for_variant(WbVariant::Cn);
    let dir = batch_dir(&paths);
    if std::fs::create_dir_all(&dir).is_err() {
        return;
    }
    let Ok(content) = serde_json::to_string_pretty(snapshot) else {
        return;
    };
    let path = dir.join(format!("batch-{job_id}.json"));
    let _ = session_backup::durable_write_str(&path, &content);
}

fn prune_reports(dir: &Path) {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return;
    };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let stamp = entry.metadata().ok()?.modified().ok()?;
            Some((stamp, entry.path()))
        })
        .collect();
    if files.len() <= KEEP_BATCH_REPORTS {
        return;
    }
    files.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    for (_, path) in files.into_iter().skip(KEEP_BATCH_REPORTS) {
        let _ = std::fs::remove_file(path);
    }
}

/// 把上一个进程留下的 running 报告标成 interrupted，返回改动条数。
///
/// 判定三条同时成立：文件是 `running`、本进程内存里没有这个 job、且开始时间早于宽限期。
/// 三联判定是为了避开「同机桌面端 + webui 各跑一个批量」这种正常并发。
pub fn mark_interrupted_batches() -> usize {
    let paths = SessionPaths::for_variant(WbVariant::Cn);
    let live: HashSet<String> = with_jobs(|jobs| jobs.keys().cloned().collect());
    mark_interrupted_in(&batch_dir(&paths), &live, config::now_ms())
}

/// [`mark_interrupted_batches`] 的可注入实现：目录、活跃集合与时间都由调用方给，
/// 便于用临时目录覆盖「旧任务被标记 / 新任务不受影响 / 已结束不动」三类情形。
fn mark_interrupted_in(dir: &Path, live: &HashSet<String>, now: i64) -> usize {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return 0;
    };
    let mut changed = 0;
    for entry in entries.flatten() {
        let path = entry.path();
        if path.extension().is_none_or(|ext| ext != "json") {
            continue;
        }
        let Ok(text) = std::fs::read_to_string(&path) else {
            continue;
        };
        let Ok(mut value) = serde_json::from_str::<Value>(&text) else {
            continue;
        };
        // 三个阶段都算「未完成」：报告是在拿到 job 锁之后才写的，
        // 所以 planning 期间被杀同样会留下文件。
        if !matches!(
            value.get("state").and_then(Value::as_str),
            Some("queued") | Some("planning") | Some("running")
        ) {
            continue;
        }
        let job_id = value
            .get("jobId")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .to_string();
        if job_id.is_empty() || live.contains(&job_id) {
            continue;
        }
        if !batch_owner_is_gone(dir, &job_id, &path, now) {
            continue;
        }
        // 中断时把「还没跑到的」与「正在写的那一组」区分开：
        // 全都留在 pending 会让人以为它们没被碰过，而正在写的那组可能已经产生
        // operation 与备份，需要按恢复提示处理。
        let current = value
            .get("current")
            .and_then(Value::as_str)
            .map(str::to_string);
        if let Some(items) = value.get_mut("items").and_then(Value::as_array_mut) {
            for item in items.iter_mut() {
                if item.get("status").and_then(Value::as_str) != Some(ITEM_PENDING) {
                    continue;
                }
                let is_current = current.as_deref() == item.get("groupId").and_then(Value::as_str);
                item["status"] = json!(ITEM_NOT_RUN);
                if is_current {
                    item["reasonCode"] = json!("interruptedMidWrite");
                    item["message"] =
                        json!("中断时正在处理这一组，可能已部分写入；请先查看恢复提示再重试");
                } else {
                    item["reasonCode"] = json!("interrupted");
                    item["message"] = json!("批量被中断，这一组还没开始处理");
                }
            }
        }
        value["state"] = json!("interrupted");
        value["interruptedAt"] = json!(now);
        value["current"] = Value::Null;
        let content = serde_json::to_string_pretty(&value).unwrap_or_default();
        if session_backup::durable_write_str(&path, &content).is_ok() {
            changed += 1;
        }
    }
    changed
}

/// 判断这份报告的持有者是否已经不在：
/// - 能拿到 job 锁 → 持有者已死（锁由系统在进程退出时释放）；
/// - 锁被占 → 还活着，不能动；
/// - 建不了锁（只读盘/网络盘）→ 退回到「文件超过宽限期没被检查点更新」。
fn batch_owner_is_gone(dir: &Path, job_id: &str, report: &Path, now: i64) -> bool {
    let lock_path = dir.join(format!("batch-{job_id}.lock"));
    match session_link::try_lock_file(&lock_path) {
        // 拿到锁即说明没人持有；探针自己持有的锁随作用域释放。
        Ok(_lock) => true,
        Err(session_link::LockError::Busy) => false,
        Err(session_link::LockError::Unavailable(_)) => report
            .metadata()
            .and_then(|meta| meta.modified())
            .ok()
            .and_then(|stamp| stamp.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| now.saturating_sub(since.as_millis() as i64) >= INTERRUPT_GRACE_MS)
            .unwrap_or(false),
    }
}

/// 进度查询：未结束返回 `running`；结束后带 `aborted` / `cancelled` 与逐项状态。
pub fn progress(job_id: &str) -> Result<Value, String> {
    let live = with_jobs(|jobs| jobs.get(job_id).map(BatchJob::snapshot));
    if let Some(snapshot) = live {
        return Ok(snapshot);
    }
    // 进程重启（或内存清理）之后仍要能回答「那次批量做到哪」：回落到落盘报告。
    // job_id 只接受 uuid 形态，避免被拼进路径做穿越。
    if valid_batch_id(job_id) {
        let paths = SessionPaths::for_variant(WbVariant::Cn);
        let path = batch_dir(&paths).join(format!("batch-{job_id}.json"));
        if let Ok(text) = std::fs::read_to_string(path) {
            if let Ok(value) = serde_json::from_str::<Value>(&text) {
                if value.get("jobId").and_then(Value::as_str) == Some(job_id) {
                    return Ok(value);
                }
            }
        }
    }
    Err("找不到该批量任务（可能已过期），请刷新后重试".to_string())
}

/// 只接受 uuid 形态的 job id（36 位、仅十六进制与连字符）。
fn valid_batch_id(job_id: &str) -> bool {
    job_id.len() == 36 && job_id.chars().all(|ch| ch.is_ascii_hexdigit() || ch == '-')
}

/// 取消：只在组与组之间生效——单组写入不可中断，硬断会留下需要恢复的现场。
pub fn cancel(job_id: &str) -> Result<Value, String> {
    with_jobs(|jobs| {
        let job = jobs
            .get(job_id)
            .ok_or_else(|| "找不到该批量任务（可能已过期），请刷新后重试".to_string())?;
        if job.is_finished() {
            return Ok(json!({ "jobId": job_id, "state": job.state, "alreadyFinished": true }));
        }
        job.cancel.store(true, Ordering::SeqCst);
        Ok(json!({ "jobId": job_id, "state": job.state, "cancelRequested": true }))
    })
}

/// 最近的批量报告，供界面提示「上次批量做到哪」。
pub fn recent_reports(limit: usize) -> Value {
    let paths = SessionPaths::for_variant(WbVariant::Cn);
    json!(read_reports(&batch_dir(&paths), limit))
}

/// 最近一份「没跑完」的报告：中断，或含失败 / 未执行项；没有则 `null`。
///
/// 由后端挑而不是让前端取「最新一份」：最新那份很可能是一次成功的小批量，
/// 而真正要提示的是更早那次没跑完的。
pub fn latest_incomplete_report() -> Value {
    let paths = SessionPaths::for_variant(WbVariant::Cn);
    read_reports(&batch_dir(&paths), KEEP_BATCH_REPORTS)
        .into_iter()
        .find(is_incomplete_report)
        .unwrap_or(Value::Null)
}

fn is_incomplete_report(report: &Value) -> bool {
    if report.get("state").and_then(Value::as_str) == Some("interrupted") {
        return true;
    }
    report
        .get("items")
        .and_then(Value::as_array)
        .map(|items| {
            items.iter().any(|item| {
                matches!(
                    item.get("status").and_then(Value::as_str),
                    Some(ITEM_FAILED) | Some(ITEM_NOT_RUN)
                )
            })
        })
        .unwrap_or(false)
}

/// 读报告目录（按修改时间倒序，最新的在前）。
fn read_reports(dir: &Path, limit: usize) -> Vec<Value> {
    let Ok(entries) = std::fs::read_dir(dir) else {
        return Vec::new();
    };
    let mut files: Vec<(std::time::SystemTime, std::path::PathBuf)> = entries
        .flatten()
        .filter(|entry| entry.path().extension().is_some_and(|ext| ext == "json"))
        .filter_map(|entry| {
            let stamp = entry.metadata().ok()?.modified().ok()?;
            Some((stamp, entry.path()))
        })
        .collect();
    files.sort_by_key(|entry| std::cmp::Reverse(entry.0));
    files
        .into_iter()
        .take(limit.min(KEEP_BATCH_REPORTS))
        .filter_map(|(_, path)| {
            let text = std::fs::read_to_string(path).ok()?;
            serde_json::from_str::<Value>(&text).ok()
        })
        .collect()
}

#[cfg(test)]
mod tests;
