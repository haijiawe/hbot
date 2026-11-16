//! S3: `ctx.device.*` end to end — real [`Sandbox`], real `dispatch`.
//!
//! Every test here builds a real isolate, evaluates a real `src/main.ts` and
//! drives a real event through it. Nothing is mocked above the two ports the
//! host owns (`DeviceGate`, `DeviceBackend`), because the whole point of the
//! gate is that *those two* are the only replaceable parts: the order
//! (sandbox mode → assembly → identity → declaration/settings/session → backend)
//! is implemented once in `crates/hbot-code/src/device/gate.rs` and must not be
//! reproducible by a caller.
//!
//! The seven numbered cases are the acceptance list; the eighth covers the
//! remaining three methods so "seven methods" is not an untested claim.

use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use hbot_code::error::CodeError;
use hbot_code::{EventOutcome, HttpConfig, Limits, Sandbox, compile_project};
use hbot_core::domain::SandboxMode;
use hbot_core::{
    BotId, ClipboardOp, ClipboardValue, DeviceBackend, DeviceCapability, DeviceCapabilityReport,
    DeviceError, DeviceGate, DeviceTarget, InputAction, Screenshot, ScreenshotRequest, WindowInfo,
};
use serde_json::{Value, json};

/// A bot id that parses; the payload carries it as `botId`.
const BOT: &str = "00000000-0000-0000-0000-000000000001";

// ---------------------------------------------------------------------------
// harness
// ---------------------------------------------------------------------------

/// A temporary project directory, removed on drop.
struct Project(PathBuf);

