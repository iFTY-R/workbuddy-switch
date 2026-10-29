//! 会话上下文完整性：自检、回填、孤儿回收。
//!
//! 客户端打开一个会话时读的不只是正文：
//! - `file-history/<sessionId>/<hash>@vN`：每一步的文件备份。正文里
//!   `snapshot.trackedFileBackups[*].backupFileName` 存的是 `<hash>@vN`，
//!   目录名才是会话 id（客户端 `PathMapper` 就是这么拼的）；
//! - `projects/<workspace>/<sessionId>/tool-results/*`：超长工具输出。正文里存的是
//!   **绝对路径**，所以这一份缺了就是死链（`<persisted-output>` 指向不存在的文件）；
//! - `todos/<sessionId>.json`：计划清单；
//! - `tasks/<sessionId>/<taskId>.json`：任务清单（**目录**，不是 `<sessionId>.json`）；
//! - `<sessionId>.file-rollback.ndjson`：与正文并排的回滚日志；
//! - `session_usage` 行：`used` 是当前上下文占用、`size` 是窗口。
//!
//! 复制 / 同步漏搬其中任何一处，正文还在但引用悬空——用户看到的就是
//! 「会话关联了、上下文不对」。本模块只做三件事：**量出来**、**补齐**、**清孤儿**。
//!
//! 明确不在自检范围内（体积与用途都不适合跟着副本走，缺了不影响会话打开）：
//! `workspace/sessions/<sessionId>/`（文件快照，实测可达数 GB）、
//! `file-tree-manifests/`、`changes-index|changes-detail/`、`artifact-index/`。
//! 这份清单只写在 [`session::NOT_CARRIED`] 一处，界面文案由它驱动。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use serde::Serialize;
use serde_json::{json, Value};

use crate::modules::config;
use crate::modules::session::{self, SessionPaths};
use crate::modules::session_groups::{self, SessionClient};
use crate::modules::session_link::{self, StoreState};
use crate::modules::variant::WbVariant;

/// 孤儿目录的最小存活时间：刚复制出来的目录可能还没有数据库行（写入尚未提交），
/// 太早回收会误删正在进行中的副本。
const ORPHAN_GRACE_MS: i64 = 7 * 24 * 60 * 60 * 1000;

/// 一次自检的结果。只描述事实，不做判断。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct ContextIntegrity {
    pub session_id: String,
    /// 正文里引用的备份文件名（去重）。
    pub referenced: usize,
    /// 其中在 `file-history/<sessionId>/` 下真实存在的数量。
    pub present: usize,
    /// **真缺**的引用数：连这个文件的任何版本都不在目录里。
    pub missing: usize,
    /// 已被同一文件的新版本取代的引用数（`<hash>@v1` 不在、但 `<hash>@v9` 在）。
    ///
    /// 客户端对内容寻址的备份只保留较新版本，正文却会保留每一步的引用。把这类算成
    /// 「缺失」会让一个完全正常的会话显示成缺上百个文件——真机实测某会话 199 个引用
    /// 里 161 个属于这一类，而它只需要最近版本就能正常打开。
    pub pruned: usize,
    /// 缺失样例（最多 5 个），便于人工核对。
    pub missing_samples: Vec<String>,
    /// 被取代的样例（最多 5 个），用于确认「不是复制丢了，是客户端清理了旧版本」。
    pub pruned_samples: Vec<String>,
    /// 是否已有 `file-history/<sessionId>/` 目录。
    pub file_history_dir: bool,
    /// 是否有 `projects/<workspace>/<sessionId>/tool-results/`（正文以绝对路径引用）。
    pub tool_results: bool,
    pub todos: bool,
    pub tasks: bool,
    /// 是否有回滚日志 `<sessionId>.file-rollback.ndjson`。
    pub rollback: bool,
    pub usage: bool,
}

