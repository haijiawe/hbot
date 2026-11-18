//! S2: `ctx.ai.complete` 端到端 —— 真的 [`Sandbox`]、真的 `dispatch`。
//!
//! 每个测试都建一个真 isolate、编译真的 `src/main.ts`、把事件真的跑一圈。唯一被
//! 替换掉的是宿主那个端口（`AiClient`），因为它是**唯一**该被替换的部分：参数闸、
//! 每事件预算、等模型时间的抵账、超时与错误码都在
//! `crates/hbot-code/src/{ai.rs,runtime/ai_api.rs}` 里实现一次，调用方不该能复现
//! 第二份。
//!
//! ① 未装配 → `HBOT_AI_UNAVAILABLE`，且后端一次都没被调用；
//! ② 装配后补全返回 `text` / `promptTokens` / `completionTokens` / `model`；
//! ③–⑤ 坏参数（缺 prompt、超长 prompt、maxTokens 越界、temperature 越界）在**后端
//!    被碰之前**就被拒 —— 参数错不许占一次配额、也不许变成"模型不可用"；
//! ⑥ **时间抵账**：一次 1.2 s 的补全之后再 `sleep(1200)` 仍然成功（合计 2.4 s，
//!    超过 2 s 的 handler 预算）；⑦ 反向对照：不放补全、同样两段 `sleep` 必须超时
//!    —— 否则 ⑥ 可能只是"预算根本没生效"；
//! ⑧ 每事件最多 4 次，第 5 次 `HBOT_AI_BUDGET`；⑨ `reset_event` 让下一个事件重新
//!    拿到额度（不清零的话同一个 bot 一辈子只能补全四次）；
//! ⑩ 上游错误原样上报（`HBOT_AI_FAILED`），不被吞成"不可用"。

use std::future::Future;
use std::path::PathBuf;
use std::pin::Pin;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hbot_code::error::CodeError;
use hbot_code::{EventOutcome, HttpConfig, Limits, Sandbox, compile_project};
use hbot_core::{AiClient, AiError, AiRequest, AiText, AiUsage};

/// 能解析成 `BotId` 的 id；事件载荷里的 `botId`。
const BOT: &str = "00000000-0000-0000-0000-000000000001";
/// 同上，`conversationId`。
const CONVERSATION: &str = "00000000-0000-0000-0000-000000000002";

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// 临时项目目录，Drop 时删掉。
struct Project(PathBuf);

impl Project {
    fn new(name: &str, source: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "hbot-s2-ai-{name}-{}-{}",
            std::process::id(),
            COUNTER.fetch_add(1, Ordering::Relaxed)
        ));
        let src = root.join("src");
        std::fs::create_dir_all(&src).expect("建项目目录");
        std::fs::write(src.join("main.ts"), source).expect("写 src/main.ts");
        Self(root)
    }
}

impl Drop for Project {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.0);
    }
}

/// 编译 `source` 并起一个 isolate（handler 预算用给定的值）。
fn sandbox(name: &str, source: &str, handler: Duration) -> (Project, Sandbox) {
    let project = Project::new(name, source);
    let compiled = compile_project(&project.0).expect("编译项目");
    let limits = Limits::default().with_handler_timeout(handler);
    let sandbox =
        Sandbox::spawn_with_http(compiled, limits, HttpConfig::deny_all()).expect("启动 isolate");
    (project, sandbox)
}

/// 一条带 bot 身份与会话身份的事件。
fn dispatch(sandbox: &Sandbox) -> Result<EventOutcome, CodeError> {
    sandbox.dispatch(&serde_json::json!({
        "botId": BOT,
        "conversationId": CONVERSATION,
        "messageId": "m-1",
        "text": "hi",
    }))
}

/// 抛出来的 JavaScript 异常 message。
fn thrown(error: &CodeError) -> &str {
    match error {
        CodeError::JsException { message, .. } => message,
        other => panic!("期望 JavaScript 异常，实际是 {other:?}"),
    }
}

