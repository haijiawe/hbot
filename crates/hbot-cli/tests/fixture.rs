//! `hbot fixture save` / `hbot fixture run` 的端到端验收：**spawn 真二进制**。
//!
//! 与 `tests/replay.rs` 同一套做法（真项目、真库、真子进程、断言退出码与输出），只换成
//! 夹具这两条子命令。六条用例对应验收：
//!
//! ① `save` 存成了 → exit 0、文件真在、`--json` 能解析、`events` 数对；
//! ② `save` 时库里没有历史 → exit 2，stderr 说清原因；
//! ③ `save --path ../x.json` 越界 → exit 2，且项目外没多出文件；
//! ④ `run` 一份夹具 → exit 0、打印回复；**把库删掉照样跑**（这条命令不查库）；
//! ⑤ `run` 一个坏文件 → exit 2（不是 1，也不是 0）；
//! ⑥ handler 抛异常 → exit 1（"跑完了但行为变了"，与"没跑起来"的 2 分开）。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};
use std::sync::atomic::{AtomicU32, Ordering};

use hbot_store::{
    Bot, BotId, Conversation, ConversationId, InboundId, NewInbound, NewOutbound, OutboundId,
    OutboundTarget, Project, ProjectId, Store,
};
use serde_json::{Value, json};
use uuid::Uuid;

/// 群号（`conversationExternalId`）；用固定的，便于断言报告里的字段。
const GROUP_EXTERNAL_ID: &str = "30001";

/// 脚手架默认模板的写法：`reply` 从 `hbot` 导入，`ctx.text` 是入站文本。
const REPLY_SOURCE: &str = r#"import { defineBot, reply } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    reply(`pong:${ctx.text}`);
  },
});
"#;

/// 每条事件都抛异常：用来钉住"跑完了但出错 → 退出码 1"。
const THROWING_SOURCE: &str = r#"import { defineBot } from 'hbot';

export default defineBot({
  async onMessage() {
    throw new Error('boom');
  },
});
"#;

static COUNTER: AtomicU32 = AtomicU32::new(0);

/// 每个用例独占一个临时目录（带进程号与序号，并行跑也不撞）。
fn temp_dir(tag: &str) -> PathBuf {
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    let dir = std::env::temp_dir().join(format!(
        "hbot-fixture-cli-{tag}-{}-{sequence}",
        std::process::id()
    ));
    let _ = std::fs::remove_dir_all(&dir);
    dir
}

/// 一条历史事件：入站文本 + 当时那条 bot 回复。
struct Event<'a> {
    external_id: &'a str,
    text: &'a str,
    reply: Option<&'a str>,
}

/// 建好的夹具：项目目录（含 `.hbot/hbot.db`）与里面的 bot id。
struct Seed {
    dir: PathBuf,
    bot_id: BotId,
}