impl Project {
    fn new(name: &str, source: &str) -> Self {
        static COUNTER: AtomicUsize = AtomicUsize::new(0);
        let root = std::env::temp_dir().join(format!(
            "hbot-s3-{name}-{}-{}",
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

/// Compile `source` and start an isolate for it.
fn sandbox(name: &str, source: &str, http: HttpConfig) -> (Project, Sandbox) {
    let project = Project::new(name, source);
    let compiled = compile_project(&project.0).expect("编译项目");
    let sandbox =
        Sandbox::spawn_with_http(compiled, Limits::default(), http).expect("启动 isolate");
    (project, sandbox)
}

/// One event carrying the bot identity the device gate needs.
fn dispatch(sandbox: &Sandbox) -> Result<EventOutcome, CodeError> {
    sandbox.dispatch(&json!({
        "botId": BOT,
        "conversationId": "c-1",
        "messageId": "m-1",
        "text": "hi",
    }))
}

/// The message of a thrown JavaScript exception.
fn thrown(error: &CodeError) -> &str {
    match error {
        CodeError::JsException { message, .. } => message,
        other => panic!("期望 JavaScript 异常，实际是 {other:?}"),
    }
}

/// Same, for the `Result::Err` of a dispatch that must fail.
fn dispatch_error(sandbox: &Sandbox) -> String {
    match dispatch(sandbox) {
        Ok(outcome) => panic!("这次调用本该失败，实际成功：{:?}", outcome.logs),
        Err(error) => thrown(&error).to_owned(),
    }
}

// ---------------------------------------------------------------------------
// the two ports the host owns
// ---------------------------------------------------------------------------

/// What the fakes observed, so assertions can be made about *calls* and not
/// only about JS-visible results.
#[derive(Default)]
struct Recorder {
    /// Times `DeviceGate::authorize` was asked.
    authorized: AtomicUsize,
    /// Times any `DeviceBackend` method was entered.
    backend_calls: AtomicUsize,
    /// `(capability, target)` pairs the gate was asked about.
    targets: Mutex<Vec<(DeviceCapability, DeviceTarget)>>,
    /// Actions the backend received.
    actions: Mutex<Vec<InputAction>>,
    /// `allowed_apps` the backend received, per `input` call.
    apps: Mutex<Vec<Vec<String>>>,
    /// Clipboard ops the backend received.
    clipboard: Mutex<Vec<ClipboardOp>>,
}

impl Recorder {
    fn authorized(&self) -> usize {
        self.authorized.load(Ordering::SeqCst)
    }

    fn backend_calls(&self) -> usize {
        self.backend_calls.load(Ordering::SeqCst)
    }

    fn targets(&self) -> Vec<(DeviceCapability, DeviceTarget)> {
        self.targets.lock().expect("targets").clone()
    }

    fn actions(&self) -> Vec<InputAction> {
        self.actions.lock().expect("actions").clone()
    }

    fn apps(&self) -> Vec<Vec<String>> {
        self.apps.lock().expect("apps").clone()
    }

    fn clipboard(&self) -> Vec<ClipboardOp> {
        self.clipboard.lock().expect("clipboard").clone()
    }
}

/// What the fake gate answers.
enum Verdict {
    /// Every子能力 granted.
    Allow,
    /// 清单里没声明：the `NotDeclared` refusal `docs/09` §4.1 requires.
    NotDeclared,
    /// 会话仍在 / 已撤销：flipped through the `AtomicBool` without touching the
    /// isolate, which is how "撤销立刻生效" is proven.
    Session(AtomicBool),
    /// 会话已撤销，且不再可恢复.
    Revoked,
}

struct FakeGate {
    recorder: Arc<Recorder>,
    verdict: Verdict,
}

impl DeviceGate for FakeGate {
    fn authorize(
        &self,
        _bot_id: &BotId,
        capability: DeviceCapability,
        target: &DeviceTarget,
    ) -> Result<(), DeviceError> {
        self.recorder.authorized.fetch_add(1, Ordering::SeqCst);
        self.recorder
            .targets
            .lock()
            .expect("targets")
            .push((capability, target.clone()));

        match &self.verdict {
            Verdict::Allow => Ok(()),
            Verdict::NotDeclared => Err(DeviceError::NotDeclared { capability }),
            Verdict::Session(allowed) => {
                if allowed.load(Ordering::SeqCst) {
                    Ok(())
                } else {
                    Err(DeviceError::NotGranted { capability })
                }
            }
            Verdict::Revoked => Err(DeviceError::Revoked),
        }
    }
}

struct FakeBackend {
    recorder: Arc<Recorder>,
    /// Make `clipboard` fail, so the error path can be inspected for the text.
    fail_clipboard: bool,
}

impl FakeBackend {
    fn new(recorder: Arc<Recorder>) -> Self {
        Self {
            recorder,
            fail_clipboard: false,
        }
    }

    fn failing_clipboard(recorder: Arc<Recorder>) -> Self {
        Self {
            recorder,
            fail_clipboard: true,
        }
    }
}

impl DeviceBackend for FakeBackend {
    fn capabilities(&self) -> DeviceCapabilityReport {
        DeviceCapabilityReport {
            screen: true,
            input: true,
            window: true,
            clipboard: true,
            detail: None,
        }
    }

    fn screenshot(&self, request: &ScreenshotRequest) -> Result<Screenshot, DeviceError> {
        self.recorder.backend_calls.fetch_add(1, Ordering::SeqCst);
        // PNG 魔数，好让 base64 断言有一个可核对的固定值。
        let png = vec![0x89, 0x50, 0x4e, 0x47];
        if png.len() > request.max_bytes {
            return Err(DeviceError::TooLarge {
                limit_bytes: request.max_bytes,
            });
        }
        Ok(Screenshot {
            width: 2,
            height: 3,
            png,
            monitor: request.monitor.unwrap_or(0),
        })
    }

    fn input(&self, action: &InputAction, allowed_apps: &[String]) -> Result<(), DeviceError> {
        self.recorder.backend_calls.fetch_add(1, Ordering::SeqCst);
        self.recorder
            .actions
            .lock()
            .expect("actions")
            .push(action.clone());
        self.recorder
            .apps
            .lock()
            .expect("apps")
            .push(allowed_apps.to_vec());
        Ok(())
    }

    fn windows(&self) -> Result<Vec<WindowInfo>, DeviceError> {
        self.recorder.backend_calls.fetch_add(1, Ordering::SeqCst);
        Ok(vec![WindowInfo {
            id: 7,
            title: "记事本".to_owned(),
            process: "notepad.exe".to_owned(),
            focused: true,
        }])
    }

    fn clipboard(&self, op: &ClipboardOp) -> Result<ClipboardValue, DeviceError> {
        self.recorder.backend_calls.fetch_add(1, Ordering::SeqCst);
        self.recorder
            .clipboard
            .lock()
            .expect("clipboard")
            .push(op.clone());
        if self.fail_clipboard {
            // Deliberately does **not** echo the clipboard text.
            return Err(DeviceError::Failed {
                message: "剪贴板被系统占用".to_owned(),
            });
        }
        Ok(ClipboardValue {
            text: "host-clipboard".to_owned(),
        })
    }
}

/// Assemble a sandbox with `verdict` + a recording backend.
fn assemble(sandbox: &Sandbox, verdict: Verdict) -> Arc<Recorder> {
    let recorder = Arc::new(Recorder::default());
    let gate: Arc<dyn DeviceGate> = Arc::new(FakeGate {
        recorder: Arc::clone(&recorder),
        verdict,
    });
    let backend: Arc<dyn DeviceBackend> = Arc::new(FakeBackend::new(Arc::clone(&recorder)));
    sandbox.set_device(gate, backend);
    recorder
}

// ---------------------------------------------------------------------------
// ① 未注入 gate
// ---------------------------------------------------------------------------

#[test]
fn without_an_assembled_device_every_call_is_unavailable() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    const frame = await ctx.device.screen();
    ctx.log(String(frame));
  },
});
"#;
    let (_project, sandbox) = sandbox("unavailable", source, HttpConfig::deny_all());

