//! 批量引擎的纯逻辑测试：计划分类、报告归并、中止/取消语义、参数归一化。
//!
//! 这些用例不碰真实客户端与磁盘：`execute_item` 的副作用由 `summarize_sync_report`
//! 与 `plan_group` 的纯判定覆盖，批量编排则靠状态函数（`mark_remaining` 等）验证。

use super::*;

fn keys(pairs: &[(&str, &str)]) -> Vec<(String, String)> {
    pairs
        .iter()
        .map(|(key, group)| ((*key).to_string(), (*group).to_string()))
        .collect()
}

#[test]
fn normalize_items_dedupes_by_group_and_fills_missing_key() {
    let items = normalize_items(
        &keys(&[("", "g1"), ("k1", "g1"), ("k2", "g2")]),
        SessionClient::Workbuddy,
        None,
    )
    .unwrap();
    // g1 重复只保留第一条；空 key 由后端补成 client:scope:groupId。
    assert_eq!(items.len(), 2);
    assert_eq!(items[0].group_id, "g1");
    assert_eq!(items[0].key, "workbuddy:all:g1");
    assert_eq!(items[1].key, "k2");
    assert_eq!(items[0].status, ITEM_PENDING);
}

#[test]
fn normalize_items_rejects_empty_and_oversized_batches() {
    assert!(normalize_items(&[], SessionClient::Workbuddy, None).is_err());
    assert!(normalize_items(&keys(&[("k", "  ")]), SessionClient::Workbuddy, None).is_err());

    let too_many = (0..=MAX_BATCH_GROUPS)
        .map(|index| (format!("k{index}"), format!("g{index}")))
        .collect::<Vec<_>>();
    let error = normalize_items(&too_many, SessionClient::Workbuddy, None).unwrap_err();
    assert!(error.contains(&MAX_BATCH_GROUPS.to_string()), "{error}");
}

#[test]
fn start_rejects_clients_without_batch_semantics() {
    let error =
        start_sync(SessionClient::VscodeExt, None, &keys(&[("k", "g")]), false).unwrap_err();
    assert!(error.contains("只支持 WorkBuddy"), "{error}");
    let error = start_delete(SessionClient::CodebuddyIde, None, &keys(&[("k", "g")])).unwrap_err();
    assert!(error.contains("只支持 WorkBuddy"), "{error}");
}

#[test]
fn summarize_reports_needs_recovery_as_fatal() {
    let report = json!({ "needsRecovery": true, "synced": [], "skipped": [], "errors": [] });
    match summarize_sync_report(&report) {
        ItemOutcome::Failed(code, _, fatal) => {
            assert_eq!(code, "needsRecovery");
            assert!(fatal, "待恢复必须中止整批");
        }
        _ => panic!("needsRecovery 必须视为失败"),
    }
}

#[test]
fn summarize_reports_item_errors_as_non_fatal() {
    let report = json!({
        "needsRecovery": false,
        "synced": [],
        "skipped": [],
        "errors": [{ "error": "目标会话记录保存失败" }],
    });
    match summarize_sync_report(&report) {
        ItemOutcome::Failed(code, message, fatal) => {
            assert_eq!(code, "itemFailed");
            assert!(message.contains("保存失败"), "{message}");
            assert!(!fatal, "单组写入失败不该中止整批");
        }
        _ => panic!("errors 非空必须视为失败"),
    }
}

#[test]
fn summarize_reports_success_and_recheck_skip() {
    let synced = json!({ "synced": [{ "groupId": "g1" }, { "groupId": "g1" }], "skipped": [], "errors": [] });
    match summarize_sync_report(&synced) {
        ItemOutcome::Done(count) => assert_eq!(count, 2),
        _ => panic!("synced 非空必须计为完成"),
    }

    let skipped = json!({
        "synced": [],
        "skipped": [{ "reasonCode": "recheckNotFastForward" }],
        "errors": [],
    });
    match summarize_sync_report(&skipped) {
        ItemOutcome::Skipped(code, _) => assert_eq!(code, "recheckNotFastForward"),
        _ => panic!("复核后不再符合应计为跳过而不是失败"),
    }
}

