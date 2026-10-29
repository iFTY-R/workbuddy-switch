//! 账号记忆迁移：把一个账号的「记忆」带给另一个账号。
//!
//! 客户端的记忆落点（真机核对，v5.8.3）：
//! - `~/.workbuddy/memory/<uid>_memory.md`，同目录还有 `.bak` 上一版；
//! - 旧版本拼写错误留下的 `memery/<uid>_memery.md`（同一份格式，只认不写）；
//! - 文件内容是 Markdown 头 + `RAW_JSON_START {...} RAW_JSON_END`，**JSON 里也带
//!   `uid`**。所以只改文件名是不够的：客户端按内嵌 uid 认身份，不改会读到
//!   「这是别人的记忆」。
//!
//! 有意**不迁** `user-<uid>-personal/`（`IDENTITY.md` / `SOUL.md` / `USER.md` /
//! `BOOTSTRAP.md`）：那是账号的人格设定，迁过去等于把目标账号改造成另一个人。
//!
//! 三条约束：
//! - 目标已有非空记忆时**默认拒绝**，必须显式 `overwrite` 才替换；
//! - 替换前把目标文件备份到 `~/.wb-switch/backups/memory/<档位>/`；
//! - 格式不认识（没有 RAW_JSON 块）时中止并保留原文件，不写半成品。

use std::path::{Path, PathBuf};

use chrono::Utc;
use serde::Serialize;
use serde_json::{json, Value};

use crate::modules::account;
use crate::modules::session::{self, SessionPaths};
use crate::modules::session_backup;
use crate::modules::variant::WbVariant;

/// 当前客户端的记忆目录名。
const MEMORY_DIR: &str = "memory";
/// 旧版本拼写（`memery`）留下的目录：内容同构，只读取不写入。
const LEGACY_MEMORY_DIR: &str = "memery";
/// 当前文件名后缀。
const MEMORY_SUFFIX: &str = "_memory.md";
/// 旧拼写后缀。
const LEGACY_MEMORY_SUFFIX: &str = "_memery.md";
/// 机器可读段的起止标记。
const RAW_START: &str = "RAW_JSON_START";
const RAW_END: &str = "RAW_JSON_END";

/// 一份记忆文件的概况（只描述事实，不含正文）。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryFileInfo {
    pub path: String,
    pub bytes: u64,
    /// 记忆正文的字符数：0 表示「有文件但没有内容」。
    pub block_chars: usize,
    /// 文件里记录的更新时间（原样透出，解析不出就是 `null`）。
    pub updated_at: Option<String>,
    /// 来自旧拼写目录 `memery/`。
    pub legacy: bool,
}

/// 迁移前的预检结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryPreview {
    pub variant: String,
    pub source_uid: String,
    pub target_uid: String,
    pub source: Option<MemoryFileInfo>,
    pub target: Option<MemoryFileInfo>,
    /// 目标是否已有非空记忆：为真时 `apply` 必须显式覆盖。
    pub target_has_memory: bool,
    /// 不带 `overwrite` 就能直接执行。
    pub can_apply: bool,
    /// 不能执行的原因（能执行时为 `null`）。
    pub blocked_reason: Option<String>,
}

/// 迁移执行结果。
#[derive(Debug, Clone, Serialize)]
#[serde(rename_all = "camelCase")]
pub struct MemoryApplyReport {
    pub copied: bool,
    pub source_path: String,
    pub target_path: String,
    /// 覆盖前备份的文件；没有覆盖就是 `null`。
    pub backup: Option<String>,
    /// 内嵌 uid 是否被改写（正常路径恒真）。
    pub uid_rewritten: bool,
    /// 跳过原因（`copied=false` 时必填）。
    pub skipped_reason: Option<String>,
}

/// 按档位预检：源账号的记忆能否带到目标账号。
pub fn preview(
    variant: WbVariant,
    source_uid: &str,
    target_uid: &str,
) -> Result<MemoryPreview, String> {
    preview_at(&variant.data_root(), variant, source_uid, target_uid)
}