/// 按客户端解析数据根后自检（供命令层调用，避免把 `store_paths` 的可见性外扩）。
pub fn integrity_for(client: SessionClient, session_id: &str) -> Result<ContextIntegrity, String> {
    let paths = client.store_paths();
    if paths.data_root.as_os_str().is_empty() {
        return Err("该客户端不支持会话上下文自检".to_string());
    }
    context_integrity(&paths, session_id)
}

/// 按客户端解析数据根后回收孤儿目录。
pub fn gc_for(client: SessionClient, apply: bool) -> Result<Value, String> {
    let paths = client.store_paths();
    if paths.data_root.as_os_str().is_empty() {
        return Err("该客户端不支持会话上下文回收".to_string());
    }
    Ok(gc_orphan_sidecars(&paths, apply))
}

/// 自检一个会话的上下文完整性（只读，不产生任何写入）。
pub fn context_integrity(
    paths: &SessionPaths,
    session_id: &str,
) -> Result<ContextIntegrity, String> {
    let body = session::find_project_jsonl(paths, session_id)
        .ok_or_else(|| format!("找不到会话正文：{session_id}"))?;
    let referenced = referenced_backups(&body)?;

    let history_dir = paths.data_root.join("file-history").join(session_id);
    // 目录里已有哪些「文件哈希」：用来区分「这个文件彻底不在」与「只是这一版被清理」。
    let present_hashes = hash_index(&history_dir);
    let mut present = 0;
    let mut pruned = 0;
    let mut missing_samples = Vec::new();
    let mut pruned_samples = Vec::new();
    for name in &referenced {
        if is_plain_file_name(name) && history_dir.join(name).is_file() {
            present += 1;
        } else if is_plain_file_name(name) && present_hashes.contains(backup_hash(name)) {
            // 同一文件还有别的版本在：客户端只保留较新版本，这一版属于正常清理。
            pruned += 1;
            if pruned_samples.len() < 5 {
                pruned_samples.push(name.clone());
            }
        } else if missing_samples.len() < 5 {
            missing_samples.push(name.clone());
        }
    }
    let referenced_count = referenced.len();
    // 工作区目录名随 cwd 的编码而变，按正文所在目录反推，不猜编码规则。
    let projects = paths.projects_dir();
    let workspace_session = body
        .parent()
        .filter(|parent| *parent != projects.as_path())
        .map(|parent| parent.join(session_id));
    Ok(ContextIntegrity {
        session_id: session_id.to_string(),
        referenced: referenced_count,
        present,
        // `referenced = present + pruned + missing`：真缺的只有第三类。
        missing: referenced_count
            .saturating_sub(present)
            .saturating_sub(pruned),
        pruned,
        missing_samples,
        pruned_samples,
        file_history_dir: history_dir.is_dir(),
        tool_results: workspace_session
            .as_ref()
            .is_some_and(|dir| dir.join("tool-results").is_dir()),
        todos: paths
            .data_root
            .join("todos")
            .join(format!("{session_id}.json"))
            .is_file(),
        // 任务清单是**目录**：`tasks/<sessionId>/<taskId>.json`。
        tasks: paths.data_root.join("tasks").join(session_id).is_dir(),
        rollback: workspace_session.as_ref().is_some_and(|dir| {
            dir.with_file_name(format!("{session_id}.file-rollback.ndjson"))
                .is_file()
        }),
        usage: usage_row_exists(paths, session_id),
    })
}

/// `file-history/<sessionId>/` 里已经存在的「文件哈希」集合。
///
/// 备份文件名是内容寻址的 `<hash>@v<N>`：同一个文件的不同版本共享 `<hash>`，
/// 客户端只保留较新的几版。有这份索引才能分清「这个文件彻底不在」与
/// 「只是这一版被清理」——两者对用户的意义完全不同。
fn hash_index(dir: &Path) -> BTreeSet<String> {
    let mut set = BTreeSet::new();
    let Ok(entries) = std::fs::read_dir(dir) else {
        return set;
    };
    for entry in entries.flatten() {
        let name = entry.file_name().to_string_lossy().to_string();
        set.insert(backup_hash(&name).to_string());
    }
    set
}

