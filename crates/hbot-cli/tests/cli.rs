//! `hbot` CLI 的纯函数级测试。
//!
//! 约定：不真的拉起进程（`serve` 需要信号、`check` 需要磁盘），只测
//! "参数 → 配置"、"配置 → 输出行"这两段纯逻辑，外加 `check` 对临时目录的判断。

use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};

use clap::Parser as _;
use hbot_cli::check::check_project;
use hbot_cli::cli::{Cli, Command};
use hbot_cli::serve::{ServePlan, TokenSource, generate_token, ready_line, resolve_token};
use hbot_cli::version::version_text;
use serde_json::Value;

/// 每次测试用独立临时目录（不引 `tempfile`，避免新依赖）。
fn temp_project(tag: &str) -> PathBuf {
    static COUNTER: AtomicU32 = AtomicU32::new(0);
    let sequence = COUNTER.fetch_add(1, Ordering::Relaxed);
    std::env::temp_dir().join(format!(
        "hbot-cli-test-{tag}-{}-{sequence}",
        std::process::id()
    ))
}

/// 写一个**能通过新语义体检**的最小项目。
///
/// 旧版本这里只写 `{}` —— 那时的 `check` 只回答"文件在不在"。现在 `check` 会把
/// 项目一路编译到"可运行"（清单合规 → 入口存在 → SWC 编过 → d.ts 未漂移），
/// 一个空对象清单和空 `src/` 必然失败。所以这里改用核心自己的脚手架，避免测试
/// 手抄一份会过期的清单文本（同 `AGENTS.md` §7.18 的"不要断言源码文本"）。
fn write_manifest(dir: &Path) {
    hbot_code::manifest::scaffold(dir, "fixture").expect("脚手架应成功");
}

/// 骨架一个能编过的项目，然后把 `platform` 块换成测试要的形状。
///
/// 用脚手架而不是手抄清单：`entry` 文件、`hbot.d.ts` 都得在，否则 `check` 会先
/// 在别的检查项上失败，测不到平台这一项。
fn write_project_with_platform(dir: &Path, platform: &str) {
    write_manifest(dir);
    std::fs::write(
        dir.join("hbot.json"),
        format!("{{\"name\":\"fixture\",\"platform\":{platform}}}"),
    )
    .expect("写 hbot.json");
}

// ── version ────────────────────────────────────────────────────────────────

#[test]
fn version_output_has_name_platform_and_capabilities() {
    let text = version_text();
    let mut lines = text.lines();

    assert_eq!(
        lines.next().expect("首行"),
        format!("hbot {}", env!("CARGO_PKG_VERSION"))
    );
    assert!(
        lines.next().unwrap_or_default().starts_with("platform: "),
        "输出：{text}"
    );
    assert!(
        lines
            .next()
            .unwrap_or_default()
            .starts_with("capabilities: "),
        "输出：{text}"
    );
    assert!(text.contains("rpc-ws"), "输出：{text}");
}

// ── ready 行 ───────────────────────────────────────────────────────────────

#[test]
fn ready_line_is_single_line_json_with_addr_and_pid() {
    let addr: SocketAddr = "127.0.0.1:54321".parse().expect("地址");
    let line = ready_line(addr, 4242);

    assert!(!line.contains('\n'), "ready 行必须单行：{line}");

    let value: Value = serde_json::from_str(&line).expect("ready 行是 JSON");
    assert_eq!(value["type"], Value::from("ready"));
    assert_eq!(value["addr"], Value::from("127.0.0.1:54321"));
    assert_eq!(value["pid"], Value::from(4242u32));
    // token 任何形式都不能出现在就绪行里。
    assert!(value.get("token").is_none(), "ready 行不应带 token：{line}");
}

// ── 参数解析 ───────────────────────────────────────────────────────────────

