//! 真 LLM 端到端评测：让**真的**模型改写脚手架的 `src/main.ts`，再由 harness
//! 自己编译 + 调度，用行为断言回答一个问题 —— Agent 到底能不能写出能跑的 bot？
//!
//! 与 `turn_over_sse.rs` 的区别：那条测试用本地回环假 provider 钉协议细节，
//! 这条**联网**、用真模型、判的是产物的行为（编译通过 + `dispatch` 出来的回复）。
//!
//! ## 怎么跑
//!
//! 需要三个环境变量（缺任一就打印说明并跳过，不算失败）：
//!
//! ```powershell
//! $env:HBOT_LLM_BASE_URL='https://…/v1'   # 必须带 /v1，客户端拼 {base_url}/chat/completions
//! $env:HBOT_LLM_API_KEY='sk-…'            # 只放在环境变量里，绝不进仓库
//! $env:HBOT_LLM_MODEL='…'
//! cargo test -p hbot-agent --test real_llm_eval -- --ignored --nocapture
//! ```
//!
//! 调试时只跑指定条目：`$env:HBOT_LLM_EVAL_ONLY='1,8,15'`。
//!
//! ## 判据与红/绿
//!
//! 每条任务分两类失败，**只有 harness 侧失败会让测试红**：
//!
//! * `MODEL`  —— 模型能力不足（写错代码、少回一句、多回一句…）。记 FAIL，继续跑下一条；
//! * `HARNESS` —— 我们的东西坏了（provider 连不上、编译/沙箱/调度本身出错）。
//!   出现任何一条都说明评测链路有问题，必须修，所以直接 `assert!` 失败。
//!
//! 打印 `PASSED x/20` 只是为了给报告用；测试本身断言的是「20 条都跑完了」+
//! 「没有 harness 侧失败」。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{Duration, Instant};

use hbot_agent::tools::{EditFileTool, ListDirTool, ReadFileTool, WriteFileTool};
use hbot_agent::{
    ApprovalReply, ApprovalRequest, Approver, CancellationToken, ChatMessage, NullAudit,
    OpenAiProvider, Provider, ReqwestTransport, SandboxMode, ToolCallRequest, ToolGateway,
    ToolOutput, ToolRegistry, TurnConfig, TurnInput, TurnSink, run_turn,
};
use hbot_code::manifest::{load, scaffold};
use hbot_code::{CodeError, EventOutcome, Limits, Sandbox, compile_project};
use hbot_core::HbotError;
use hbot_workspace::Workspace;
use serde_json::{Value, json};

// ---------------------------------------------------------------------------
// 配置：只从环境变量读，key 绝不落盘
// ---------------------------------------------------------------------------

/// 真 provider 的三元组。`Debug` 刻意手写，防止 key 被顺手 `{:?}` 进日志。
struct LlmConfig {
    base_url: String,
    api_key: String,
    model: String,
}

impl LlmConfig {
    /// 三个变量缺任一就返回 `None`（调用方打印说明并跳过）。
    fn from_env() -> Option<Self> {
        let read = |name: &str| {
            std::env::var(name)
                .ok()
                .map(|value| value.trim().to_owned())
                .filter(|value| !value.is_empty())
        };
        let missing: Vec<&str> = ["HBOT_LLM_BASE_URL", "HBOT_LLM_API_KEY", "HBOT_LLM_MODEL"]
            .into_iter()
            .filter(|name| read(name).is_none())
            .collect();
        if !missing.is_empty() {
            eprintln!(
                "跳过真 LLM 评测：缺少环境变量 {}（三个都要给：HBOT_LLM_BASE_URL / \
                 HBOT_LLM_API_KEY / HBOT_LLM_MODEL）",
                missing.join("、")
            );
            return None;
        }
        Some(Self {
            base_url: read("HBOT_LLM_BASE_URL").unwrap_or_default(),
            api_key: read("HBOT_LLM_API_KEY").unwrap_or_default(),
            model: read("HBOT_LLM_MODEL").unwrap_or_default(),
        })
    }
}

// ---------------------------------------------------------------------------
// 临时项目目录
// ---------------------------------------------------------------------------

struct TempProject {
    path: PathBuf,
}

impl TempProject {
    fn new(id: u32) -> Self {
        static SEQ: AtomicU64 = AtomicU64::new(0);
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hbot-real-eval-{id:02}-{}-{seq}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&path);
        std::fs::create_dir_all(&path).expect("建临时项目目录");
        Self { path }
    }
}

impl Drop for TempProject {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

// ---------------------------------------------------------------------------
// 工具链装配：写工具在 workspace_write 档下要审批，所以必须给一个「总批准」的实现
// ---------------------------------------------------------------------------

struct AllowAllApprover;

#[async_trait::async_trait]
impl Approver for AllowAllApprover {
    async fn approve(&self, request: &ApprovalRequest) -> Result<ApprovalReply, HbotError> {
        // 评测里的写操作都是 harness 自己造的临时项目，没有需要拦的东西。
        let _ = &request.tool;
        Ok(ApprovalReply::approve_always())
    }
}

/// 记录循环事件的输出口：除了断言要用，还能在失败时说明「模型是没调工具、
/// 还是工具调用全失败了」。
#[derive(Default)]
struct RecordingSink {
    steps: u32,
    deltas: String,
    tool_calls: Vec<String>,
    tool_failures: Vec<String>,
}

impl TurnSink for RecordingSink {
    fn on_step(&mut self, _step: u32) {
        self.steps += 1;
    }