/// `<hash>@v<N>` → `<hash>`；没有 `@` 时整名当哈希（保守：不会把别的文件认成同一份）。
fn backup_hash(name: &str) -> &str {
    name.split('@').next().unwrap_or(name)
}

/// 扫出正文里引用到的备份文件名（`trackedFileBackups[*].backupFileName`）。
/// 按行解析：一行损坏只跳过该行，不让整次自检失败——自检的用途是给人看现场，
/// 不是校验正文合法性（那是 `normalize_jsonl` 的职责）。
fn referenced_backups(body: &Path) -> Result<BTreeSet<String>, String> {
    let text = std::fs::read_to_string(body).map_err(|error| format!("正文读取失败：{error}"))?;
    let mut names = BTreeSet::new();
    for line in text.lines() {
        let line = line.trim();
        if line.is_empty() || !line.contains("trackedFileBackups") {
            continue;
        }
        let Ok(value) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        let Some(backups) = value
            .get("snapshot")
            .and_then(|snapshot| snapshot.get("trackedFileBackups"))
            .and_then(Value::as_object)
        else {
            continue;
        };
        for info in backups.values() {
            if let Some(name) = info.get("backupFileName").and_then(Value::as_str) {
                names.insert(name.to_string());
            }
        }
    }
    Ok(names)
}

/// 备份文件名必须是单段文件名：拼进路径前挡住 `..` 与分隔符。
fn is_plain_file_name(name: &str) -> bool {
    !name.is_empty() && name.len() <= 128 && !name.contains(['/', '\\']) && !name.contains("..")
}

fn usage_row_exists(paths: &SessionPaths, session_id: &str) -> bool {
    let db = paths.workbuddy_db();
    if !db.is_file() {
        return false;
    }
    let Some(conn) = session::open_db(&db, true) else {
        return false;
    };
    let has_table: bool = conn
        .query_row(
            "SELECT 1 FROM sqlite_master WHERE type = 'table' AND name = 'session_usage'",
            [],
            |_| Ok(true),
        )
        .unwrap_or(false);
    if !has_table {
        return false;
    }
    conn.query_row(
        "SELECT 1 FROM session_usage WHERE session_id = ?1",
        [session_id],
        |_| Ok(true),
    )
    .unwrap_or(false)
}