#[test]
fn serve_args_parse_defaults_and_repeated_origins() {
    let cli = Cli::parse_from([
        "hbot",
        "serve",
        "--origin",
        "http://localhost:1420",
        "--origin",
        "http://localhost:1421",
    ]);

    let Command::Serve(args) = cli.command else {
        panic!("应是 serve 子命令");
    };
    assert_eq!(args.bind, "127.0.0.1:0".parse().expect("默认地址"));
    assert!(args.token.is_none(), "默认不给 token");
    assert!(args.data_dir.is_none());
    assert_eq!(
        args.origins,
        vec![
            "http://localhost:1420".to_owned(),
            "http://localhost:1421".to_owned()
        ]
    );
}

#[test]
fn serve_plan_carries_bind_token_and_origins() {
    let cli = Cli::parse_from([
        "hbot",
        "serve",
        "--bind",
        "127.0.0.1:7777",
        "--token",
        "cli-token",
        "--origin",
        "http://localhost:1420",
        "--data-dir",
        "/tmp/hbot-data",
    ]);
    let Command::Serve(args) = cli.command else {
        panic!("应是 serve 子命令");
    };

    let plan = ServePlan::from_args(args);
    assert_eq!(plan.bind, "127.0.0.1:7777".parse().expect("地址"));
    assert_eq!(plan.token, TokenSource::Args("cli-token".to_owned()));
    assert_eq!(plan.token.describe(), "from-arg");
    assert_eq!(plan.origins, vec!["http://localhost:1420".to_owned()]);
    assert!(plan.data_dir.is_some());

    // 传输层配置只搬 token/Origin/监听地址，其余取默认值。
    let config = plan.ws_config();
    assert_eq!(config.token, "cli-token");
    assert_eq!(config.bind, plan.bind);
    assert_eq!(config.allowed_origins, plan.origins);
}

#[test]
fn check_args_parse_project_dir() {
    let cli = Cli::parse_from(["hbot", "check", "C:\\projects\\demo"]);
    let Command::Check(args) = cli.command else {
        panic!("应是 check 子命令");
    };
    assert_eq!(args.project_dir, PathBuf::from("C:\\projects\\demo"));
}

#[test]
fn version_subcommand_parses() {
    let cli = Cli::parse_from(["hbot", "version"]);
    assert!(matches!(cli.command, Command::Version));
}

// ── token 解析 ─────────────────────────────────────────────────────────────

#[test]
fn resolve_token_priority_args_over_env_and_generated() {
    assert_eq!(
        resolve_token(Some("arg".to_owned()), Some("env".to_owned())),
        TokenSource::Args("arg".to_owned())
    );
    assert_eq!(
        resolve_token(None, Some("env".to_owned())),
        TokenSource::Env("env".to_owned())
    );
    assert_eq!(
        resolve_token(None, Some("env".to_owned())).describe(),
        "from-env"
    );

    let generated = resolve_token(None, None);
    assert_eq!(generated.describe(), "generated (32 bytes)");
    assert_eq!(generated.token().len(), 64, "32 字节 = 64 个 hex 字符");
}

#[test]
fn generated_tokens_are_hex_and_unique() {
    let first = generate_token();
    let second = generate_token();
    assert_eq!(first.len(), 64);
    assert!(first.chars().all(|char| char.is_ascii_hexdigit()));
    assert_ne!(first, second);
}

// ── check ──────────────────────────────────────────────────────────────────

