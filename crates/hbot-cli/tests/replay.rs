//! `hbot replay` 的端到端验收：**spawn 真二进制**，断言退出码与报告原文。
//!
//! 每条用例都走真路径，不用 mock：
//!
//! * 真项目 —— `hbot_code::manifest::scaffold` 生成完整脚手架（`hbot.json` / `tsconfig.json` /
//!   `hbot.d.ts` / `src/main.ts`…），再把 `src/main.ts` 换成我们要的 handler；
//! * 真库 —— `hbot_store::Store` 打开 `<project>/.hbot/hbot.db`，真写 `projects` / `bots` /
//!   `conversations` / `inbound` / `outbound` 行；`inbound.payload_json` 用 OneBot 11 群消息的
//!   **归一化载荷**形状（`hbot-adapters` 的 `NormalizedInbound::payload_json` 就长这样，
//!   原始事件嵌在 `raw` 下）；
//! * 真回放 —— `Command::new(env!("CARGO_BIN_EXE_hbot"))`，读 stdout / stderr / 退出码。
//!
//! 这五条对应任务验收：① 一致时 exit 0、② 改一句回复后 exit 1 并指出多出/缺少、
//! ③ `--json` 能被 `serde_json::from_str` 解析、④ `--limit` / `--from` 真生效、
//! ⑤ 空库 / 不存在的 bot → exit 2 且说清原因。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

// 报告类型从实现它的 crate 拿（`hbot-cli` 仍重导出，但测试直接指真源）。
use hbot_replay::{EntryStatus, ReplayReport};
use hbot_store::{
    Bot, BotId, Conversation, ConversationId, InboundId, NewInbound, NewOutbound, OutboundId,
    OutboundTarget, Project, ProjectId, Store,
};
use serde_json::{Value, json};
use uuid::Uuid;

/// 群号（`conversationExternalId`）；用固定的，便于断言报告里的字段。
const GROUP_EXTERNAL_ID: &str = "30001";

/// 脚手架默认模板的写法：`reply` 从 `hbot` 导入，`ctx.text` 是入站文本。
const REPLY_V1: &str = r#"import { defineBot, reply } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    reply(`pong:${ctx.text}`);
  },
});
"#;

/// 只改一句：回复文本从 `pong:` 变成 `pong2:`（验收 ② 的"改一句代码"）。
const REPLY_V2: &str = r#"import { defineBot, reply } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    reply(`pong2:${ctx.text}`);
  },
});
"#;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// 每个用例独占一个临时目录（带进程号与序号，并行跑也不撞）。
fn temp_dir(tag: &str) -> PathBuf {
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "hbot-replay-{tag}-{}-{sequence}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// 一条历史事件：入站文本 + 当时那条 bot 回复（`None` = 当时没回复）。
struct Event<'a> {
    external_id: &'a str,
    text: &'a str,
    reply: Option<&'a str>,
    received_at: i64,
}

/// 建好的夹具：项目目录（含 `.hbot/hbot.db`）与里面的 bot id。
struct Seed {
    dir: PathBuf,
    bot_id: BotId,
}