    assert!(
        !sandbox.is_device_assembled(),
        "新建的 Sandbox 不该自带设备后端"
    );

    let message = dispatch_error(&sandbox);
    assert!(
        message.starts_with("HBOT_DEVICE_UNAVAILABLE:"),
        "未装配时必须是 HBOT_DEVICE_UNAVAILABLE，实际：{message}"
    );
    assert!(
        message.contains("设备能力尚未装配"),
        "错误信息应说清它还没被装配，实际：{message}"
    );
}

// ---------------------------------------------------------------------------
// ② 全拒的 gate
// ---------------------------------------------------------------------------

#[test]
fn a_not_declared_gate_refuses_before_the_backend_is_touched() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    await ctx.device.screen();
  },
});
"#;
    let (_project, sandbox) = sandbox("not-declared", source, HttpConfig::deny_all());
    let recorder = assemble(&sandbox, Verdict::NotDeclared);

    let message = dispatch_error(&sandbox);
    assert!(
        message.starts_with("HBOT_DEVICE_NOT_DECLARED:"),
        "清单没声明时必须是 HBOT_DEVICE_NOT_DECLARED，实际：{message}"
    );
    assert_eq!(
        recorder.backend_calls(),
        0,
        "闸拒了就不许调后端（一个字节都不碰）"
    );
    assert_eq!(recorder.authorized(), 1, "授权只问一次，且真的问过");
}

// ---------------------------------------------------------------------------
// ③ 全允许的 gate + 假后端
// ---------------------------------------------------------------------------

#[test]
fn an_allowing_gate_returns_the_frame_and_the_window_list() {
    let source = r#"
import { defineBot, device } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    const frame = await ctx.device.screen({ monitor: 1 });
    ctx.log(JSON.stringify(frame));
    // 模块级 device 与 ctx.device 是同一份实现。
    const windows = await device.windows();
    ctx.log(JSON.stringify(windows));
  },
});
"#;
    let (_project, sandbox) = sandbox("allowed", source, HttpConfig::deny_all());
    let recorder = assemble(&sandbox, Verdict::Allow);

    let outcome = dispatch(&sandbox).expect("全允许时应成功");
    assert_eq!(outcome.logs.len(), 2, "两次 log：{:?}", outcome.logs);

    let frame: Value = serde_json::from_str(&outcome.logs[0]).expect("frame 是 JSON");
    assert_eq!(frame["width"], json!(2));
    assert_eq!(frame["height"], json!(3));
    assert_eq!(frame["monitor"], json!(1));
    assert_eq!(
        frame["pngBase64"],
        json!("iVBORw=="),
        "PNG 必须是 base64 字符串（JS 侧没有 Buffer）"
    );

    let windows: Value = serde_json::from_str(&outcome.logs[1]).expect("windows 是 JSON");
    assert_eq!(
        windows,
        json!([{ "id": 7, "title": "记事本", "process": "notepad.exe", "focused": true }])
    );
    assert_eq!(recorder.backend_calls(), 2, "截图 + 枚举各一次");
}

