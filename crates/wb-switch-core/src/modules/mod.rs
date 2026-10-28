pub mod account;
pub mod auth_file;
pub mod checkin;
pub mod codebuddy_cli;
pub mod codebuddy_cn_ide;
pub mod codebuddy_ide;
pub mod codebuddy_ide_session;
pub mod codebuddy_ide_session_sync;
pub mod config;
pub mod credit_usage;
pub mod credits;
pub mod error_log;
pub mod export_import;
pub mod jetbrains;
pub mod limits;
#[cfg(target_os = "linux")]
pub mod linux_keyring;
pub mod notifications;
pub mod oauth;
pub mod official_usage;
pub mod process;
pub mod rate_limit_events;
pub mod rate_limit_hook;
pub mod refresh;
pub mod rotate;
pub mod session;
pub mod session_backup;
pub mod session_batch;
pub mod session_groups;
pub mod session_link;
pub mod switch;
pub mod token_stats;
pub mod travel;
pub mod update;
pub mod variant;
pub mod vscode_cn_inject;
pub mod vscode_ext;
pub mod vscode_session;
pub mod vscode_session_link;
pub mod vscode_session_sync;

/// 取互斥锁，**中毒也继续用**。
///
/// 这些锁保护的都是缓存/日志/运行标志这类可重建状态：临界区里 panic 过不代表数据
/// 结构从此不可用，而 `lock().unwrap()` 会把一次 panic 变成「此后每次访问都 panic」。
/// 对本地服务尤其致命——一个 handler 崩过，后续所有请求都 500，只能重启进程。
pub fn lock_or_recover<T>(mutex: &std::sync::Mutex<T>) -> std::sync::MutexGuard<'_, T> {
    mutex.lock().unwrap_or_else(|poison| poison.into_inner())
}