/// 按目标账号预检：来源是**当前登录账号**，目标是即将切过去的账号。
///
/// 界面在勾选「迁移记忆」后调用。此时认证文件还没被改写，所以
/// `session::current_user_uid` 取到的就是切换前的账号——正是要带走的记忆来源。
/// 预检只读：不建目录、不写文件，用户取消勾选不会留下任何痕迹。
pub fn preview_for_account(account_id: &str) -> Result<MemoryPreview, String> {
    let acc =
        account::find_account(account_id).ok_or_else(|| format!("账号不存在: {account_id}"))?;
    let variant = account::variant_of(&acc);
    let target_uid = account::get_str(&acc, "uid").unwrap_or_default();
    let target_uid = target_uid.trim();
    if target_uid.is_empty() {
        return Err("目标账号缺少 uid，无法确定记忆写入位置".to_string());
    }
    let source_uid = session::current_user_uid(variant)
        .ok_or_else(|| "没有读到当前登录账号，无法确定记忆来源".to_string())?;
    preview(variant, &source_uid, target_uid)
}

/// 按档位执行迁移；`overwrite=false` 时目标已有记忆会直接拒绝。
pub fn apply(
    variant: WbVariant,
    source_uid: &str,
    target_uid: &str,
    overwrite: bool,
) -> Result<MemoryApplyReport, String> {
    apply_at(
        &variant.data_root(),
        &memory_backup_root(variant),
        variant,
        source_uid,
        target_uid,
        overwrite,
    )
}

/// 备份根：与其它备份同处 `~/.wb-switch/backups`，单独一个 `memory/` 子目录。
fn memory_backup_root(variant: WbVariant) -> PathBuf {
    SessionPaths::for_variant(variant)
        .backup_root()
        .join("memory")
}

/// [`preview`] 的可测实现：数据根显式传入。
fn preview_at(
    data_root: &Path,
    variant: WbVariant,
    source_uid: &str,
    target_uid: &str,
) -> Result<MemoryPreview, String> {
    validate_uid(source_uid)?;
    validate_uid(target_uid)?;
    let source = read_info(&source_path(data_root, source_uid));
    let target = read_info(&target_path(data_root, target_uid));
    let target_has_memory = target
        .as_ref()
        .is_some_and(|info| info.block_chars > 0 || info.legacy);
    let blocked_reason = if source_uid == target_uid {
        Some("源账号与目标账号相同，无需迁移记忆".to_string())
    } else if source.is_none() {
        Some("源账号没有记忆文件".to_string())
    } else if target_has_memory {
        // 只是提示，不带 overwrite 时 `apply` 会拒绝；`can_apply` 反映的是「无需覆盖即可执行」。
        Some("目标账号已有记忆，需要显式选择覆盖".to_string())
    } else {
        None
    };
    Ok(MemoryPreview {
        variant: variant.as_str().to_string(),
        source_uid: source_uid.to_string(),
        target_uid: target_uid.to_string(),
        source,
        target,
        target_has_memory,
        can_apply: blocked_reason.is_none(),
        blocked_reason,
    })
}

