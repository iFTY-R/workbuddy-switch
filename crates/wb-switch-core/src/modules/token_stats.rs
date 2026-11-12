//! 本地 WorkBuddy / WorkBuddy 国际版 / CodeBuddy CLI / CodeBuddy IDE Token 统计。
//!
//! 这个模块是统计数据的唯一归属：日志只在这里解码、去重和按时间聚合，
//! Tauri 与 HTTP 层只负责转发结果。响应只包含聚合数字和脱敏标识，不返回
//! 消息正文、arguments 或认证信息。
//!
//! CodeBuddy IDE 不写 JSONL，用量在 `CodeBuddyExtension/Data/**/history/**/index.json`
//! 的 `requests[].usage` 中；消息正文文件（`messages/`）不会被扫描。
//!
//! 国内版与国际版**分开统计**（`workbuddy` / `workbuddy-ai` 两个 source），
//! 不合并：两档位是不同账号体系，混算会让用量无法归因。

use chrono::{Datelike, Local, Timelike};
use serde_json::{json, Map, Value};
use std::collections::{HashMap, HashSet};
use std::io::{BufRead, BufReader};
use std::path::{Path, PathBuf};

use crate::modules::variant::WbVariant;

/// 明细中最多返回的请求条数（响应窗口）。
const REQUEST_WINDOW: usize = 500;
/// 扫描期间明细缓存的内存硬上限；达到该值后立即裁剪到 `REQUEST_WINDOW`，
/// 使峰值内存恒定，不随历史总量增长。
const REQUEST_COLLECT_LIMIT: usize = 4_000;

#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
struct Usage {
    input: u64,
    output: u64,
    read: u64,
    write: u64,
}

#[derive(Clone, Debug, Default)]
struct Totals {
    usage: Usage,
    records: u64,
}

#[derive(Clone, Debug)]
struct SessionTotals {
    key: String,
    title: Option<String>,
    project: String,
    session_id: String,
    totals: Totals,
    /// 主会话 / 子代理。两种行完全同构，只靠该字段区分。
    kind: SessionKind,
    /// 路径首段项目名（[`project_name`] 的结果）。父子归属**只认它**：父行的
    /// `project` 来自记录 cwd，而子代理的 cwd 可能属于另一个仓库，按 `project`
    /// 字符串匹配会把它们全部漏配成孤儿。
    dir_project: String,
    /// 仅子代理有值：路径里 `subagents` 的上一级目录名（父会话文件 stem）。
    parent_session_id: Option<String>,
    /// 仅子代理有值：等于文件 stem（`agent-<uuid>`），与父会话里的 `task_id` 对齐。
    agent_id: Option<String>,
    /// 文件 mtime（毫秒），只用于「子代理 #n」的稳定序号。
    mtime_ms: u64,
    /// 父行最终的 `key`（含同名冲突后缀）。父行不存在时为 `None`（孤儿根行）。
    parent_key: Option<String>,
    agent_type: Option<String>,
    agent_label: Option<String>,
    /// 仅父行：其子行 token 之和（权威值，前端只做渲染，不再自行求和）。
    subagent_total: u64,
    /// 仅父行：有 usage 记录的子代理数。
    subagent_session_count: u32,
    /// 仅父行：扫到的子代理文件数（含 0 用量）。
    subagent_file_count: u32,
    /// 仅父行：0 条 usage 的子代理文件数（P1-4 的灰字提示）。
    subagent_empty_count: u32,
}

/// 展示总量：`input` 已包含 cache reads，因此只追加 output 与 cache write，
/// 避免把缓存命中重复计入。聚合与请求明细共用这一口径。
fn usage_total(usage: Usage) -> u64 {
    usage
        .input
        .saturating_add(usage.output)
        .saturating_add(usage.write)
}

impl Totals {
    fn add(&mut self, usage: Usage) {
        self.usage.input = self.usage.input.saturating_add(usage.input);
        self.usage.output = self.usage.output.saturating_add(usage.output);
        self.usage.read = self.usage.read.saturating_add(usage.read);
        self.usage.write = self.usage.write.saturating_add(usage.write);
        self.records = self.records.saturating_add(1);
    }

    fn value(&self) -> Value {
        let cache_hit_rate =
            (self.usage.input > 0).then(|| self.usage.read as f64 / self.usage.input as f64);
        // `input` already includes cache reads; expose the same headline total
        // used by the dashboard without double-counting the cached portion.
        let total = usage_total(self.usage);
        json!({
            "total": total,
            "input": self.usage.input,
            "output": self.usage.output,
            "cacheRead": self.usage.read,
            "cacheWrite": self.usage.write,
            "uncachedInput": self.usage.input.saturating_sub(self.usage.read),
            "records": self.records,
            "cacheHitRate": cache_hit_rate,
        })
    }
}

/// 一行统计属于主会话还是子代理。序列化为 `"main"` / `"subagent"`（小写）。
#[derive(Clone, Copy, Debug, Default, PartialEq, Eq)]
enum SessionKind {
    #[default]
    Main,
    Subagent,
}

impl SessionKind {
    fn as_str(self) -> &'static str {
        match self {
            Self::Main => "main",
            Self::Subagent => "subagent",
        }
    }
}

/// 扫描阶段的一个候选文件。
///
/// mtime 在收集时算一次，排序与「子代理 #n」降级共用，避免重复 `stat`。
#[derive(Clone, Debug)]
struct ScanFile {
    root: PathBuf,
    path: PathBuf,
    kind: SessionKind,
    /// 仅子代理有值：路径里 `subagents` 的上一级目录名。
    parent_session_id: Option<String>,
    mtime_ms: u64,
}

/// 一个聚合桶的「合计 + 主会话子集」。
///
/// 子代理子集 = `all - main`，恒等成立，故**不单独累加**：只维护两份计数，
/// 第三份由减法导出，避免三份计数日后漂移。
#[derive(Clone, Debug, Default)]
struct SplitTotals {
    all: Totals,
    main: Totals,
}

impl SplitTotals {
    fn add(&mut self, usage: Usage, kind: SessionKind) {
        self.all.add(usage);
        if kind == SessionKind::Main {
            self.main.add(usage);
        }
    }

    /// [`Totals::value`] 的全部键 + `mainTotal` / `subagentTotal`。
    ///
    /// 不变式：`mainTotal + subagentTotal == total`（`summary` 与每个 `daily`
    /// 点都必须成立，否则四档范围会互相矛盾）。
    fn value(&self) -> Value {
        let mut value = self.all.value();
        let main_total = usage_total(self.main.usage);
        value["mainTotal"] = json!(main_total);
        value["subagentTotal"] = json!(usage_total(self.all.usage).saturating_sub(main_total));
        value
    }
}

/// 聚合桶的输出契约：`Totals` 与 `SplitTotals` 共用同一套分组排序实现，
/// 避免两份排序逻辑日后漂移。
trait TotalsValue {
    fn to_value(&self) -> Value;
}

impl TotalsValue for Totals {
    fn to_value(&self) -> Value {
        self.value()
    }
}

impl TotalsValue for SplitTotals {
    fn to_value(&self) -> Value {
        self.value()
    }
}

/// 父会话里一条 `Agent` 派发所携带的可读标签。
///
/// 只有两个标量字段：`arguments.prompt` 是任务正文（隐私红线），
/// 本结构在构造时就把 `arguments` 丢掉，不留驻内存。
#[derive(Clone, Debug, Default)]
struct AgentLabel {
    subagent_type: Option<String>,
    description: Option<String>,
}

/// 某个父会话下子代理文件的计数（P1-4 的灰字与 P2-1 的指标）。
#[derive(Clone, Copy, Debug, Default)]
struct SubagentTally {
    /// 扫到的子代理文件数（含 0 用量）。
    files: u32,
    /// 其中 usage 记录数为 0 的。
    empty: u32,
}

/// 收尾一个文件时需要的身份信息。打包成结构体，避免 `push_session` 参数过多。
#[derive(Clone, Debug)]
struct SessionIdentity {
    session_id: String,
    kind: SessionKind,
    dir_project: String,
    parent_session_id: Option<String>,
    agent_id: Option<String>,
    mtime_ms: u64,
}

/// 一次模型调用的明细行。只携带脱敏标识与 token 数字：不含消息正文、
/// 工具参数、绝对路径或认证信息。
#[derive(Clone, Debug)]
struct RequestRow {
    timestamp: i64,
    model: String,
    project: String,
    session_id: String,
    title: Option<String>,
    usage: Usage,
    /// 思考过程 token 数（`output` 的子集），仅用于拆分「思考 / 回复」。
    thinking: u64,
}

impl RequestRow {
    fn value(&self) -> Value {
        json!({
            "timestamp": self.timestamp,
            "model": self.model,
            "project": self.project,
            "sessionId": self.session_id,
            "title": self.title,
            "input": self.usage.input,
            "output": self.usage.output,
            "cacheRead": self.usage.read,
            "cacheWrite": self.usage.write,
            // 与 `Totals::value()` 同一口径：input 已含 cacheRead，
            // 未命中部分即两者之差，下溢时饱和为 0。
            "uncachedInput": self.usage.input.saturating_sub(self.usage.read),
            // 回复内容 = max(0, output - thinking) 由前端做，这里只给原始计数；
            // 实测存在 thinking > output 的异常记录，故前端必须饱和减。
            "thinking": self.thinking,
            "total": usage_total(self.usage),
        })
    }
}

/// Read a non-negative integer from a JSON number or string.
fn number(value: &Value) -> Option<u64> {
    value
        .as_u64()
        .or_else(|| value.as_i64().and_then(|n| u64::try_from(n).ok()))
        .or_else(|| {
            value
                .as_f64()
                .filter(|n| n.is_finite() && *n >= 0.0)
                .map(|n| n as u64)
        })
        .or_else(|| value.as_str()?.trim().parse::<u64>().ok())
}

fn field(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(number))
}

fn positive_field(object: &Map<String, Value>, keys: &[&str]) -> Option<u64> {
    keys.iter()
        .find_map(|key| object.get(*key).and_then(number).filter(|value| *value > 0))
}

fn cached_input_field(object: &Map<String, Value>) -> u64 {
    // Providers have emitted both flat aliases and OpenAI-compatible nested
    // details. Prefer a positive flat alias so a stale `cache_read...: 0`
    // field cannot hide a populated `prompt_cache_hit_tokens` value.
    positive_field(
        object,
        &[
            "cache_read_input_tokens",
            "cacheReadInputTokens",
            "prompt_cache_hit_tokens",
            "cached_tokens",
        ],
    )
    .or_else(|| {
        object
            .get("prompt_tokens_details")
            .and_then(Value::as_object)
            .and_then(|details| positive_field(details, &["cached_tokens"]))
    })
    .or_else(|| {
        object
            .get("inputTokensDetails")
            .and_then(Value::as_array)
            .and_then(|details| {
                details.iter().find_map(|detail| {
                    detail
                        .as_object()
                        .and_then(|detail| positive_field(detail, &["cached_tokens"]))
                })
            })
    })
    .unwrap_or(0)
}

const CACHE_WRITE_KEYS: &[&str] = &[
    "cache_write_input_tokens",
    "cacheWriteInputTokens",
    "cache_creation_input_tokens",
    "prompt_cache_write_tokens",
];

fn usage_fields(object: &Map<String, Value>) -> Usage {
    Usage {
        input: field(object, &["input_tokens", "inputTokens", "prompt_tokens"]).unwrap_or(0),
        output: field(
            object,
            &["output_tokens", "outputTokens", "completion_tokens"],
        )
        .unwrap_or(0),
        read: cached_input_field(object),
        write: positive_field(object, CACHE_WRITE_KEYS).unwrap_or(0),
    }
}

fn usage_object(value: Option<&Value>) -> Option<&Map<String, Value>> {
    value?.as_object().filter(|object| {
        // Input is the required anchor for a usage record. It may legitimately
        // be zero (for example a provider reports output-only retries), so do
        // not use `input > 0` as the validity check.
        field(object, &["input_tokens", "inputTokens", "prompt_tokens"]).is_some()
    })
}

/// Decode one record. Usage precedence is message.usage > providerData.usage >
/// top-level usage. Cache-write metadata may only exist on a non-selected
/// usage object or rawUsage, so those objects are consulted without counting
/// their input/output again.
fn usage(value: &Value) -> Option<Usage> {
    let provider = value.get("providerData");
    let candidates = [
        value
            .get("message")
            .and_then(|message| message.get("usage")),
        provider.and_then(|data| data.get("usage")),
        value.get("usage"),
    ];
    let selected = candidates.iter().copied().find_map(usage_object)?;
    let mut result = usage_fields(selected);

    if result.write == 0 {
        result.write = candidates
            .iter()
            .copied()
            .filter_map(|candidate| candidate.and_then(Value::as_object))
            .chain(
                provider
                    .and_then(|data| data.get("rawUsage"))
                    .and_then(Value::as_object),
            )
            .find_map(|object| positive_field(object, CACHE_WRITE_KEYS))
            .unwrap_or(0);
    }

    // prompt_cache_miss_tokens is deliberately not a write alias: current
    // WorkBuddy/CodeBuddy logs use it for newly computed (uncached) input,
    // while their explicit cache-write fields may legitimately remain zero.

    Some(result)
}