/// 造一个真项目 + 真库，写入 `events` 对应的 inbound / outbound 行。
fn seed(tag: &str, bot_source: &str, events: &[Event<'_>]) -> Seed {
    let dir = temp_dir(tag);
    hbot_code::manifest::scaffold(&dir, "replay-fixture").expect("脚手架应成功");
    std::fs::write(dir.join("src/main.ts"), bot_source).expect("写 src/main.ts");

    let data_dir = dir.join(".hbot");
    std::fs::create_dir_all(&data_dir).expect("建 .hbot");
    let store = Store::open(data_dir.join("hbot.db")).expect("打开项目库");

    let project_id = ProjectId::from_uuid(Uuid::new_v4());
    store
        .insert_project(&Project {
            id: project_id,
            name: "replay-fixture".to_owned(),
            path: dir.display().to_string(),
            created_at: 0,
        })
        .expect("插 projects 行");
    let bot_id = BotId::from_uuid(Uuid::new_v4());
    store
        .insert_bot(&Bot {
            id: bot_id,
            project_id,
            name: "replay-bot".to_owned(),
            platform: "onebot11".to_owned(),
            config_json: "{}".to_owned(),
            enabled: true,
            created_at: 0,
        })
        .expect("插 bots 行");
    let conversation_id = ConversationId::from_uuid(Uuid::new_v4());
    store
        .insert_conversation(&Conversation {
            id: conversation_id,
            bot_id,
            external_id: GROUP_EXTERNAL_ID.to_owned(),
            kind: "group".to_owned(),
            created_at: 0,
        })
        .expect("插 conversations 行");

    for event in events {
        store
            .insert_inbound_with_sender(
                NewInbound {
                    id: InboundId::from_uuid(Uuid::new_v4()),
                    bot_id,
                    external_id: event.external_id.to_owned(),
                    conversation_id,
                    payload_json: payload_json(event.external_id, event.text),
                    received_at: event.received_at,
                },
                "10086".to_owned(),
            )
            .expect("插 inbound 行");
        if let Some(reply) = event.reply {
            store
                .enqueue_outbound_with_target(
                    NewOutbound {
                        id: OutboundId::from_uuid(Uuid::new_v4()),
                        bot_id,
                        conversation_id,
                        text: reply.to_owned(),
                        // 出站一定晚于入站（报告按 created_at 排序，同一毫秒也稳）。
                        created_at: event.received_at + 1,
                    },
                    // 被动回复：`reply_to` = 入站 external_id，这是回放归属的唯一外键。
                    OutboundTarget::new(
                        json!({"groupId": GROUP_EXTERNAL_ID}).to_string(),
                        Some(event.external_id.to_owned()),
                    ),
                )
                .expect("插 outbound 行");
        }
    }
    drop(store);
    Seed { dir, bot_id }
}

/// 一份 OneBot 11 群消息的**归一化载荷**（`hbot-adapters` 落库的就是这个形状）。
fn payload_json(external_id: &str, text: &str) -> String {
    json!({
        "text": text,
        "senderId": "10086",
        "conversationExternalId": GROUP_EXTERNAL_ID,
        "kind": "group",
        "messageId": external_id,
        "replyTo": null,
        "botPlatformId": "20000",
        "raw": {
            "post_type": "message",
            "message_type": "group",
            "sub_type": "normal",
            "message_id": external_id.parse::<i64>().unwrap_or_default(),
            "group_id": GROUP_EXTERNAL_ID.parse::<i64>().unwrap_or_default(),
            "user_id": 10086,
            "raw_message": text,
            "message": [{"type": "text", "data": {"text": text}}],
            "sender": {"user_id": 10086, "nickname": "测试"},
        },
    })
    .to_string()
}

/// 跑一次真二进制：`hbot replay <args…>`。
fn replay(args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hbot"))
        .arg("replay")
        .args(args)
        .output()
        .expect("hbot 二进制应能启动")
}

fn code(output: &Output) -> i32 {
    output
        .status
        .code()
        .expect("进程应正常退出（不是被信号杀掉）")
}

fn stdout(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

fn cleanup(dir: &Path) {
    let _ = std::fs::remove_dir_all(dir);
}

/// 验收 ①：历史出站与重跑结果一致 → exit 0，报告写"一致"。
#[test]
fn consistent_history_exits_zero() {
    let seed = seed(
        "consistent",
        REPLY_V1,
        &[
            Event {
                external_id: "900001",
                text: "你好",
                reply: Some("pong:你好"),
                received_at: 1_700_000_000_000,
            },
            Event {
                external_id: "900002",
                text: "在吗",
                reply: Some("pong:在吗"),
                received_at: 1_700_000_001_000,
            },
        ],
    );
    let dir = seed.dir.display().to_string();
    let bot = seed.bot_id.to_string();
    let output = replay(&[&dir, "--bot", &bot]);
    let out = stdout(&output);
    assert_eq!(
        code(&output),
        0,
        "一致时应 exit 0；stdout=\n{out}\nstderr=\n{}",
        stderr(&output)
    );
    assert!(
        out.contains("汇总：2 条入站 / 2 条一致 / 0 条有差异 / 0 条出错"),
        "报告汇总不对：\n{out}"
    );
    cleanup(&seed.dir);
}

/// 验收 ②：改一句回复文本 → exit 1，并指出哪条变成"多出 / 缺少"。
#[test]
fn changed_reply_text_exits_one_and_names_the_difference() {
    let seed = seed(
        "changed",
        REPLY_V1,
        &[
            Event {
                external_id: "900101",
                text: "你好",
                reply: Some("pong:你好"),
                received_at: 1_700_000_000_000,
            },
            Event {
                external_id: "900102",
                text: "在吗",
                reply: Some("pong:在吗"),
                received_at: 1_700_000_001_000,
            },
        ],
    );
    // "改一句代码"：回复前缀 pong: → pong2:
    std::fs::write(seed.dir.join("src/main.ts"), REPLY_V2).expect("改 src/main.ts");

    let dir = seed.dir.display().to_string();
    let bot = seed.bot_id.to_string();
    let output = replay(&[&dir, "--bot", &bot]);
    let out = stdout(&output);
    assert_eq!(
        code(&output),
        1,
        "有差异时应 exit 1（不是 0 也不是 2）；stdout=\n{out}\nstderr=\n{}",
        stderr(&output)
    );
    assert!(out.contains("2 条有差异"), "报告应指出 2 条有差异：\n{out}");
    assert!(
        out.contains("缺少：\"pong:你好\""),
        "报告应指出缺少的历史回复：\n{out}"
    );
    assert!(
        out.contains("多出：\"pong2:你好\""),
        "报告应指出多出的新回复：\n{out}"
    );
    cleanup(&seed.dir);
}

/// 验收 ③：`--json` 的输出能被 `serde_json::from_str` 解析成报告类型。
#[test]
fn json_report_round_trips_through_serde() {
    let seed = seed(
        "json",
        REPLY_V1,
        &[Event {
            external_id: "900201",
            text: "你好",
            reply: Some("pong:你好"),
            received_at: 1_700_000_000_000,
        }],
    );
    let dir = seed.dir.display().to_string();
    let bot = seed.bot_id.to_string();
    let output = replay(&[&dir, "--bot", &bot, "--json"]);
    assert_eq!(code(&output), 0, "stderr=\n{}", stderr(&output));
    let out = stdout(&output);
    let line = out.trim_end();
    assert!(
        !line.contains('\n'),
        "`--json` 应该是单行 JSON（stdout 只写数据）：\n{out}"
    );
    let report: ReplayReport =
        serde_json::from_str(line).unwrap_or_else(|error| panic!("{error}；原文：\n{out}"));
    assert!(report.ok);
    assert_eq!(report.events, 1);
    assert_eq!(report.same, 1);
    assert_eq!(report.extra, 0);
    assert_eq!(report.missing, 0);
    assert_eq!(report.entries.len(), 1);
    assert_eq!(report.entries[0].status, EntryStatus::Match);
    assert_eq!(report.entries[0].external_id, "900201");
    assert_eq!(report.entries[0].historical, vec!["pong:你好".to_owned()]);
    assert_eq!(report.entries[0].replayed, vec!["pong:你好".to_owned()]);
    assert!(report.error.is_none());
    cleanup(&seed.dir);
}

/// 验收 ④：`--limit` 与 `--from` 真生效（少读几条）。
#[test]
fn limit_and_from_narrow_the_batch() {
    let seed = seed(
        "window",
        REPLY_V1,
        &[
            Event {
                external_id: "900301",
                text: "一",
                reply: Some("pong:一"),
                received_at: 1_700_000_000_000, // 2023-11-14T22:13:20Z
            },
            Event {
                external_id: "900302",
                text: "二",
                reply: Some("pong:二"),
                received_at: 1_700_000_001_000, // 22:13:21Z
            },
            Event {
                external_id: "900303",
                text: "三",
                reply: Some("pong:三"),
                received_at: 1_700_000_002_000, // 22:13:22Z
            },
        ],
    );
    let dir = seed.dir.display().to_string();
    let bot = seed.bot_id.to_string();

    // --limit 2：只回放最早两条。
    let limited = replay(&[&dir, "--bot", &bot, "--limit", "2", "--json"]);
    assert_eq!(code(&limited), 0, "stderr=\n{}", stderr(&limited));
    let report: ReplayReport =
        serde_json::from_str(stdout(&limited).trim_end()).expect("JSON 可解析");
    assert_eq!(report.events, 2, "`--limit 2` 应只读两条");
    assert_eq!(report.entries[0].external_id, "900301");
    assert_eq!(report.entries[1].external_id, "900302");

    // --from 22:13:21Z（含）：跳过第一条。
    let windowed = replay(&[
        &dir,
        "--bot",
        &bot,
        "--from",
        "2023-11-14T22:13:21Z",
        "--json",
    ]);
    assert_eq!(code(&windowed), 0, "stderr=\n{}", stderr(&windowed));
    let report: ReplayReport =
        serde_json::from_str(stdout(&windowed).trim_end()).expect("JSON 可解析");
    assert_eq!(report.events, 2, "`--from` 应跳过 22:13:20 那条");
    assert_eq!(report.entries[0].external_id, "900302");
    assert_eq!(report.from, Some(1_700_000_001_000));

    // 两个一起用：从 22:13:21Z 起、只要 1 条。
    let both = replay(&[
        &dir,
        "--bot",
        &bot,
        "--from",
        "2023-11-14T22:13:21Z",
        "--limit",
        "1",
        "--json",
    ]);
    let report: ReplayReport = serde_json::from_str(stdout(&both).trim_end()).expect("JSON 可解析");
    assert_eq!(report.events, 1);
    assert_eq!(report.entries[0].external_id, "900302");
    cleanup(&seed.dir);
}

/// 验收 ⑤·a：没有库 → exit 2，且说明"找不到项目库"（不会顺手造一个空库）。
#[test]
fn missing_database_exits_two() {
    let dir = temp_dir("no-db");
    hbot_code::manifest::scaffold(&dir, "replay-fixture").expect("脚手架应成功");
    let bot = BotId::from_uuid(Uuid::new_v4()).to_string();
    let output = replay(&[&dir.display().to_string(), "--bot", &bot]);
    assert_eq!(code(&output), 2, "stdout=\n{}", stdout(&output));
    assert!(
        stderr(&output).contains("找不到项目库"),
        "错误信息应说清缺的是库：\n{}",
        stderr(&output)
    );
    assert!(
        !dir.join(".hbot").exists(),
        "回放不该替项目造 `.hbot/`（Store::open 会建库并跑迁移）"
    );
    cleanup(&dir);
}

/// 验收 ⑤·b：bot 不存在 → exit 2，且错误里带上那个 bot id。
#[test]
fn unknown_bot_exits_two() {
    let seed = seed(
        "unknown-bot",
        REPLY_V1,
        &[Event {
            external_id: "900401",
            text: "你好",
            reply: Some("pong:你好"),
            received_at: 1_700_000_000_000,
        }],
    );
    let dir = seed.dir.display().to_string();
    let missing = Uuid::new_v4().to_string();
    let output = replay(&[&dir, "--bot", &missing]);
    assert_eq!(code(&output), 2, "stdout=\n{}", stdout(&output));
    let err = stderr(&output);
    assert!(err.contains("库里没有 bot"), "错误信息不对：\n{err}");
    assert!(err.contains(&missing), "错误信息应带上 bot id：\n{err}");
    cleanup(&seed.dir);
}

/// 验收 ⑤·c：库在、bot 在，但一条历史入站都没有 → exit 2 并给下一步提示。
#[test]
fn empty_history_exits_two() {
    let seed = seed("empty", REPLY_V1, &[]);
    let dir = seed.dir.display().to_string();
    let bot = seed.bot_id.to_string();
    let output = replay(&[&dir, "--bot", &bot]);
    assert_eq!(code(&output), 2, "stdout=\n{}", stdout(&output));
    let err = stderr(&output);
    assert!(
        err.contains("没有可回放的历史入站"),
        "错误信息不对：\n{err}"
    );
    assert!(err.contains("先让它真跑几条消息"), "应给出下一步：\n{err}");
    cleanup(&seed.dir);
}

/// `--json` 在失败时也输出可解析的报告（`error` 字段承载原因），退出码仍是 2。
#[test]
fn json_report_carries_the_fatal_error() {
    let seed = seed("json-error", REPLY_V1, &[]);
    let dir = seed.dir.display().to_string();
    let bot = seed.bot_id.to_string();
    let output = replay(&[&dir, "--bot", &bot, "--json"]);
    assert_eq!(code(&output), 2);
    let report: ReplayReport =
        serde_json::from_str(stdout(&output).trim_end()).expect("JSON 可解析");
    assert!(!report.ok);
    assert!(report.error.is_some());
    assert_eq!(report.events, 0);
    cleanup(&seed.dir);
}

/// `--save-fixture` 把这一批入站 payload 存成 JSON 数组（可复用）。
#[test]
fn save_fixture_writes_the_payload_batch() {
    let seed = seed(
        "fixture",
        REPLY_V1,
        &[
            Event {
                external_id: "900501",
                text: "你好",
                reply: Some("pong:你好"),
                received_at: 1_700_000_000_000,
            },
            Event {
                external_id: "900502",
                text: "在吗",
                reply: Some("pong:在吗"),
                received_at: 1_700_000_001_000,
            },
        ],
    );
    let dir = seed.dir.display().to_string();
    let bot = seed.bot_id.to_string();
    let fixture = temp_dir("fixture-file").with_extension("json");
    let fixture_text = fixture.display().to_string();
    let output = replay(&[
        &dir,
        "--bot",
        &bot,
        "--save-fixture",
        &fixture_text,
        "--json",
    ]);
    assert_eq!(code(&output), 0, "stderr=\n{}", stderr(&output));

    let text = std::fs::read_to_string(&fixture).expect("fixture 应已写出");
    let values: Vec<Value> = serde_json::from_str(&text).expect("fixture 应是 JSON 数组");
    assert_eq!(values.len(), 2, "fixture 应含 2 条 payload");
    assert_eq!(values[0]["text"], "你好");
    assert_eq!(values[0]["kind"], "group");
    assert_eq!(values[0]["conversationExternalId"], GROUP_EXTERNAL_ID);
    assert_eq!(
        values[0]["raw"]["message_type"], "group",
        "OneBot 11 原始事件应原样保留在 raw 下"
    );
    assert_eq!(values[1]["text"], "在吗");

    let _ = std::fs::remove_file(&fixture);
    cleanup(&seed.dir);
}