/// [`apply`] 的可测实现：数据根与备份根都显式传入。
fn apply_at(
    data_root: &Path,
    backup_root: &Path,
    variant: WbVariant,
    source_uid: &str,
    target_uid: &str,
    overwrite: bool,
) -> Result<MemoryApplyReport, String> {
    validate_uid(source_uid)?;
    validate_uid(target_uid)?;
    if source_uid == target_uid {
        return Err("源账号与目标账号相同，无需迁移记忆".to_string());
    }
    let source_path = source_path(data_root, source_uid);
    let target_path = target_path(data_root, target_uid);
    if read_info(&source_path).is_none() {
        return Err("源账号没有记忆文件，未迁移".to_string());
    }
    let text = std::fs::read_to_string(&source_path)
        .map_err(|error| format!("记忆文件读取失败（{error}），未迁移"))?;
    // 先分辨「格式不认识」与「内容为空」：两者的处理都是拒绝，但用户看到的解释不同。
    let Some(raw) = raw_json(&text) else {
        return Err("记忆文件缺少 RAW_JSON 块，格式不认识，未迁移".to_string());
    };
    let block = raw
        .get("memoryBlock")
        .and_then(Value::as_str)
        .unwrap_or_default()
        .trim();
    if block.is_empty() {
        return Err("源账号的记忆是空的，未迁移".to_string());
    }

    // 目标已有内容：不带 overwrite 一律拒绝，宁可让用户再点一次，也不静默替换。
    let existing = read_info(&target_path);
    let target_has_memory = existing.as_ref().is_some_and(|info| info.block_chars > 0);
    if target_has_memory && !overwrite {
        return Err("目标账号已有记忆，未覆盖；如确认替换请选择覆盖".to_string());
    }

    let rendered = rewrite_for_target(&text, target_uid)?;

    // 覆盖前先备份：备份失败就不写，避免「换了内容又没有退路」。
    let backup = if existing.is_some() {
        let dir = backup_root.join(variant.as_str());
        std::fs::create_dir_all(&dir).map_err(|error| format!("备份目录创建失败：{error}"))?;
        let path = dir.join(format!(
            "{target_uid}-{}.md",
            crate::modules::config::now_ms()
        ));
        let bytes = std::fs::read(&target_path)
            .map_err(|error| format!("目标记忆读取失败（{error}），未迁移"))?;
        session_backup::durable_write(&path, &bytes)
            .map_err(|error| format!("目标记忆备份失败（{error}），未迁移"))?;
        Some(path.to_string_lossy().to_string())
    } else {
        None
    };

    write_memory(&target_path, rendered.as_bytes())?;
    Ok(MemoryApplyReport {
        copied: true,
        source_path: source_path.to_string_lossy().to_string(),
        target_path: target_path.to_string_lossy().to_string(),
        backup,
        uid_rewritten: true,
        skipped_reason: None,
    })
}

/// 记忆文件路径：优先当前拼写，其次旧拼写。
fn source_path(data_root: &Path, uid: &str) -> PathBuf {
    let current = data_root
        .join(MEMORY_DIR)
        .join(format!("{uid}{MEMORY_SUFFIX}"));
    if current.is_file() {
        return current;
    }
    let legacy = data_root
        .join(LEGACY_MEMORY_DIR)
        .join(format!("{uid}{LEGACY_MEMORY_SUFFIX}"));
    if legacy.is_file() {
        return legacy;
    }
    current
}

/// 写入位置恒为当前拼写：旧拼写只读不改，避免升级后又写回已经废弃的目录。
fn target_path(data_root: &Path, uid: &str) -> PathBuf {
    data_root
        .join(MEMORY_DIR)
        .join(format!("{uid}{MEMORY_SUFFIX}"))
}

/// 读概况；文件不存在返回 `None`，格式异常时把正文长度按 0 处理（保守：视为无内容）。
fn read_info(path: &Path) -> Option<MemoryFileInfo> {
    let meta = std::fs::symlink_metadata(path).ok()?;
    if !meta.is_file() {
        return None;
    }
    let legacy = path
        .parent()
        .and_then(|parent| parent.file_name())
        .is_some_and(|name| name == LEGACY_MEMORY_DIR);
    let text = std::fs::read_to_string(path).unwrap_or_default();
    let raw = raw_json(&text);
    let block_chars = raw
        .as_ref()
        .and_then(|value| value.get("memoryBlock"))
        .and_then(Value::as_str)
        .map(|block| block.trim().chars().count())
        .unwrap_or(0);
    let updated_at = raw
        .as_ref()
        .and_then(|value| value.get("updatedAt"))
        .and_then(Value::as_str)
        .map(str::to_string);
    Some(MemoryFileInfo {
        path: path.to_string_lossy().to_string(),
        bytes: meta.len(),
        block_chars,
        updated_at,
        legacy,
    })
}