fn timestamp(value: &Value) -> Option<i64> {
    value
        .get("timestamp")
        .or_else(|| value.get("ts"))
        .and_then(|timestamp| {
            timestamp
                .as_i64()
                .or_else(|| timestamp.as_u64().and_then(|n| i64::try_from(n).ok()))
                .or_else(|| timestamp.as_str()?.trim().parse::<i64>().ok())
        })
}

fn date(value: &Value) -> Option<String> {
    let timestamp = timestamp(value)?;
    chrono::DateTime::from_timestamp_millis(timestamp)
        .map(|date| date.with_timezone(&Local).format("%Y-%m-%d").to_string())
}

fn hour(value: &Value) -> Option<String> {
    let timestamp = timestamp(value)?;
    chrono::DateTime::from_timestamp_millis(timestamp).map(|date| {
        let local = date.with_timezone(&Local);
        format!(
            "{}-{}",
            local.weekday().num_days_from_monday(),
            local.hour()
        )
    })
}

fn model(value: &Value) -> String {
    value
        .get("providerData")
        .and_then(|data| data.get("model"))
        .or_else(|| value.get("model"))
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|model| !model.is_empty())
        .unwrap_or("未知模型")
        .to_string()
}

/// 思考过程 token 数：优先 `providerData.rawUsage.completion_thinking_tokens`，
/// 回退到 OpenAI 形状的 `completion_tokens_details.reasoning_tokens`，都没有则为 0。
///
/// 只读取计数字段：`providerData.reasoning` 是思考**正文**，属隐私红线，
/// 任何情况下都不得读取或返回。
fn thinking(value: &Value) -> u64 {
    let Some(raw_usage) = value
        .get("providerData")
        .and_then(|data| data.get("rawUsage"))
        .and_then(Value::as_object)
    else {
        return 0;
    };
    field(raw_usage, &["completion_thinking_tokens"])
        .or_else(|| {
            raw_usage
                .get("completion_tokens_details")
                .and_then(Value::as_object)
                .and_then(|details| field(details, &["reasoning_tokens"]))
        })
        .unwrap_or(0)
}

/// 递归收集 `*.jsonl`。
///
/// `include_subagents` 为 `false` 时保持原有行为（跳过 `subagents` 目录），供
/// CodeBuddy CLI / IDE 使用：这两个档位不写该目录，用显式开关而不是「本机恰好
/// 没有这个目录」的运气来保证它们的数值逐位不变。
fn files(root: &Path, include_subagents: bool, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            // 子代理日志是**独立请求**（实测消息 id 交集 0、时间戳无重叠），
            // 不再视为父会话上下文的重放，因此默认纳入统计。
            if include_subagents
                || path.file_name().and_then(|name| name.to_str()) != Some("subagents")
            {
                files(&path, include_subagents, output);
            }
        } else if path.extension().and_then(|extension| extension.to_str()) == Some("jsonl") {
            output.push(path);
        }
    }
}

/// 判定一个 jsonl 是主会话还是子代理，并推导父会话 id。
///
/// 父归属**只认路径** `<root>/<project>/<parentSessionId>/subagents/agent-<uuid>.jsonl`。
/// 实测 40 个子代理文件里 28 个的 `_meta.baggage` 装的是**它自己**的 session id，
/// 所以 baggage 不能作为兜底来源（会形成自环）；路径取不到父目录时父 id 记
/// `None`，该行降级为孤儿根行，不丢 token、不报错。
fn classify(path: &Path) -> (SessionKind, Option<String>) {
    let parent = path.parent();
    let is_subagent = parent
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        == Some("subagents");
    if !is_subagent {
        return (SessionKind::Main, None);
    }
    let parent_session_id = parent
        .and_then(Path::parent)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty() && !name.ends_with(".jsonl"))
        .map(str::to_string);
    (SessionKind::Subagent, parent_session_id)
}

/// 只从 `arguments` 里取 `subagent_type` / `description` 两个白名单标量。
///
/// `arguments` 实测是 **JSON 字符串**（需要二次解析），未来版本也可能是对象，
/// 两种形态都兼容。解析失败返回 `None` 并静默降级——旧客户端不写 `Agent`
/// 调用是正常情况，**不得**抬高 `parseErrors`。
///
/// `arguments.prompt` 是任务正文，**禁止读取**：`map` 在函数返回时立即释放。
fn agent_call_label(arguments: Option<&Value>) -> Option<AgentLabel> {
    let map = match arguments? {
        Value::String(text) => serde_json::from_str::<Map<String, Value>>(text).ok()?,
        Value::Object(map) => map.clone(),
        _ => return None,
    };
    let subagent_type = map
        .get("subagent_type")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(|value| value.chars().take(64).collect::<String>());
    let description = map
        .get("description")
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        // 用 `chars()` 而不是字节切片：按字节截断会在多字节字符中间 panic。
        .map(|value| value.chars().take(24).collect::<String>());
    (subagent_type.is_some() || description.is_some()).then_some(AgentLabel {
        subagent_type,
        description,
    })
}

/// 从 `function_call_result.output` 里取 `task_id: agent-<uuid>`。
///
/// `output` 实测是 `{type:"text", text:…}`，也可能是裸字符串或数组，三种形态都兜。
/// 只接受以 `agent-` 开头的值：`output.text` 里还有
/// `agent_id: software-architect@team`，绝不能误取它当 agentId。
fn agent_task_id(output: Option<&Value>) -> Option<String> {
    let text = match output? {
        Value::String(text) => text.as_str(),
        Value::Object(map) => map.get("text").and_then(Value::as_str)?,
        Value::Array(items) => items
            .iter()
            .find_map(|item| item.get("text").and_then(Value::as_str))?,
        _ => return None,
    };
    text.lines()
        .find_map(|line| line.trim().strip_prefix("task_id:"))
        .map(str::trim)
        .filter(|value| value.starts_with("agent-") && value.len() <= 64)
        .map(str::to_string)
}

/// `agent-<uuid>` 的短形式：`agent-` 前缀 + uuid 前 8 个字符。
///
/// 返回 `None` 表示 stem 不是 `agent-` 形状（降级到第 4 级标签）。
fn short_agent_id(stem: &str) -> Option<String> {
    let rest = stem.strip_prefix("agent-")?;
    let head: String = rest.chars().take(8).collect();
    (head.len() >= 4).then(|| format!("agent-{head}"))
}

/// 标签降级阶梯，取第一个可用值（P1-3）：
///
/// 1. `subagent_type · description`
/// 2. `subagent_type · agent-xxxxxxxx`
/// 3. `子代理 · agent-xxxxxxxx`
/// 4. `子代理 #n`（同父会话内按 mtime 升序的 1-based 序号）
fn agent_label(
    subagent_type: Option<&str>,
    description: Option<&str>,
    short_id: Option<&str>,
    sequence: usize,
) -> String {
    match (subagent_type, description, short_id) {
        (Some(kind), Some(description), _) => format!("{kind} · {description}"),
        (Some(kind), None, Some(short)) => format!("{kind} · {short}"),
        (None, _, Some(short)) => format!("子代理 · {short}"),
        _ => format!("子代理 #{sequence}"),
    }
}

fn project_name(root: &Path, file: &Path) -> String {
    let name = file
        .strip_prefix(root)
        .ok()
        .and_then(|relative| relative.components().next())
        .and_then(|component| component.as_os_str().to_str())
        .filter(|name| !name.is_empty() && !name.ends_with(".jsonl"));
    match name {
        // Product directories commonly encode the complete absolute path.
        // Returning that would leak a user name and parent directories.
        Some(name) if !name.starts_with("Users-") && !name.starts_with("home-") => name.to_string(),
        _ => "未知项目".to_string(),
    }
}

fn record_project(value: &Value, fallback: &str) -> String {
    value
        .get("cwd")
        .and_then(Value::as_str)
        .map(Path::new)
        .and_then(Path::file_name)
        .and_then(|name| name.to_str())
        .map(str::trim)
        .filter(|name| !name.is_empty() && name.len() <= 120)
        .unwrap_or(fallback)
        .to_string()
}