/// 必须失败的那次 dispatch 的 message。
fn dispatch_error(sandbox: &Sandbox) -> String {
    match dispatch(sandbox) {
        Ok(outcome) => panic!("这次调用本该失败，实际成功：{:?}", outcome.logs),
        Err(error) => thrown(&error).to_owned(),
    }
}

// ---------------------------------------------------------------------------
// 宿主那个端口
// ---------------------------------------------------------------------------

/// 假模型客户端。
///
/// 记的不只是返回什么，还有**被调用几次**：坏参数与未装配的用例要证明后端连一次
/// 都没有被碰；预算用例要证明第 5 次根本没走到这里。
struct FakeAi {
    calls: Arc<hbot_code::CallCounter>,
    delay: Duration,
    result: Mutex<Result<AiText, AiError>>,
    /// 收到的 prompt（按调用顺序），用于断言参数真的传下来了。
    prompts: Mutex<Vec<String>>,
}

impl FakeAi {
    fn answering(text: &str) -> Self {
        Self::with_result(Ok(AiText {
            text: text.to_owned(),
            usage: AiUsage {
                prompt_tokens: 11,
                completion_tokens: 7,
            },
            model: "fake-model".to_owned(),
        }))
    }

    fn with_result(result: Result<AiText, AiError>) -> Self {
        Self {
            calls: Arc::new(hbot_code::CallCounter::default()),
            delay: Duration::ZERO,
            result: Mutex::new(result),
            prompts: Mutex::new(Vec::new()),
        }
    }

    fn slow(mut self, delay: Duration) -> Self {
        self.delay = delay;
        self
    }

    fn calls(&self) -> u64 {
        self.calls.count()
    }
}

impl AiClient for FakeAi {
    fn complete<'a>(
        &'a self,
        request: &'a AiRequest,
    ) -> Pin<Box<dyn Future<Output = Result<AiText, AiError>> + Send + 'a>> {
        self.calls.bump();
        self.prompts
            .lock()
            .expect("prompts")
            .push(request.prompt.clone());
        let delay = self.delay;
        let result = self.result.lock().expect("result").clone();
        Box::pin(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            result
        })
    }
}

/// 装配假后端，返回它以便断言。
fn assemble(sandbox: &Sandbox, client: &Arc<FakeAi>) -> Arc<FakeAi> {
    let concrete = Arc::clone(client);
    let erased: Arc<dyn AiClient> = concrete;
    sandbox.set_ai(erased);
    Arc::clone(client)
}

// ---------------------------------------------------------------------------
// ① 未装配
// ---------------------------------------------------------------------------

#[test]
fn without_an_assembled_client_the_call_is_unavailable() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    await ctx.ai.complete({ prompt: '你好' });
  },
});
"#;
    let (_project, sandbox) = sandbox("unavailable", source, Duration::from_secs(2));

    assert!(
        !sandbox.is_ai_assembled(),
        "新建的 Sandbox 不该自带模型客户端"
    );

    let message = dispatch_error(&sandbox);
    assert!(
        message.starts_with("HBOT_AI_UNAVAILABLE:"),
        "未装配时必须是 HBOT_AI_UNAVAILABLE，实际：{message}"
    );
    assert!(
        message.contains("宿主没有可用的模型配置"),
        "错误信息应说清模型还没被配置，实际：{message}"
    );
}

// ---------------------------------------------------------------------------
// ② 正常补全
// ---------------------------------------------------------------------------

#[test]
fn a_declared_call_returns_the_documented_shape() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    const result = await ctx.ai.complete({ prompt: '说一句', system: '你是助手', maxTokens: 64, temperature: 0.5 });
    ctx.log(result.text);
    ctx.log(String(result.promptTokens) + '/' + String(result.completionTokens));
    ctx.log(result.model);
    ctx.log(Object.keys(result).join(','));
  },
});
"#;
    let (_project, sandbox) = sandbox("shape", source, Duration::from_secs(2));
    let client = assemble(&sandbox, &Arc::new(FakeAi::answering("收到")));

    let outcome = dispatch(&sandbox).expect("装配后应成功");
    assert_eq!(
        outcome.logs,
        vec![
            "收到".to_owned(),
            "11/7".to_owned(),
            "fake-model".to_owned(),
            // 四个键都在、且没有第五个（文档承诺的形状就是这四个）。
            "text,promptTokens,completionTokens,model".to_owned()
        ],
        "返回值形状应与 hbot.d.ts 一致"
    );
    assert_eq!(client.calls(), 1, "一次补全应只调一次后端");
    assert_eq!(
        client.prompts.lock().expect("prompts").as_slice(),
        ["说一句".to_owned()],
        "prompt 应原样传到后端"
    );
}