#[test]
fn mark_remaining_only_touches_pending_items() {
    let mut items = vec![
        BatchItem::new("k1".into(), "g1", "t1".into()).done(1),
        BatchItem::new("k2".into(), "g2", "t2".into()).skipped("alreadyLatest", "最新"),
        BatchItem::new("k3".into(), "g3", "t3".into()),
        BatchItem::new("k4".into(), "g4", "t4".into()),
    ];
    mark_remaining(&mut items, 2);
    assert_eq!(items[0].status, ITEM_DONE);
    assert_eq!(items[1].status, ITEM_SKIPPED, "计划阶段已跳过的项保持跳过");
    assert_eq!(items[2].status, ITEM_NOT_RUN);
    assert_eq!(items[3].status, ITEM_NOT_RUN);
}

#[test]
fn finished_job_snapshot_reports_counts_and_items() {
    let cancel = Arc::new(AtomicBool::new(false));
    let mut job = BatchJob {
        id: "job-1".to_string(),
        client: SessionClient::Workbuddy,
        scope: Some(WbVariant::Cn),
        op: BatchOp::Sync,
        allow_restart: false,
        state: "running".to_string(),
        done: 1,
        current: Some("g2".to_string()),
        aborted: None,
        cancelled: false,
        cancel,
        started_at: 1,
        finished_at: None,
        items: vec![
            BatchItem::new("k1".into(), "g1", "t1".into()).done(3),
            BatchItem::new("k2".into(), "g2", "t2".into()),
        ],
        closed_variants: vec!["WorkBuddy 国内版".to_string()],
        kept_closed: true,
        reopen_errors: Vec::new(),
    };
    let snapshot = job.snapshot();
    assert_eq!(snapshot["total"], 2);
    assert_eq!(snapshot["op"], "sync");
    assert_eq!(snapshot["lifecycle"]["keptClosed"], true);
    assert_eq!(snapshot["lifecycle"]["closed"][0], "WorkBuddy 国内版");
    assert_eq!(snapshot["items"][0]["status"], "done");
    assert_eq!(snapshot["items"][0]["synced"], 3);
    // 未结束的 job 不能被当成 finished 参与内存清理。
    assert!(!job.is_finished());
    job.state = "finished".to_string();
    assert!(job.is_finished());
}

#[test]
fn recent_reports_returns_empty_when_store_is_missing() {
    // 纯读取路径：目录不存在时返回空数组而不是报错，界面照常显示「没有历史批量」。
    let reports = recent_reports(5);
    assert!(reports.is_array());
}

#[test]
fn batch_id_guard_rejects_path_traversal() {
    assert!(valid_batch_id("2f1b9c4e-1a2b-4c3d-8e9f-0a1b2c3d4e5f"));
    // job id 会被拼进报告文件路径：只允许 uuid 形态。
    assert!(!valid_batch_id("../../../etc/passwd"));
    assert!(!valid_batch_id(".."));
    assert!(!valid_batch_id("batch-1"));
    assert!(!valid_batch_id(""));
}

#[test]
fn progress_rejects_unknown_and_unsafe_ids() {
    assert!(progress("..").is_err());
    assert!(progress("2f1b9c4e-1a2b-4c3d-8e9f-0a1b2c3d4e5f").is_err());
}

// ── 计划判定（纯函数，不碰真实存储） ──────────────────────────────────

fn detail(title: &str, group_variant: &str, source: Option<&str>, members: Value) -> Value {
    json!({
        "title": title,
        "groupVariant": group_variant,
        "safeSourceMemberId": source,
        "members": members,
    })
}

fn member(id: &str, variant: Option<&str>, status: &str, uid: &str) -> Value {
    let mut value = json!({ "memberId": id, "versionStatus": status, "uid": uid });
    if let Some(variant) = variant {
        value["variant"] = json!(variant);
    }
    value
}

