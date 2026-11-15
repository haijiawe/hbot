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

fn write_manifest(dir: &Path) {
    std::fs::create_dir_all(dir).expect("建临时目录");
    std::fs::write(dir.join("hbot.json"), "{}\n").expect("写 hbot.json");
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
        report.to_json().expect("序列化"),
        r#"{"ok":true,"errors":[]}"#
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
