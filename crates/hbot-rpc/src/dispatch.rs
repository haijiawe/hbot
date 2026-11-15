//! [`Dispatcher`]：UI 命令的唯一入口。
//!
//! 桌面模式由 Tauri 命令调用，后台模式由 `ws.rs` 的 JSON-RPC `dispatch` 方法调用；
//! 两条路径进的是同一个函数，不存在"桌面能做的事后台做不了"。
//!
//! # M0 覆盖范围
//!
//! 只实现 [`Command::AppInfo`]（UI 启动第一条命令，必须真数据）；其余命令统一返回
//! `-32006` + `尚未实现：<command>`。**不返回假数据**：宁可报错，也不让 UI 以为功能
//! 已上线（`AGENTS.md` §0.8 不静默吞错）。
//!
//! 依赖的服务以 `Option<Arc<dyn …>>` 持有：M0 装配方还没有把 store / agent / workspace
//! 接进来，`None` 表示"这个能力还没装配"，实现某个命令时再取用。

use std::fmt;
use std::path::PathBuf;
use std::sync::Arc;

use hbot_core::{Clock, EventSink, HbotError, IdGenerator};

use crate::dto::{ApiError, AppInfo, Command, Event, Response};

/// 未实现命令的错误码。
///
/// 与 [`HbotError::Internal`] 的码相同（`-32006`，唯一真源
/// `docs/02-architecture.md` §9.4）。这里不经过 `From<HbotError>` 是因为 `message`
/// 要精确等于 `尚未实现：<command>`，而 `HbotError::Display` 会加 `internal: ` 前缀；
/// `unimplemented_code_matches_hbot_error` 测试盯着这两个值不许分叉。
const UNIMPLEMENTED_CODE: i32 = -32006;

/// 命令分发器。
///
/// 持有装配方注入的服务（均可缺省）。字段私有，靠 `with_*` 构造、`*()` 读取，
/// 避免实现方绕过入口直接调用服务。
#[derive(Default)]
pub struct Dispatcher {
    clock: Option<Arc<dyn Clock>>,
    ids: Option<Arc<dyn IdGenerator>>,
    events: Option<Arc<dyn EventSink<Event>>>,
    state: Option<Arc<crate::app::AppState>>,
}

impl Dispatcher {
    /// 空装配：只有 `AppInfo` 可用。M0 的默认状态。
    pub fn new() -> Self {
        Self::default()
    }

    /// 注入时钟（命令实现里需要"现在"时用，便于测试注入固定时间）。
    pub fn with_clock(mut self, clock: Arc<dyn Clock>) -> Self {
        self.clock = Some(clock);
        self
    }

    /// 注入 ID 生成器（审批 / 编辑 / 搜索等关联键只能由它产出）。
    pub fn with_ids(mut self, ids: Arc<dyn IdGenerator>) -> Self {
        self.ids = Some(ids);
        self
    }

    /// 注入事件出口（`EventSink<Event>`；`hbot-core` 不依赖 `hbot-rpc`，所以这里具体化）。
    pub fn with_events(mut self, events: Arc<dyn EventSink<Event>>) -> Self {
        self.events = Some(events);
        self
    }

    /// 接上真实应用状态：此后命令走真实实现（store / workspace / bot / agent）。
    ///
    /// 没有调用这个构造的命令分发器是 M0 的空壳：除 `AppInfo` 外全部 `-32006`。
    /// 两条路径共用同一个 `dispatch`，所以"桌面能做的事后台做不了"不会发生
    /// （`docs/02-architecture.md` §2）。
    pub fn with_state(mut self, state: Arc<crate::app::AppState>) -> Self {
        self.clock = Some(Arc::clone(state.clock()));
        self.ids = Some(Arc::clone(state.ids()));
        self.events = Some(state.events().clone());
        self.state = Some(state);
        self
    }

    /// 已装配的应用状态；`None` 表示空壳。
    pub fn state(&self) -> Option<&Arc<crate::app::AppState>> {
        self.state.as_ref()
    }

    /// 已注入的时钟。
    pub fn clock(&self) -> Option<&Arc<dyn Clock>> {
        self.clock.as_ref()
    }

    /// 已注入的 ID 生成器。
    pub fn ids(&self) -> Option<&Arc<dyn IdGenerator>> {
        self.ids.as_ref()
    }

    /// 已注入的事件出口。
    pub fn events(&self) -> Option<&Arc<dyn EventSink<Event>>> {
        self.events.as_ref()
    }