    fn on_text_delta(&mut self, delta: &str) {
        self.deltas.push_str(delta);
    }

    fn on_tool_call(&mut self, call: &ToolCallRequest) {
        self.tool_calls.push(call.name.clone());
    }

    fn on_tool_result(&mut self, _call_id: &str, output: &ToolOutput) {
        if output.is_error {
            self.tool_failures
                .push(first_line(&output.content, 120).to_owned());
        }
    }
}

// ---------------------------------------------------------------------------
// 20 条任务
// ---------------------------------------------------------------------------

/// 入站消息的形状（`onMessage(ctx)` 能看到的字段）。
enum Inbound {
    Text(&'static str),
    /// 非文本消息：`text` 为空、`raw` 是占位串、`isText = false`。
    NonText(&'static str),
    /// 重复 `n` 个「长」字（超长文本）。
    Long(usize),
}

struct EventSpec {
    message_id: &'static str,
    sender_id: &'static str,
    inbound: Inbound,
}

impl EventSpec {
    fn text(text: &'static str) -> Self {
        Self {
            message_id: "msg-0001",
            sender_id: "u-1001",
            inbound: Inbound::Text(text),
        }
    }

    fn to_json(&self) -> Value {
        let (text, raw, is_text) = match self.inbound {
            Inbound::Text(text) => (text.to_owned(), text.to_owned(), true),
            Inbound::NonText(raw) => (String::new(), raw.to_owned(), false),
            Inbound::Long(count) => {
                let text = "长".repeat(count);
                (text.clone(), text, true)
            }
        };
        json!({
            "type": "message",
            "text": text,
            "raw": raw,
            "isText": is_text,
            "messageId": self.message_id,
            "senderId": self.sender_id,
            "selfId": "bot-1000",
            "conversationId": "group-3000",
            "isGroup": true,
        })
    }
}

/// 判完 dispatch 之后能看到的东西。
struct Evidence {
    dispatch: EventOutcome,
    /// `src/**` 下的源码（相对路径 → 内容），用于「文件存在 / 入口结构」类断言。
    sources: Vec<(String, String)>,
}

impl Evidence {
    fn replies(&self) -> &[String] {
        &self.dispatch.replies
    }

    fn logs(&self) -> &[String] {
        &self.dispatch.logs
    }

    fn source(&self, relative: &str) -> Option<&str> {
        self.sources
            .iter()
            .find(|(path, _)| path == relative)
            .map(|(_, text)| text.as_str())
    }
}

struct Case {
    id: u32,
    /// 表格里的任务名，例如 `A① 原样回显`。
    title: &'static str,
    /// 表格里的期望列（一句话）。
    expect: &'static str,
    /// 发给模型的用户消息（需求）。
    prompt: &'static str,
    event: EventSpec,
    check: fn(&Evidence) -> Result<(), String>,
}

// ---------------------------------------------------------------------------
// 断言小工具
// ---------------------------------------------------------------------------

fn first_line(text: &str, max_chars: usize) -> &str {
    let line = text.lines().next().unwrap_or("").trim();
    match line.char_indices().nth(max_chars) {
        Some((cut, _)) => &line[..cut],
        None => line,
    }
}

/// 把任意文本压成一行（换行显示成 `⏎`）并截断，用于表格里的「实际」列。
fn compact(text: &str, max_chars: usize) -> String {
    let flat = text.replace(['\n', '\r'], "⏎");
    let flat = flat.trim();
    match flat.char_indices().nth(max_chars) {
        Some((cut, _)) => format!("{}…", &flat[..cut]),
        None => flat.to_owned(),
    }
}

/// 恰好一条回复，返回去掉首尾空白后的内容。
fn single_reply(evidence: &Evidence) -> Result<String, String> {
    let replies = evidence.replies();
    match replies {
        [only] => Ok(only.trim().to_owned()),
        _ => Err(format!(
            "期望 1 条回复，实际 {} 条：{replies:?}",
            replies.len()
        )),
    }
}

fn reply_eq(evidence: &Evidence, want: &str) -> Result<(), String> {
    let got = single_reply(evidence)?;
    if got == want {
        Ok(())
    } else {
        Err(format!("期望回复 == {want:?}，实际 {got:?}"))
    }
}

fn reply_contains(evidence: &Evidence, needles: &[&str]) -> Result<(), String> {
    let got = single_reply(evidence)?;
    for needle in needles {
        if !got.contains(needle) {
            return Err(format!("期望回复包含 {needle:?}，实际 {got:?}"));
        }
    }
    Ok(())
}

fn no_reply_with_log(evidence: &Evidence) -> Result<(), String> {
    if !evidence.replies().is_empty() {
        return Err(format!(
            "期望 0 条回复，实际 {} 条：{:?}",
            evidence.replies().len(),
            evidence.replies()
        ));
    }
    if evidence.logs().is_empty() {
        return Err("期望至少 1 条日志，实际 0 条".to_owned());
    }
    Ok(())
}

fn require_source<'a>(evidence: &'a Evidence, relative: &str) -> Result<&'a str, String> {
    evidence
        .source(relative)
        .ok_or_else(|| format!("期望存在文件 {relative}，实际只有 {:?}", paths(evidence)))
}

fn paths(evidence: &Evidence) -> Vec<&str> {
    evidence
        .sources
        .iter()
        .map(|(path, _)| path.as_str())
        .collect()
}

fn require_in_source(evidence: &Evidence, relative: &str, needle: &str) -> Result<(), String> {
    let text = require_source(evidence, relative)?;
    if text.contains(needle) {
        Ok(())
    } else {
        Err(format!("期望 {relative} 里出现 {needle:?}，实际没有"))
    }
}

// ---------------------------------------------------------------------------
// 具体断言
// ---------------------------------------------------------------------------

fn check_01(ev: &Evidence) -> Result<(), String> {
    reply_eq(ev, "你好，世界！")
}

fn check_02(ev: &Evidence) -> Result<(), String> {
    reply_contains(ev, &["10086"])
}

fn check_03(ev: &Evidence) -> Result<(), String> {
    reply_contains(ev, &["你好，我是 HBot，很高兴为你服务！"])
}

fn check_04(ev: &Evidence) -> Result<(), String> {
    reply_contains(ev, &["/help"])
}

fn check_05(ev: &Evidence) -> Result<(), String> {
    reply_contains(ev, &["基础", "专业", "旗舰", "299", "999"])
}

fn check_06(ev: &Evidence) -> Result<(), String> {
    reply_contains(ev, &["抱歉，我没听懂"])
}

fn check_07(ev: &Evidence) -> Result<(), String> {
    reply_contains(ev, &["/help"])
}

fn check_08(ev: &Evidence) -> Result<(), String> {
    let got = single_reply(ev)?;
    if got.contains("@机器人") {
        return Err(format!("期望前缀已被去掉，实际还带着：{got:?}"));
    }
    if got != "你好呀" {
        return Err(format!("期望回复 == \"你好呀\"，实际 {got:?}"));
    }
    Ok(())
}

fn check_09(ev: &Evidence) -> Result<(), String> {
    let got = single_reply(ev)?;
    if !got.contains("第一行内容") {
        return Err(format!("期望回复含第一行，实际 {got:?}"));
    }
    if got.contains("第二行内容") {
        return Err(format!("期望只处理第一行，实际把第二行也带上了：{got:?}"));
    }
    Ok(())
}

fn check_10(ev: &Evidence) -> Result<(), String> {
    reply_eq(ev, "开心 😀🎉 测试")
}

fn check_11(ev: &Evidence) -> Result<(), String> {
    require_source(ev, "src/lib/format.ts")?;
    require_in_source(ev, "src/lib/format.ts", "export")?;
    require_in_source(ev, "src/main.ts", "lib/format")?;
    reply_contains(ev, &["回复：", "测试一下格式化"])
}

fn check_12(ev: &Evidence) -> Result<(), String> {
    require_source(ev, "src/lib/router.ts")?;
    require_in_source(ev, "src/lib/router.ts", "export")?;
    require_in_source(ev, "src/main.ts", "lib/router")?;
    require_in_source(ev, "src/main.ts", "lib/format")?;
    reply_contains(ev, &["/help"])
}

fn check_13(ev: &Evidence) -> Result<(), String> {
    let format = require_source(ev, "src/lib/format.ts")?;
    let router = require_source(ev, "src/lib/router.ts")?;
    if !format.contains("router") {
        return Err("期望 src/lib/format.ts 里 import 了 router".to_owned());
    }
    if !router.contains("format") {
        return Err("期望 src/lib/router.ts 里 import 了 format".to_owned());
    }
    reply_contains(ev, &["前缀：", "循环 import 测试"])
}

fn check_14(ev: &Evidence) -> Result<(), String> {
    let main = require_source(ev, "src/main.ts")?;
    let export_lines: Vec<&str> = main
        .lines()
        .map(str::trim)
        .filter(|line| line.starts_with("export "))
        .collect();
    if export_lines.len() != 1 || !export_lines[0].contains("export default defineBot(") {
        return Err(format!(
            "期望入口只有一条 `export default defineBot(`，实际 {} 条：{export_lines:?}",
            export_lines.len()
        ));
    }
    reply_contains(ev, &["pong"])
}

fn check_15(ev: &Evidence) -> Result<(), String> {
    no_reply_with_log(ev)
}

fn check_16(ev: &Evidence) -> Result<(), String> {
    let got = single_reply(ev)?;
    if !got.contains("消息太长了") {
        return Err(format!(
            "期望固定话术「消息太长了，请分段发送」，实际 {got:?}"
        ));
    }
    if got.contains("长长长") {
        return Err(format!("期望不回显超长文本，实际回显了：{got:?}"));
    }
    Ok(())
}

fn check_17(ev: &Evidence) -> Result<(), String> {
    reply_contains(ev, &["暂不支持非文本消息"])
}

fn check_18(ev: &Evidence) -> Result<(), String> {
    no_reply_with_log(ev)
}

fn check_19(ev: &Evidence) -> Result<(), String> {
    let replies = ev.replies();
    if replies.len() != 1 {
        return Err(format!(
            "期望恰好 1 条回复，实际 {} 条：{replies:?}",
            replies.len()
        ));
    }
    if !replies[0].contains("只回一条") {
        return Err(format!("期望回复含入站文本，实际 {:?}", replies[0]));
    }
    Ok(())
}

fn check_20(ev: &Evidence) -> Result<(), String> {
    reply_contains(ev, &["msg-2026-0001"])
}

// ---------------------------------------------------------------------------
// 任务表
// ---------------------------------------------------------------------------

fn cases() -> Vec<Case> {
    vec![
        Case {
            id: 1,
            title: "A① 原样回显",
            expect: "回复 == 入站文本",
            prompt: "需求：无论收到什么文本，都原样回复 ctx.text 本身，不要加任何前后缀。\
                     例如收到「你好」就回复「你好」。",
            event: EventSpec::text("你好，世界！"),
            check: check_01,
        },
        Case {
            id: 2,
            title: "A② 带上发送者 id",
            expect: "回复含 senderId",
            prompt: "需求：回复里必须出现发送者 id（ctx.senderId），格式固定为「来自 {senderId} 的消息」。",
            event: EventSpec {
                message_id: "msg-0002",
                sender_id: "u-10086",
                inbound: Inbound::Text("在吗"),
            },
            check: check_02,
        },
        Case {
            id: 3,
            title: "A③ 固定问候语",
            expect: "回复 == 指定问候语",
            prompt: "需求：不管收到什么，都回复这一句固定问候语，一字不差：\
                     「你好，我是 HBot，很高兴为你服务！」",
            event: EventSpec::text("随便说点什么"),
            check: check_03,
        },
        Case {
            id: 4,
            title: "B④ 含「帮助」→ /help",
            expect: "回复含 /help",
            prompt: "需求：当 ctx.text 包含「帮助」时，回复必须以「/help」开头，例如「/help 可用指令：…」。",
            event: EventSpec::text("请帮助我"),
            check: check_04,
        },
        Case {
            id: 5,
            title: "B⑤ 含「价格」→ 三档价格表",
            expect: "回复含 基础/专业/旗舰 + 299/999",
            prompt: "需求：当 ctx.text 包含「价格」时，回复价格表，必须同时出现三档名称和三档价格：\
                     基础版 99 元/月、专业版 299 元/月、旗舰版 999 元/月。",
            event: EventSpec::text("价格是多少"),
            check: check_05,
        },
        Case {
            id: 6,
            title: "B⑥ 其它 → 默认话术",
            expect: "回复 == 默认话术",
            prompt: "需求：当 ctx.text 既不包含「帮助」也不包含「价格」时，回复这句默认话术，一字不差：\
                     「抱歉，我没听懂，可以说「帮助」看看我能做什么。」",
            event: EventSpec::text("今天天气怎么样"),
            check: check_06,
        },
        Case {
            id: 7,
            title: "B⑦ 大小写不敏感",
            expect: "「HeLp ME」→ 含 /help",
            prompt: "需求：关键词判断不区分大小写。先把 ctx.text 转成小写再判断：\
                     包含「help」回复以「/help」开头；包含「price」reply 以「PRICE」开头；\
                     都不包含则回复「DEFAULT」。",
            event: EventSpec::text("HeLp ME"),
            check: check_07,
        },
        Case {
            id: 8,
            title: "C⑧ 去掉 @机器人 前缀",
            expect: "回复 == 去前缀后的文本",
            prompt: "需求：如果 ctx.text 以「@机器人 」（@机器人 加一个半角空格）开头，\
                     先去掉这个前缀，再把剩下的内容原样回复；没有这个前缀就把整条文本原样回复。",
            event: EventSpec::text("@机器人 你好呀"),
            check: check_08,
        },
        Case {
            id: 9,
            title: "C⑨ 多行只处理第一行",
            expect: "回复含第一行、不含第二行",
            prompt: "需求：只处理第一行。把 ctx.text 按换行符切开，只取第一行原样回复，忽略后面的行。",
            event: EventSpec::text("第一行内容\n第二行内容"),
            check: check_09,
        },
        Case {
            id: 10,
            title: "C⑩ emoji 原样保留",
            expect: "回复 == 入站文本（含 emoji）",
            prompt: "需求：原样回显 ctx.text，emoji 必须完整保留，不得替换或删除。",
            event: EventSpec::text("开心 😀🎉 测试"),
            check: check_10,
        },
        Case {
            id: 11,
            title: "D⑪ 拆出 src/lib/format.ts",
            expect: "文件存在 + main.ts import + 回复含「回复：」",
            prompt: "需求：把「组装回复文本」的逻辑拆到新文件 src/lib/format.ts，\
                     导出一个函数（例如 formatReply），在 src/main.ts 里 import 它并使用。\
                     最终回复内容为：「回复：」+ ctx.text。",
            event: EventSpec::text("测试一下格式化"),
            check: check_11,
        },
        Case {
            id: 12,
            title: "D⑫ 再拆 src/lib/router.ts",
            expect: "文件存在 + main.ts import + 回复含 /help",
            prompt: "需求：src/lib/format.ts 已经存在（导出 formatReply）；现在把路由判断逻辑再拆到\
                     新文件 src/lib/router.ts。src/main.ts 只负责接线：从 ./lib/format 和\
                     ./lib/router 引入并使用。行为：ctx.text 包含「帮助」时回复以「/help」开头，\
                     其它回复「默认回复」。",
            event: EventSpec::text("我需要帮助"),
            check: check_12,
        },
        Case {
            id: 13,
            title: "D⑬ 两文件互相 import",
            expect: "双向往来 + 不死循环 + 回复含「前缀：」",
            prompt: "需求：src/lib/format.ts 与 src/lib/router.ts 必须互相 import\
                     （各自从对方 import 一个导出），且不能因为循环 import 死循环或报错。\
                     前缀常量定义在 router.ts，最终回复内容为：「前缀：」+ ctx.text。\
                     src/main.ts 负责接线。",
            event: EventSpec::text("循环 import 测试"),
            check: check_13,
        },
        Case {
            id: 14,
            title: "D⑭ 入口只 export default",
            expect: "main.ts 只有一条 export default defineBot",
            prompt: "需求：入口 src/main.ts 里只允许有一条 export，且必须是默认导出\
                     「export default defineBot({ onMessage })」；逻辑挪到 src/lib/ 下的文件。\
                     行为：收到任意文本都回复「pong」。",
            event: EventSpec::text("随便聊聊"),
            check: check_14,
        },
        Case {
            id: 15,
            title: "E⑮ 空文本不回复只 log",
            expect: "0 条回复 + 至少 1 条日志",
            prompt: "需求：ctx.text 为空字符串或只有空白字符时，不要回复任何消息，\
                     只用 ctx.log(...) 记一条日志；非空时原样回复 ctx.text。",
            event: EventSpec::text(""),
            check: check_15,
        },
        Case {
            id: 16,
            title: "E⑯ 超长文本只回固定话术",
            expect: "回复 == 「消息太长了，请分段发送」",
            prompt: "需求：ctx.text 长度超过 1000 字时，不要原样回显，只回复固定话术\
                     「消息太长了，请分段发送」；不超过 1000 字时原样回复 ctx.text。",
            event: EventSpec {
                message_id: "msg-0016",
                sender_id: "u-1001",
                inbound: Inbound::Long(5000),
            },
            check: check_16,
        },
        Case {
            id: 17,
            title: "E⑰ 非文本消息回固定话术",
            expect: "回复 == 「暂不支持非文本消息」",
            prompt: "需求：非文本消息（ctx.isText === false，例如 ctx.raw 为「[图片]」）\
                     回复固定话术「暂不支持非文本消息」；文本消息（ctx.isText === true）原样回复 ctx.text。",
            event: EventSpec {
                message_id: "msg-0017",
                sender_id: "u-1001",
                inbound: Inbound::NonText("[图片]"),
            },
            check: check_17,
        },
        Case {
            id: 18,
            title: "F⑱ 只 log 不回复",
            expect: "0 条回复 + 至少 1 条日志",
            prompt: "需求：收到消息后只用 ctx.log(...) 记一条日志（内容不限），不要回复任何消息。",
            event: EventSpec::text("记一条日志"),
            check: check_18,
        },
        Case {
            id: 19,
            title: "F⑲ 一条入站只回一条",
            expect: "恰好 1 条回复且含入站文本",
            prompt: "需求：一条入站消息只能产生一条回复，绝对不能重复调用 reply；\
                     回复内容包含 ctx.text。",
            event: EventSpec::text("只回一条"),
            check: check_19,
        },
        Case {
            id: 20,
            title: "F⑳ 回复含消息 id",
            expect: "回复含 messageId",
            prompt: "需求：回复里必须包含消息 id（ctx.messageId），便于排查问题。",
            event: EventSpec {
                message_id: "msg-2026-0001",
                sender_id: "u-1001",
                inbound: Inbound::Text("包含消息 id"),
            },
            check: check_20,
        },
    ]
}

// ---------------------------------------------------------------------------
// system prompt：把运行时的真实契约讲清楚（模型看不到源码以外的文档）
// ---------------------------------------------------------------------------

const SYSTEM_PROMPT: &str = "\
你在一个 hbot 机器人项目里改 TypeScript 代码。运行时会用 QuickJS 直接执行 src/main.ts\
（只做语法转译，不做类型检查：类型错误不影响运行，语法错误会让项目编译失败）。

## 项目结构
- 入口 src/main.ts，形如 `export default defineBot({ async onMessage(ctx) { ... } })`。
- 可写目录：项目根、src/、src/lib/（已存在）。没有建目录的工具，只能往已存在的目录写。
- import 只允许相对路径（`./x`、`./lib/x`）和内建模块 `hbot`。没有 npm 包，\
没有 fs / net / process / require / eval / 动态 import()。

## 可用工具
read_file、list_dir、write_file、edit_file。write_file 会整体覆盖文件。\
没有 run_command，你无法执行代码或跑测试，所以必须一次写对。

## hbot 模块
- defineBot(config)：返回 config；config.onMessage 必须是函数。
- log(message: string)：写一条运行日志，用户看不到。
- reply(text: string)：给当前这条消息回一条文本。
- sleep(ms: number): Promise<void>
- ctx.log / ctx.reply / ctx.sleep 是同一组函数，挂在事件对象上。

## onMessage(ctx) 的字段（OneBot11 群消息）
- ctx.type: string，固定 \"message\"
- ctx.text: string，文本内容；非文本消息为 \"\"
- ctx.raw: string，原始文本；非文本消息是占位串（如 \"[图片]\"）
- ctx.isText: boolean，false 表示图片/表情等非文本消息
- ctx.messageId: string，消息 id
- ctx.senderId: string，发送者 id
- ctx.selfId: string，机器人自己的 id
- ctx.conversationId: string，会话 id
- ctx.isGroup: boolean

## 要求
- 每条入站消息默认只回一条。
- 回复内容直接给用户看，用中文，不要解释你的实现。
- 改完文件就结束，不要反复确认。
";

// ---------------------------------------------------------------------------
// 单条任务：跑模型 → harness 自己编译 + 调度 → 断言
// ---------------------------------------------------------------------------

#[derive(Clone, Copy, PartialEq, Eq)]
enum Verdict {
    Pass,
    /// 模型没做到（写错代码 / 行为不符合要求）。
    Model,
    /// 我们的链路有问题（provider、编译、沙箱、调度本身）。
    Harness,
}

impl Verdict {
    fn tag(self) -> &'static str {
        match self {
            Self::Pass => "PASS",
            Self::Model => "FAIL",
            Self::Harness => "HARNESS",
        }
    }
}