// ---------------------------------------------------------------------------
// ③–⑤ 坏参数：后端一次都不许被碰
// ---------------------------------------------------------------------------

#[test]
fn bad_arguments_are_refused_before_the_backend_is_touched() {
    // 每段代码都在 handler 里用 try/catch 打印错误 message 的第一段（码），
    // 这样一次 dispatch 能把四种坏参数都验掉。
    let source = r#"
import { defineBot } from 'hbot';

function codeOf(error) {
  const message = error.message;
  const index = message.indexOf(':');
  return index < 0 ? message : message.slice(0, index);
}

export default defineBot({
  async onMessage(ctx) {
    const attempts = [
      {},
      { prompt: 'x'.repeat(32769) },
      { prompt: 'ok', maxTokens: 0 },
      { prompt: 'ok', maxTokens: 4097 },
      { prompt: 'ok', temperature: 2.5 },
      { prompt: 1 },
    ];
    for (const attempt of attempts) {
      try {
        await ctx.ai.complete(attempt);
        ctx.log('NO_ERROR');
      } catch (error) {
        ctx.log(codeOf(error));
      }
    }
  },
});
"#;
    let (_project, sandbox) = sandbox("bad-args", source, Duration::from_secs(5));
    let client = assemble(&sandbox, &Arc::new(FakeAi::answering("不该被调用")));

    let outcome = dispatch(&sandbox).expect("坏参数应被 catch 住，dispatch 本身不失败");
    assert_eq!(
        outcome.logs,
        vec!["HBOT_AI_INVALID"; 6],
        "六种坏参数都必须是 HBOT_AI_INVALID，实际：{:?}",
        outcome.logs
    );
    assert_eq!(client.calls(), 0, "坏参数不许占一次后端调用");
}

// ---------------------------------------------------------------------------
// ⑥–⑦ 时间抵账与反向对照
// ---------------------------------------------------------------------------

/// 一次 1.2 s 的补全之后再做 1.2 s 的忙等：墙钟合计 2.4 s，超过 2 s 的 handler 预算。
///
/// 忙等（而不是 `sleep`）是关键：**`sleep` 会被夹到剩余预算**，所以它无论如何都在
/// 截止时刻返回，用它测不出"预算有没有被抵账"。忙等不夹，于是：
///
/// * 抵账后 handler 在 t=1.2 s 拿到完整的 2 s，截止时刻 3.2 s，1.2 s 的忙等在
///   2.4 s 结束 → 成功；
/// * 不抵账则截止时刻 2.0 s，忙等在 2.0 s 被中断器打断 → `HBOT_TIMEOUT`。
///
/// 上面这条 2400 ms 的忙等由 `Date.now()` 卡时长，不依赖机器快慢。
#[test]
fn model_wait_does_not_count_against_the_handler_budget() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    const result = await ctx.ai.complete({ prompt: '慢一点' });
    const started = Date.now();
    while (Date.now() - started < 1200) { /* 忙等，不夹预算 */ }
    ctx.log(result.text);
  },
});
"#;
    let (_project, sandbox) = sandbox("credit", source, Duration::from_secs(2));
    let client = FakeAi::answering("抵账成功").slow(Duration::from_millis(1200));
    let client = assemble(&sandbox, &Arc::new(client));

    let outcome = dispatch(&sandbox).expect("补全的等待应从 handler 预算里抵掉");
    assert_eq!(outcome.logs, vec!["抵账成功".to_owned()]);
    assert_eq!(client.calls(), 1);
}