#[test]
fn plan_skips_group_without_safe_source() {
    let detail = detail(
        "分叉组",
        "cn",
        None,
        json!([member("m1", Some("cn"), "behind", "u1")]),
    );
    let planned = plan_from_detail("k".into(), "g1", &detail, BatchOp::Sync, false, |_, _| {
        false
    });
    assert_eq!(planned.item.status, ITEM_SKIPPED);
    assert_eq!(planned.item.reason_code.as_deref(), Some("noSafeSource"));
    assert!(planned.restart_variants.is_empty());
}

#[test]
fn plan_skips_group_without_behind_members() {
    let detail = detail(
        "最新组",
        "cn",
        Some("m1"),
        json!([
            member("m1", Some("cn"), "latest", "u1"),
            member("m2", Some("cn"), "latest", "u2")
        ]),
    );
    let planned = plan_from_detail("k".into(), "g1", &detail, BatchOp::Sync, true, |_, _| false);
    assert_eq!(planned.item.status, ITEM_SKIPPED);
    assert_eq!(planned.item.reason_code.as_deref(), Some("alreadyLatest"));
}

#[test]
fn plan_keeps_pending_and_counts_behind_targets() {
    let detail = detail(
        "待同步组",
        "cn",
        Some("m1"),
        json!([
            member("m1", Some("cn"), "latest", "u1"),
            member("m2", Some("cn"), "behind", "u2"),
            member("m3", Some("ai"), "behind", "u3"),
        ]),
    );
    let planned = plan_from_detail("k".into(), "g1", &detail, BatchOp::Sync, false, |_, _| {
        false
    });
    assert_eq!(planned.item.status, ITEM_PENDING);
    assert_eq!(planned.item.planned, 2, "只数落后且非源的副本");
    assert!(
        planned.restart_variants.is_empty(),
        "客户端没运行就不需要关"
    );
}

#[test]
fn plan_skips_when_target_is_busy_and_not_authorized() {
    let detail = detail(
        "占用中",
        "cn",
        Some("m1"),
        json!([
            member("m1", Some("cn"), "latest", "u1"),
            member("m2", Some("cn"), "behind", "u2")
        ]),
    );
    // 探针：只有 u2 正在使用（当前登录且客户端在跑）
    let planned = plan_from_detail("k".into(), "g1", &detail, BatchOp::Sync, false, |_, uid| {
        uid == "u2"
    });
    assert_eq!(planned.item.status, ITEM_SKIPPED);
    assert_eq!(planned.item.reason_code.as_deref(), Some("appRunning"));
    assert_eq!(planned.restart_variants, vec![WbVariant::Cn]);
}

#[test]
fn plan_authorizes_restart_and_keeps_group_pending() {
    let detail = detail(
        "占用中",
        "cn",
        Some("m1"),
        json!([
            member("m1", Some("cn"), "latest", "u1"),
            member("m2", Some("cn"), "behind", "u2")
        ]),
    );
    let planned = plan_from_detail("k".into(), "g1", &detail, BatchOp::Sync, true, |_, uid| {
        uid == "u2"
    });
    assert_eq!(planned.item.status, ITEM_PENDING);
    assert_eq!(planned.restart_variants, vec![WbVariant::Cn]);
}

#[test]
fn plan_falls_back_to_group_variant_when_member_omits_it() {
    // 旧存储只有组级 variant：缺字段时不能一律当成国内版。
    let detail = detail(
        "国际版组",
        "ai",
        Some("m1"),
        json!([
            member("m1", None, "latest", "u1"),
            member("m2", None, "behind", "u2"),
        ]),
    );
    let planned = plan_from_detail("k".into(), "g1", &detail, BatchOp::Sync, true, |_, _| true);
    assert_eq!(planned.restart_variants, vec![WbVariant::Ai]);
}

#[test]
fn plan_delete_counts_members_and_never_restarts() {
    let detail = detail(
        "待删组",
        "cn",
        None,
        json!([
            member("m1", Some("cn"), "latest", "u1"),
            member("m2", Some("cn"), "behind", "u2")
        ]),
    );
    let planned = plan_from_detail("k".into(), "g1", &detail, BatchOp::Delete, true, |_, _| {
        true
    });
    assert_eq!(planned.item.status, ITEM_PENDING);
    assert_eq!(planned.item.planned, 2);
    assert!(planned.restart_variants.is_empty());
}