struct Row {
    id: u32,
    title: &'static str,
    compiled: bool,
    expect: &'static str,
    actual: String,
    steps: u32,
    elapsed: Duration,
    verdict: Verdict,
    detail: String,
}

fn collect_sources(root: &Path) -> Vec<(String, String)> {
    fn walk(root: &Path, dir: &Path, out: &mut Vec<(String, String)>) {
        let Ok(entries) = std::fs::read_dir(dir) else {
            return;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                walk(root, &path, out);
            } else if matches!(
                path.extension().and_then(|ext| ext.to_str()),
                Some("ts" | "js" | "mjs")
            ) {
                let Ok(text) = std::fs::read_to_string(&path) else {
                    continue;
                };
                let relative = path
                    .strip_prefix(root)
                    .map(|rest| rest.to_string_lossy().replace('\\', "/"))
                    .unwrap_or_else(|_| path.to_string_lossy().into_owned());
                out.push((relative, text));
            }
        }
    }
    let mut out = Vec::new();
    walk(root, &root.join("src"), &mut out);
    out.sort();
    out
}

fn describe_code_error(error: &CodeError) -> String {
    first_line(&error.to_string(), 200).to_owned()
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
#[ignore = "需要真 provider（HBOT_LLM_BASE_URL/API_KEY/MODEL），手动跑"]
async fn real_llm_writes_working_bots() {
    let Some(config) = LlmConfig::from_env() else {
        return;
    };

    let transport = match ReqwestTransport::new() {
        Ok(transport) => transport,
        Err(error) => panic!("无法装配 reqwest 传输层：{error}"),
    };
    let provider = OpenAiProvider::new(config.base_url.clone(), config.api_key.clone(), transport)
        .with_timeout(Duration::from_secs(120));

    println!("== 真 LLM 评测 ==");
    println!("endpoint = {}", provider.endpoint());
    println!("model    = {}", config.model);
    println!("sandbox  = workspace_write，工具 = read_file/list_dir/write_file/edit_file");
    println!();

    let only = only_filter();
    let all = cases();
    let selected: Vec<&Case> = all
        .iter()
        .filter(|case| only.as_ref().is_none_or(|ids| ids.contains(&case.id)))
        .collect();
    println!("本轮跑 {} 条任务", selected.len());
    println!();

    let mut rows: Vec<Row> = Vec::new();
    let mut usage = UsageTotal::default();

    for (index, case) in selected.iter().enumerate() {
        println!(
            "[{}/{}] #{} {}",
            index + 1,
            selected.len(),
            case.id,
            case.title
        );
        let row = run_case(&provider, &config, case, &mut usage).await;
        println!(
            "      -> {} ({} 步, {:.1}s) {}",
            row.verdict.tag(),
            row.steps,
            row.elapsed.as_secs_f64(),
            row.actual
        );
        rows.push(row);
    }

    print_table(&rows);
    print_failures(&rows);
    usage.print();

    assert_eq!(rows.len(), selected.len(), "每条任务都要产出一行结果");

    let passed = rows
        .iter()
        .filter(|row| row.verdict == Verdict::Pass)
        .count();
    let harness: Vec<&Row> = rows
        .iter()
        .filter(|row| row.verdict == Verdict::Harness)
        .collect();
    println!();
    println!("PASSED {passed}/{}", rows.len());
    assert!(
        harness.is_empty(),
        "有 {} 条是 harness 侧失败（必须修，不是模型能力问题）：{:?}",
        harness.len(),
        harness.iter().map(|row| row.id).collect::<Vec<_>>()
    );
}

fn only_filter() -> Option<Vec<u32>> {
    let raw = std::env::var("HBOT_LLM_EVAL_ONLY").ok()?;
    let ids: Vec<u32> = raw
        .split(',')
        .filter_map(|part| part.trim().parse().ok())
        .collect();
    if ids.is_empty() { None } else { Some(ids) }
}

#[derive(Default)]
struct UsageTotal {
    prompt: u64,
    completion: u64,
    total: u64,
    turns: u32,
}

impl UsageTotal {
    fn add(&mut self, usage: &hbot_agent::Usage) {
        self.prompt += usage.prompt_tokens;
        self.completion += usage.completion_tokens;
        self.total += usage.total_tokens;
        self.turns += 1;
    }

    fn print(&self) {
        println!();
        println!("== token 消耗（provider 回传的 usage 累加，含重试）==");
        println!(
            "turns = {}，prompt = {}，completion = {}，total = {}",
            self.turns, self.prompt, self.completion, self.total
        );
    }
}

async fn run_case(
    provider: &dyn Provider,
    config: &LlmConfig,
    case: &Case,
    usage: &mut UsageTotal,
) -> Row {
    let started = Instant::now();
    let mut last_infra: Option<String> = None;

    // provider 抖一下不算能力问题：同一条任务最多重开两次（每次都是全新项目）。
    for attempt in 0..2 {
        let project = TempProject::new(case.id);
        if let Err(error) = scaffold(&project.path, "eval-bot") {
            return harness_row(case, started, format!("scaffold 失败：{error}"));
        }
        // 写工具不会建父目录，而工具集里没有 mkdir —— 先把 src/lib 备好。
        if let Err(error) = std::fs::create_dir_all(project.path.join("src/lib")) {
            return harness_row(case, started, format!("建 src/lib 失败：{error}"));
        }
        for (relative, content) in seed_files(case.id) {
            let target = project.path.join(relative);
            if let Some(parent) = target.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            if let Err(error) = std::fs::write(&target, content) {
                return harness_row(
                    case,
                    started,
                    format!("写前置文件 {relative} 失败：{error}"),
                );
            }
        }
        // 记下脚手架的入口内容：模型必须真的改过它。否则「一个工具都没调」的回合同样
        // 能跑出脚手架的回复，某些断言（例如「只回一条」）会假阳性通过。
        let baseline_main =
            std::fs::read_to_string(project.path.join("src/main.ts")).unwrap_or_default();

        let workspace = match Workspace::open(project.path.clone()) {
            Ok(workspace) => Arc::new(workspace),
            Err(error) => {
                return harness_row(case, started, format!("打开 workspace 失败：{error}"));
            }
        };
        let mut registry = ToolRegistry::new();
        registry.register(Arc::new(ReadFileTool));
        registry.register(Arc::new(ListDirTool));
        registry.register(Arc::new(WriteFileTool));
        registry.register(Arc::new(EditFileTool));
        let gateway = ToolGateway::new(
            Arc::new(registry),
            Arc::clone(&workspace),
            SandboxMode::WorkspaceWrite,
            Arc::new(hbot_agent::ApprovalStore::new()),
            Arc::new(NullAudit),
        );

        let input = TurnInput {
            session_id: session_id(),
            system_prompt: SYSTEM_PROMPT.to_owned(),
            messages: vec![ChatMessage::user(case.prompt)],
        };
        let turn_config = TurnConfig::new(config.model.clone())
            .with_max_steps(14)
            .with_max_duration(Duration::from_secs(150));
        let cancel = CancellationToken::new();
        let approver = AllowAllApprover;
        let mut sink = RecordingSink::default();

        let outcome = tokio::time::timeout(
            Duration::from_secs(170),
            run_turn(
                provider,
                &gateway,
                input,
                &turn_config,
                &mut sink,
                &cancel,
                &approver,
            ),
        )
        .await;

        match outcome {
            Err(_) => {
                last_infra = Some("run_turn 超过 170s 硬超时".to_owned());
                if attempt == 0 {
                    continue;
                }
                return harness_row(case, started, last_infra.take().unwrap_or_default());
            }
            Ok(Err(error)) => {
                // provider/网络层失败算 harness 侧（重试一次再定论）。
                last_infra = Some(format!("run_turn 失败：{error}"));
                if attempt == 0 {
                    continue;
                }
                return harness_row(case, started, last_infra.take().unwrap_or_default());
            }
            Ok(Ok(outcome)) => {
                usage.add(&outcome.usage);
                return finish_case(
                    case,
                    &project.path,
                    &outcome,
                    &sink,
                    &baseline_main,
                    started,
                );
            }
        }
    }

    harness_row(case, started, last_infra.unwrap_or_default())
}

fn finish_case(
    case: &Case,
    dir: &Path,
    outcome: &hbot_agent::TurnOutcome,
    sink: &RecordingSink,
    baseline_main: &str,
    started: Instant,
) -> Row {
    let sources = collect_sources(dir);
    let elapsed = started.elapsed();
    let steps = outcome.steps;
    let tool_note = if sink.tool_calls.is_empty() {
        "没调任何工具".to_owned()
    } else {
        format!("工具调用 {:?}", sink.tool_calls)
    };

    let compiled = compile_project(dir);
    let compiled_ok = compiled.is_ok();
    let verification = match compiled {
        Err(error) => {
            let harness = matches!(
                error,
                CodeError::Io { .. } | CodeError::Scaffold { .. } | CodeError::SandboxDead { .. }
            );
            let actual = format!("编译失败：{}", describe_code_error(&error));
            return Row {
                id: case.id,
                title: case.title,
                compiled: false,
                expect: case.expect,
                actual,
                steps,
                elapsed,
                verdict: if harness {
                    Verdict::Harness
                } else {
                    Verdict::Model
                },
                detail: format!(
                    "{tool_note}；模型最终文本：{}\n源码：{}",
                    first_line(&outcome.text, 300),
                    summarize_sources(&sources)
                ),
            };
        }
        Ok(compiled) => compiled,
    };

    // 入口一个字节都没动 = 模型什么都没做。这不是「做对了」，要单独判掉，
    // 否则脚手架的默认回复会让某些断言（尤其「只回一条」）假阳性通过。
    let main_changed = sources
        .iter()
        .find(|(path, _)| path == "src/main.ts")
        .is_some_and(|(_, text)| text.trim() != baseline_main.trim());
    if !main_changed {
        return Row {
            id: case.id,
            title: case.title,
            compiled: compiled_ok,
            expect: case.expect,
            actual: "模型没有修改 src/main.ts".to_owned(),
            steps,
            elapsed,
            verdict: Verdict::Model,
            detail: format!(
                "{tool_note}；入口文件与脚手架一致；模型最终文本：{}\n源码：{}",
                first_line(&outcome.text, 300),
                summarize_sources(&sources)
            ),
        };
    }

    let limits = load(dir)
        .map(|manifest| manifest.sandbox_limits())
        .unwrap_or_else(|_| Limits::default());
    let sandbox = match Sandbox::spawn(verification, limits) {
        Ok(sandbox) => sandbox,
        Err(error) => {
            let harness = matches!(
                error,
                CodeError::Io { .. } | CodeError::Scaffold { .. } | CodeError::SandboxDead { .. }
            );
            let actual = format!("沙箱启动失败：{}", describe_code_error(&error));
            return Row {
                id: case.id,
                title: case.title,
                compiled: true,
                expect: case.expect,
                actual,
                steps,
                elapsed,
                verdict: if harness {
                    Verdict::Harness
                } else {
                    Verdict::Model
                },
                detail: format!(
                    "{tool_note}；模型最终文本：{}\n源码：{}",
                    first_line(&outcome.text, 300),
                    summarize_sources(&sources)
                ),
            };
        }
    };

    let event = case.event.to_json();
    let dispatched = sandbox.dispatch(&event);
    drop(sandbox);

    let dispatch = match dispatched {
        Ok(outcome) => outcome,
        Err(error) => {
            let harness = matches!(
                error,
                CodeError::Io { .. } | CodeError::Scaffold { .. } | CodeError::SandboxDead { .. }
            );
            let actual = format!("调度失败：{}", describe_code_error(&error));
            return Row {
                id: case.id,
                title: case.title,
                compiled: true,
                expect: case.expect,
                actual,
                steps,
                elapsed,
                verdict: if harness {
                    Verdict::Harness
                } else {
                    Verdict::Model
                },
                detail: format!(
                    "{tool_note}；dispatch 事件：{event}\n源码：{}",
                    summarize_sources(&sources)
                ),
            };
        }
    };

    let actual = format!(
        "{} 条回复 {:?} / {} 条日志",
        dispatch.replies.len(),
        dispatch
            .replies
            .iter()
            .map(|reply| compact(reply, 60))
            .collect::<Vec<_>>(),
        dispatch.logs.len()
    );
    let evidence = Evidence {
        dispatch,
        sources: sources.clone(),
    };

    match (case.check)(&evidence) {
        Ok(()) => Row {
            id: case.id,
            title: case.title,
            compiled: compiled_ok,
            expect: case.expect,
            actual,
            steps,
            elapsed,
            verdict: Verdict::Pass,
            detail: format!("{tool_note}；{}", summarize_sources(&sources)),
        },
        Err(reason) => Row {
            id: case.id,
            title: case.title,
            compiled: compiled_ok,
            expect: case.expect,
            actual,
            steps,
            elapsed,
            verdict: Verdict::Model,
            detail: format!(
                "断言失败：{reason}\n{tool_note}；回复 {:?}；日志 {:?}\n源码：{}",
                evidence.replies(),
                evidence.logs(),
                summarize_sources(&sources)
            ),
        },
    }
}

fn session_id() -> hbot_core::SessionId {
    "00000000-0000-0000-0000-000000000000"
        .parse()
        .expect("固定会话 id")
}

/// 有些任务要求「在已完成的一步之上继续拆」，这里给出那条任务的前置文件。
///
/// ⑫ 的前提是 ⑪ 已经把格式化逻辑落在 `src/lib/format.ts` —— 每条任务都从全新
/// 脚手架开始，所以要由 harness 把这个前提摆上，否则 ⑫ 只是在考「能不能新建一个
/// 文件」，跟「再拆一个」不是一回事。
fn seed_files(id: u32) -> &'static [(&'static str, &'static str)] {
    match id {
        12 => &[(
            "src/lib/format.ts",
            "export function formatReply(text: string): string {\n  return `回复：${text}`;\n}\n",
        )],
        _ => &[],
    }
}