    /// 执行一条命令。
    ///
    /// 永不 panic、永不以 `Result` 报错：失败也走 [`Response::Error`]，保证 wire 上
    /// 只有一种返回形状（`AGENTS.md` §4.3 加字段要兼容旧 UI，形状不能漂）。
    pub async fn dispatch(&self, command: Command) -> Response {
        let Some(state) = &self.state else {
            // 空壳模式：除 `AppInfo` 外一律"尚未实现"（M0 行为，测试钉住它）。
            return match command {
                Command::AppInfo => app_info(),
                other => unimplemented(&other),
            };
        };

        match &command {
            Command::AppInfo => crate::app::commands::system::app_info(state),
            Command::ProjectOpen { path } => {
                crate::app::commands::system::project_open(state, path.clone()).await
            }
            Command::FileRead { .. }
            | Command::FileWrite { .. }
            | Command::FileList { .. }
            | Command::FileCreate { .. }
            | Command::FileRename { .. }
            | Command::FileDelete { .. }
            | Command::SearchStart { .. }
            | Command::SearchCancel { .. }
            | Command::Diagnostics { .. } => {
                crate::app::commands::files::handle(state, command).await
            }
            Command::BotStart { .. }
            | Command::BotStop { .. }
            | Command::BotInject { .. }
            | Command::LogsTail { .. } => crate::app::commands::bots::handle(state, command).await,
            Command::AgentSend { .. }
            | Command::AgentCancel { .. }
            | Command::AgentApprove { .. }
            | Command::EditApply { .. }
            | Command::EditReject { .. } => {
                crate::app::commands::agent::handle(state, command).await
            }
            Command::DebugSetBreakpoints { .. }
            | Command::DebugResume { .. }
            | Command::DebugStep { .. } => {
                crate::app::commands::debug::handle(state, command).await
            }
        }
    }
}

impl fmt::Debug for Dispatcher {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Dispatcher")
            .field("clock", &self.clock.is_some())
            .field("ids", &self.ids.is_some())
            .field("events", &self.events.is_some())
            .finish()
    }
}

/// [`Command::AppInfo`]：真实构建事实，不查库、不联网。
fn app_info() -> Response {
    let info = AppInfo {
        name: "hbot".to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        platform: std::env::consts::OS.to_owned(),
        data_dir: default_data_dir().to_string_lossy().into_owned(),
    };

    match serde_json::to_value(info) {
        Ok(value) => Response::with_data(value),
        // 这个结构没有非 UTF-8 / 非有限浮点，理论上不会失败；仍然显式报错而不是
        // `unwrap`（库代码禁用，`AGENTS.md` §5）。
        Err(error) => Response::error(ApiError::from(HbotError::internal(format!(
            "序列化 AppInfo 失败：{error}"
        )))),
    }
}

/// 未实现命令：`-32006` + `尚未实现：<command>`。
fn unimplemented(command: &Command) -> Response {
    Response::error(ApiError {
        code: UNIMPLEMENTED_CODE,
        message: format!("尚未实现：{}", command.kind()),
        data: None,
    })
}