#[test]
fn check_missing_manifest_reports_error_json() {
    let dir = temp_project("missing-manifest");
    std::fs::create_dir_all(&dir).expect("建临时目录");

    let report = check_project(&dir);
    assert!(!report.ok, "缺 hbot.json 应不通过");
    assert!(
        report
            .errors
            .iter()
            .any(|error| error.contains("hbot.json")),
        "errors：{:?}",
        report.errors
    );

    let json: Value = serde_json::from_str(&report.to_json().expect("序列化")).expect("JSON");
    assert_eq!(json["ok"], Value::from(false));
    assert!(!json["errors"].as_array().expect("errors 是数组").is_empty());
    // 形状只加字段不删字段：`warnings` 一定存在，哪怕是空数组。
    assert!(
        json["warnings"]
            .as_array()
            .expect("warnings 是数组")
            .is_empty(),
        "缺清单时没有平台可探：{}",
        report.to_json().expect("序列化")
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_with_manifest_passes() {
    let dir = temp_project("with-manifest");
    write_manifest(&dir);

    let report = check_project(&dir);
    assert!(report.ok, "errors：{:?}", report.errors);
    assert!(report.errors.is_empty());
    assert_eq!(
        report.warnings,
        Vec::<String>::new(),
        "OneBot 骨架不该有告警"
    );
    assert_eq!(
        report.to_json().expect("序列化"),
        r#"{"ok":true,"errors":[],"warnings":[]}"#
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_missing_directory_reports_error() {
    let dir = temp_project("missing-dir");
    let report = check_project(&dir);
    assert!(!report.ok);
    assert_eq!(report.errors.len(), 1, "errors：{:?}", report.errors);
    assert!(
        report.errors[0].contains("项目目录不可用"),
        "errors：{:?}",
        report.errors
    );
}

#[test]
fn check_reports_manifest_violations_as_one_error() {
    let dir = temp_project("bad-manifest");
    std::fs::create_dir_all(&dir).expect("建临时目录");
    // 合规 JSON，但 handlerMs 超过宿主硬上限：不许静默 clamp。
    std::fs::write(
        dir.join("hbot.json"),
        r#"{"name":"bad","platform":{"kind":"onebot11","ws":"ws://127.0.0.1:1"},
            "limits":{"handlerMs":999999}}"#,
    )
    .expect("写 hbot.json");

    let report = check_project(&dir);
    assert!(!report.ok);
    assert_eq!(report.errors.len(), 1, "首个失败即停：{:?}", report.errors);
    assert!(
        report.errors[0].starts_with("HBOT_MANIFEST_INVALID"),
        "errors：{:?}",
        report.errors
    );
    assert!(
        report.errors[0].contains("handlerMs"),
        "要指出字段：{:?}",
        report.errors
    );

    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn check_reports_a_compile_error_with_its_typescript_line() {
    let dir = temp_project("bad-source");
    hbot_code::manifest::scaffold(&dir, "fixture").expect("脚手架");
    // 语法错：`defineBot(` 少了右括号。
    std::fs::write(
        dir.join("src/main.ts"),
        "import { defineBot } from 'hbot';\nexport default defineBot({ onMessage(ctx) { } };\n",
    )
    .expect("写坏源码");

    let report = check_project(&dir);
    assert!(!report.ok, "语法错应体检失败");
    assert!(
        report.errors[0].starts_with("HBOT_COMPILE"),
        "errors：{:?}",
        report.errors
    );
    assert!(
        report.errors[0].contains("src/main.ts"),
        "错误要带文件名：{:?}",
        report.errors
    );

    let _ = std::fs::remove_dir_all(&dir);
}

// ── check：平台块（`kind` 开放取值） ────────────────────────────────────────

/// 平台取值是**开放**的：本版本不认识的平台只告警，体检仍通过（退出码 0）。
#[test]
fn check_warns_about_an_unrecognised_platform_but_still_passes() {
    let dir = temp_project("unknown-platform");
    write_project_with_platform(&dir, r#"{"kind":"telegram"}"#);

    let report = check_project(&dir);
    assert!(report.ok, "未知平台只告警：errors={:?}", report.errors);
    assert!(report.errors.is_empty(), "errors：{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1, "warnings：{:?}", report.warnings);
    assert!(
        report.warnings[0].contains("未被本版本识别"),
        "warnings：{:?}",
        report.warnings
    );
    assert!(
        report.warnings[0].contains("\"telegram\"") && report.warnings[0].contains("qq_official"),
        "提示要点名收到的值与合法取值：{:?}",
        report.warnings
    );

    let json: Value = serde_json::from_str(&report.to_json().expect("序列化")).expect("JSON");
    assert_eq!(json["ok"], Value::from(true));
    assert_eq!(json["errors"].as_array().expect("errors 数组").len(), 0);
    assert_eq!(json["warnings"].as_array().expect("warnings 数组").len(), 1);

    let _ = std::fs::remove_dir_all(&dir);
}

/// 未知平台上还留着 `ws`：两条告警（平台不认识 + ws 会被忽略），仍然通过。
#[test]
fn check_warns_about_an_ignored_ws_on_an_unknown_platform() {
    let dir = temp_project("unknown-platform-ws");
    write_project_with_platform(&dir, r#"{"kind":"telegram","ws":"ws://127.0.0.1:3001"}"#);

    let report = check_project(&dir);
    assert!(report.ok, "errors：{:?}", report.errors);
    assert_eq!(report.warnings.len(), 2, "warnings：{:?}", report.warnings);
    assert!(
        report
            .warnings
            .iter()
            .any(|warning| warning.contains("未被本版本识别")),
        "warnings：{:?}",
        report.warnings
    );
    assert!(
        report.warnings.iter().any(|warning| {
            warning.contains("不需要 ws") && warning.contains("会被忽略")
        }),
        "warnings：{:?}",
        report.warnings
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// QQ 官方项目的正解：只写 `kind`，不写 `ws`。零告警、零错误。
#[test]
fn check_accepts_qq_official_without_ws() {
    let dir = temp_project("qq-official");
    write_project_with_platform(&dir, r#"{"kind":"qq_official"}"#);

    let report = check_project(&dir);
    assert!(report.ok, "errors：{:?}", report.errors);
    assert!(report.errors.is_empty(), "errors：{:?}", report.errors);
    assert_eq!(report.warnings, Vec::<String>::new(), "不该有告警");

    let _ = std::fs::remove_dir_all(&dir);
}

/// 反过来：QQ 官方写了 `ws`（手写草稿常见），只提示会被忽略，不失败。
#[test]
fn check_warns_that_qq_official_ignores_ws() {
    let dir = temp_project("qq-official-ws");
    write_project_with_platform(&dir, r#"{"kind":"qq_official","ws":"ws://127.0.0.1:3001"}"#);

    let report = check_project(&dir);
    assert!(report.ok, "errors：{:?}", report.errors);
    assert!(report.errors.is_empty(), "errors：{:?}", report.errors);
    assert_eq!(report.warnings.len(), 1, "warnings：{:?}", report.warnings);
    assert!(
        report.warnings[0].contains("qq_official"),
        "warnings：{:?}",
        report.warnings
    );
    assert!(
        report.warnings[0].contains("会被忽略"),
        "warnings：{:?}",
        report.warnings
    );
    assert!(
        report.warnings[0].contains("/gateway"),
        "要说明地址从哪来：{:?}",
        report.warnings
    );

    let _ = std::fs::remove_dir_all(&dir);
}

/// `onebot11` 缺 `ws` 是**硬错误**（exit 1 这条路）：`manifest::load` 拒绝，
/// `check` 把它报成 `HBOT_MANIFEST_INVALID`，不是 warning。
#[test]
fn check_fails_when_onebot11_has_no_ws() {
    let dir = temp_project("onebot11-no-ws");
    write_project_with_platform(&dir, r#"{"kind":"onebot11"}"#);

    let report = check_project(&dir);
    assert!(!report.ok, "onebot11 缺 ws 必须失败");
    assert_eq!(report.errors.len(), 1, "errors：{:?}", report.errors);
    assert!(
        report.errors[0].starts_with("HBOT_MANIFEST_INVALID"),
        "errors：{:?}",
        report.errors
    );
    assert!(
        report.errors[0].contains("ws"),
        "错误要点名字段：{:?}",
        report.errors
    );

    let _ = std::fs::remove_dir_all(&dir);
}