// ---------------------------------------------------------------------------
// ④ read-only 档
// ---------------------------------------------------------------------------

#[test]
fn read_only_refuses_even_an_allowing_gate_and_never_calls_the_backend() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    await ctx.device.screen();
  },
});
"#;
    // read-only 档：`permissions.device` 写成什么都不能放行。
    let (_project, sandbox) = sandbox(
        "read-only",
        source,
        HttpConfig::new(SandboxMode::ReadOnly, Vec::new()),
    );
    let recorder = assemble(&sandbox, Verdict::Allow);

    let message = dispatch_error(&sandbox);
    assert!(
        message.starts_with("HBOT_DEVICE_NOT_GRANTED:"),
        "read-only 档应报 denied，实际：{message}"
    );
    assert!(
        message.contains("read-only"),
        "错误信息要点明档位，实际：{message}"
    );
    assert_eq!(
        recorder.backend_calls(),
        0,
        "read-only 档下后端调用计数必须是 0（不开 socket、不碰 OS）"
    );
    assert_eq!(
        recorder.authorized(),
        0,
        "read-only 档在闸之前就该拒掉，连授权都不用问"
    );
}

// ---------------------------------------------------------------------------
// ⑤ click 的参数透传
// ---------------------------------------------------------------------------

#[test]
fn click_forwards_the_coordinates_the_button_and_the_allowed_apps() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    await ctx.device.click(10.6, 20.4, 'right');
    ctx.log('clicked');
  },
});
"#;
    let (_project, sandbox) = sandbox("click", source, HttpConfig::deny_all());
    let recorder = assemble(&sandbox, Verdict::Allow);
    sandbox.set_device_apps(vec!["notepad.exe".to_owned()]);

    let outcome = dispatch(&sandbox).expect("应成功");
    assert_eq!(outcome.logs, vec!["clicked".to_owned()]);
    assert_eq!(
        recorder.actions(),
        vec![InputAction::Click {
            // 四舍五入到物理像素；负数合法，NaN/∞ 会被拒。
            x: 11,
            y: 20,
            button: hbot_core::MouseButton::Right,
        }],
        "传给 backend 的 InputAction 必须与调用一致"
    );
    assert_eq!(
        recorder.targets(),
        vec![(DeviceCapability::Input, DeviceTarget::None)],
        "点的是坐标，没有窗口目标；能力必须是 input"
    );
    assert_eq!(
        recorder.apps(),
        vec![vec!["notepad.exe".to_owned()]],
        "白名单原样传给 backend，让它做最后一道前台比对"
    );
}

// ---------------------------------------------------------------------------
// ⑥ 撤销立刻生效（不是 spawn 时快照）
// ---------------------------------------------------------------------------

