//! `hbot fixture save` / `hbot fixture run`：把历史存成夹具，或拿夹具重跑当前代码。
//!
//! 实现内核**全部**在 `hbot-replay::fixture`（与 IDE 的 `replay_save_fixture` /
//! `fixture_run` 共用同一份）；本模块只做 CLI 侧的三件事：开库、打印、退出码。
//!
//! ## 两条子命令的差别（不只是 save / run）
//!
//! | | 读库 | 编译项目 | 起 isolate | 需要的参数 |
//! |---|---|---|---|---|
//! | `fixture save` | 是（项目库 `.hbot/hbot.db`） | 否 | 否 | `--bot` `--path` |
//! | `fixture run` | **否** | 是 | 是 | `--path` |
//!
//! `run` 不查库是刻意的：夹具自带事件，把别处导出的夹具（CI 里的回归用例）拿来跑时，
//! 库里有没有这个 bot 根本不该影响结果。所以它也不需要 `--bot`。
//!
//! ## 退出码
//!
//! 与 `hbot replay` 同一套三档语义（`0` / `1` / `2`，见 [`crate::Outcome`]）：
//!
//! - `save`：存成了 `0`，跑不起来（库缺失、bot 不存在、没有历史、路径越界、写盘失败）`2`；
//!   `save` **没有**"跑完了但有差异"这一档，所以不会返回 `1`。
//! - `run`：全部事件都正常 `0`；跑完了但有 handler 抛异常 `1`（那是"代码现在的行为变了"）；
//!   夹具读不了 / 编译失败 / isolate 起不来 `2`。
//!
//! ## 库必须先在
//!
//! `save` 与 `replay` 一样：`Store::open` 对不存在的路径会顺手造库，所以先检查
//! `<project-dir>/.hbot/hbot.db` 存在再打开，否则"读历史"会变成"造空库"。

use std::fmt::Write as _;

use anyhow::Context as _;

use hbot_replay::{
    FixtureError, FixtureRunReport, ReplayOptions, SavedFixture, database_path,
    parse_rfc3339_millis, resolve_fixture_path, run_fixture, save_fixture,
};
use hbot_store::Store;

use crate::Outcome;
use crate::cli::{FixtureArgs, FixtureCommand, FixtureRunArgs, FixtureSaveArgs};

/// `hbot fixture` 的分发。
pub async fn run(args: FixtureArgs) -> anyhow::Result<Outcome> {
    match args.command {
        FixtureCommand::Save(args) => save(args).await,
        FixtureCommand::Run(args) => run_one(args).await,
    }
}

/// `hbot fixture save <project-dir> --bot <id> --path <path>`。
async fn save(args: FixtureSaveArgs) -> anyhow::Result<Outcome> {
    let json = args.json;
    let project_dir = args.project_dir;
    let database = database_path(&project_dir);

    // 路径先判：它不需要库、也不需要编译，越界就是越界（与 RPC 层同一道守卫、同一份实现）。
    if let Err(error) = resolve_fixture_path(&project_dir, &args.path) {
        write_stderr(&format!("保存夹具失败：{error}"))?;
        return Ok(Outcome::Error);
    }
    // `--from` 也先解析：`hbot-replay` 只认 Unix 毫秒。
    let from = match args.from.as_deref().map(parse_rfc3339_millis).transpose() {
        Ok(from) => from,
        Err(error) => {
            write_stderr(&format!("保存夹具失败：{error}"))?;
            return Ok(Outcome::Error);
        }
    };
    // 库必须先存在再打开（见模块文档）。
    if !database.is_file() {
        write_stderr(&format!(
            "找不到项目库 {}（存夹具要读 `.hbot/hbot.db`；先启动过 bot 并收到消息才会生成，\
             且本命令不会替你造库）",
            database.display()
        ))?;
        return Ok(Outcome::Error);
    }

    let options = ReplayOptions {
        project_dir,
        database: database.clone(),
        bot: args.bot,
        limit: args.limit,
        from,
        // 历史遗留的 payload 落盘与这条子命令无关：夹具走 `hbot_replay::save_fixture`。
        save_fixture: None,
    };
    let path = args.path;
    let result = tokio::task::spawn_blocking(move || match Store::open(&database) {
        Ok(store) => save_fixture(&store, &options, &path),
        Err(error) => Err(FixtureError::Engine(format!(
            "打开项目库失败（{}）：{error}",
            database.display()
        ))),
    })
    .await
    .context("保存夹具任务异常中止")?;

    match result {
        Ok(saved) => {
            print_saved(&saved, json)?;
            Ok(Outcome::Success)
        }
        Err(error) => {
            write_stderr(&format!("保存夹具失败：{error}"))?;
            Ok(Outcome::Error)
        }
    }
}

