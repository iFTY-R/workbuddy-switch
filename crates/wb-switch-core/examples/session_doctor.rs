//! 会话上下文体检（只读）。
//!
//! 打印某个档位数据根下每个会话的「正文引用了多少个文件备份 / 实际存在多少个」，
//! 以及 `todos` / `tasks` / `session_usage` 是否存在。用来量化
//! 「关联过来的会话上下文是否完整」，也可作为修复前后的对照基线。
//!
//! 用法：`cargo run -p wb-switch-core --example session_doctor [cn|ai]`

use wb_switch_core::modules::session::SessionPaths;
use wb_switch_core::modules::session_context;
use wb_switch_core::modules::variant::WbVariant;

fn main() {
    let variant = match std::env::args().nth(1).as_deref() {
        Some("ai") => WbVariant::Ai,
        _ => WbVariant::Cn,
    };
    let paths = SessionPaths::for_variant(variant);
    println!("data root: {}", paths.data_root.display());

    let results = session_context::scan_all(&paths);
    if results.is_empty() {
        println!("没有找到会话正文。");
        return;
    }

    let summary = session_context::summarize(&results);
    println!(
        "sessions={} broken={} missingReferences={} withoutFileHistoryDir={} withoutUsage={}",
        summary["sessions"],
        summary["brokenSessions"],
        summary["missingReferences"],
        summary["withoutFileHistoryDir"],
        summary["withoutUsage"]
    );

    let mut broken: Vec<_> = results.iter().filter(|item| item.missing > 0).collect();
    broken.sort_by(|a, b| b.missing.cmp(&a.missing));
    println!("\n-- 悬空引用最多的 10 个会话 --");
    for item in broken.iter().take(10) {
        println!(
            "{}  referenced={} present={} missing={} dir={} todos={} tasks={} usage={}  e.g. {:?}",
            item.session_id,
            item.referenced,
            item.present,
            item.missing,
            item.file_history_dir,
            item.todos,
            item.tasks,
            item.usage,
            item.missing_samples.first()
        );
    }
}