/// 数据目录：Windows 用 `%APPDATA%\hbot`，Unix 用 `$XDG_DATA_HOME/hbot`。
///
/// 自己算而不引入 `directories` / Tauri path API：这一层必须能被后台模式
/// （无 Tauri）复用，依赖越少越好。macOS 的 `~/Library/Application Support`
/// 等真需要时再补，当前落到 Unix 分支。
fn default_data_dir() -> PathBuf {
    if cfg!(windows) {
        if let Some(appdata) = std::env::var_os("APPDATA") {
            return PathBuf::from(appdata).join("hbot");
        }
        if let Some(profile) = std::env::var_os("USERPROFILE") {
            return PathBuf::from(profile)
                .join("AppData")
                .join("Roaming")
                .join("hbot");
        }
    } else {
        if let Some(data_home) = std::env::var_os("XDG_DATA_HOME") {
            return PathBuf::from(data_home).join("hbot");
        }
        if let Some(home) = std::env::var_os("HOME") {
            return PathBuf::from(home)
                .join(".local")
                .join("share")
                .join("hbot");
        }
    }

    // 环境变量全缺（容器 / 精简服务）：当前目录下的 hbot，至少不 panic。
    PathBuf::from("hbot")
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;
    use std::str::FromStr;

    const BOT: &str = "22222222-2222-4222-8222-222222222222";
    const CONVERSATION: &str = "33333333-3333-4333-8333-333333333333";
    const SESSION: &str = "44444444-4444-4444-8444-444444444444";
    const APPROVAL: &str = "88888888-8888-4888-8888-888888888888";
    const EDIT: &str = "99999999-9999-4999-8999-999999999999";
    const SEARCH: &str = "aaaaaaaa-aaaa-4aaa-8aaa-aaaaaaaaaaaa";

    fn parse<T: FromStr>(raw: &str) -> T
    where
        T::Err: std::fmt::Debug,
    {
        T::from_str(raw).expect("测试 UUID 必须合法")
    }

    /// 23 条命令各一条样例；与 `Command` 的变体一一对应。
    fn sample_commands() -> Vec<Command> {
        vec![
            Command::AppInfo,
            Command::ProjectOpen {
                path: "C:\\work\\demo".to_owned(),
            },
            Command::AgentSend {
                session_id: parse(SESSION),
                text: "hello".to_owned(),
            },
            Command::AgentCancel {
                session_id: parse(SESSION),
            },
            Command::AgentApprove {
                approval_id: parse(APPROVAL),
                decision: hbot_core::ApprovalDecision::Approve,
                always: false,
            },
            Command::FileRead {
                path: "src/main.rs".to_owned(),
            },
            Command::FileWrite {
                path: "src/main.rs".to_owned(),
                content: "fn main() {}\n".to_owned(),
                base_hash: Some("sha256:00".to_owned()),
            },
            Command::FileList {
                path: "src".to_owned(),
            },
            Command::FileCreate {
                path: "src/lib.rs".to_owned(),
                content: String::new(),
            },
            Command::FileRename {
                from: "a.rs".to_owned(),
                to: "b.rs".to_owned(),
            },
            Command::FileDelete {
                path: "a.rs".to_owned(),
                to_trash: true,
            },
            Command::EditApply {
                edit_id: parse(EDIT),
                accept_hunks: vec![0, 2],
            },
            Command::EditReject {
                edit_id: parse(EDIT),
            },
            Command::SearchStart {
                query: "fn main".to_owned(),
                regex: false,
                globs: vec!["**/*.rs".to_owned()],
            },
            Command::SearchCancel {
                search_id: parse(SEARCH),
            },
            Command::Diagnostics {
                path: Some("src/main.rs".to_owned()),
            },
            Command::BotStart { bot_id: parse(BOT) },
            Command::BotStop { bot_id: parse(BOT) },
            Command::BotInject {
                bot_id: parse(BOT),
                conversation_id: parse(CONVERSATION),
                text: "ping".to_owned(),
            },
            Command::DebugSetBreakpoints {
                file: "src/main.rs".to_owned(),
                lines: vec![3, 7],
            },
            Command::DebugResume {
                session_id: parse(SESSION),
            },
            Command::DebugStep {
                session_id: parse(SESSION),
                kind: crate::dto::StepKind::Over,
            },
            Command::LogsTail {
                bot_id: parse(BOT),
                limit: 50,
            },
        ]
    }

    #[test]
    fn kind_matches_the_wire_type() {
        let commands = sample_commands();
        assert_eq!(
            commands.len(),
            23,
            "命令数与 Command 变体数不一致：新增/删除命令时同步这张表"
        );
        for command in commands {
            let value = serde_json::to_value(&command).expect("命令必须可序列化");
            let wire = value["type"].as_str().expect("必须有 type 字段");
            assert_eq!(wire, command.kind(), "kind() 与 serde tag 分叉");
        }
    }

    #[tokio::test]
    async fn app_info_returns_real_build_facts() {
        let response = Dispatcher::new().dispatch(Command::AppInfo).await;
        let Response::Ok { data } = response.clone() else {
            panic!("AppInfo 必须成功，实际 {response:?}");
        };
        let info: AppInfo =
            serde_json::from_value(data.expect("AppInfo 必须带 data")).expect("data 应是 AppInfo");

        assert_eq!(info.name, "hbot");
        assert_eq!(info.version, env!("CARGO_PKG_VERSION"));
        assert_eq!(info.platform, std::env::consts::OS);
        assert!(
            info.data_dir.ends_with("hbot"),
            "dataDir 应该以 hbot 结尾，实际 {}",
            info.data_dir
        );
    }

    #[tokio::test]
    async fn every_other_command_reports_unimplemented() {
        let dispatcher = Dispatcher::new();
        for command in sample_commands() {
            if matches!(command, Command::AppInfo) {
                continue;
            }
            let expected = format!("尚未实现：{}", command.kind());
            match dispatcher.dispatch(command).await {
                Response::Error { error } => {
                    assert_eq!(error.code, UNIMPLEMENTED_CODE);
                    assert_eq!(error.message, expected);
                    assert!(error.data.is_none(), "未实现错误不该带 data");
                }
                other => panic!("未实现命令必须报错，实际 {other:?}"),
            }
        }
    }

    #[test]
    fn unimplemented_code_matches_hbot_error() {
        assert_eq!(
            UNIMPLEMENTED_CODE,
            HbotError::internal("x").code(),
            "-32006 必须在 hbot-core 与 dispatch 之间保持一致"
        );
    }

    #[test]
    fn response_wire_shape_is_tagged() {
        let ok = serde_json::to_value(Response::with_data(json!({ "a": 1 }))).expect("ok");
        assert_eq!(ok, json!({ "type": "ok", "data": { "a": 1 } }));

        let empty = serde_json::to_value(Response::ok()).expect("ok");
        assert_eq!(empty, json!({ "type": "ok", "data": null }));

        let error = serde_json::to_value(Response::error(ApiError {
            code: UNIMPLEMENTED_CODE,
            message: "x".to_owned(),
            data: None,
        }))
        .expect("error");
        assert_eq!(
            error,
            json!({ "type": "error", "error": { "code": -32006, "message": "x", "data": null } })
        );
    }

    #[test]
    fn dispatcher_reports_injected_services() {
        let bare = Dispatcher::new();
        assert!(bare.clock().is_none() && bare.ids().is_none() && bare.events().is_none());
        assert_eq!(
            format!("{bare:?}"),
            "Dispatcher { clock: false, ids: false, events: false }"
        );
    }
}