#[test]
fn revoking_the_session_takes_effect_on_the_very_next_call() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    const windows = await ctx.device.windows();
    ctx.log('OK ' + windows.length);
  },
});
"#;
    let (_project, sandbox) = sandbox("revoke", source, HttpConfig::deny_all());
    let recorder = Arc::new(Recorder::default());
    let gate: Arc<dyn DeviceGate> = Arc::new(FakeGate {
        recorder: Arc::clone(&recorder),
        verdict: Verdict::Session(AtomicBool::new(true)),
    });
    let backend: Arc<dyn DeviceBackend> = Arc::new(FakeBackend::new(Arc::clone(&recorder)));
    sandbox.set_device(gate, backend);

    // First call: allowed, and it really reached the backend.
    let outcome = dispatch(&sandbox).expect("撤销前应成功");
    assert_eq!(outcome.logs, vec!["OK 1".to_owned()]);
    assert_eq!(recorder.backend_calls(), 1);

    // Revoke: replace the gate on the *same running isolate* — no re-spawn, no
    // new `Sandbox`. If authorization had been snapshotted at spawn time this
    // second dispatch would still succeed.
    let revoked: Arc<dyn DeviceGate> = Arc::new(FakeGate {
        recorder: Arc::clone(&recorder),
        verdict: Verdict::Revoked,
    });
    let backend: Arc<dyn DeviceBackend> = Arc::new(FakeBackend::new(Arc::clone(&recorder)));
    sandbox.set_device(revoked, backend);

    let message = dispatch_error(&sandbox);
    assert!(
        message.starts_with("HBOT_DEVICE_REVOKED:"),
        "撤销后必须在**下一次**调用就失败，实际：{message}"
    );
    assert_eq!(
        recorder.backend_calls(),
        1,
        "被撤销的这次调用不许再碰后端（计数仍是第一次那一次）"
    );
}

// ---------------------------------------------------------------------------
// ⑦ clipboardWrite 的内容不进错误信息
// ---------------------------------------------------------------------------

#[test]
fn a_failing_clipboard_write_never_echoes_what_was_written() {
    const SECRET: &str = "SECRET-CLIPBOARD-PAYLOAD-42";

    let source = format!(
        r#"
import {{ defineBot }} from 'hbot';

export default defineBot({{
  async onMessage(ctx) {{
    await ctx.device.clipboardWrite('{SECRET}');
  }},
}});
"#
    );
    let (_project, sandbox) = sandbox("clipboard-error", &source, HttpConfig::deny_all());

    let recorder = Arc::new(Recorder::default());
    let gate: Arc<dyn DeviceGate> = Arc::new(FakeGate {
        recorder: Arc::clone(&recorder),
        verdict: Verdict::Allow,
    });
    let backend: Arc<dyn DeviceBackend> =
        Arc::new(FakeBackend::failing_clipboard(Arc::clone(&recorder)));
    sandbox.set_device(gate, backend);

    let message = dispatch_error(&sandbox);
    assert!(
        message.starts_with("HBOT_DEVICE_FAILED:"),
        "后端失败应转成 HBOT_DEVICE_FAILED，实际：{message}"
    );
    assert!(
        !message.contains(SECRET),
        "剪贴板内容不许进异常信息：{message}"
    );
    assert!(
        !message.contains("CLIPBOARD-PAYLOAD"),
        "哪怕是片段也不许回显：{message}"
    );
    assert_eq!(
        recorder.clipboard(),
        vec![ClipboardOp::Write {
            text: SECRET.to_owned(),
        }],
        "内容确实传到了后端（只是不进错误信息）"
    );
}

// ---------------------------------------------------------------------------
// ⑧ 其余三个方法（key / keyCombo / clipboardRead）
// ---------------------------------------------------------------------------

#[test]
fn key_combo_and_clipboard_read_hit_the_backend_in_order() {
    let source = r#"
import { defineBot } from 'hbot';

export default defineBot({
  async onMessage(ctx) {
    await ctx.device.key('hello');
    await ctx.device.keyCombo(['ctrl', 'c']);
    const text = await ctx.device.clipboardRead();
    ctx.log(text);
  },
});
"#;
    let (_project, sandbox) = sandbox("rest", source, HttpConfig::deny_all());
    let recorder = assemble(&sandbox, Verdict::Allow);

    let outcome = dispatch(&sandbox).expect("应成功");
    assert_eq!(
        outcome.logs,
        vec!["host-clipboard".to_owned()],
        "clipboardRead 应把后端内容原样返回"
    );
    assert_eq!(
        recorder.actions(),
        vec![
            InputAction::Text {
                text: "hello".to_owned(),
            },
            InputAction::Combo {
                keys: vec!["ctrl".to_owned(), "c".to_owned()],
            },
        ]
    );
    assert_eq!(recorder.clipboard(), vec![ClipboardOp::Read]);
    assert_eq!(recorder.backend_calls(), 3, "两次输入 + 一次读剪贴板");
}