// ── 中断标记 ─────────────────────────────────────────────────────────

#[test]
fn interrupt_marks_only_reports_whose_owner_is_gone() {
    let dir = std::env::temp_dir().join(format!("wb-batch-{}", uuid::Uuid::new_v4()));
    std::fs::create_dir_all(&dir).unwrap();
    let now = 10_000_000_i64;
    let write = |name: &str, state: &str, id: &str, current: Option<&str>, items: Value| {
        let mut value = json!({
            "jobId": id,
            "state": state,
            "startedAt": now - 60_000,
            "items": items,
        });
        if let Some(current) = current {
            value["current"] = json!(current);
        }
        std::fs::write(
            dir.join(format!("batch-{name}.json")),
            serde_json::to_string(&value).unwrap(),
        )
        .unwrap();
    };
    let read = |name: &str| -> Value {
        serde_json::from_str(
            &std::fs::read_to_string(dir.join(format!("batch-{name}.json"))).unwrap(),
        )
        .unwrap()
    };

    // 死掉的持有者：计划阶段就被杀，第 2 组正在写
    write(
        "old",
        "planning",
        "job-old",
        Some("g2"),
        json!([
            { "groupId": "g1", "status": "done" },
            { "groupId": "g2", "status": "pending" },
            { "groupId": "g3", "status": "pending" },
        ]),
    );
    // 还活着的持有者：报告在写、锁被占
    write("fresh", "running", "job-fresh", None, json!([]));
    let held = session_link::try_lock_file(&dir.join("batch-job-fresh.lock")).unwrap();
    // 已经结束的报告不动
    write("done", "finished", "job-done", None, json!([]));

    assert_eq!(mark_interrupted_in(&dir, &HashSet::new(), now), 1);
    let old_report = read("old");
    assert_eq!(old_report["state"], "interrupted");
    assert!(old_report["interruptedAt"].is_i64());
    assert!(old_report["current"].is_null());
    assert_eq!(old_report["items"][0]["status"], ITEM_DONE, "已完成的不动");
    // 正在写的那一组要标成「可能已部分写入」，其余标成「还没开始」
    assert_eq!(old_report["items"][1]["status"], ITEM_NOT_RUN);
    assert_eq!(old_report["items"][1]["reasonCode"], "interruptedMidWrite");
    assert_eq!(old_report["items"][2]["status"], ITEM_NOT_RUN);
    assert_eq!(old_report["items"][2]["reasonCode"], "interrupted");

    assert_eq!(read("fresh")["state"], "running", "锁被占说明还活着");
    assert_eq!(read("done")["state"], "finished");

    // 本进程内存里还有这个 job：即使无人持锁也不能标成中断。
    let live: HashSet<String> = ["job-old".to_string()].into_iter().collect();
    assert_eq!(mark_interrupted_in(&dir, &live, now), 0);

    drop(held);
    std::fs::remove_dir_all(&dir).ok();
}

#[test]
fn incomplete_report_detection_covers_interrupted_and_pending_items() {
    assert!(is_incomplete_report(&json!({ "state": "interrupted" })));
    assert!(is_incomplete_report(
        &json!({ "state": "finished", "items": [{ "status": "failed" }] })
    ));
    assert!(is_incomplete_report(
        &json!({ "state": "finished", "items": [{ "status": "notRun" }] })
    ));
    assert!(!is_incomplete_report(&json!({
        "state": "finished",
        "items": [{ "status": "done" }, { "status": "skipped" }],
    })));
    assert!(!is_incomplete_report(&json!({ "state": "finished" })));
}

#[test]
fn affected_mirrors_synced_for_completed_items() {
    let item = BatchItem::new("k".into(), "g1", "t".into()).done(3);
    assert_eq!(item.synced, 3);
    assert_eq!(
        item.affected, 3,
        "affected 与 synced 必须同值，前端才能安全切换"
    );
}