/// 对若干关联组执行旁路回填：以组内「安全来源」（`safeSourceMemberId`，与界面
/// 「统一内容」用的是同一个判据）为源，把 `file-history/todos/tasks` 补到其他成员名下。
///
/// `apply = false` 时只汇报每个目标当前缺多少引用，不写任何文件。
pub fn backfill_group_context(
    client: SessionClient,
    scope: Option<WbVariant>,
    group_ids: &[String],
    apply: bool,
) -> Result<Value, String> {
    let paths = client.store_paths();
    if paths.data_root.as_os_str().is_empty() {
        return Err("该客户端不支持会话上下文回填".to_string());
    }
    let mut groups: Vec<Value> = Vec::new();
    let mut copied_total = 0_u64;
    let mut warnings: Vec<String> = Vec::new();

    for group_id in group_ids {
        let detail = session_groups::detail(client, scope, group_id)?;
        let source_member_id = detail
            .get("safeSourceMemberId")
            .and_then(Value::as_str)
            .map(str::to_string)
            // 检查与补齐共用这条路径：文案不能只对「补齐」成立，否则用户点「检查」也会看到
            // 「无法回填」，读起来像操作失败而不是「这个组没有可信来源」。
            .ok_or_else(|| format!("组 {group_id} 没有可确认的安全来源，无法检查或补齐上下文"))?;
        let members = detail
            .get("members")
            .and_then(Value::as_array)
            .cloned()
            .unwrap_or_default();
        let Some(source) = members.iter().find(|member| {
            member.get("memberId").and_then(Value::as_str) == Some(source_member_id.as_str())
        }) else {
            return Err(format!("组 {group_id} 的来源成员不在成员列表里"));
        };
        let source_session = source
            .get("sessionId")
            .and_then(Value::as_str)
            .unwrap_or_default();
        if source_session.is_empty() {
            return Err(format!("组 {group_id} 的来源成员缺少会话 id"));
        }
        let source_paths = SessionPaths::for_variant(member_variant(source));

        let mut targets: Vec<Value> = Vec::new();
        for member in &members {
            let member_id = member.get("memberId").and_then(Value::as_str).unwrap_or("");
            if member_id == source_member_id {
                continue;
            }
            let target_session = member
                .get("sessionId")
                .and_then(Value::as_str)
                .unwrap_or("");
            if target_session.is_empty() {
                continue;
            }
            let target_paths = SessionPaths::for_variant(member_variant(member));
            if apply {
                let result = session::copy_session_sidecars(
                    &source_paths,
                    &target_paths,
                    source_session,
                    target_session,
                );
                copied_total += result.copied.len() as u64;
                warnings.extend(result.warnings.iter().cloned());
                targets.push(json!({
                    "memberId": member_id,
                    "sessionId": target_session,
                    "copied": result.copied,
                    "warnings": result.warnings,
                }));
            } else {
                let integrity = context_integrity(&target_paths, target_session).ok();
                targets.push(json!({
                    "memberId": member_id,
                    "sessionId": target_session,
                    "missing": integrity.as_ref().map(|item| item.missing),
                }));
            }
        }
        groups.push(json!({
            "groupId": group_id,
            "sourceSessionId": source_session,
            "targets": targets,
        }));
    }

    Ok(json!({
        "applied": apply,
        "groups": groups,
        "copiedSidecars": copied_total,
        "warnings": warnings,
        // 「补了什么」之外还要说「没补什么」：不搬的那几类体积是正文的几十倍，
        // 用户看到「已补齐」却翻不到变更记录时会以为功能坏了。
        "notCarried": session::not_carried_report(),
    }))
}

fn member_variant(member: &Value) -> WbVariant {
    member
        .get("variant")
        .and_then(Value::as_str)
        .map(|raw| WbVariant::parse(Some(raw)))
        .unwrap_or(WbVariant::Cn)
}

/// 回收「没有会话行、也没有关联成员引用」的 `file-history/<id>/` 目录。
///
/// 回收是不可逆的，所以三条同时成立才动手：目录名是合法会话 id、当前没有任何
/// 数据库行或关联成员引用它、最后修改时间早于 [`ORPHAN_GRACE_MS`]。
/// `apply = false` 时只列出候选。
pub fn gc_orphan_sidecars(paths: &SessionPaths, apply: bool) -> Value {
    let mut report = gc_orphan_sidecars_at(paths, apply, config::now_ms());
    // 只有真删了东西才留痕：干跑不写日志，否则「上次清理」会被一堆空跑刷掉。
    if apply && report.get("removed").and_then(Value::as_u64).unwrap_or(0) > 0 {
        if let Some(entry) = append_gc_audit(paths, &report) {
            report["audit"] = entry;
        }
    }
    // 上次清理一并带上：GC 是不可逆操作，用户下次打开面板要能看到「上次删了什么」。
    report["lastRun"] = last_gc_audit(paths).unwrap_or(Value::Null);
    report
}

/// GC 留痕文件：与其它备份同处 `~/.wb-switch/backups/`，按档位分开。
fn gc_audit_path(paths: &SessionPaths) -> PathBuf {
    paths.backup_root().join("gc-audit.jsonl")
}

