//! `hbot replay <project-dir> --bot <bot-id>`：用历史入站事件重跑**当前代码**并对比输出。
//!
//! 回放本身在 `hbot-replay`（与 IDE 的 `replay_run` 共用同一份实现）；本模块只做 CLI 侧的
//! 四件事：
//!
//! 1. 解析参数：`--from` 从 RFC3339 文本解析成 Unix 毫秒（`hbot-replay` 只收毫秒，IDE 直接
//!    给毫秒），组 [`ReplayOptions`]；
//! 2. 打开**项目库** `<project-dir>/.hbot/hbot.db`。IDE 用的是**应用库**
//!    `<data_dir>/hbot.db`，所以"开哪个库"留在两侧各自的壳里，理由见 `hbot-replay` 的
//!    crate 文档（"两种库"）；
//! 3. 打印：`--json` 输出单行报告；否则成功/有差异打印人类可读报告，致命错误走 stderr；
//! 4. 退出码。
//!
//! 退出码三档，**不许把"有差异"和"跑挂了"混成一个码**：
//!
//! - `0`：全部一致；
//! - `1`：跑完了，但有差异（含单条 handler 抛异常——那也是"代码现在的行为变了"）；
//! - `2`：回放本身跑不起来（库缺失/打不开、bot 不存在、清单读不了、编译失败、沙箱起不来、
//!   一条事件都没有、`--from` 解析不了、`--limit 0`）。
//!
//! 只读：本命令不写 inbound/outbound/logs，也不改项目文件（`--save-fixture` 是用户显式
//! 指定的落点，例外）。唯一的写来自 `Store::open` 内部的 `PRAGMA journal_mode = WAL` 与
//! 迁移版本检查——那是 store 的既有行为，不是回放加的；所以库文件必须**先存在**才打开，
//! 否则 `Store::open` 会顺手造一个空库，"回放"就变成了"造库"。

use std::fmt::Write as _;

use anyhow::Context as _;

use hbot_replay::{ReplayOptions, ReplayReport, database_path, parse_rfc3339_millis, replay};
use hbot_store::Store;

use crate::Outcome;
use crate::cli::ReplayArgs;

/// `hbot replay` 的实现入口：跑回放、按 `--json` 选输出、返回退出码语义。
///
/// 致命错误**不**返回 `Err`：`main` 会把 `Err` 映射成退出码 1，而"回放跑不起来"必须是 2。
pub async fn run(args: ReplayArgs) -> anyhow::Result<Outcome> {
    let json = args.json;
    let project_dir = args.project_dir.clone();
    let database = database_path(&project_dir);
    // `--from` 先在这里解析：它不需要库、也不需要编译，解析不过就直接给失败报告（退出码 2），
    // 不必先开库再报这个错。`hbot-replay` 那边只认 Unix 毫秒。
    let from = match args.from.as_deref().map(parse_rfc3339_millis).transpose() {
        Ok(from) => from,
        Err(error) => {
            let options = ReplayOptions {
                project_dir,
                database,
                bot: args.bot,
                limit: args.limit,
                from: None,
                save_fixture: args.save_fixture,
            };
            return finish(&ReplayReport::failed(&options, error), json);
        }
    };
    let options = ReplayOptions {
        project_dir,
        database: database.clone(),
        bot: args.bot,
        limit: args.limit,
        from,
        save_fixture: args.save_fixture,
    };
    // 库必须**先存在**再打开：`Store::open` 对不存在的路径会顺手造库（见模块文档）。
    if !database.is_file() {
        let error = format!(
            "找不到项目库 {}（回放要读 `.hbot/hbot.db`；先启动过 bot 并收到消息才会生成，\
             且本命令不会替你造库）",
            database.display()
        );
        return finish(&ReplayReport::failed(&options, error), json);
    }
    let report = tokio::task::spawn_blocking(move || match Store::open(&database) {
        Ok(store) => replay(&store, &options),
        Err(error) => ReplayReport::failed(
            &options,
            format!("打开项目库失败（{}）：{error}", database.display()),
        ),
    })
    .await
    .context("回放任务异常中止")?;
    finish(&report, json)
}