fn non_empty_text(value: Option<&Value>) -> Option<String> {
    value
        .and_then(Value::as_str)
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

fn groups<T: TotalsValue>(groups: HashMap<String, T>) -> Vec<Value> {
    let mut values: Vec<_> = groups
        .into_iter()
        .map(|(key, totals)| {
            let mut value = totals.to_value();
            value["key"] = json!(key);
            value
        })
        .collect();
    values.sort_by_key(|right| std::cmp::Reverse(total_value(right)));
    values
}

/// 会话行输出。**后端只输出扁平数组 + `parentKey`**，组树与截断交给前端：
/// 嵌套会破坏「按父会话取前 N」的语义。
///
/// 「父行不存在」一律输出 `null` 而不是省略键，前端用 `=== null` 判定更稳。
/// 这里**不做任何截断**：一旦截断，某个父行的子行会被切开，出现「父行合计
/// 91.5M 但只列出 1 个子代理」的错误。
fn session_groups(sessions: Vec<SessionTotals>) -> Vec<Value> {
    let mut values: Vec<_> = sessions
        .into_iter()
        .map(|session| {
            let mut value = session.totals.value();
            value["key"] = json!(session.key);
            value["title"] = json!(session.title);
            value["project"] = json!(session.project);
            value["sessionId"] = json!(session.session_id);
            value["kind"] = json!(session.kind.as_str());
            value["parentSessionId"] = json!(session.parent_session_id);
            value["parentKey"] = json!(session.parent_key);
            value["agentId"] = json!(session.agent_id);
            value["agentType"] = json!(session.agent_type);
            value["agentLabel"] = json!(session.agent_label);
            value["subagentTotal"] = json!(session.subagent_total);
            value["subagentSessionCount"] = json!(session.subagent_session_count);
            value["subagentFileCount"] = json!(session.subagent_file_count);
            value["subagentEmptyCount"] = json!(session.subagent_empty_count);
            value
        })
        .collect();
    values.sort_by_key(|right| std::cmp::Reverse(total_value(right)));
    values
}

fn total_value(value: &Value) -> u64 {
    value
        .get("total")
        .and_then(Value::as_u64)
        .unwrap_or_else(|| {
            value
                .get("input")
                .and_then(Value::as_u64)
                .unwrap_or(0)
                .saturating_add(value.get("output").and_then(Value::as_u64).unwrap_or(0))
                .saturating_add(value.get("cacheWrite").and_then(Value::as_u64).unwrap_or(0))
        })
}

#[derive(Default)]
struct SourceCollector {
    total: SplitTotals,
    models: HashMap<String, Totals>,
    projects: HashMap<String, Totals>,
    sessions: Vec<SessionTotals>,
    daily: HashMap<String, SplitTotals>,
    daily_by_model: HashMap<String, HashMap<String, Totals>>,
    hours: HashMap<String, Totals>,
    parse_errors: u64,
    coverage_start_at: Option<i64>,
    coverage_end_at: Option<i64>,
    session_key_counts: HashMap<String, usize>,
    requests: Vec<RequestRow>,
    collect_requests: bool,
    /// 是否扫描 `subagents/` 目录。CodeBuddy CLI / IDE 固定为 `false`——
    /// 这是「四个 source 隔离不可破坏」的唯一可靠保证（不依赖文件系统现状）。
    include_subagents: bool,
    /// `Agent` 工具调用的可读标签，键为 `@<callId>`（只有结果侧才知道 agentId）。
    agent_labels: HashMap<String, AgentLabel>,
    /// `callId` → 子代理文件 stem（`agent-<uuid>`），由 `function_call_result` 提供。
    agent_task_ids: HashMap<String, String>,
    /// `(dir_project, parent_session_id)` → 子代理文件计数。
    subagent_tallies: HashMap<(String, String), SubagentTally>,
    /// 扫到的子代理文件数（含 0 用量）。
    subagent_files: u32,
    /// 有 usage 记录的子代理数。
    subagent_sessions: u32,
    /// 只读诊断：跨文件命中 `seen` 的次数（P2-2）。**不改去重指纹**。
    dedupe_hits: u64,
}

impl SourceCollector {
    fn note_parse_error(&mut self) {
        self.parse_errors = self.parse_errors.saturating_add(1);
    }

    /// Append one request row. Once the in-memory hard limit is exceeded, keep
    /// only the newest window: the sort is stable, so rows sharing a timestamp
    /// keep their collection order and the oldest rows are dropped.
    fn push_request(&mut self, row: RequestRow) {
        if !self.collect_requests {
            return;
        }
        self.requests.push(row);
        if self.requests.len() > REQUEST_COLLECT_LIMIT {
            self.requests
                .sort_by_key(|row| std::cmp::Reverse(row.timestamp));
            self.requests.truncate(REQUEST_WINDOW);
        }
    }

    /// 从一条 `Agent` 工具调用 / 结果里采集子代理标签。
    ///
    /// 只读 `type` / `name` / `callId` 与 `arguments` 的两个白名单标量；
    /// `arguments.prompt`（任务正文）与 `output.text` 全文都不读取、不输出。
    fn collect_agent_call(&mut self, value: &Value) {
        if value.get("name").and_then(Value::as_str) != Some("Agent") {
            return;
        }
        let Some(call_id) = value.get("callId").and_then(Value::as_str) else {
            return;
        };
        match value.get("type").and_then(Value::as_str) {
            Some("function_call") => {
                if let Some(label) = agent_call_label(value.get("arguments")) {
                    // 同一 callId 多次出现时以首次为准（保留最早一次派发的标签）。
                    self.agent_labels
                        .entry(format!("@{call_id}"))
                        .or_insert(label);
                }
            }
            Some("function_call_result") => {
                if let Some(agent_id) = agent_task_id(value.get("output")) {
                    self.agent_task_ids.insert(call_id.to_string(), agent_id);
                }
            }
            _ => {}
        }
    }

    /// 累加一条 usage 记录。
    ///
    /// 只有 `summary` 与 `daily` 需要主 / 子拆分；`models` / `projects` /
    /// `hours` / `dailyByModel` 继续累加**全部**记录，因此自动包含子代理，
    /// 且与 `usage_total()` 共用唯一口径。
    fn add(&mut self, usage: Usage, value: &Value, project: &str, kind: SessionKind) {
        self.total.add(usage, kind);
        let model_name = model(value);
        self.models
            .entry(model_name.clone())
            .or_default()
            .add(usage);
        self.projects
            .entry(project.to_string())
            .or_default()
            .add(usage);
        if let Some(day) = date(value) {
            self.daily.entry(day.clone()).or_default().add(usage, kind);
            self.daily_by_model
                .entry(model_name)
                .or_default()
                .entry(day)
                .or_default()
                .add(usage);
        }
        if let Some(hour) = hour(value) {
            self.hours.entry(hour).or_default().add(usage);
        }
        if let Some(timestamp) = timestamp(value) {
            self.coverage_start_at = Some(
                self.coverage_start_at
                    .map_or(timestamp, |current| current.min(timestamp)),
            );
            self.coverage_end_at = Some(
                self.coverage_end_at
                    .map_or(timestamp, |current| current.max(timestamp)),
            );
        }
    }

    /// 收尾一个文件。
    ///
    /// `records == 0` 的早退保留（空文件不产生排行行），但子代理文件的计数
    /// 必须在早退**之前**累加，否则父行报不出「另有 N 个子代理无用量记录」。
    fn push_session(
        &mut self,
        identity: SessionIdentity,
        title: Option<String>,
        project: String,
        totals: Totals,
    ) {
        if identity.kind == SessionKind::Subagent {
            self.subagent_files = self.subagent_files.saturating_add(1);
            if totals.records > 0 {
                self.subagent_sessions = self.subagent_sessions.saturating_add(1);
            }
            if let Some(parent) = identity.parent_session_id.clone() {
                let tally = self
                    .subagent_tallies
                    .entry((identity.dir_project.clone(), parent))
                    .or_default();
                tally.files = tally.files.saturating_add(1);
                if totals.records == 0 {
                    tally.empty = tally.empty.saturating_add(1);
                }
            }
        }
        if totals.records == 0 {
            return;
        }
        let base_key = format!("{project} · {}", identity.session_id);
        let count = self.session_key_counts.entry(base_key.clone()).or_default();
        *count += 1;
        let key = if *count == 1 {
            base_key
        } else {
            format!("{base_key} · {}", *count)
        };
        self.sessions.push(SessionTotals {
            key,
            title,
            project,
            session_id: identity.session_id,
            totals,
            kind: identity.kind,
            dir_project: identity.dir_project,
            parent_session_id: identity.parent_session_id,
            agent_id: identity.agent_id,
            mtime_ms: identity.mtime_ms,
            parent_key: None,
            agent_type: None,
            agent_label: None,
            subagent_total: 0,
            subagent_session_count: 0,
            subagent_file_count: 0,
            subagent_empty_count: 0,
        });
    }

    /// 阶段 2：纯内存解析（无 IO、无二次扫描）。
    ///
    /// ① `callId → agentId → AgentLabel`，再按 4 级阶梯算出 `agentLabel`；
    /// ② `(dir_project, parent_session_id) → parentKey`，父行不存在则留 `None`；
    /// ③ 父行回填四个 `subagent*` 汇总字段。
    fn resolve_sessions(&mut self) {
        // `callId → agentId` 的中间表只在本次扫描内有效，先折算成 `agentId → label`。
        let mut labels: HashMap<String, AgentLabel> = HashMap::new();
        for (call_id, agent_id) in &self.agent_task_ids {
            if let Some(label) = self.agent_labels.get(&format!("@{call_id}")) {
                labels
                    .entry(agent_id.clone())
                    .or_insert_with(|| label.clone());
            }
        }

        // ① 第 4 级降级的序号必须**同父会话内**按 mtime 升序，跨父不共享。
        let mut siblings: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for (index, session) in self.sessions.iter().enumerate() {
            if session.kind != SessionKind::Subagent {
                continue;
            }
            let parent = session.parent_session_id.clone().unwrap_or_default();
            siblings
                .entry((session.dir_project.clone(), parent))
                .or_default()
                .push(index);
        }
        let mut sequence: HashMap<usize, usize> = HashMap::new();
        for indexes in siblings.values_mut() {
            indexes.sort_by_key(|index| {
                (
                    self.sessions[*index].mtime_ms,
                    self.sessions[*index].session_id.clone(),
                )
            });
            for (position, index) in indexes.iter().enumerate() {
                sequence.insert(*index, position + 1);
            }
        }

        for index in 0..self.sessions.len() {
            if self.sessions[index].kind != SessionKind::Subagent {
                continue;
            }
            let stem = self.sessions[index].session_id.clone();
            let label = labels.get(&stem);
            let agent_type = label.and_then(|label| label.subagent_type.clone());
            let description = label.and_then(|label| label.description.clone());
            let short_id = short_agent_id(&stem);
            let order = sequence.get(&index).copied().unwrap_or(1);
            self.sessions[index].agent_label = Some(agent_label(
                agent_type.as_deref(),
                description.as_deref(),
                short_id.as_deref(),
                order,
            ));
            self.sessions[index].agent_type = agent_type;
        }

        // ② 父子 key 解析。索引键用 `dir_project`（路径首段）而**不是** `project`：
        //    父行的 `project` 来自记录 cwd，子代理的 cwd 可能属于另一个仓库。
        let mut index_by_session: HashMap<(String, String), Vec<usize>> = HashMap::new();
        for (index, session) in self.sessions.iter().enumerate() {
            index_by_session
                .entry((session.dir_project.clone(), session.session_id.clone()))
                .or_default()
                .push(index);
        }
        let mut child_sum: HashMap<usize, u64> = HashMap::new();
        let mut child_count: HashMap<usize, u32> = HashMap::new();
        for index in 0..self.sessions.len() {
            if self.sessions[index].kind != SessionKind::Subagent {
                continue;
            }
            let Some(parent_id) = self.sessions[index].parent_session_id.clone() else {
                continue;
            };
            let lookup = (self.sessions[index].dir_project.clone(), parent_id);
            // 同名冲突时取第一条（主会话先入表），保证 parentKey 唯一。
            let Some(&parent) = index_by_session
                .get(&lookup)
                .and_then(|indexes| indexes.first())
            else {
                continue;
            };
            if parent == index {
                // 自环保护：父归属绝不指向自己。
                continue;
            }
            let parent_key = self.sessions[parent].key.clone();
            self.sessions[index].parent_key = Some(parent_key);
            let child_total = usage_total(self.sessions[index].totals.usage);
            let sum = child_sum.entry(parent).or_default();
            *sum = sum.saturating_add(child_total);
            let count = child_count.entry(parent).or_default();
            *count = count.saturating_add(1);
        }

        // ③ 父行回填。`subagentTotal` 由后端给出权威值，前端只做渲染。
        for index in 0..self.sessions.len() {
            let lookup = (
                self.sessions[index].dir_project.clone(),
                self.sessions[index].session_id.clone(),
            );
            let tally = self
                .subagent_tallies
                .get(&lookup)
                .copied()
                .unwrap_or_default();
            self.sessions[index].subagent_total = child_sum.get(&index).copied().unwrap_or(0);
            self.sessions[index].subagent_session_count =
                child_count.get(&index).copied().unwrap_or(0);
            self.sessions[index].subagent_file_count = tally.files;
            self.sessions[index].subagent_empty_count = tally.empty;
        }
    }

    fn into_value(mut self, name: &str, files_scanned: usize) -> Value {
        self.resolve_sessions();
        let daily_by_model = self
            .daily_by_model
            .into_iter()
            .map(|(model, points)| (model, Value::Array(groups(points))))
            .collect::<Map<String, Value>>();
        // `summary` 的 8 个既有键语义不变（`records` 现在含子代理记录），
        // 追加 `mainTotal` / `subagentTotal` / `subagentFiles` / `subagentSessions`。
        let mut summary = self.total.value();
        summary["subagentFiles"] = json!(self.subagent_files);
        summary["subagentSessions"] = json!(self.subagent_sessions);
        let mut value = json!({
            "source": name,
            "summary": summary,
            "models": groups(self.models),
            "projects": groups(self.projects),
            "sessions": session_groups(self.sessions),
            "daily": groups(self.daily),
            "dailyByModel": daily_by_model,
            "hours": groups(self.hours),
            "filesScanned": files_scanned,
            "parseErrors": self.parse_errors,
            "coverageStartAt": self.coverage_start_at,
            "coverageEndAt": self.coverage_end_at,
        });
        // Only detail sources take the new key; every other source keeps its
        // previous response shape byte for byte.
        if self.collect_requests {
            self.requests
                .sort_by_key(|row| std::cmp::Reverse(row.timestamp));
            self.requests.truncate(REQUEST_WINDOW);
            value["requests"] = Value::Array(self.requests.iter().map(RequestRow::value).collect());
        }
        value
    }
}

/// 单根统计：转发到多根实现，供 CodeBuddy CLI 这类固定单根的数据源使用。
/// `detail` additionally keeps a bounded window of per-call request rows; both
/// outputs share the same decode, cutoff, dedupe pipeline so a detail row can
/// never disagree with the aggregate totals.
///
/// 该档位不写 `subagents/` 目录，故固定 `include_subagents = false`：数值与
/// 改造前逐位一致，不依赖「本机恰好没有这个目录」的运气。
fn source(root: PathBuf, name: &str, cutoff: Option<i64>, detail: bool) -> Value {
    source_from_roots(std::slice::from_ref(&root), name, cutoff, detail, false)
}

/// 多根合并统计：同一档位下的多个 jsonl 根合并成**一个** source。
///
/// 国际版数据根与国内版不同构（实测无 `projects/`、只有 `sessions/`），因此按
/// 「存在的根」探测；一个都不存在时返回空集且不报错（前端显示空状态）。
///
/// `include_subagents` 决定 `subagents/` 目录是否进入扫描：`workbuddy` /
/// `workbuddy-ai` 传 `true`，CodeBuddy CLI / IDE 传 `false`。
fn source_from_roots(
    roots: &[PathBuf],
    name: &str,
    cutoff: Option<i64>,
    detail: bool,
    include_subagents: bool,
) -> Value {
    // 阶段 0：纯文件系统收集，mtime 只算一次（排序与「子代理 #n」共用）。
    let mut scanned: Vec<ScanFile> = Vec::new();
    for root in roots {
        let mut found = Vec::new();
        files(root, include_subagents, &mut found);
        for path in found {
            let mtime_ms = std::fs::metadata(&path)
                .and_then(|metadata| metadata.modified())
                .ok()
                .and_then(|time| time.duration_since(std::time::UNIX_EPOCH).ok())
                .map(|duration| duration.as_millis() as u64)
                .unwrap_or(u64::MAX);
            let (kind, parent_session_id) = classify(&path);
            scanned.push(ScanFile {
                root: root.clone(),
                path,
                kind,
                parent_session_id,
                mtime_ms,
            });
        }
    }
    let mut collector = SourceCollector {
        collect_requests: detail,
        include_subagents,
        ..SourceCollector::default()
    };

    // Copied/forked sessions replay the parent history (including usage
    // records with their original timestamps) into their own JSONL, so the
    // same request would otherwise be counted once per copy. Process files
    // oldest first and skip fingerprints already seen in an earlier file so
    // usage stays attributed to the original session.
    scanned.sort_by_key(|file| file.mtime_ms);
    let mut seen: HashSet<(i64, u64, u64, u64, u64, String)> = HashSet::new();

    for file in &scanned {
        let root = &file.root;
        let path = &file.path;
        // 子代理的 `sessionId` 一律取**文件 stem**（`agent-<uuid>`）：记录里的
        // `sessionId` 同一文件内可能混有两种值，不参与归属或去重。
        let session_id = path
            .file_stem()
            .and_then(|name| name.to_str())
            .filter(|name| !name.is_empty())
            .unwrap_or("未知会话")
            .to_string();
        let fallback_project = project_name(root, path);
        let Ok(file_handle) = std::fs::File::open(path) else {
            collector.note_parse_error();
            continue;
        };

        let mut session_totals = Totals::default();
        // Detail rows are buffered per file so the file-scoped title can be
        // backfilled once the whole file has been read.
        let mut file_requests: Vec<RequestRow> = Vec::new();
        let mut session_project: Option<String> = None;
        let mut ai_title: Option<String> = None;
        let mut summary: Option<String> = None;

        for line in BufReader::new(file_handle).lines() {
            let Ok(line) = line else {
                collector.note_parse_error();
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&line) else {
                collector.note_parse_error();
                continue;
            };
            // Title metadata belongs to the whole JSONL session file. Read it
            // before applying the usage cutoff so an older title can still
            // label usage that falls inside the selected range. aiTitle has
            // precedence over summary regardless of event order.
            if let Some(title) = non_empty_text(value.get("aiTitle")) {
                ai_title = Some(title);
            }
            if let Some(value) = non_empty_text(value.get("summary")) {
                summary = Some(value);
            }
            // 子代理可读标签也必须在 cutoff 与 `usage()` **之前**采集：父会话的
            // 派发行往往早于统计范围起点，放在后面会整段丢掉标签。父文件本来
            // 就要被读一遍，这里是顺手采集，没有第二次读盘。
            if collector.include_subagents {
                collector.collect_agent_call(&value);
            }
            // Records with a missing timestamp are excluded from a bounded
            // range rather than guessed from file mtime or browser time.
            if cutoff.is_some_and(|minimum| timestamp(&value).is_none_or(|ts| ts < minimum)) {
                continue;
            }
            let Some(usage) = usage(&value) else {
                continue;
            };
            // Fingerprint = (timestamp, full usage, model). Records without a
            // timestamp cannot be fingerprinted and are counted as before; they
            // are also unsortable and therefore never appear in the detail
            // window. Reuse the parsed model name for the fingerprint and the
            // detail row instead of decoding it twice.
            //
            // 指纹**禁止修改**：加 agent id 会破坏 fork 副本的去重。跨文件命中
            // 只记一个只读计数，供日后诊断（P2-2）。
            let ts = timestamp(&value);
            let model_name = model(&value);
            let duplicate = ts.is_some_and(|ts| {
                !seen.insert((
                    ts,
                    usage.input,
                    usage.output,
                    usage.read,
                    usage.write,
                    model_name.clone(),
                ))
            });
            if duplicate {
                collector.dedupe_hits = collector.dedupe_hits.saturating_add(1);
                continue;
            }
            let project = record_project(&value, &fallback_project);
            if session_project.is_none() {
                session_project = Some(project.clone());
            }
            session_totals.add(usage);
            collector.add(usage, &value, &project, file.kind);
            if collector.collect_requests {
                if let Some(ts) = ts {
                    file_requests.push(RequestRow {
                        timestamp: ts,
                        model: model_name,
                        project,
                        session_id: session_id.clone(),
                        title: None,
                        usage,
                        thinking: thinking(&value),
                    });
                }
            }
        }

        // `aiTitle` can appear anywhere in the file (including after the usage
        // records), so the file title is only final once the file is read.
        // 子代理行恒无标题：只靠 `agentLabel` 标识，避免与主会话标题语义混淆。
        let title = match file.kind {
            SessionKind::Main => ai_title.or(summary),
            SessionKind::Subagent => None,
        };
        if !file_requests.is_empty() {
            for row in &mut file_requests {
                row.title = title.clone();
            }
            for row in file_requests {
                collector.push_request(row);
            }
        }
        let agent_id = (file.kind == SessionKind::Subagent).then(|| session_id.clone());
        collector.push_session(
            SessionIdentity {
                session_id,
                kind: file.kind,
                dir_project: fallback_project.clone(),
                parent_session_id: file.parent_session_id.clone(),
                agent_id,
                mtime_ms: file.mtime_ms,
            },
            title,
            session_project.unwrap_or(fallback_project),
            session_totals,
        );
    }

    collector.into_value(name, scanned.len())
}

/// 某档位的候选 jsonl 根（顺序固定；纯函数，便于单测）。
///
/// 国内版保持既有单一 `projects/` 根；国际版两个根都探测（实测没有
/// `projects/`，只有 `sessions/`）。
fn jsonl_root_candidates(variant: WbVariant, root: &Path) -> Vec<PathBuf> {
    match variant {
        WbVariant::Cn => vec![root.join("projects")],
        WbVariant::Ai => vec![root.join("projects"), root.join("sessions")],
    }
}

/// 某档位下实际存在的 jsonl 扫描根；都不存在则返回空列表（空源，不报错）。
fn variant_source_roots(variant: WbVariant) -> Vec<PathBuf> {
    jsonl_root_candidates(variant, &variant.data_root())
        .into_iter()
        .filter(|path| path.is_dir())
        .collect()
}

fn codebuddy_extension_data_dir() -> PathBuf {
    dirs::data_local_dir()
        .unwrap_or_else(|| PathBuf::from("."))
        .join("CodeBuddyExtension")
        .join("Data")
}

fn is_ide_conversation_index(path: &Path) -> bool {
    path.file_name().and_then(|name| name.to_str()) == Some("index.json")
        && path
            .parent()
            .and_then(|parent| parent.parent())
            .and_then(|parent| parent.parent())
            .and_then(|parent| parent.file_name())
            .and_then(|name| name.to_str())
            == Some("history")
}

fn ide_index_files(root: &Path, output: &mut Vec<PathBuf>) {
    let Ok(entries) = std::fs::read_dir(root) else {
        return;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if path.is_dir() {
            let name = path.file_name().and_then(|name| name.to_str());
            // Message bodies contain chat content and must not be scanned.
            // Checkpoints and the shared Public bucket are unrelated to usage.
            if !matches!(
                name,
                Some("messages" | "check-point" | "backups" | "Public")
            ) {
                ide_index_files(&path, output);
            }
        } else if is_ide_conversation_index(&path) {
            output.push(path);
        }
    }
}

fn decode_genie_workspace(name: &str) -> Option<String> {
    use base64::Engine;
    let engine = base64::engine::general_purpose::STANDARD;
    let try_decode = |value: &str| -> Option<String> {
        let padded = match value.len() % 4 {
            0 => value.to_string(),
            remainder => format!("{value}{}", "=".repeat(4 - remainder)),
        };
        let bytes = engine.decode(padded).ok()?;
        let text = String::from_utf8(bytes).ok()?;
        let trimmed = text.trim();
        if trimmed.is_empty() || trimmed.contains('\0') {
            return None;
        }
        Some(trimmed.to_string())
    };
    try_decode(name)
        .or_else(|| try_decode(&name.replace('_', "/")))
        .or_else(|| try_decode(&name.replace('_', "+")))
}

fn ide_project_by_session() -> HashMap<String, String> {
    let mut map = HashMap::new();
    let Some(root) = crate::modules::vscode_cn_inject::codebuddy_cn_data_dir().map(|dir| {
        dir.join("User")
            .join("globalStorage")
            .join("tencent-cloud.coding-copilot")
            .join("genie-history")
    }) else {
        return map;
    };
    let Ok(entries) = std::fs::read_dir(root) else {
        return map;
    };
    for entry in entries.flatten() {
        let path = entry.path();
        if !path.is_dir() {
            continue;
        }
        let folder = path
            .file_name()
            .and_then(|name| name.to_str())
            .unwrap_or("");
        let project = decode_genie_workspace(folder)
            .as_deref()
            .map(Path::new)
            .and_then(Path::file_name)
            .and_then(|name| name.to_str())
            .map(str::trim)
            .filter(|name| !name.is_empty() && name.len() <= 120)
            .unwrap_or("未知项目")
            .to_string();
        let Ok(conversations) = std::fs::read_dir(path.join("conversations")) else {
            continue;
        };
        for conversation in conversations.flatten() {
            if let Some(id) = conversation.file_name().to_str() {
                if !id.is_empty() {
                    map.insert(id.to_string(), project.clone());
                }
            }
        }
    }
    map
}

fn ide_workspace_meta(conv_index: &Path, conv_id: &str) -> (Option<String>, String) {
    let Some(ws_index) = conv_index
        .parent()
        .and_then(|parent| parent.parent())
        .map(|parent| parent.join("index.json"))
    else {
        return (None, "未知模型".to_string());
    };
    let Ok(text) = std::fs::read_to_string(ws_index) else {
        return (None, "未知模型".to_string());
    };
    let Ok(value) = serde_json::from_str::<Value>(&text) else {
        return (None, "未知模型".to_string());
    };
    let Some(conversations) = value.get("conversations").and_then(Value::as_array) else {
        return (None, "未知模型".to_string());
    };
    for conversation in conversations {
        if conversation.get("id").and_then(Value::as_str) != Some(conv_id) {
            continue;
        }
        let title = non_empty_text(conversation.get("name"))
            .or_else(|| non_empty_text(conversation.get("title")));
        let model = non_empty_text(conversation.get("selectedModelId"))
            .or_else(|| non_empty_text(conversation.get("modelId")))
            .or_else(|| non_empty_text(conversation.get("model")))
            .unwrap_or_else(|| "未知模型".to_string());
        return (title, model);
    }
    (None, "未知模型".to_string())
}

fn ide_request_usage(request: &Value) -> Option<Usage> {
    let object = request.get("usage")?.as_object()?;
    field(object, &["inputTokens", "input_tokens", "prompt_tokens"])?;
    Some(Usage {
        input: field(object, &["inputTokens", "input_tokens", "prompt_tokens"]).unwrap_or(0),
        output: field(
            object,
            &["outputTokens", "output_tokens", "completion_tokens"],
        )
        .unwrap_or(0),
        read: field(
            object,
            &[
                "cacheTokens",
                "cacheReadInputTokens",
                "cache_read_input_tokens",
            ],
        )
        .unwrap_or(0),
        write: positive_field(
            object,
            &[
                "cachedWriteTokens",
                "cacheWriteInputTokens",
                "cache_write_input_tokens",
                "cache_creation_input_tokens",
            ],
        )
        .unwrap_or(0),
    })
}

fn ide_request_timestamp(request: &Value) -> Option<i64> {
    timestamp(request).or_else(|| {
        request.get("startedAt").and_then(|value| {
            value
                .as_i64()
                .or_else(|| value.as_u64().and_then(|n| i64::try_from(n).ok()))
                .or_else(|| value.as_str()?.trim().parse::<i64>().ok())
        })
    })
}

/// 单条请求的指纹：优先「时间戳 + 用量」，缺时间戳时退回请求 id。
///
/// 副本之间是逐字重放——时间戳与用量都不变——所以这个指纹可以直接当并集去重的键；
/// 两者都没有就返回 `None`（不进并集，按原样计数，实测不存在这种记录）。
fn ide_request_fingerprint(
    request: &Value,
    usage: &Usage,
    timestamp: Option<i64>,
) -> Option<String> {
    Some(match timestamp {
        Some(timestamp) => format!(
            "{timestamp}:{}:{}:{}:{}",
            usage.input, usage.output, usage.read, usage.write
        ),
        None => non_empty_text(request.get("id"))?.to_string(),
    })
}

/// 会话 id：会话目录名。副本会被重随机 id，所以它**不是**会话身份（见 [`ide_file_weight`]）。
fn ide_session_id(path: &Path) -> String {
    path.parent()
        .and_then(|parent| parent.file_name())
        .and_then(|name| name.to_str())
        .filter(|name| !name.is_empty())
        .unwrap_or("未知会话")
        .to_string()
}

/// 一份会话 index 的轻量指标：**会话指纹**（首个可计入请求的指纹，同一会话的所有副本
/// 共享它）、可计入的请求条数、token 总量。
///
/// 切号复制会让同一会话在多个账号数据目录下各留一份：副本整份重放父历史，之后各自继续
/// 追加——所以各份长短不一、内容并不完全相同，会话 id 也可能被重随机（插件侧
/// `CopyIdPolicy::AlwaysNew`）。会话身份因此不能按目录名判，只能按「同一个起点请求」判。
/// 指标里的条数/总量用于挑出最完整的那份，作为该会话的标题、模型与项目归属。
fn ide_file_weight(path: &Path) -> Option<(String, usize, u64)> {
    let text = std::fs::read_to_string(path).ok()?;
    let value = serde_json::from_str::<Value>(&text).ok()?;
    let requests = value.get("requests")?.as_array()?;
    let mut key: Option<String> = None;
    let mut count = 0usize;
    let mut total = 0u64;
    for request in requests {
        let Some(usage) = ide_request_usage(request) else {
            continue;
        };
        if key.is_none() {
            key = ide_request_fingerprint(request, &usage, ide_request_timestamp(request));
        }
        count += 1;
        total = total.saturating_add(usage_total(usage));
    }
    Some((key?, count, total))
}

/// 把一个会话（`requests` 是它的某一份副本）里可计入的请求并入统计。
///
/// `seen` 按请求指纹去重、跨副本共享：副本重放父历史时带的是原始时间戳，所以并集
/// 既不会把历史日期重复抬高，也不会丢掉各副本复制后各自追加的请求。指纹缺失的记录
/// 一律计入。
fn collect_ide_requests(
    collector: &mut SourceCollector,
    requests: &[Value],
    cutoff: Option<i64>,
    model_name: &str,
    project: &str,
    seen: &mut HashSet<String>,
    session_totals: &mut Totals,
) {
    for request in requests {
        let ts = ide_request_timestamp(request);
        if cutoff.is_some_and(|minimum| ts.is_none_or(|value| value < minimum)) {
            continue;
        }
        let Some(usage) = ide_request_usage(request) else {
            continue;
        };
        if let Some(fingerprint) = ide_request_fingerprint(request, &usage, ts) {
            // 同一请求已在另一份副本里计过。
            if !seen.insert(fingerprint) {
                continue;
            }
        }
        session_totals.add(usage);
        collector.add(
            usage,
            &json!({
                "timestamp": ts,
                "providerData": { "model": model_name },
            }),
            project,
            // IDE 数据源没有子代理概念，全部记为主会话，数字逐位不变。
            SessionKind::Main,
        );
    }
}

fn ide_source(
    root: PathBuf,
    name: &str,
    cutoff: Option<i64>,
    project_by_session: &HashMap<String, String>,
) -> Value {
    let mut paths = Vec::new();
    ide_index_files(&root, &mut paths);
    paths.sort();

    // 同一会话（按起点请求判身份）因切号复制可能散落在多个账号数据目录里。各份之间按
    // 请求指纹取**并集**：父历史只算一次（副本会把历史日期回溯性抬高），副本复制后各自
    // 追加的请求也都保留（只取「最长一份」会漏掉别的副本独有的请求）。会话标题、模型与
    // 项目归属取最完整的那份。
    let weights: Vec<Option<(String, usize, u64)>> =
        paths.iter().map(|path| ide_file_weight(path)).collect();
    let mut groups: HashMap<&str, Vec<usize>> = HashMap::new();
    let mut lead: HashMap<&str, (usize, usize, u64)> = HashMap::new();
    for (index, weight) in weights.iter().enumerate() {
        let Some((fingerprint, count, total)) = weight else {
            continue;
        };
        groups.entry(fingerprint.as_str()).or_default().push(index);
        let replace = lead
            .get(fingerprint.as_str())
            .is_none_or(|current| (*count, *total) > (current.1, current.2));
        if replace {
            lead.insert(fingerprint.as_str(), (index, *count, *total));
        }
    }

    let mut collector = SourceCollector::default();

    for (index, _) in paths.iter().enumerate() {
        let fingerprint = weights[index]
            .as_ref()
            .map(|(fingerprint, _, _)| fingerprint.as_str());
        // 同一会话只在它的第一份副本处处理一次，并把其余副本一起并入。读不出会话指纹
        // （读取/解析失败）的文件照旧单独走一遍，让解析错误被记一笔。
        if let Some(fingerprint) = fingerprint {
            if groups.get(fingerprint).and_then(|members| members.first()) != Some(&index) {
                continue;
            }
        }
        // 标题/模型/项目取最完整的那份：副本 id 被重随机过，元信息不一定都齐全。
        let lead_index = fingerprint
            .and_then(|fingerprint| lead.get(fingerprint).map(|current| current.0))
            .unwrap_or(index);
        let lead_session_id = ide_session_id(&paths[lead_index]);
        let (title, model_name) = ide_workspace_meta(&paths[lead_index], &lead_session_id);
        let project = project_by_session
            .get(&lead_session_id)
            .cloned()
            .unwrap_or_else(|| "未知项目".to_string());
        let members: &[usize] = match fingerprint {
            Some(fingerprint) => groups
                .get(fingerprint)
                .map(Vec::as_slice)
                .unwrap_or_default(),
            None => std::slice::from_ref(&index),
        };
        let mut session_totals = Totals::default();
        let mut seen: HashSet<String> = HashSet::new();
        for member in members {
            let Ok(text) = std::fs::read_to_string(&paths[*member]) else {
                collector.note_parse_error();
                continue;
            };
            let Ok(value) = serde_json::from_str::<Value>(&text) else {
                collector.note_parse_error();
                continue;
            };
            let Some(requests) = value.get("requests").and_then(Value::as_array) else {
                continue;
            };
            collect_ide_requests(
                &mut collector,
                requests,
                cutoff,
                &model_name,
                &project,
                &mut seen,
                &mut session_totals,
            );
        }

        collector.push_session(
            SessionIdentity {
                session_id: lead_session_id,
                kind: SessionKind::Main,
                // IDE 侧没有「路径首段项目」的概念，用会话的项目归属充当归属键。
                dir_project: project.clone(),
                parent_session_id: None,
                agent_id: None,
                mtime_ms: 0,
            },
            title,
            project,
            session_totals,
        );
    }

    collector.into_value(name, paths.len())
}

/// Return independent WorkBuddy (CN), WorkBuddy AI, CodeBuddy CLI, and
/// CodeBuddy IDE aggregates.
/// `days` is interpreted in Rust using the same millisecond clock for every source.
pub fn get_statistics(days: Option<i64>) -> Value {
    let home = dirs::home_dir().unwrap_or_else(|| PathBuf::from("."));
    let generated_at = crate::modules::config::now_ms();
    let range_days = match days {
        Some(7) => Some(7),
        Some(30) => Some(30),
        Some(90) => Some(90),
        _ => None,
    };
    let cutoff = range_days.map(|value| generated_at - value * 86_400_000);
    let ide_projects = ide_project_by_session();
    json!({
        "generatedAt": generated_at,
        "rangeDays": range_days,
        "sources": [
            // 子代理日志是独立请求，纳入统计（开关为 `true`）。
            source_from_roots(
                &variant_source_roots(WbVariant::Cn),
                "workbuddy",
                cutoff,
                false,
                true
            ),
            // 国际版独立 source：不与国内版混算（不同账号体系）。
            source_from_roots(
                &variant_source_roots(WbVariant::Ai),
                "workbuddy-ai",
                cutoff,
                false,
                true
            ),
            // 请求明细目前只对 CodeBuddy CLI 开放（见 spec 的 requests 契约）。
            // 该档位不写 subagents 目录，`source()` 固定不扫描它。
            source(home.join(".codebuddy/projects"), "codebuddy-cli", cutoff, true),
            ide_source(
                codebuddy_extension_data_dir(),
                "codebuddy-ide",
                cutoff,
                &ide_projects,
            ),
        ],
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// 固定 fixture 的 mtime，让「原始会话先于副本被处理」的断言不依赖目录遍历顺序。
    ///
    /// Windows 上 `File::set_modified` 需要 `FILE_WRITE_ATTRIBUTES`，而 `File::open`
    /// 只申请 `GENERIC_READ`——只读句柄改时间会得到 `os error 5`，故用可写句柄打开。
    fn pin_mtime(path: std::path::PathBuf, mtime: std::time::SystemTime) {
        fs::OpenOptions::new()
            .write(true)
            .open(path)
            .expect("open fixture for mtime")
            .set_modified(mtime)
            .expect("pin fixture mtime");
    }

    #[test]
    fn usage_priority_aliases_and_raw_cache_write() {
        let value = json!({
            "providerData": {
                "usage": { "inputTokens": 99, "outputTokens": 22 },
                "rawUsage": { "prompt_cache_write_tokens": 2 }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "cache_read_input_tokens": 4
            }}
        });
        assert_eq!(
            usage(&value),
            Some(Usage {
                input: 10,
                output: 3,
                read: 4,
                write: 2
            })
        );
    }

    #[test]
    fn cache_write_uses_explicit_aliases_but_never_cache_miss() {
        let provider_usage_write = json!({
            "providerData": {
                "usage": {
                    "inputTokens": 99,
                    "outputTokens": 22,
                    "cache_write_input_tokens": 0,
                    "cache_creation_input_tokens": 7
                },
                "rawUsage": {
                    "prompt_cache_miss_tokens": 91,
                    "prompt_cache_write_tokens": 0
                }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3,
                "cache_read_input_tokens": 4
            }}
        });
        assert_eq!(
            usage(&provider_usage_write),
            Some(Usage {
                input: 10,
                output: 3,
                read: 4,
                write: 7,
            })
        );

        let cache_miss_only = json!({
            "providerData": {
                "rawUsage": { "prompt_cache_miss_tokens": 91 }
            },
            "message": { "usage": {
                "input_tokens": 10,
                "output_tokens": 3
            }}
        });
        assert_eq!(
            usage(&cache_miss_only),
            Some(Usage {
                input: 10,
                output: 3,
                read: 0,
                write: 0,
            })
        );
    }

    #[test]
    fn cache_read_accepts_nested_provider_details() {
        let value = json!({
            "providerData": {
                "usage": {
                    "inputTokens": 99,
                    "outputTokens": 3,
                    "inputTokensDetails": [{ "cached_tokens": 7 }]
                }
            }
        });
        assert_eq!(
            usage(&value),
            Some(Usage {
                input: 99,
                output: 3,
                read: 7,
                write: 0,
            })
        );

        let raw = json!({
            "usage": {
                "prompt_tokens": 20,
                "completion_tokens": 2,
                "cache_read_input_tokens": 0,
                "prompt_cache_hit_tokens": 12
            }
        });
        assert_eq!(
            usage(&raw),
            Some(Usage {
                input: 20,
                output: 2,
                read: 12,
                write: 0,
            })
        );
    }

    /// 子代理文件与主会话文件进入同一条解码/去重/聚合管线。
    ///
    /// fixture 把子代理写在 `<project>/subagents/`（没有父会话目录层），父 id
    /// 因此推不出来 —— 该行必须被**提升为孤儿根行**（`kind="subagent"`、
    /// `parentKey=null`），而不是丢 token 或报错。
    #[test]
    fn source_counts_subagent_records_and_promotes_orphans() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-subagents-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        let orphan = project.join("subagents");
        fs::create_dir_all(&orphan).expect("create fixture dirs");
        // 两个文件各一条 13 token 的记录；模型不同，指纹因此不同（各自计入）。
        let record = |timestamp: i64, model_name: &str| {
            json!({
                "timestamp": timestamp,
                "providerData": {
                    "model": model_name,
                    "usage": { "inputTokens": 20, "outputTokens": 5 }
                },
                "message": { "usage": {
                    "input_tokens": 10,
                    "output_tokens": 3,
                    "cache_read_input_tokens": 4
                }}
            })
        };
        fs::write(
            project.join("session.jsonl"),
            format!("{}\nnot-json\n", record(now, "fixture-model")),
        )
        .expect("write fixture");
        fs::write(
            orphan.join("agent.jsonl"),
            format!("{}\n", record(now, "fixture-subagent-model")),
        )
        .expect("write subagent fixture");

        let result = source_from_roots(std::slice::from_ref(&root), "fixture", None, false, true);
        assert_eq!(result["filesScanned"], 2);
        assert_eq!(result["parseErrors"], 1);
        assert_eq!(result["summary"]["records"], 2);
        assert_eq!(result["summary"]["input"], 20);
        assert_eq!(result["summary"]["output"], 6);
        assert_eq!(result["summary"]["cacheRead"], 8);
        // 每条 10 + 3 + 0 = 13，合计 26。
        assert_eq!(result["summary"]["total"], 26);
        assert_eq!(result["summary"]["mainTotal"], 13);
        assert_eq!(result["summary"]["subagentTotal"], 13);
        assert_eq!(result["summary"]["subagentFiles"], 1);
        assert_eq!(result["summary"]["subagentSessions"], 1);
        // `daily` 必须与 `summary` 同口径拆分，否则四档范围会互相矛盾。
        let daily = result["daily"].as_array().expect("daily points");
        assert_eq!(daily.len(), 1);
        assert_eq!(daily[0]["total"], 26);
        assert_eq!(daily[0]["mainTotal"], 13);
        assert_eq!(daily[0]["subagentTotal"], 13);
        // 模型 / 项目分布自动包含子代理，无需额外改动。
        assert_eq!(result["projects"].as_array().map(Vec::len), Some(1));
        assert_eq!(result["projects"][0]["total"], 26);
        assert_eq!(result["models"].as_array().map(Vec::len), Some(2));

        let sessions = result["sessions"].as_array().expect("session groups");
        assert_eq!(sessions.len(), 2);
        let subagent = sessions
            .iter()
            .find(|session| session["kind"] == "subagent")
            .expect("subagent row kept as an orphan root");
        assert_eq!(subagent["sessionId"], "agent");
        assert_eq!(subagent["agentId"], "agent");
        assert_eq!(subagent["parentSessionId"], "fixture-project");
        assert!(subagent["parentKey"].is_null());
        assert!(subagent["title"].is_null());
        assert_eq!(subagent["total"], 13);
        let main = sessions
            .iter()
            .find(|session| session["kind"] == "main")
            .expect("main row");
        assert!(main["parentKey"].is_null());
        assert_eq!(main["total"], 13);
        assert_eq!(main["subagentSessionCount"], 0);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    /// 硬开关：`include_subagents = false` 时与改造前逐位一致（四个 source 隔离）。
    #[test]
    fn source_ignores_subagents_when_the_switch_is_off() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-no-subagents-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        let subagents = project.join("parent-session").join("subagents");
        fs::create_dir_all(&subagents).expect("create fixture dirs");
        let record = |input: u64| {
            json!({
                "timestamp": now,
                "message": { "usage": { "input_tokens": input, "output_tokens": 3 } }
            })
        };
        fs::write(
            project.join("parent-session.jsonl"),
            format!("{}\n", record(10)),
        )
        .expect("write fixture");
        fs::write(
            subagents.join("agent-aaaabbbb-cccc-dddd-eeee-ffff00001111.jsonl"),
            format!("{}\n", record(99)),
        )
        .expect("write subagent fixture");

        let result = source_from_roots(std::slice::from_ref(&root), "fixture", None, false, false);
        assert_eq!(result["filesScanned"], 1);
        assert_eq!(result["parseErrors"], 0);
        assert_eq!(result["summary"]["records"], 1);
        assert_eq!(result["summary"]["input"], 10);
        assert_eq!(result["summary"]["output"], 3);
        assert_eq!(result["summary"]["cacheRead"], 0);
        assert_eq!(result["summary"]["cacheWrite"], 0);
        assert_eq!(result["summary"]["uncachedInput"], 10);
        assert_eq!(result["summary"]["total"], 13);
        // 无子代理数据的 source 必须自洽地报 0，而不是省略键。
        assert_eq!(result["summary"]["mainTotal"], 13);
        assert_eq!(result["summary"]["subagentTotal"], 0);
        assert_eq!(result["summary"]["subagentFiles"], 0);
        assert_eq!(result["summary"]["subagentSessions"], 0);
        assert_eq!(result["sessions"].as_array().map(Vec::len), Some(1));
        assert_eq!(result["sessions"][0]["kind"], "main");
        assert_eq!(result["models"].as_array().map(Vec::len), Some(1));
        assert_eq!(result["projects"][0]["total"], 13);
        assert_eq!(result["daily"][0]["subagentTotal"], 0);

        // CodeBuddy CLI 走的 `source()` 必须与该开关逐位一致。
        let single = source(root.clone(), "fixture", None, false);
        assert_eq!(single["filesScanned"], result["filesScanned"]);
        assert_eq!(single["summary"], result["summary"]);
        assert_eq!(single["daily"], result["daily"]);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    /// 父归属必须按**路径首段**（`dir_project`）匹配，不能按 `project`：
    /// 父行的 `project` 来自记录 cwd，子代理的 cwd 可能属于另一个仓库。
    #[test]
    fn subagent_attaches_to_parent_by_directory_not_by_record_project() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-parent-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        let subagents = project.join("parent-session").join("subagents");
        fs::create_dir_all(&subagents).expect("create fixture dirs");
        let record = |input: u64, cwd: &str| {
            json!({
                "timestamp": now,
                "cwd": cwd,
                "message": { "usage": { "input_tokens": input, "output_tokens": 0 } }
            })
        };
        fs::write(
            project.join("parent-session.jsonl"),
            format!("{}\n", record(10, "/fixture/another-repo")),
        )
        .expect("write parent fixture");
        fs::write(
            subagents.join("agent-11112222-3333-4444-5555-666677778888.jsonl"),
            format!("{}\n", record(90, "/fixture/fixture-project")),
        )
        .expect("write subagent fixture");

        let result = source_from_roots(std::slice::from_ref(&root), "fixture", None, false, true);
        let sessions = result["sessions"].as_array().expect("session groups");
        assert_eq!(sessions.len(), 2);
        let parent = sessions
            .iter()
            .find(|session| session["sessionId"] == "parent-session")
            .expect("parent row");
        let child = sessions
            .iter()
            .find(|session| session["kind"] == "subagent")
            .expect("subagent row");
        // 两个 project 字符串不同：按 project 匹配会漏配成孤儿。
        assert_eq!(parent["project"], "another-repo");
        assert_eq!(child["project"], "fixture-project");
        assert_eq!(child["parentKey"], parent["key"]);
        assert_eq!(parent["subagentTotal"], 90);
        assert_eq!(parent["subagentSessionCount"], 1);
        assert_eq!(parent["subagentFileCount"], 1);
        assert_eq!(parent["subagentEmptyCount"], 0);
        // 子行 token 不重复计入父行自身。
        assert_eq!(parent["total"], 10);
        assert_eq!(child["total"], 90);
        assert_eq!(child["subagentTotal"], 0);
        // 汇总恒等：mainTotal + subagentTotal == total。
        assert_eq!(result["summary"]["mainTotal"], 10);
        assert_eq!(result["summary"]["subagentTotal"], 90);
        assert_eq!(result["summary"]["total"], 100);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    /// 标签 4 级降级各一条，并验证响应里不存在任务正文。
    #[test]
    fn subagent_labels_follow_the_four_level_fallback() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-labels-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        let subagents = project.join("parent-session").join("subagents");
        fs::create_dir_all(&subagents).expect("create fixture dirs");
        let usage_record = |timestamp: i64| {
            json!({
                "timestamp": timestamp,
                "message": { "usage": { "input_tokens": 1, "output_tokens": 0 } }
            })
        };
        // 父会话：两条 Agent 派发（一条带 description、一条只有类型）。
        // `arguments` 是 JSON **字符串**，且带 prompt（禁止读取）。
        let arguments = |value: Value| serde_json::to_string(&value).expect("serialize arguments");
        fs::write(
            project.join("parent-session.jsonl"),
            format!(
                "{}\n{}\n{}\n{}\n{}\n",
                usage_record(now),
                json!({
                    "type": "function_call",
                    "name": "Agent",
                    "callId": "call-labelled",
                    "arguments": arguments(json!({
                        "subagent_type": "Explore",
                        "description": "查旧版检查项实现",
                        "prompt": "这段任务正文不得进入响应"
                    }))
                }),
                json!({
                    "type": "function_call_result",
                    "name": "Agent",
                    "callId": "call-labelled",
                    "output": {
                        "type": "text",
                        "text": "Spawned successfully.\nagent_id: Explore@fixture\ntask_id: agent-aaaa1111-2222-3333-4444-555566667777\n"
                    }
                }),
                json!({
                    "type": "function_call",
                    "name": "Agent",
                    "callId": "call-typed",
                    "arguments": arguments(json!({ "subagent_type": "software-engineer" }))
                }),
                json!({
                    "type": "function_call_result",
                    "name": "Agent",
                    "callId": "call-typed",
                    "output": "task_id: agent-bbbb2222-3333-4444-5555-666677778888"
                }),
            ),
        )
        .expect("write parent fixture");
        let agent_file = |stem: &str| subagents.join(format!("{stem}.jsonl"));
        let labelled = "agent-aaaa1111-2222-3333-4444-555566667777";
        let typed = "agent-bbbb2222-3333-4444-5555-666677778888";
        let unlabelled = "agent-cccc3333-4444-5555-6666-777788889999";
        for (index, stem) in [labelled, typed, unlabelled].iter().enumerate() {
            fs::write(
                agent_file(stem),
                format!("{}\n", usage_record(now - 1_000 * (index as i64 + 1))),
            )
            .expect("write subagent fixture");
        }
        // stem 不是 `agent-` 形状 -> 第 4 级；序号取同父内 mtime 升序。
        fs::write(
            agent_file("sub"),
            format!("{}\n", usage_record(now - 10_000)),
        )
        .expect("write unnamed subagent fixture");
        let base = std::time::SystemTime::now();
        for (offset, stem) in [labelled, typed, unlabelled, "sub"].iter().enumerate() {
            pin_mtime(
                agent_file(stem),
                base - std::time::Duration::from_secs(40 - 10 * offset as u64),
            );
        }

        let result = source_from_roots(std::slice::from_ref(&root), "fixture", None, false, true);
        assert_eq!(result["parseErrors"], 0);
        let sessions = result["sessions"].as_array().expect("session groups");
        let by_id = |session_id: &str| {
            sessions
                .iter()
                .find(|session| session["sessionId"] == session_id)
                .expect("subagent row by id")
        };
        // 1 级：type · description
        assert_eq!(by_id(labelled)["agentType"], "Explore");
        assert_eq!(by_id(labelled)["agentLabel"], "Explore · 查旧版检查项实现");
        // 2 级：type · agent-xxxxxxxx
        assert_eq!(by_id(typed)["agentType"], "software-engineer");
        assert_eq!(
            by_id(typed)["agentLabel"],
            "software-engineer · agent-bbbb2222"
        );
        // 3 级：无类型信息
        assert!(by_id(unlabelled)["agentType"].is_null());
        assert_eq!(by_id(unlabelled)["agentLabel"], "子代理 · agent-cccc3333");
        // 4 级：stem 不是 agent- 形状，同父内 mtime 升序第 4 个
        assert!(by_id("sub")["agentType"].is_null());
        assert_eq!(by_id("sub")["agentLabel"], "子代理 #4");
        // 隐私红线：响应里不得出现 `prompt` 键或任务正文。
        let serialized = serde_json::to_string(&result).expect("serialize response");
        assert!(!serialized.contains("prompt"));
        assert!(!serialized.contains("不得进入响应"));
        assert!(!serialized.contains("Spawned successfully"));

        fs::remove_dir_all(root).expect("remove fixture");
    }

    /// `arguments` 是坏 JSON：静默降级到第 3 级，**不得**抬高 `parseErrors`。
    #[test]
    fn invalid_agent_arguments_degrade_silently() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-bad-args-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        let subagents = project.join("parent-session").join("subagents");
        fs::create_dir_all(&subagents).expect("create fixture dirs");
        let agent_id = "agent-dddd4444-5555-6666-7777-888899990000";
        fs::write(
            project.join("parent-session.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                json!({
                    "timestamp": now,
                    "message": { "usage": { "input_tokens": 1, "output_tokens": 0 } }
                }),
                json!({
                    "type": "function_call",
                    "name": "Agent",
                    "callId": "call-broken",
                    "arguments": "{坏 JSON"
                }),
                json!({
                    "type": "function_call_result",
                    "name": "Agent",
                    "callId": "call-broken",
                    "output": { "type": "text", "text": format!("task_id: {agent_id}") }
                }),
            ),
        )
        .expect("write parent fixture");
        fs::write(
            subagents.join(format!("{agent_id}.jsonl")),
            format!(
                "{}\n",
                json!({
                    "timestamp": now - 1_000,
                    "message": { "usage": { "input_tokens": 5, "output_tokens": 0 } }
                })
            ),
        )
        .expect("write subagent fixture");

        let result = source_from_roots(std::slice::from_ref(&root), "fixture", None, false, true);
        assert_eq!(result["parseErrors"], 0);
        let sessions = result["sessions"].as_array().expect("session groups");
        let child = sessions
            .iter()
            .find(|session| session["kind"] == "subagent")
            .expect("subagent row");
        assert!(child["agentType"].is_null());
        assert_eq!(child["agentLabel"], "子代理 · agent-dddd4444");

        fs::remove_dir_all(root).expect("remove fixture");
    }

    /// 空子代理（有文件、无 usage）不建行，只在父行上计数（P1-4）。
    #[test]
    fn empty_subagent_files_are_counted_but_create_no_rows() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-empty-subagent-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        let subagents = project.join("parent-session").join("subagents");
        fs::create_dir_all(&subagents).expect("create fixture dirs");
        fs::write(
            project.join("parent-session.jsonl"),
            format!(
                "{}\n",
                json!({
                    "timestamp": now,
                    "message": { "usage": { "input_tokens": 10, "output_tokens": 2 } }
                })
            ),
        )
        .expect("write parent fixture");
        // 有文件、无 usage：不新增排行行，但父行必须报出来。
        fs::write(
            subagents.join("agent-eeee5555-6666-7777-8888-999900001111.jsonl"),
            "",
        )
        .expect("write empty subagent fixture");

        let result = source_from_roots(std::slice::from_ref(&root), "fixture", None, false, true);
        assert_eq!(result["filesScanned"], 2);
        assert_eq!(result["parseErrors"], 0);
        let sessions = result["sessions"].as_array().expect("session groups");
        assert_eq!(sessions.len(), 1);
        let parent = &sessions[0];
        assert_eq!(parent["kind"], "main");
        assert_eq!(parent["subagentTotal"], 0);
        assert_eq!(parent["subagentSessionCount"], 0);
        assert_eq!(parent["subagentFileCount"], 1);
        assert_eq!(parent["subagentEmptyCount"], 1);
        assert_eq!(result["summary"]["subagentFiles"], 1);
        assert_eq!(result["summary"]["subagentSessions"], 0);
        assert_eq!(result["summary"]["subagentTotal"], 0);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn source_deduplicates_copied_session_history() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-fork-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let record = json!({
            "timestamp": now,
            "cwd": "/fixture/example-project",
            "message": { "usage": { "input_tokens": 10, "output_tokens": 3 } }
        });
        // The copied session replays the same usage record under a new file.
        fs::write(
            project.join("session-original.jsonl"),
            format!("{}\n", record),
        )
        .expect("write original fixture");
        fs::write(
            project.join("session-forked.jsonl"),
            format!(
                "{}\n{}\n",
                record,
                json!({
                    "timestamp": now + 1_000,
                    "message": { "usage": { "input_tokens": 7, "output_tokens": 2 } }
                })
            ),
        )
        .expect("write forked fixture");
        // Back-to-back writes can land in the same millisecond, and the
        // millisecond mtime key leaves ordering to the filesystem's directory
        // iteration, which is not sorted. The copy would then be processed
        // first, own the replayed record, and leave the original with zero
        // records. Pin the mtimes so the original always precedes its copy.
        pin_mtime(
            project.join("session-original.jsonl"),
            std::time::SystemTime::now() - std::time::Duration::from_secs(60),
        );
        pin_mtime(
            project.join("session-forked.jsonl"),
            std::time::SystemTime::now(),
        );

        let result = source(root.clone(), "fixture", None, false);
        // The replayed record counts once; the fork's new record still counts.
        assert_eq!(result["summary"]["records"], 2);
        assert_eq!(result["summary"]["input"], 17);
        assert_eq!(result["summary"]["output"], 5);
        let sessions = result["sessions"].as_array().expect("session groups");
        assert_eq!(sessions.len(), 2);
        let forked = sessions
            .iter()
            .find(|session| session["sessionId"] == "session-forked")
            .expect("forked session kept its new record");
        assert_eq!(forked["input"], 7);
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn bounded_source_excludes_records_before_cutoff() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-range-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let record = |timestamp| {
            json!({
                "timestamp": timestamp,
                "cwd": "/fixture/example-project",
                "message": { "usage": { "input_tokens": 10, "output_tokens": 2 } }
            })
        };
        fs::write(
            project.join("session.jsonl"),
            format!("{}\n{}\n", record(now - 10_000), record(now - 100_000)),
        )
        .expect("write fixture");

        let result = source(root.clone(), "fixture", Some(now - 50_000), false);
        assert_eq!(result["summary"]["records"], 1);
        assert_eq!(result["summary"]["input"], 10);
        assert_eq!(result["projects"][0]["key"], "example-project");
        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn session_titles_are_file_scoped_and_independent_from_usage_cutoff() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-titles-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let usage_record = |input| {
            json!({
                "timestamp": now,
                "cwd": "/private/example-project",
                "message": { "usage": { "input_tokens": input, "output_tokens": 2 } }
            })
        };

        fs::write(
            project.join("session-a.jsonl"),
            format!(
                "{}\n{}\n{}\n{}\n",
                json!({ "type": "summary", "summary": "摘要不应覆盖 AI 标题" }),
                json!({ "type": "ai-title", "aiTitle": "旧标题" }),
                usage_record(10),
                json!({ "type": "ai-title", "aiTitle": "最新 AI 标题" }),
            ),
        )
        .expect("write ai title fixture");
        fs::write(
            project.join("session-b.jsonl"),
            format!(
                "{}\n{}\n",
                json!({
                    "type": "ai-title",
                    "timestamp": now - 100_000,
                    "aiTitle": "范围外保留标题"
                }),
                usage_record(20),
            ),
        )
        .expect("write cutoff title fixture");
        fs::write(
            project.join("session-c.jsonl"),
            format!(
                "{}\n{}\n",
                usage_record(30),
                json!({ "type": "summary", "summary": "摘要回退标题" }),
            ),
        )
        .expect("write summary fixture");
        fs::write(
            project.join("session-d.jsonl"),
            format!("{}\n", usage_record(40)),
        )
        .expect("write untitled fixture");
        fs::write(
            project.join("session-e.jsonl"),
            format!(
                "{}\n{}\n",
                json!({ "type": "ai-title", "aiTitle": "最新 AI 标题" }),
                usage_record(50),
            ),
        )
        .expect("write duplicate title fixture");

        let result = source(root.clone(), "fixture", Some(now - 50_000), false);
        let sessions = result["sessions"].as_array().expect("session groups");
        let by_id = |session_id: &str| {
            sessions
                .iter()
                .find(|session| session["sessionId"] == session_id)
                .expect("session group by id")
        };

        assert_eq!(result["summary"]["input"], 150);
        assert_eq!(result["summary"]["records"], 5);
        assert_eq!(sessions.len(), 5);
        assert_eq!(by_id("session-a")["title"], "最新 AI 标题");
        assert_eq!(by_id("session-b")["title"], "范围外保留标题");
        assert_eq!(by_id("session-c")["title"], "摘要回退标题");
        assert!(by_id("session-d")["title"].is_null());
        assert_eq!(by_id("session-a")["project"], "example-project");
        assert_ne!(by_id("session-a")["key"], by_id("session-e")["key"]);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_key_only_present_for_detail_sources() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-key-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        fs::write(
            project.join("session.jsonl"),
            format!(
                "{}\n",
                json!({
                    "timestamp": now,
                    "message": { "usage": { "input_tokens": 10, "output_tokens": 3 } }
                })
            ),
        )
        .expect("write fixture");

        let aggregate_only = source(root.clone(), "workbuddy", None, false);
        assert!(aggregate_only.get("requests").is_none());

        let detailed = source(root.clone(), "codebuddy-cli", None, true);
        let rows = detailed["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), 1);
        // 明细只是附加键，聚合数值与不采集明细时完全一致。
        assert_eq!(detailed["summary"], aggregate_only["summary"]);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_rows_share_the_total_formula_and_sort_newest_first() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-rows-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let record = |timestamp, input, write| {
            json!({
                "timestamp": timestamp,
                "cwd": "/private/example-project",
                "providerData": { "model": "fixture-model" },
                "message": { "usage": {
                    "input_tokens": input,
                    "output_tokens": 2,
                    "cache_read_input_tokens": 5,
                    "cache_write_input_tokens": write
                }}
            })
        };
        // 文件顺序刻意与时间顺序相反，排序必须来自 timestamp。
        fs::write(
            project.join("session-detail.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                record(now - 1_000, 10, 1),
                record(now - 2_000, 20, 2),
                record(now - 3_000, 30, 3),
            ),
        )
        .expect("write fixture");

        let result = source(root.clone(), "codebuddy-cli", None, true);
        assert_eq!(result["summary"]["records"], 3);
        // input 60 + output 6 + cacheWrite 6；cacheRead 不计入合计。
        assert_eq!(result["summary"]["total"], 72);
        let rows = result["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), 3);
        assert_eq!(rows[0]["timestamp"], now - 1_000);
        assert_eq!(rows[2]["timestamp"], now - 3_000);
        assert_eq!(rows[0]["model"], "fixture-model");
        // 只暴露 cwd 的 basename，不返回绝对路径。
        assert_eq!(rows[0]["project"], "example-project");
        assert_eq!(rows[0]["sessionId"], "session-detail");
        assert!(rows[0]["title"].is_null());
        assert_eq!(rows[0]["input"], 10);
        assert_eq!(rows[0]["output"], 2);
        assert_eq!(rows[0]["cacheRead"], 5);
        assert_eq!(rows[0]["cacheWrite"], 1);
        assert_eq!(rows[0]["total"], 13);
        // 缓存未命中 = input - cacheRead，与 `summary` 同口径。
        assert_eq!(rows[0]["uncachedInput"], 5);
        // 无 providerData.rawUsage 时思考过程为 0。
        assert_eq!(rows[0]["thinking"], 0);
        let row_total: u64 = rows
            .iter()
            .map(|row| row["total"].as_u64().unwrap_or(0))
            .sum();
        assert_eq!(row_total, result["summary"]["total"].as_u64().unwrap_or(0));
        let row_uncached: u64 = rows
            .iter()
            .map(|row| row["uncachedInput"].as_u64().unwrap_or(0))
            .sum();
        assert_eq!(
            row_uncached,
            result["summary"]["uncachedInput"].as_u64().unwrap_or(0)
        );

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_window_keeps_only_the_newest_rows() {
        let row = |timestamp| RequestRow {
            timestamp,
            model: "fixture-model".to_string(),
            project: "fixture-project".to_string(),
            session_id: "fixture-session".to_string(),
            title: None,
            usage: Usage {
                input: 1,
                output: 1,
                read: 0,
                write: 0,
            },
            thinking: 0,
        };

        // 触达内存硬上限后只保留最新窗口，且按时间倒序。
        let mut collector = SourceCollector {
            collect_requests: true,
            ..SourceCollector::default()
        };
        for timestamp in 0..=(REQUEST_COLLECT_LIMIT as i64) {
            collector.push_request(row(timestamp));
        }
        assert_eq!(collector.requests.len(), REQUEST_WINDOW);
        assert_eq!(
            collector.requests[0].timestamp,
            REQUEST_COLLECT_LIMIT as i64
        );

        // 未触达硬上限时，输出窗口同样截断到最新 REQUEST_WINDOW 条。
        let mut bounded = SourceCollector {
            collect_requests: true,
            ..SourceCollector::default()
        };
        for timestamp in 0..(REQUEST_WINDOW as i64 + 100) {
            bounded.push_request(row(timestamp));
        }
        let value = bounded.into_value("fixture", 1);
        let rows = value["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), REQUEST_WINDOW);
        assert_eq!(rows[0]["timestamp"], REQUEST_WINDOW as i64 + 99);
        assert_eq!(rows[REQUEST_WINDOW - 1]["timestamp"], 100);

        // 关闭明细采集时不缓存任何行，也不输出 requests 键。
        let mut off = SourceCollector::default();
        off.push_request(row(1));
        assert!(off.into_value("fixture", 1).get("requests").is_none());
    }

    #[test]
    fn request_detail_window_stays_the_global_newest_after_any_arrival_order() {
        let row = |timestamp| RequestRow {
            timestamp,
            model: "fixture-model".to_string(),
            project: "fixture-project".to_string(),
            session_id: "fixture-session".to_string(),
            title: None,
            usage: Usage {
                input: 1,
                output: 1,
                read: 0,
                write: 0,
            },
            thinking: 0,
        };

        // 文件按 mtime 顺序处理、行内时间戳可以乱序：中途裁剪过之后，后到的
        // 旧行不能占位，后到的新行仍须进入窗口 —— 结果恒等于「全局最新 500」。
        let mut collector = SourceCollector {
            collect_requests: true,
            ..SourceCollector::default()
        };
        for timestamp in 0..=(REQUEST_COLLECT_LIMIT as i64) {
            collector.push_request(row(timestamp));
        }
        collector.push_request(row(-1));
        for timestamp in 10_000..10_100 {
            collector.push_request(row(timestamp));
        }

        let value = collector.into_value("fixture", 1);
        let rows = value["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), REQUEST_WINDOW);
        assert_eq!(rows[0]["timestamp"], 10_099);
        assert_eq!(rows[99]["timestamp"], 10_000);
        // 裁剪后残留的旧行必须让位：第 101 条是最新一轮保留里的最新一行。
        assert_eq!(rows[100]["timestamp"], REQUEST_COLLECT_LIMIT as i64);
        // 全局最新 500 = 100 条新行 + 上一轮保留里最新的 400 条（4000..=3601）。
        assert_eq!(rows[REQUEST_WINDOW - 1]["timestamp"], 3_601);
        assert!(rows.iter().all(|row| row["timestamp"] != -1));
    }

    #[test]
    fn request_detail_deduplicates_copied_session_history() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-fork-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let record = json!({
            "timestamp": now,
            "message": { "usage": { "input_tokens": 10, "output_tokens": 3 } }
        });
        fs::write(
            project.join("session-original.jsonl"),
            format!("{}\n", record),
        )
        .expect("write original fixture");
        fs::write(
            project.join("session-forked.jsonl"),
            format!(
                "{}\n{}\n",
                record,
                json!({
                    "timestamp": now + 1_000,
                    "message": { "usage": { "input_tokens": 7, "output_tokens": 2 } }
                })
            ),
        )
        .expect("write forked fixture");
        // 与聚合去重用例相同：固定 mtime，保证原始会话先于副本被处理。
        pin_mtime(
            project.join("session-original.jsonl"),
            std::time::SystemTime::now() - std::time::Duration::from_secs(60),
        );
        pin_mtime(
            project.join("session-forked.jsonl"),
            std::time::SystemTime::now(),
        );

        let result = source(root.clone(), "fixture", None, true);
        assert_eq!(result["summary"]["records"], 2);
        let rows = result["requests"].as_array().expect("request rows");
        // 复制会话重放的记录沿用同一指纹，明细里只出现一次。
        assert_eq!(rows.len(), 2);
        let inputs: Vec<u64> = rows
            .iter()
            .filter_map(|row| row["input"].as_u64())
            .collect();
        assert_eq!(inputs, [7, 10]);
        assert_eq!(rows[0]["sessionId"], "session-forked");
        assert_eq!(rows[1]["sessionId"], "session-original");

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_skips_records_without_timestamp() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-undated-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        fs::write(
            project.join("session.jsonl"),
            format!(
                "{}\n{}\n",
                json!({
                    "timestamp": now,
                    "message": { "usage": { "input_tokens": 10, "output_tokens": 3 } }
                }),
                // 没有 timestamp 的记录仍然计入聚合，但无法排序，不进明细。
                json!({ "message": { "usage": { "input_tokens": 90, "output_tokens": 9 } } }),
            ),
        )
        .expect("write fixture");

        let result = source(root.clone(), "fixture", None, true);
        assert_eq!(result["summary"]["records"], 2);
        assert_eq!(result["summary"]["input"], 100);
        let rows = result["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["input"], 10);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_backfills_file_title_after_usage() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-title-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        let record = |timestamp| {
            json!({
                "timestamp": timestamp,
                "message": { "usage": { "input_tokens": 10, "output_tokens": 3 } }
            })
        };
        fs::write(
            project.join("session.jsonl"),
            format!(
                "{}\n{}\n{}\n",
                json!({ "type": "summary", "summary": "摘要回退标题" }),
                record(now - 1_000),
                // aiTitle 出现在 usage 之后：文件读完后必须回填到所有明细行。
                json!({ "type": "ai-title", "aiTitle": "最新 AI 标题" }),
            ),
        )
        .expect("write fixture");

        let result = source(root.clone(), "fixture", None, true);
        let rows = result["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["title"], "最新 AI 标题");

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_thinking_prefers_completion_thinking_tokens() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-thinking-priority-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        fs::write(
            project.join("session.jsonl"),
            format!(
                "{}\n",
                json!({
                    "timestamp": now,
                    "message": { "usage": { "input_tokens": 10, "output_tokens": 40 } },
                    // 两个来源同时存在时取扁平的 completion_thinking_tokens。
                    "providerData": { "rawUsage": {
                        "completion_thinking_tokens": 7,
                        "completion_tokens_details": { "reasoning_tokens": 3 }
                    }}
                })
            ),
        )
        .expect("write fixture");

        let result = source(root.clone(), "fixture", None, true);
        let rows = result["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["thinking"], 7);
        assert_eq!(rows[0]["output"], 40);
        // 思考过程只是 output 的拆分，不改变合计口径。
        assert_eq!(rows[0]["total"], result["summary"]["total"]);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_thinking_falls_back_to_reasoning_tokens_and_defaults_to_zero() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-thinking-fallback-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        fs::write(
            project.join("session.jsonl"),
            format!(
                "{}\n{}\n",
                json!({
                    "timestamp": now,
                    "message": { "usage": { "input_tokens": 11, "output_tokens": 21 } },
                    // rawUsage 只有思考计数字段时取 OpenAI 形状的 reasoning_tokens。
                    "providerData": { "rawUsage": {
                        "completion_tokens_details": { "reasoning_tokens": 5 }
                    }}
                }),
                json!({
                    "timestamp": now - 1_000,
                    "message": { "usage": { "input_tokens": 10, "output_tokens": 20 } },
                    // rawUsage 缺少思考计数 -> 0；`reasoning` 是思考正文，
                    // 只用来验证它不会被当成计数读取（隐私红线）。
                    "providerData": {
                        "reasoning": "思考正文不得进入明细",
                        "rawUsage": { "prompt_cache_write_tokens": 1 }
                    }
                })
            ),
        )
        .expect("write fixture");

        let result = source(root.clone(), "fixture", None, true);
        let rows = result["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[0]["thinking"], 5);
        assert_eq!(rows[1]["thinking"], 0);
        assert_eq!(rows[1]["cacheWrite"], 1);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn request_detail_thinking_over_output_saturates_reply_to_zero() {
        // 实测存在 thinking > output 的异常记录：后端如实返回两个原始计数，
        // 前端的 `output.saturating_sub(thinking)` 必须得到 0 而不是负数。
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-detail-thinking-overflow-{}-{now}",
            std::process::id()
        ));
        let project = root.join("fixture-project");
        fs::create_dir_all(&project).expect("create fixture dirs");
        fs::write(
            project.join("session.jsonl"),
            format!(
                "{}\n",
                json!({
                    "timestamp": now,
                    "message": { "usage": { "input_tokens": 10, "output_tokens": 2 } },
                    "providerData": { "rawUsage": { "completion_thinking_tokens": 9 } }
                })
            ),
        )
        .expect("write fixture");

        let result = source(root.clone(), "fixture", None, true);
        let rows = result["requests"].as_array().expect("request rows");
        assert_eq!(rows.len(), 1);
        assert_eq!(rows[0]["output"], 2);
        assert_eq!(rows[0]["thinking"], 9);
        let reply = rows[0]["output"]
            .as_u64()
            .unwrap_or(0)
            .saturating_sub(rows[0]["thinking"].as_u64().unwrap_or(0));
        assert_eq!(reply, 0);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn ide_source_reads_request_usage_and_skips_message_bodies() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-ide-{}-{now}",
            std::process::id()
        ));
        let history = root
            .join("uid")
            .join("CodeBuddyIDE")
            .join("uid")
            .join("history")
            .join("workspace-hash");
        let conv_a = history.join("conv-a");
        let conv_b = history.join("conv-b");
        let messages = conv_a.join("messages");
        fs::create_dir_all(&messages).expect("create ide fixture dirs");
        fs::create_dir_all(&conv_b).expect("create second conversation");

        fs::write(
            history.join("index.json"),
            json!({
                "conversations": [
                    {
                        "id": "conv-a",
                        "name": "IDE 会话标题",
                        "selectedModelId": "deepseek-v4-flash"
                    },
                    {
                        "id": "conv-b",
                        "name": "范围外会话",
                        "selectedModelId": "hy4-preview"
                    }
                ]
            })
            .to_string(),
        )
        .expect("write workspace index");
        fs::write(
            conv_a.join("index.json"),
            json!({
                "messages": [{ "id": "m1", "role": "assistant", "isComplete": true }],
                "requests": [{
                    "id": "req-1",
                    "state": "complete",
                    "startedAt": now,
                    "usage": {
                        "inputTokens": 100,
                        "outputTokens": 20,
                        "cacheTokens": 40,
                        "cachedWriteTokens": 5
                    }
                }]
            })
            .to_string(),
        )
        .expect("write conversation index");
        fs::write(
            conv_b.join("index.json"),
            json!({
                "requests": [{
                    "id": "req-old",
                    "state": "complete",
                    "startedAt": now - 100_000,
                    "usage": {
                        "inputTokens": 999,
                        "outputTokens": 9,
                        "cacheTokens": 1,
                        "cachedWriteTokens": 0
                    }
                }]
            })
            .to_string(),
        )
        .expect("write out-of-range conversation");
        fs::write(
            messages.join("ignored.json"),
            json!({
                "role": "assistant",
                "usage": { "inputTokens": 10_000, "outputTokens": 10_000 }
            })
            .to_string(),
        )
        .expect("write ignored message body");

        let mut projects = HashMap::new();
        projects.insert("conv-a".to_string(), "example-project".to_string());
        let result = ide_source(root.clone(), "codebuddy-ide", Some(now - 50_000), &projects);

        assert_eq!(result["source"], "codebuddy-ide");
        assert_eq!(result["filesScanned"], 2);
        assert_eq!(result["summary"]["records"], 1);
        assert_eq!(result["summary"]["input"], 100);
        assert_eq!(result["summary"]["output"], 20);
        assert_eq!(result["summary"]["cacheRead"], 40);
        assert_eq!(result["summary"]["cacheWrite"], 5);
        assert_eq!(result["summary"]["total"], 125);
        assert_eq!(result["summary"]["uncachedInput"], 60);
        assert_eq!(result["models"][0]["key"], "deepseek-v4-flash");
        assert_eq!(result["projects"][0]["key"], "example-project");
        assert_eq!(result["sessions"][0]["sessionId"], "conv-a");
        assert_eq!(result["sessions"][0]["title"], "IDE 会话标题");
        assert_eq!(result["sessions"].as_array().map(Vec::len), Some(1));
        // IDE 来源不是明细来源，响应形状保持不变。
        assert!(result.get("requests").is_none());
        let today = chrono::Local::now().format("%Y-%m-%d").to_string();
        assert_eq!(result["dailyByModel"]["deepseek-v4-flash"][0]["key"], today);

        fs::remove_dir_all(root).expect("remove fixture");
    }

    /// 切号复制会让同一会话散落在多个账号数据目录里：副本整份重放父历史（请求连时间戳
    /// 一起逐字复制）后各自继续追加，会话 id 还会被重随机。统计必须按请求指纹取**并集**：
    /// 父历史不重复累计，各副本复制后各自追加的请求也不丢。
    #[test]
    fn ide_source_merges_copied_session_history_as_a_union() {
        let now = crate::modules::config::now_ms();
        let root = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-ide-copy-{}-{now}",
            std::process::id()
        ));
        let conversation = |uid: &str, conv: &str| {
            root.join(uid)
                .join("VSCode")
                .join(uid)
                .join("history")
                .join("workspace-hash")
                .join(conv)
        };
        let original = conversation("uid-a", "conv-a");
        // 插件侧副本取新 id：目录名与源不同，但起点请求相同。
        let replay = conversation("uid-b", "conv-replay");
        // 另一个账号从同一份历史往下走：两份分叉后各自独有的请求都要保留。
        let fork = conversation("uid-c", "conv-fork");
        for dir in [&original, &replay, &fork] {
            fs::create_dir_all(dir).expect("create conversation dirs");
        }
        let request = |id: &str, input: u64| {
            json!({
                "id": id,
                "state": "complete",
                "startedAt": now,
                "usage": {
                    "inputTokens": input,
                    "outputTokens": 0,
                    "cacheTokens": 0,
                    "cachedWriteTokens": 0
                }
            })
        };
        fs::write(
            original.join("index.json"),
            json!({ "requests": [request("req-1", 100)] }).to_string(),
        )
        .expect("write original conversation");
        fs::write(
            replay.join("index.json"),
            json!({ "requests": [request("req-1", 100), request("req-2", 200)] }).to_string(),
        )
        .expect("write replayed conversation");
        fs::write(
            fork.join("index.json"),
            json!({ "requests": [request("req-1", 100), request("req-3", 400)] }).to_string(),
        )
        .expect("write forked conversation");

        let result = ide_source(root.clone(), "codebuddy-ide", None, &HashMap::new());

        assert_eq!(result["filesScanned"], 3);
        // 并集：100 + 200 + 400；重放的 req-1 只算一次，分叉两边独有的请求都在。
        assert_eq!(result["summary"]["records"], 3);
        assert_eq!(result["summary"]["input"], 700);
        assert_eq!(result["summary"]["total"], 700);
        // 同一个会话只出一行，不出现重复副本。
        assert_eq!(result["sessions"].as_array().map(Vec::len), Some(1));

        fs::remove_dir_all(root).expect("remove fixture");
    }

    #[test]
    fn decode_genie_workspace_recovers_unix_project_path() {
        assert_eq!(
            decode_genie_workspace("L1VzZXJzL2FwcGxlL0RvY3VtZW50cy9Qcm9qZWN0L215LWFnZW50")
                .as_deref(),
            Some("/Users/apple/Documents/Project/my-agent")
        );
    }

    #[test]
    fn get_statistics_returns_four_isolated_sources() {
        let value = get_statistics(None);
        let sources = value["sources"].as_array().expect("sources");
        let names: Vec<_> = sources
            .iter()
            .map(|source| source["source"].as_str().unwrap_or_default())
            .collect();
        assert_eq!(
            names,
            [
                "workbuddy",
                "workbuddy-ai",
                "codebuddy-cli",
                "codebuddy-ide"
            ]
        );
        // 国际版与国内版是两个独立 source，不合并。
        assert_ne!(sources[0]["source"], sources[1]["source"]);
    }

    /// 国际版数据根缺少 jsonl 根时返回空集，不报错。
    #[test]
    fn ai_source_is_empty_and_error_free_when_roots_are_missing() {
        let value = source_from_roots(&[], "workbuddy-ai", None, false, true);
        assert_eq!(value["source"], "workbuddy-ai");
        assert_eq!(value["filesScanned"], 0);
        assert_eq!(value["summary"]["input"], 0);
        assert_eq!(value["parseErrors"], 0);
        assert_eq!(value["sessions"].as_array().map(Vec::len), Some(0));
        assert_eq!(value["projects"].as_array().map(Vec::len), Some(0));
    }

    /// 多根共享同一个指纹集：国际版两个根之间重放的同一请求只计一次，
    /// 同时第二个根自身的新记录仍要计入（证明第二个根确实被扫了）。
    #[test]
    fn multi_root_source_deduplicates_across_roots() {
        let now = crate::modules::config::now_ms();
        let base = std::env::temp_dir().join(format!(
            "wb-switch-token-stats-multiroot-{}-{now}",
            std::process::id()
        ));
        let projects = base.join("projects");
        let sessions = base.join("sessions");
        fs::create_dir_all(&projects).expect("create projects root");
        fs::create_dir_all(&sessions).expect("create sessions root");
        let replayed = json!({
            "timestamp": now,
            "cwd": "/fixture/example-project",
            "message": { "usage": { "input_tokens": 10, "output_tokens": 3 } }
        });
        fs::write(
            projects.join("session-original.jsonl"),
            format!("{}\n", replayed),
        )
        .expect("write original fixture");
        // 第二个根里的会话重放了同一个请求，并另有一条自己的新请求。
        fs::write(
            sessions.join("session-root-two.jsonl"),
            format!(
                "{}\n{}\n",
                replayed,
                json!({
                    "timestamp": now + 1_000,
                    "message": { "usage": { "input_tokens": 7, "output_tokens": 2 } }
                })
            ),
        )
        .expect("write second-root fixture");
        // 与 source_deduplicates_copied_session_history 同理：毫秒级 mtime 并列时
        // 顺序退化为 readdir，重放记录可能先被第二个根认领。pin 住 mtime 让
        // 原始会话先处理，断言才稳定。
        pin_mtime(
            projects.join("session-original.jsonl"),
            std::time::SystemTime::now() - std::time::Duration::from_secs(60),
        );
        pin_mtime(
            sessions.join("session-root-two.jsonl"),
            std::time::SystemTime::now(),
        );

        let result = source_from_roots(
            &[projects.clone(), sessions.clone()],
            "workbuddy-ai",
            None,
            false,
            true,
        );
        // 两个根都被扫到；重放记录计一次，第二个根的新记录另计一次。
        assert_eq!(result["filesScanned"], 2);
        assert_eq!(result["summary"]["records"], 2);
        assert_eq!(result["summary"]["input"], 17);
        assert_eq!(result["summary"]["output"], 5);
        fs::remove_dir_all(base).expect("remove fixture");
    }

    /// 候选根按档位不同：国内版单根，国际版双根（无 projects/ 时只扫 sessions/）。
    #[test]
    fn jsonl_root_candidates_differ_by_variant() {
        let root = PathBuf::from("/tmp/wb-root");
        assert_eq!(
            jsonl_root_candidates(WbVariant::Cn, &root),
            vec![PathBuf::from("/tmp/wb-root/projects")]
        );
        assert_eq!(
            jsonl_root_candidates(WbVariant::Ai, &root),
            vec![
                PathBuf::from("/tmp/wb-root/projects"),
                PathBuf::from("/tmp/wb-root/sessions")
            ]
        );
    }
}