/// 追加一条清理记录，返回写进去的那条（写失败返回 `None`，不影响回收本身）。
fn append_gc_audit(paths: &SessionPaths, report: &Value) -> Option<Value> {
    let removed: Vec<&str> = report
        .get("orphans")
        .and_then(Value::as_array)?
        .iter()
        .filter(|item| {
            item.get("removed")
                .and_then(Value::as_bool)
                .unwrap_or(false)
        })
        .filter_map(|item| item.get("sessionId").and_then(Value::as_str))
        .collect();
    if removed.is_empty() {
        return None;
    }
    let entry = json!({
        "at": config::now_ms(),
        "removed": removed.len(),
        "sessionIds": removed,
    });
    let path = gc_audit_path(paths);
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok()?;
    }
    // 追加写而不是整篇重写：日志是旁证，坏一行不该让「已经删掉的目录」变成失败。
    let line = format!("{entry}\n");
    let mut file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    use std::io::Write;
    file.write_all(line.as_bytes()).ok()?;
    Some(entry)
}

/// 最后一条清理记录；没有日志或末行损坏时返回 `None`。
fn last_gc_audit(paths: &SessionPaths) -> Option<Value> {
    let text = std::fs::read_to_string(gc_audit_path(paths)).ok()?;
    text.lines()
        .rev()
        .find_map(|line| serde_json::from_str::<Value>(line).ok())
}

/// [`gc_orphan_sidecars`] 的可注入实现：`now` 由调用方给，测试才能覆盖「超过宽限期」，
/// 而不必去改文件时间戳。
fn gc_orphan_sidecars_at(paths: &SessionPaths, apply: bool, now: i64) -> Value {
    if paths.data_root.as_os_str().is_empty() {
        return json!({"scanned": 0, "orphans": [], "removed": 0, "applied": apply});
    }
    let alive = live_session_ids(paths);
    let history_root = paths.data_root.join("file-history");
    let mut orphans: Vec<Value> = Vec::new();
    let mut removed = 0_u64;
    let mut scanned = 0_u64;

    let Ok(entries) = std::fs::read_dir(&history_root) else {
        return json!({"scanned": 0, "orphans": [], "removed": 0, "applied": apply});
    };
    for entry in entries.flatten() {
        let path = entry.path();
        let Ok(meta) = std::fs::symlink_metadata(&path) else {
            continue;
        };
        // 只处理真实目录：符号链接一律不碰。
        if !meta.is_dir() || meta.file_type().is_symlink() {
            continue;
        }
        let name = entry.file_name().to_string_lossy().to_string();
        if !is_plain_file_name(&name) || alive.contains(&name) {
            continue;
        }
        scanned += 1;
        let modified = meta
            .modified()
            .ok()
            .and_then(|stamp| stamp.duration_since(std::time::UNIX_EPOCH).ok())
            .map(|since| since.as_millis() as i64)
            .unwrap_or(now);
        if now.saturating_sub(modified) < ORPHAN_GRACE_MS {
            continue;
        }
        let mut report = json!({
            "sessionId": name,
            "path": path.to_string_lossy(),
            "ageDays": now.saturating_sub(modified) / (24 * 60 * 60 * 1000),
        });
        if apply {
            match std::fs::remove_dir_all(&path) {
                Ok(()) => {
                    removed += 1;
                    report["removed"] = json!(true);
                }
                Err(error) => report["error"] = json!(error.to_string()),
            }
        }
        orphans.push(report);
    }
    json!({
        "scanned": scanned,
        "orphans": orphans,
        "removed": removed,
        "applied": apply,
    })
}