/// 造一个真项目 + 真库，写入 `events` 对应的 inbound / outbound 行。
fn seed(tag: &str, bot_source: &str, events: &[Event<'_>]) -> Seed {
    let dir = temp_dir(tag);
    hbot_code::manifest::scaffold(&dir, "fixture-cli").expect("脚手架应成功");
    std::fs::write(dir.join("src/main.ts"), bot_source).expect("写 src/main.ts");

    let data_dir = dir.join(".hbot");
    std::fs::create_dir_all(&data_dir).expect("建 .hbot");
    let store = Store::open(data_dir.join("hbot.db")).expect("打开项目库");

    let project_id = ProjectId::from_uuid(Uuid::new_v4());
    store
        .insert_project(&Project {
            id: project_id,
            name: "fixture-cli".to_owned(),
            path: dir.display().to_string(),
            created_at: 0,
        })
        .expect("插 projects 行");
    let bot_id = BotId::from_uuid(Uuid::new_v4());
    store
        .insert_bot(&Bot {
            id: bot_id,
            project_id,
            name: "fixture-bot".to_owned(),
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

    for (index, event) in events.iter().enumerate() {
        let received_at = 1_700_000_000_000 + index as i64;
        store
            .insert_inbound_with_sender(
                NewInbound {
                    id: InboundId::from_uuid(Uuid::new_v4()),
                    bot_id,
                    external_id: event.external_id.to_owned(),
                    conversation_id,
                    payload_json: payload_json(event.external_id, event.text),
                    received_at,
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
                        created_at: received_at + 1,
                    },
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

/// 跑一次真二进制：`hbot fixture <sub> <args…>`。
fn fixture(sub: &str, args: &[&str]) -> Output {
    Command::new(env!("CARGO_BIN_EXE_hbot"))
        .arg("fixture")
        .arg(sub)
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

/// 验收 ①：`save` 存成 → exit 0、文件真在、`--json` 可解析且 `events` 对得上。
#[test]
fn save_writes_an_event_array_and_reports_the_count() {
    let seed = seed(
        "save-ok",
        REPLY_SOURCE,
        &[
            Event {
                external_id: "m1",
                text: "你好",
                reply: Some("pong:你好"),
            },
            Event {
                external_id: "m2",
                text: "在吗",
                reply: Some("pong:在吗"),
            },
        ],
    );
    let dir = seed.dir.display().to_string();
    let bot = seed.bot_id.to_string();
    let output = fixture(
        "save",
        &[
            &dir,
            "--bot",
            &bot,
            "--path",
            ".hbot/fixtures/two.json",
            "--json",
        ],
    );
    assert_eq!(code(&output), 0, "stderr：{}", stderr(&output));

    let saved: Value = serde_json::from_str(stdout(&output).trim()).expect("--json 应是一行 JSON");
    assert_eq!(saved["events"], json!(2), "两条历史入站：{saved}");
    let path = PathBuf::from(saved["path"].as_str().expect("path 是字符串"));
    assert!(path.is_file(), "返回的路径必须真的存在：{}", path.display());
    let content: Value =
        serde_json::from_str(&std::fs::read_to_string(&path).expect("读夹具")).expect("合法 JSON");
    let events = content.as_array().expect("顶层是 JSON 数组");
    assert_eq!(events.len(), 2, "夹具里应当有 2 条事件：{content}");
    assert_eq!(events[0]["externalId"], json!("m1"));
    assert_eq!(events[1]["text"], json!("在吗"));
    cleanup(&seed.dir);
}

/// 验收 ②：库里没有历史 → exit 2（`save` 没有"跑完了但结论不好"这一档）。
#[test]
fn save_without_history_exits_two() {
    let seed = seed("save-empty", REPLY_SOURCE, &[]);
    let dir = seed.dir.display().to_string();
    let bot = seed.bot_id.to_string();
    let output = fixture(
        "save",
        &[&dir, "--bot", &bot, "--path", ".hbot/fixtures/none.json"],
    );
    assert_eq!(code(&output), 2, "stderr：{}", stderr(&output));
    let err = stderr(&output);
    assert!(err.contains("历史"), "stderr 该说清是没有历史，实际：{err}");
    assert!(
        !seed.dir.join(".hbot/fixtures/none.json").exists(),
        "失败时不该留下空夹具"
    );
    cleanup(&seed.dir);
}

/// 验收 ③：`--path` 越界 → exit 2，且项目外没多出文件。
#[test]
fn save_outside_the_project_exits_two() {
    let seed = seed(
        "save-escape",
        REPLY_SOURCE,
        &[Event {
            external_id: "m1",
            text: "你好",
            reply: Some("pong:你好"),
        }],
    );
    let dir = seed.dir.display().to_string();
    let bot = seed.bot_id.to_string();
    let output = fixture("save", &[&dir, "--bot", &bot, "--path", "../escaped.json"]);
    assert_eq!(code(&output), 2, "stderr：{}", stderr(&output));
    let err = stderr(&output);
    assert!(
        err.contains("项目内"),
        "stderr 该说明夹具必须落在项目内，实际：{err}"
    );
    let escaped = seed.dir.parent().map(|parent| parent.join("escaped.json"));
    if let Some(escaped) = escaped {
        assert!(
            !escaped.exists(),
            "越界路径不该真的写盘：{}",
            escaped.display()
        );
    }
    cleanup(&seed.dir);
}

/// 验收 ④：`run` 一份夹具 → exit 0、打印回复；**删掉库照样能跑**（不查库）。
#[test]
fn run_does_not_need_the_database() {
    let seed = seed(
        "run-ok",
        REPLY_SOURCE,
        &[Event {
            external_id: "m1",
            text: "你好",
            reply: Some("pong:你好"),
        }],
    );
    let dir = seed.dir.display().to_string();
    let bot = seed.bot_id.to_string();
    assert_eq!(
        code(&fixture(
            "save",
            &[&dir, "--bot", &bot, "--path", ".hbot/fixtures/one.json"]
        )),
        0,
        "先存一份夹具"
    );

    // 把整个 `.hbot` 删掉：夹具文件也在这里，所以先把夹具拷到别处再删库，
    // 再放回一个"没有库"的项目里 —— 这一步证明 `run` 既不查库、也不依赖库存在。
    let fixture_file = seed.dir.join(".hbot/fixtures/one.json");
    let fixture_text = std::fs::read_to_string(&fixture_file).expect("读夹具");
    std::fs::remove_dir_all(seed.dir.join(".hbot")).expect("删掉 .hbot（连库一起）");
    std::fs::create_dir_all(seed.dir.join(".hbot/fixtures")).expect("重建夹具目录");
    std::fs::write(&fixture_file, &fixture_text).expect("放回夹具");
    assert!(
        !seed.dir.join(".hbot/hbot.db").exists(),
        "这一步的前提是库真的没了"
    );

    let output = fixture("run", &[&dir, "--path", ".hbot/fixtures/one.json"]);
    assert_eq!(code(&output), 0, "stderr：{}", stderr(&output));
    let out = stdout(&output);
    assert!(
        out.contains("pong:你好"),
        "报告该带出重跑得到的回复，实际：{out}"
    );
    assert!(
        out.contains("1 条事件 / 1 条回复 / 0 条出错"),
        "汇总行不对，实际：{out}"
    );
    cleanup(&seed.dir);
}

/// 验收 ⑤：`run` 一个坏文件 → exit 2。
#[test]
fn run_with_a_broken_fixture_exits_two() {
    let seed = seed(
        "run-broken",
        REPLY_SOURCE,
        &[Event {
            external_id: "m1",
            text: "你好",
            reply: None,
        }],
    );
    let target = seed.dir.join(".hbot/fixtures/broken.json");
    std::fs::create_dir_all(target.parent().expect("父目录")).expect("建夹具目录");
    std::fs::write(&target, r#"{"version":1}"#).expect("写坏夹具");
    let dir = seed.dir.display().to_string();

    let output = fixture("run", &[&dir, "--path", ".hbot/fixtures/broken.json"]);
    assert_eq!(
        code(&output),
        2,
        "坏夹具是「跑不起来」：{}",
        stderr(&output)
    );
    let err = stderr(&output);
    assert!(
        err.contains("JSON 数组"),
        "stderr 该说清顶层必须是数组，实际：{err}"
    );
    cleanup(&seed.dir);
}

/// 验收 ⑥：handler 抛异常 → exit 1（跑完了但行为变了），而不是 2。
#[test]
fn run_with_a_throwing_handler_exits_one() {
    let seed = seed(
        "run-throws",
        THROWING_SOURCE,
        &[Event {
            external_id: "m1",
            text: "你好",
            reply: None,
        }],
    );
    let dir = seed.dir.display().to_string();
    let bot = seed.bot_id.to_string();
    assert_eq!(
        code(&fixture(
            "save",
            &[&dir, "--bot", &bot, "--path", ".hbot/fixtures/throw.json"]
        )),
        0,
        "先存一份夹具"
    );

    let output = fixture("run", &[&dir, "--path", ".hbot/fixtures/throw.json"]);
    assert_eq!(code(&output), 1, "stderr：{}", stderr(&output));
    let out = stdout(&output);
    assert!(
        out.contains("1 条出错"),
        "汇总行该说清有一条出错，实际：{out}"
    );
    assert!(
        out.contains("boom") || out.contains("handler 抛异常"),
        "报告该带出真实错误原因，实际：{out}"
    );
    cleanup(&seed.dir);
}