/// 打印报告并返回退出码语义。
///
/// `--json` 一行 JSON；否则有致命错误时走 stderr（不混进 stdout 的报告里），成功/有差异走
/// stdout。
fn finish(report: &ReplayReport, json: bool) -> anyhow::Result<Outcome> {
    let outcome = report_outcome(report);
    if json {
        crate::write_line(&report_json(report)?)?;
    } else if let Some(error) = &report.error {
        write_stderr(&format!("回放失败：{error}"))?;
    } else {
        crate::write_line(&report_text(report))?;
    }
    Ok(outcome)
}

/// 内部报告 → 退出码语义（`2` 有致命错误 / `0` 全一致 / `1` 有差异）。
fn report_outcome(report: &ReplayReport) -> Outcome {
    if report.error.is_some() {
        Outcome::Error
    } else if report.ok {
        Outcome::Success
    } else {
        Outcome::Failure
    }
}

/// 内部报告 → **单行** JSON（与 `hbot check` 的 `CheckReport::to_json` 一致）。
fn report_json(report: &ReplayReport) -> anyhow::Result<String> {
    Ok(serde_json::to_string(report)?)
}

/// 内部报告 → 给人看的报告文本（多行，末尾不带换行——外层 `write_line` 补）。
fn report_text(report: &ReplayReport) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "hbot replay / {}", report.project);
    let _ = writeln!(text, "bot={} 库={}", report.bot, report.database);
    let _ = writeln!(
        text,
        "范围：from={} limit={} 共 {} 条历史入站",
        match report.from {
            Some(millis) => millis.to_string(),
            None => "不限".to_owned(),
        },
        match report.limit {
            Some(limit) => limit.to_string(),
            None => "不限".to_owned(),
        },
        report.events,
    );
    if let Some(error) = &report.error {
        let _ = writeln!(text, "回放失败：{error}");
        return text;
    }
    let _ = writeln!(text, "{}", "-".repeat(64));
    for (index, entry) in report.entries.iter().enumerate() {
        let _ = writeln!(
            text,
            "[{}] {} external_id={} received_at={} 历史 {} 条 / 重跑 {} 条",
            index + 1,
            entry.status.label(),
            entry.external_id,
            entry.received_at,
            entry.historical.len(),
            entry.replayed.len(),
        );
        if let Some(error) = &entry.error {
            let _ = writeln!(text, "    事件失败：{error}");
        }
        for reordered in &entry.reordered {
            let _ = writeln!(
                text,
                "    顺序不同：{:?}（历史第 {} 条 -> 重跑第 {} 条）",
                reordered.text,
                reordered.historical_index + 1,
                reordered.replayed_index + 1,
            );
        }
        for missing in &entry.missing {
            let _ = writeln!(text, "    缺少：{missing:?}");
        }
        for extra in &entry.extra {
            let _ = writeln!(text, "    多出：{extra:?}");
        }
    }
    let _ = writeln!(text, "{}", "-".repeat(64));
    let _ = writeln!(
        text,
        "汇总：{} 条入站 / {} 条一致 / {} 条有差异 / {} 条出错；逐条：一致 {}、多出 {}、\
         缺少 {}、顺序不同 {}",
        report.events,
        report.matched_events,
        report.differing_events,
        report.failed_events,
        report.same,
        report.extra,
        report.missing,
        report.reordered,
    );
    text
}

/// 往 stderr 写一行并 flush（错误信息不混进 stdout 的报告里）。
fn write_stderr(line: &str) -> anyhow::Result<()> {
    use std::io::Write as _;
    let mut stderr = std::io::stderr().lock();
    writeln!(stderr, "{line}")?;
    stderr.flush()?;
    Ok(())
}