/// 反向对照：同样 2400 ms 的忙等、不放补全，必须超时。
///
/// 没有这条，上面那条可能只是"预算根本没生效"（比如 `Date.now()` 被夹住、
/// 或者中断器压根没接上）。它同时钉住"中断器在这个 harness 里真的会打断脚本"。
#[test]
fn the_same_busy_wait_without_a_model_call_still_times_out() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    const started = Date.now();
    while (Date.now() - started < 2400) { /* 忙等，不夹预算 */ }
    ctx.log('不会到这里');
  },
});
"#;
    let (_project, sandbox) = sandbox("control", source, Duration::from_secs(2));

    let error = dispatch(&sandbox).expect_err("2.4 s 的忙等必须撞 2 s 预算");
    assert!(
        matches!(error, CodeError::Timeout { .. }),
        "应是超时，实际：{error:?}"
    );
}

// ---------------------------------------------------------------------------
// ⑧–⑨ 每事件额度与清零
// ---------------------------------------------------------------------------

#[test]
fn the_fifth_call_in_one_event_is_a_budget_error() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    for (let index = 0; index < 4; index += 1) {
      await ctx.ai.complete({ prompt: '第 ' + String(index) + ' 次' });
    }
    ctx.log('四次都过了');
    try {
      await ctx.ai.complete({ prompt: '第 5 次' });
      ctx.log('NO_ERROR');
    } catch (error) {
      ctx.log(error.message);
    }
  },
});
"#;
    let (_project, sandbox) = sandbox("budget", source, Duration::from_secs(5));
    let client = assemble(&sandbox, &Arc::new(FakeAi::answering("ok")));

    let outcome = dispatch(&sandbox).expect("前四次应成功；第五次被 catch");
    assert_eq!(outcome.logs.len(), 2, "logs：{:?}", outcome.logs);
    assert_eq!(outcome.logs[0], "四次都过了");
    assert!(
        outcome.logs[1].starts_with("HBOT_AI_BUDGET:"),
        "第 5 次必须是 HBOT_AI_BUDGET，实际：{}",
        outcome.logs[1]
    );
    assert_eq!(client.calls(), 4, "第 5 次不该走到后端");
}

#[test]
fn a_second_event_gets_its_own_call_budget() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    for (let index = 0; index < 4; index += 1) {
      await ctx.ai.complete({ prompt: 'p' });
    }
    ctx.log('四次都过了');
  },
});
"#;
    let (_project, sandbox) = sandbox("reset", source, Duration::from_secs(5));
    let client = assemble(&sandbox, &Arc::new(FakeAi::answering("ok")));

    // 同一个 isolate 连着跑两个事件：不清零的话第二个事件的第一次调用就 BUDGET。
    for round in 1..=2 {
        let outcome = dispatch(&sandbox).unwrap_or_else(|error| {
            panic!("第 {round} 个事件应成功，实际：{error:?}");
        });
        assert_eq!(
            outcome.logs,
            vec!["四次都过了".to_owned()],
            "第 {round} 个事件"
        );
    }
    assert_eq!(client.calls(), 8, "两个事件各四次");
}

// ---------------------------------------------------------------------------
// ⑩ 上游错误
// ---------------------------------------------------------------------------

#[test]
fn an_upstream_failure_keeps_its_own_code() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    try {
      await ctx.ai.complete({ prompt: 'p' });
      ctx.log('NO_ERROR');
    } catch (error) {
      ctx.log(String(error.message.startsWith('HBOT_AI_FAILED:')));
    }
  },
});
"#;
    let (_project, sandbox) = sandbox("upstream", source, Duration::from_secs(5));
    let client = FakeAi::with_result(Err(AiError::Failed {
        message: "上游 401：密钥无效".to_owned(),
    }));
    let client = assemble(&sandbox, &Arc::new(client));

    let outcome = dispatch(&sandbox).expect("上游错误应被 catch");
    assert_eq!(outcome.logs, vec!["true".to_owned()]);
    assert_eq!(client.calls(), 1, "上游错误不该被重试成两次");
}