/// 取出 `RAW_JSON_START ... RAW_JSON_END` 之间的 JSON；缺失或非法返回 `None`。
fn raw_json(text: &str) -> Option<Value> {
    let start = text.find(RAW_START)? + RAW_START.len();
    let end = start + text[start..].find(RAW_END)?;
    serde_json::from_str(text[start..end].trim()).ok()
}

/// 把源文件改写成「属于目标账号」的样子：只动内嵌 uid 与 updatedAt，其余原样保留。
///
/// 没有 RAW_JSON 块时中止：那说明文件不是客户端写的格式，猜着改会把用户的记忆写坏。
fn rewrite_for_target(text: &str, target_uid: &str) -> Result<String, String> {
    let start = text
        .find(RAW_START)
        .ok_or_else(|| "记忆文件缺少 RAW_JSON 块，格式不认识，未迁移".to_string())?
        + RAW_START.len();
    let end = start
        + text[start..]
            .find(RAW_END)
            .ok_or_else(|| "记忆文件的 RAW_JSON 块没有结束标记，未迁移".to_string())?;
    let mut value: Value = serde_json::from_str(text[start..end].trim())
        .map_err(|error| format!("记忆文件的 RAW_JSON 无法解析（{error}），未迁移"))?;
    let object = value
        .as_object_mut()
        .ok_or_else(|| "记忆文件的 RAW_JSON 不是对象，未迁移".to_string())?;
    object.insert("uid".to_string(), json!(target_uid));
    object.insert(
        "updatedAt".to_string(),
        json!(Utc::now().format("%Y-%m-%dT%H:%M:%S%.3fZ").to_string()),
    );
    let rendered = serde_json::to_string_pretty(&value)
        .map_err(|error| format!("记忆文件重新序列化失败（{error}），未迁移"))?;
    // head 含 RAW_JSON_START，tail 从 RAW_JSON_END 开始：中间统一换成格式化后的 JSON。
    Ok(format!(
        "{}\n{}\n{}",
        &text[..start],
        rendered,
        &text[end..]
    ))
}

/// 写目标文件：先写 `.bak` 再写主文件，顺序与客户端一致（`.bak` 是上一版）。
fn write_memory(path: &Path, bytes: &[u8]) -> Result<(), String> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|error| format!("记忆目录创建失败：{error}"))?;
    }
    if let Ok(previous) = std::fs::read(path) {
        let bak = path.with_extension("md.bak");
        session_backup::durable_write(&bak, &previous)
            .map_err(|error| format!("记忆备份文件写入失败（{error}）"))?;
    }
    session_backup::durable_write(path, bytes).map_err(|error| format!("记忆写入失败（{error}）"))
}