/// 「仍然有主」的会话 id：数据库行（任何账号）或关联存储里的成员。
fn live_session_ids(paths: &SessionPaths) -> BTreeSet<String> {
    let mut ids = BTreeSet::new();
    if let Some(conn) = session::open_db(&paths.workbuddy_db(), true) {
        if let Ok(mut stmt) = conn.prepare("SELECT id FROM sessions") {
            if let Ok(rows) = stmt.query_map([], |row| row.get::<_, String>(0)) {
                ids.extend(rows.flatten());
            }
        }
    }
    if let StoreState::Ready(store) = session_link::load_store(paths) {
        for group in store.groups {
            for member in group.members {
                ids.insert(member.session_id);
            }
        }
    }
    ids
}

/// 全量扫描某个数据根下的所有会话（供命令行体检使用）。
pub fn scan_all(paths: &SessionPaths) -> Vec<ContextIntegrity> {
    let mut results = Vec::new();
    let Ok(workspaces) = std::fs::read_dir(paths.projects_dir()) else {
        return results;
    };
    for workspace in workspaces.flatten() {
        let Ok(files) = std::fs::read_dir(workspace.path()) else {
            continue;
        };
        for file in files.flatten() {
            let path = file.path();
            if path.extension().is_none_or(|ext| ext != "jsonl") {
                continue;
            }
            let Some(stem) = path.file_stem().and_then(|stem| stem.to_str()) else {
                continue;
            };
            if let Ok(integrity) = context_integrity(paths, stem) {
                results.push(integrity);
            }
        }
    }
    results
}

