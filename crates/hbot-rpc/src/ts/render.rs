//! 渲染：把 Rust 类型树摊平成「一类型一文件」的 TypeScript 文本。
//!
//! 纯函数、不碰磁盘，方便在测试里逐字节比对；写盘在 [`super::export_typescript`]。

use std::any::TypeId;
use std::collections::{BTreeMap, BTreeSet};

use hbot_core::{
    ApprovalDecision, ApprovalId, BotId, BotStatus, ConversationId, ConversationKind, EditId,
    InboundId, MessageId, OutboundId, OutboundState, ProjectId, SandboxMode, SearchId, SessionId,
    Timestamp, ToolCall, ToolCallStatus,
};
use ts_rs::{Config, TS};

use super::{GeneratedFile, HEADER, INDEX_FILE, TsGenError};
use crate::dto::{
    ApiError, AppInfo, BotSummary, Command, ConversationSummary, DebugScope, DeviceStatus,
    Diagnostic, DiagnosticSeverity, DiffHunk, DirEntry, EditFile, Event, FileChangeKind,
    FileContent, LogLevel, ProjectOpenResult, ProjectSummary, Response, RiskLevel, RunLog,
    SearchMatch, StackFrame, StepKind,
};

/// 渲染结果：文件本身 + 它 import 的依赖（供悬空 import 检查）。
struct Rendered {
    file: GeneratedFile,
    deps: Vec<(String, String)>,
}

/// 生成全部文件内容（纯函数，不碰磁盘）。
///
/// 顺序稳定：按文件名排序，所以同一个仓库跑两次结果逐字节相同。
pub fn generated_files() -> Result<Vec<GeneratedFile>, TsGenError> {
    // `large_int("number")`：`i64` 在线上是 JSON 数字；ts-rs 默认给 `bigint`，
    // 那样 UI 侧 `JSON.parse` 出来的 number 无法直接赋值，属于契约事故。
    let cfg = Config::new().with_large_int("number");

    let mut rendered: Vec<Rendered> = Vec::new();
    macro_rules! export {
        ($($ty:ty),* $(,)?) => {
            $( rendered.push(render::<$ty>(&cfg)?); )*
        };
    }

    // 契约根类型：Command/Event 及其载荷；Response/ApiError 给 dispatch 用。
    export!(
        Command,
        Event,
        Response,
        ApiError,
        AppInfo,
        FileContent,
        DirEntry,
        RunLog,
        Diagnostic,
        DiagnosticSeverity,
        SearchMatch,
        EditFile,
        DiffHunk,
        DebugScope,
        StackFrame,
        StepKind,
        RiskLevel,
        LogLevel,
        FileChangeKind,
        // `bot_list` 的行：UI 拿它播种运行态，必须与 `bot.status` 事件同一套枚举。
        BotSummary,
        // `project_create` 的结果：新项目的根 / 名字 / 入口文件。
        ProjectSummary,
        // `project_open` 的结果：只有规范化后的项目根（打开不要求已有 hbot.json）。
        ProjectOpenResult,
        // `bot_conversations` 的行：`bot_inject` 要的 conversationId 由这里给。
        ConversationSummary,
        // `device_status` / `device_grant` 的快照，也是 `device.status` 的载荷。
        //
        // 字段全是开放字符串（`state` / `sub` / `allowedApps` 不是枚举）：设备能力是
        // 产品面最容易继续长的一处，加一种子能力不该逼着 UI 重新生成一遍枚举。
        // 代价是前端要自己把未知值当"不认识的能力"忽略，不能 throw。
        DeviceStatus,
    );
    // 领域类型：事件载荷里出现的枚举 / 结构。
    export!(
        BotStatus,
        ConversationKind,
        OutboundState,
        SandboxMode,
        ApprovalDecision,
        ToolCall,
        ToolCallStatus,
        Timestamp,
    );
    // ID 新类型：UI 侧就是 string。
    export!(
        ProjectId,
        BotId,
        ConversationId,
        SessionId,
        MessageId,
        InboundId,
        OutboundId,
        ApprovalId,
        EditId,
        SearchId,
    );

    rendered.sort_by(|a, b| a.file.path.cmp(&b.file.path));

    // 悬空 import 检查：依赖必须也是根类型，否则 UI 侧 import 会指向不存在的文件。
    let known: BTreeSet<&str> = rendered.iter().map(|r| r.file.path.as_str()).collect();
    for item in &rendered {
        for (type_name, file) in &item.deps {
            if !known.contains(file.as_str()) {
                return Err(TsGenError::Missing {
                    type_name: type_name.clone(),
                    file: file.clone(),
                    referenced_by: item.file.path.clone(),
                });
            }
        }
    }

    let mut files: Vec<GeneratedFile> = rendered.into_iter().map(|r| r.file).collect();
    let index = index_file(&files);
    files.push(index);
    Ok(files)
}

/// 渲染一个类型。
fn render<T: TS + 'static>(cfg: &Config) -> Result<Rendered, TsGenError> {
    let rel = <T as TS>::output_path().ok_or_else(|| TsGenError::NotExportable {
        name: <T as TS>::name(cfg),
    })?;
    let path = rel.to_string_lossy().replace('\\', "/");

    let mut imports: BTreeMap<String, BTreeSet<String>> = BTreeMap::new();
    let mut deps = Vec::new();
    for dep in <T as TS>::dependencies(cfg) {
        if dep.type_id == TypeId::of::<T>() {
            continue;
        }
        let dep_file = dep.output_path.to_string_lossy().replace('\\', "/");
        imports
            .entry(import_spec(&path, &dep_file))
            .or_default()
            .insert(dep.ts_name.clone());
        deps.push((dep.ts_name, dep_file));
    }

    let mut out = String::with_capacity(512);
    out.push_str(HEADER);
    out.push('\n');
    if let Some(docs) = <T as TS>::docs() {
        out.push('\n');
        out.push_str(docs.trim_end());
        out.push('\n');
    }
    if !imports.is_empty() {
        out.push('\n');
        for (spec, names) in &imports {
            out.push_str("import type { ");
            out.push_str(&names.iter().cloned().collect::<Vec<_>>().join(", "));
            out.push_str(" } from \"");
            out.push_str(spec);
            out.push_str("\";\n");
        }
    }
    out.push('\n');
    out.push_str("export ");
    out.push_str(&<T as TS>::decl(cfg));
    out.push('\n');

    Ok(Rendered {
        file: GeneratedFile { path, content: out },
        deps,
    })
}

/// 索引文件：只转发，保持 UI 侧可以 `from "../generated"` 一次拿到全部类型。
fn index_file(files: &[GeneratedFile]) -> GeneratedFile {
    let mut content = String::from(HEADER);
    content.push('\n');
    content.push('\n');
    for file in files {
        let stem = file.path.strip_suffix(".ts").unwrap_or(&file.path);
        content.push_str("export type { ");
        content.push_str(stem);
        content.push_str(" } from \"./");
        content.push_str(stem);
        content.push_str("\";\n");
    }
    GeneratedFile {
        path: INDEX_FILE.to_owned(),
        content,
    }
}

/// 从 `from`（含目录）指向 `to` 的 TS import 说明符。
fn import_spec(from: &str, to: &str) -> String {
    let from_dir = match from.rsplit_once('/') {
        Some((dir, _)) => dir,
        None => "",
    };
    let to_no_ext = to.strip_suffix(".ts").unwrap_or(to);
    match to_no_ext.strip_prefix(&format!("{from_dir}/")) {
        Some(rest) if !from_dir.is_empty() => format!("./{rest}"),
        _ => format!("./{to_no_ext}"),
    }
}