/// uid 会拼进文件名：挡住分隔符与 `..`，其余按客户端 uid 形态放行。
fn validate_uid(uid: &str) -> Result<(), String> {
    if uid.is_empty() {
        return Err("账号 uid 为空".to_string());
    }
    if uid.len() > 128 || uid.contains(['/', '\\']) || uid.contains("..") || uid == "default" {
        return Err(format!("账号 uid 形态异常（{uid}），拒绝拼接路径"));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// 临时数据根：`<tmp>/data` 与 `<tmp>/backups`。
    struct Env {
        root: PathBuf,
        data: PathBuf,
        backups: PathBuf,
    }

    impl Env {
        fn new(name: &str) -> Self {
            let root = std::env::temp_dir().join(format!(
                "wb_memory_{name}_{}",
                uuid::Uuid::new_v4().simple()
            ));
            let env = Env {
                data: root.join("data"),
                backups: root.join("backups"),
                root,
            };
            std::fs::create_dir_all(env.data.join(MEMORY_DIR)).unwrap();
            env
        }

        fn write(&self, uid: &str, block: &str) {
            let dir = self.data.join(MEMORY_DIR);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(
                dir.join(format!("{uid}{MEMORY_SUFFIX}")),
                memory_text(uid, block),
            )
            .unwrap();
        }

        fn preview(&self, src: &str, dst: &str) -> MemoryPreview {
            preview_at(&self.data, WbVariant::Cn, src, dst).unwrap()
        }

        fn apply(
            &self,
            src: &str,
            dst: &str,
            overwrite: bool,
        ) -> Result<MemoryApplyReport, String> {
            apply_at(
                &self.data,
                &self.backups,
                WbVariant::Cn,
                src,
                dst,
                overwrite,
            )
        }
    }

    impl Drop for Env {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.root);
        }
    }

    /// 与客户端同构的记忆文件（头部 + RAW_JSON 块）。
    fn memory_text(uid: &str, block: &str) -> String {
        format!(
            "# User Memory Profile\n> Last updated: 2026-10-01T00:00:00.000Z\n> Version: 0\n\n## Memory Block\n\n{block}\n\n---\n\n<!-- {RAW_START}\n{}\n{RAW_END} -->\n",
            serde_json::to_string_pretty(&json!({
                "uid": uid,
                "memoryBlock": block,
                "updatedAt": "2026-10-01T00:00:00.000Z",
            }))
            .unwrap()
        )
    }

    fn read_block(path: &Path) -> String {
        let text = std::fs::read_to_string(path).unwrap();
        raw_json(&text)
            .and_then(|value| {
                value
                    .get("memoryBlock")
                    .and_then(Value::as_str)
                    .map(str::to_string)
            })
            .unwrap()
    }

    #[test]
    fn preview_reports_both_sides_and_allows_empty_target() {
        let env = Env::new("preview");
        env.write("uid-a", "喜欢简洁的答案");

        let preview = env.preview("uid-a", "uid-b");
        assert!(preview.can_apply, "{preview:?}");
        assert!(preview.source.as_ref().unwrap().block_chars > 0);
        assert!(preview.target.is_none());
        assert!(!preview.target_has_memory);
    }

    #[test]
    fn preview_blocks_when_target_already_has_memory() {
        let env = Env::new("occupied");
        env.write("uid-a", "源记忆");
        env.write("uid-b", "目标已有记忆");

        let preview = env.preview("uid-a", "uid-b");
        assert!(!preview.can_apply);
        assert!(preview.target_has_memory);
        assert!(preview
            .blocked_reason
            .as_deref()
            .unwrap_or_default()
            .contains("已有记忆"));
    }

    #[test]
    fn apply_rewrites_embedded_uid_and_keeps_block() {
        let env = Env::new("apply");
        env.write("uid-a", "记住：用户偏好中文");

        let report = env.apply("uid-a", "uid-b", false).unwrap();
        assert!(report.copied);
        assert!(report.uid_rewritten);
        assert!(report.backup.is_none(), "目标原本没有文件，不该产生备份");

        let written = env
            .data
            .join(MEMORY_DIR)
            .join(format!("uid-b{MEMORY_SUFFIX}"));
        assert_eq!(read_block(&written), "记住：用户偏好中文");
        let text = std::fs::read_to_string(&written).unwrap();
        let raw = raw_json(&text).unwrap();
        assert_eq!(raw["uid"], "uid-b", "内嵌 uid 必须改成目标账号");
        assert_ne!(
            raw["updatedAt"], "2026-10-01T00:00:00.000Z",
            "迁移时间要刷新，否则界面上看不出刚迁过"
        );
        // 源文件不得被改动。
        assert_eq!(
            read_block(
                &env.data
                    .join(MEMORY_DIR)
                    .join(format!("uid-a{MEMORY_SUFFIX}"))
            ),
            "记住：用户偏好中文"
        );
    }

    #[test]
    fn apply_refuses_to_overwrite_without_explicit_flag() {
        let env = Env::new("refuse");
        env.write("uid-a", "源记忆");
        env.write("uid-b", "目标原有记忆");

        let error = env.apply("uid-a", "uid-b", false).unwrap_err();
        assert!(error.contains("已有记忆"), "{error}");
        // 拒绝时目标内容必须原样。
        assert_eq!(
            read_block(
                &env.data
                    .join(MEMORY_DIR)
                    .join(format!("uid-b{MEMORY_SUFFIX}"))
            ),
            "目标原有记忆"
        );
    }

    #[test]
    fn apply_with_overwrite_backs_up_the_previous_file() {
        let env = Env::new("overwrite");
        env.write("uid-a", "源记忆");
        env.write("uid-b", "目标原有记忆");

        let report = env.apply("uid-a", "uid-b", true).unwrap();
        let backup = report.backup.expect("覆盖必须留下退路");
        assert_eq!(read_block(Path::new(&backup)), "目标原有记忆");
        assert_eq!(
            read_block(
                &env.data
                    .join(MEMORY_DIR)
                    .join(format!("uid-b{MEMORY_SUFFIX}"))
            ),
            "源记忆"
        );
        // 客户端自己的 `.bak` 也要更新成被覆盖的版本。
        let bak = env
            .data
            .join(MEMORY_DIR)
            .join(format!("uid-b{MEMORY_SUFFIX}.bak"));
        assert_eq!(read_block(&bak), "目标原有记忆");
    }

    #[test]
    fn legacy_memery_directory_is_read_but_not_written() {
        let env = Env::new("legacy");
        let legacy_dir = env.data.join(LEGACY_MEMORY_DIR);
        std::fs::create_dir_all(&legacy_dir).unwrap();
        std::fs::write(
            legacy_dir.join(format!("uid-a{LEGACY_MEMORY_SUFFIX}")),
            memory_text("uid-a", "旧拼写目录里的记忆"),
        )
        .unwrap();

        let preview = env.preview("uid-a", "uid-b");
        assert!(preview.source.as_ref().unwrap().legacy, "要能认出旧拼写");
        assert!(preview.can_apply);

        env.apply("uid-a", "uid-b", false).unwrap();
        assert!(env
            .data
            .join(MEMORY_DIR)
            .join(format!("uid-b{MEMORY_SUFFIX}"))
            .is_file());
        assert!(
            !env.data
                .join(LEGACY_MEMORY_DIR)
                .join(format!("uid-b{LEGACY_MEMORY_SUFFIX}"))
                .exists(),
            "旧拼写只读，不得再写回"
        );
    }

    #[test]
    fn empty_source_is_rejected_instead_of_clearing_target() {
        let env = Env::new("empty");
        env.write("uid-a", "");
        let error = env.apply("uid-a", "uid-b", true).unwrap_err();
        assert!(error.contains("空的"), "{error}");
        assert!(!env
            .data
            .join(MEMORY_DIR)
            .join(format!("uid-b{MEMORY_SUFFIX}"))
            .exists());
    }

    #[test]
    fn unknown_format_is_rejected_without_touching_target() {
        let env = Env::new("format");
        std::fs::write(
            env.data
                .join(MEMORY_DIR)
                .join(format!("uid-a{MEMORY_SUFFIX}")),
            "# 手写的记忆\n没有 RAW_JSON 块\n",
        )
        .unwrap();

        let error = env.apply("uid-a", "uid-b", false).unwrap_err();
        assert!(error.contains("RAW_JSON"), "{error}");
        assert!(!env
            .data
            .join(MEMORY_DIR)
            .join(format!("uid-b{MEMORY_SUFFIX}"))
            .exists());
    }

    #[test]
    fn same_uid_and_unsafe_uid_are_refused() {
        let env = Env::new("guard");
        env.write("uid-a", "记忆");
        assert!(env.preview("uid-a", "uid-a").blocked_reason.is_some());
        assert!(env.apply("uid-a", "uid-a", true).is_err());
        assert!(env.apply("uid-a", "../escape", true).is_err());
        assert!(env.apply("uid-a", "", true).is_err());
    }
}