fn summarize_sources(sources: &[(String, String)]) -> String {
    sources
        .iter()
        .map(|(path, text)| format!("{path}（{} 字符）", text.chars().count()))
        .collect::<Vec<_>>()
        .join("、")
}

fn harness_row(case: &Case, started: Instant, detail: String) -> Row {
    Row {
        id: case.id,
        title: case.title,
        compiled: false,
        expect: case.expect,
        actual: first_line(&detail, 80).to_owned(),
        steps: 0,
        elapsed: started.elapsed(),
        verdict: Verdict::Harness,
        detail,
    }
}

// ---------------------------------------------------------------------------
// 输出
// ---------------------------------------------------------------------------

fn print_table(rows: &[Row]) {
    println!();
    println!("== 结果表 ==");
    println!();
    println!("| # | 任务 | 编译 | 期望 | 实际 | 步数 | 耗时 | 结论 |");
    println!("|---|------|------|------|------|------|------|------|");
    for row in rows {
        println!(
            "| {} | {} | {} | {} | {} | {} | {:.1}s | {} |",
            row.id,
            row.title,
            if row.compiled { "✅" } else { "❌" },
            row.expect,
            compact(&row.actual, 56),
            row.steps,
            row.elapsed.as_secs_f64(),
            row.verdict.tag()
        );
    }
}

fn print_failures(rows: &[Row]) {
    let failures: Vec<&Row> = rows
        .iter()
        .filter(|row| row.verdict != Verdict::Pass)
        .collect();
    if failures.is_empty() {
        return;
    }
    println!();
    println!("== 失败明细（{} 条）==", failures.len());
    for row in failures {
        println!();
        println!("--- #{} {} [{}] ---", row.id, row.title, row.verdict.tag());
        println!("期望：{}", row.expect);
        println!("实际：{}", row.actual);
        for line in row.detail.lines() {
            println!("  {line}");
        }
    }
}