/// `hbot fixture run <project-dir> --path <path>`。
async fn run_one(args: FixtureRunArgs) -> anyhow::Result<Outcome> {
    let json = args.json;
    let project_dir = args.project_dir;
    // 路径守卫先跑一遍：坏路径不必先编译几秒才报出来。
    let resolved = match resolve_fixture_path(&project_dir, &args.path) {
        Ok(resolved) => resolved,
        Err(error) => {
            write_stderr(&format!("跑夹具失败：{error}"))?;
            return Ok(Outcome::Error);
        }
    };
    let report = tokio::task::spawn_blocking(move || run_fixture(&project_dir, &resolved))
        .await
        .context("夹具重跑任务异常中止")?;

    match report {
        Ok(report) => {
            print_report(&report, json)?;
            Ok(report_outcome(&report))
        }
        Err(error) => {
            write_stderr(&format!("跑夹具失败：{error}"))?;
            Ok(Outcome::Error)
        }
    }
}

/// 跑完了的结论：`errors > 0` 是"代码现在的行为变了"（退出码 1），不是"没跑起来"。
fn report_outcome(report: &FixtureRunReport) -> Outcome {
    if report.errors > 0 {
        Outcome::Failure
    } else {
        Outcome::Success
    }
}

/// 打印保存结果（`--json` 一行 JSON，否则一句人话）。
fn print_saved(saved: &SavedFixture, json: bool) -> anyhow::Result<()> {
    if json {
        return crate::write_line(&serde_json::to_string(saved)?);
    }
    crate::write_line(&format!(
        "已保存夹具 {}（{} 条事件）",
        saved.path, saved.events
    ))
}

/// 打印重跑报告（`--json` 一行 JSON，否则逐条 + 汇总）。
fn print_report(report: &FixtureRunReport, json: bool) -> anyhow::Result<()> {
    if json {
        return crate::write_line(&serde_json::to_string(report)?);
    }
    crate::write_line(&report_text(report))
}

/// 内部报告 → 给人看的文本（多行，末尾不带换行——外层 `write_line` 补）。
fn report_text(report: &FixtureRunReport) -> String {
    let mut text = String::new();
    let _ = writeln!(text, "hbot fixture run / {}", report.path);
    let _ = writeln!(text, "{}", "-".repeat(64));
    for entry in &report.entries {
        // 有回复就逐条列出来；抛错就只写错因（那一刻通常没有回复）。
        if let Some(error) = &entry.error {
            let _ = writeln!(
                text,
                "[{}] error external_id={} 事件失败：{error}",
                entry.index, entry.external_id
            );
            continue;
        }
        let _ = writeln!(
            text,
            "[{}] external_id={} 回复 {} 条",
            entry.index,
            entry.external_id,
            entry.replies.len()
        );
        for reply in &entry.replies {
            let _ = writeln!(text, "    {reply:?}");
        }
    }
    let _ = writeln!(text, "{}", "-".repeat(64));
    let _ = writeln!(
        text,
        "汇总：{} 条事件 / {} 条回复 / {} 条出错",
        report.events, report.replies, report.errors
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