/// 便于命令行打印的摘要。
pub fn summarize(results: &[ContextIntegrity]) -> Value {
    // `brokenSessions` 只数**真缺**的会话：被新版本取代的引用是客户端正常清理，
    // 把它算进来会让「22/33 会话是坏的」这种假警报淹没真正需要处理的那几个。
    let broken = results.iter().filter(|item| item.missing > 0).count();
    let missing: usize = results.iter().map(|item| item.missing).sum();
    let pruned: usize = results.iter().map(|item| item.pruned).sum();
    json!({
        "sessions": results.len(),
        "brokenSessions": broken,
        "missingReferences": missing,
        "prunedReferences": pruned,
        "withoutFileHistoryDir": results.iter().filter(|item| !item.file_history_dir).count(),
        "withoutToolResults": results.iter().filter(|item| !item.tool_results).count(),
        "withoutUsage": results.iter().filter(|item| !item.usage).count(),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    struct TempEnv {
        root: PathBuf,
        paths: SessionPaths,
    }

    impl TempEnv {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir()
                .join(format!("wb_ctx_{}_{name}", uuid::Uuid::new_v4().simple()));
            let paths = SessionPaths {
                store_root: root.join("store"),
                data_root: root.join("data"),
                auth_file: root.join("auth.info"),
                link_namespace: crate::modules::session::LinkNamespace::WorkBuddy,
            };
            std::fs::create_dir_all(paths.projects_dir().join("ws-a")).unwrap();
            std::fs::create_dir_all(paths.data_root.join("file-history")).unwrap();
            TempEnv { root, paths }
        }

        fn write_body(&self, session_id: &str, snapshots: &[(&str, &str)]) {
            let mut lines = Vec::new();
            for (index, (key, backup)) in snapshots.iter().enumerate() {
                lines.push(format!(
                    r#"{{"type":"file-history-snapshot","sessionId":"{session_id}","snapshot":{{"trackedFileBackups":{{"{key}":{{"backupFileName":"{backup}","version":1}}}}}}}}"#
                ));
                let _ = index;
            }
            std::fs::write(
                self.paths
                    .projects_dir()
                    .join("ws-a")
                    .join(format!("{session_id}.jsonl")),
                lines.join("\n") + "\n",
            )
            .unwrap();
        }

        fn write_backup(&self, session_id: &str, name: &str) {
            let dir = self.paths.data_root.join("file-history").join(session_id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join(name), b"bytes").unwrap();
        }

        fn create_sessions_table(&self, ids: &[&str]) {
            let conn = rusqlite::Connection::open(self.paths.workbuddy_db()).unwrap();
            conn.execute_batch("CREATE TABLE sessions (id TEXT PRIMARY KEY);")
                .unwrap();
            for id in ids {
                conn.execute("INSERT INTO sessions (id) VALUES (?1)", [id])
                    .unwrap();
            }
        }
    }

    impl Drop for TempEnv {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    #[test]
    fn integrity_counts_present_and_missing_backups() {
        let env = TempEnv::new("integrity");
        env.write_body("s1", &[("a.txt", "h1@v1"), ("b.txt", "h2@v1")]);
        env.write_backup("s1", "h1@v1");

        let integrity = context_integrity(&env.paths, "s1").unwrap();
        assert_eq!(integrity.referenced, 2);
        assert_eq!(integrity.present, 1);
        assert_eq!(integrity.missing, 1);
        assert_eq!(integrity.missing_samples, vec!["h2@v1".to_string()]);
        assert!(integrity.file_history_dir);
        assert!(!integrity.todos, "没有清单文件就该是 false");
    }

    #[test]
    fn superseded_versions_count_as_pruned_not_missing() {
        let env = TempEnv::new("integrity-pruned");
        // 正文引用了同一文件的三代版本，目录里只剩最新那版——这是客户端的正常保留策略。
        env.write_body(
            "s4",
            &[("a.txt", "h1@v1"), ("a.txt", "h1@v2"), ("a.txt", "h1@v9")],
        );
        env.write_backup("s4", "h1@v9");
        // 另一个文件彻底不在，才算真缺。
        env.write_body("s5", &[("b.txt", "h2@v1")]);

        let integrity = context_integrity(&env.paths, "s4").unwrap();
        assert_eq!(integrity.present, 1);
        assert_eq!(integrity.pruned, 2, "同哈希的旧版本属于被取代");
        assert_eq!(integrity.missing, 0, "整份文件还在，不该报缺失");
        assert_eq!(integrity.pruned_samples.len(), 2);

        let gone = context_integrity(&env.paths, "s5").unwrap();
        assert_eq!(gone.pruned, 0);
        assert_eq!(gone.missing, 1, "连任何版本都不在才是真缺");
    }

    #[test]
    fn integrity_reports_usage_and_plan_files() {
        let env = TempEnv::new("integrity-extra");
        env.write_body("s2", &[]);
        std::fs::create_dir_all(env.paths.data_root.join("todos")).unwrap();
        std::fs::write(env.paths.data_root.join("todos/s2.json"), b"{}").unwrap();
        // 任务清单是目录：`tasks/<sid>/<taskId>.json`。
        std::fs::create_dir_all(env.paths.data_root.join("tasks/s2")).unwrap();
        std::fs::write(env.paths.data_root.join("tasks/s2/1.json"), b"{}").unwrap();
        // 工具输出与回滚日志都挂在正文所在的工作区目录下。
        let workspace = env.paths.projects_dir().join("ws-a");
        std::fs::create_dir_all(workspace.join("s2/tool-results")).unwrap();
        std::fs::write(workspace.join("s2/tool-results/call_1-render.txt"), b"{}").unwrap();
        std::fs::write(workspace.join("s2.file-rollback.ndjson"), b"{}\n").unwrap();
        let conn = rusqlite::Connection::open(env.paths.workbuddy_db()).unwrap();
        conn.execute_batch(
            "CREATE TABLE session_usage (session_id TEXT PRIMARY KEY, used INTEGER, size INTEGER);
             INSERT INTO session_usage (session_id, used, size) VALUES ('s2', 1, 2);",
        )
        .unwrap();
        drop(conn);

        let integrity = context_integrity(&env.paths, "s2").unwrap();
        assert_eq!(integrity.referenced, 0, "没有快照行就没有引用");
        assert!(integrity.todos);
        assert!(integrity.tasks, "tasks/<sid>/ 是目录，必须按目录判定");
        assert!(integrity.tool_results);
        assert!(integrity.rollback);
        assert!(integrity.usage);
    }

    #[test]
    fn integrity_reports_missing_tool_results_and_tasks() {
        let env = TempEnv::new("integrity-absent");
        env.write_body("s3", &[]);

        let integrity = context_integrity(&env.paths, "s3").unwrap();
        assert!(!integrity.tool_results, "没有目录就不该报存在");
        assert!(!integrity.tasks);
        assert!(!integrity.rollback);
    }

    #[test]
    fn gc_keeps_referenced_and_removes_only_old_orphans() {
        let env = TempEnv::new("gc");
        env.write_backup("alive", "h@v1");
        env.write_backup("orphan", "h@v1");
        env.create_sessions_table(&["alive"]);
        let now = crate::modules::config::now_ms() + ORPHAN_GRACE_MS + 1;

        // 干跑：只列候选，不动磁盘。
        let dry = gc_orphan_sidecars_at(&env.paths, false, now);
        assert_eq!(dry["orphans"].as_array().unwrap().len(), 1, "{dry}");
        assert_eq!(dry["removed"], 0);
        assert!(env.paths.data_root.join("file-history/orphan").is_dir());

        let applied = gc_orphan_sidecars_at(&env.paths, true, now);
        assert_eq!(applied["removed"], 1, "{applied}");
        assert!(!env.paths.data_root.join("file-history/orphan").exists());
        assert!(
            env.paths.data_root.join("file-history/alive").is_dir(),
            "有会话行的目录不得回收"
        );
    }

    #[test]
    fn gc_skips_directories_inside_grace_period() {
        let env = TempEnv::new("gc-grace");
        env.write_backup("fresh", "h@v1");
        // now 取当前时间：目录刚建，在宽限期内。
        let report = gc_orphan_sidecars_at(&env.paths, true, crate::modules::config::now_ms());
        assert_eq!(report["removed"], 0, "{report}");
        assert!(env.paths.data_root.join("file-history/fresh").is_dir());
    }

    #[test]
    fn gc_audit_records_only_actual_removals() {
        let env = TempEnv::new("gc-audit");

        // 干跑（或没删成）不留痕：否则「上次清理」会被一堆空跑刷掉。
        let dry = json!({"orphans": [{"sessionId": "a", "removed": false}], "removed": 0});
        assert!(append_gc_audit(&env.paths, &dry).is_none());
        assert!(last_gc_audit(&env.paths).is_none(), "没删东西就不该有日志");

        let applied = json!({
            "orphans": [
                {"sessionId": "a", "removed": true},
                {"sessionId": "b", "removed": false},
            ],
            "removed": 1,
        });
        let entry = append_gc_audit(&env.paths, &applied).expect("删了就要留痕");
        assert_eq!(entry["removed"], 1);
        assert_eq!(entry["sessionIds"], json!(["a"]), "只记真正删掉的");
        assert_eq!(
            last_gc_audit(&env.paths).unwrap()["sessionIds"],
            json!(["a"])
        );
    }

    #[test]
    fn not_carried_list_stays_in_sync_with_count() {
        let report = session::not_carried_report();
        assert_eq!(
            report["count"].as_u64().unwrap() as usize,
            session::NOT_CARRIED.len(),
            "count 必须由同一份清单算出"
        );
        for item in report["items"].as_array().unwrap() {
            assert!(!item["name"].as_str().unwrap_or_default().is_empty());
            // 每一项都要有「为什么不搬」：只说「不搬」而不说理由，用户没法判断影响。
            assert!(!item["reason"].as_str().unwrap_or_default().is_empty());
        }
    }

    #[test]
    fn backup_name_guard_rejects_traversal() {
        assert!(is_plain_file_name("abc123@v1"));
        assert!(!is_plain_file_name("../x"));
        assert!(!is_plain_file_name("a/b"));
        assert!(!is_plain_file_name(""));
    }
}
