//! 会话上下文完整性：自检、回填、孤儿回收。
//!
//! 客户端打开一个会话时读的不只是正文：
//! - `file-history/<sessionId>/<hash>@vN`：每一步的文件备份。正文里
//!   `snapshot.trackedFileBackups[*].backupFileName` 存的是 `<hash>@vN`，
//!   目录名才是会话 id（客户端 `PathMapper` 就是这么拼的）；
//! - `todos/<sessionId>.json`、`tasks/<sessionId>.json`：计划与任务清单；
//! - `session_usage` 行：`used` 是当前上下文占用、`size` 是窗口。
//!
//! 复制 / 同步漏搬其中任何一处，正文还在但引用悬空——用户看到的就是
//! 「会话关联了、上下文不对」。本模块只做三件事：**量出来**、**补齐**、**清孤儿**。

use std::collections::BTreeSet;
use std::path::Path;

use serde::Serialize;
use serde_json::{json, Value};

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
    /// 悬空引用数量：等于 `referenced - present`。
    pub missing: usize,
    /// 缺失样例（最多 5 个），便于人工核对。
    pub missing_samples: Vec<String>,
    /// 是否已有 `file-history/<sessionId>/` 目录。
    pub file_history_dir: bool,
    pub todos: bool,
    pub tasks: bool,
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
    let mut present = 0;
    let mut missing_samples = Vec::new();
    for name in &referenced {
        if is_plain_file_name(name) && history_dir.join(name).is_file() {
            present += 1;
        } else if missing_samples.len() < 5 {
            missing_samples.push(name.clone());
        }
    }
    let referenced_count = referenced.len();
    Ok(ContextIntegrity {
        session_id: session_id.to_string(),
        referenced: referenced_count,
        present,
        missing: referenced_count - present,
        missing_samples,
        file_history_dir: history_dir.is_dir(),
        todos: paths
            .data_root
            .join("todos")
            .join(format!("{session_id}.json"))
            .is_file(),
        tasks: paths
            .data_root
            .join("tasks")
            .join(format!("{session_id}.json"))
            .is_file(),
        usage: usage_row_exists(paths, session_id),
    })
}

/// 扫出正文里引用到的备份文件名（`trackedFileBackups[*].backupFileName`）。
///
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
            .ok_or_else(|| format!("组 {group_id} 没有安全来源，无法回填"))?;
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
    gc_orphan_sidecars_at(paths, apply, crate::modules::config::now_ms())
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
    let broken = results.iter().filter(|item| item.missing > 0).count();
    let missing: usize = results.iter().map(|item| item.missing).sum();
    json!({
        "sessions": results.len(),
        "brokenSessions": broken,
        "missingReferences": missing,
        "withoutFileHistoryDir": results.iter().filter(|item| !item.file_history_dir).count(),
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
    fn integrity_reports_usage_and_plan_files() {
        let env = TempEnv::new("integrity-extra");
        env.write_body("s2", &[]);
        std::fs::create_dir_all(env.paths.data_root.join("todos")).unwrap();
        std::fs::write(env.paths.data_root.join("todos/s2.json"), b"{}").unwrap();
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
        assert!(!integrity.tasks);
        assert!(integrity.usage);
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
    fn backup_name_guard_rejects_traversal() {
        assert!(is_plain_file_name("abc123@v1"));
        assert!(!is_plain_file_name("../x"));
        assert!(!is_plain_file_name("a/b"));
        assert!(!is_plain_file_name(""));
    }
}
