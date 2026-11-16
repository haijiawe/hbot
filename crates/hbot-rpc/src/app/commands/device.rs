//! 设备能力命令：`device_*`。
//!
//! 设计见 `docs/09-device-capability.md`（§4 三道闸、§7 会话状态机、§8 契约）。
//! **当前是契约冻结阶段的桩**：四条命令都返回 `-32006 尚未实现`，接线由
//! `device-cmd` 单元替换本文件的实现（写域：本文件 + `crate::app::device`）。
//!
//! ## 为什么现在就要有这个文件
//!
//! `dto/command.rs` 的四个变体已经落地、`cargo xtask gen` 已经把 `DeviceStatus`
//! 生成到 `apps/web/src/generated/`。如果路由不存在，`dispatch` 的 `match` 就不封闭
//! （编译不过）；如果路由到别的域，UI 会收到一条与设备无关的错误文案。所以桩也
//! 必须**在自己的域里**，且返回的错误要能被 `display_capability_missing` 之类的前端
//! 逻辑识别成"能力未装配"而不是"设备被拒绝"——`-32006` 与 `-32005` 的区别就在这里。
//!
//! ## 四条命令的分工（接线时不要合并）
//!
//! | 命令 | 谁调 | 语义 |
//! |---|---|---|
//! | `device_status` | 设置页 / 状态条 | 查询，**不改状态** |
//! | `device_grant` | 用户（设置页、托盘、快捷键） | 开一段限时会话；Agent 不许调 |
//! | `device_revoke` | 用户 | 一键全局撤销，幂等 |
//! | `device_request` | Agent | 只发请求，等用户点；**不含授权能力** |
//!
//! `device_grant` 与 `device_request` 分开是安全边界，不是风格问题（`docs/09` §13 问 4）。

use crate::app::AppState;
use crate::dto::{Command, Response};

/// 把设备域的命令分派到各自的 handler。
///
/// 只做路由：本函数不校验、不读库、不发事件。
pub async fn handle(state: &AppState, command: Command) -> Response {
    let _ = state;
    match command {
        Command::DeviceStatus { .. } => stubbed("device_status"),
        Command::DeviceGrant { .. } => stubbed("device_grant"),
        Command::DeviceRevoke { .. } => stubbed("device_revoke"),
        Command::DeviceRequest { .. } => stubbed("device_request"),
        // 路由表已经把这四条之外的一切挡在外面；走到这里是 dispatch 的 bug，
        // 但仍要返回一个形状合法的 Response（`AGENTS.md` §4.3：永不 panic）。
        other => stubbed(other.kind()),
    }
}

/// 未接线的设备命令。
///
/// 单独一个函数而不是直接调 [`super::unimplemented`]，是为了在接线阶段能一眼看出
/// 还剩几处桩（改完一处就少一处调用点）。
fn stubbed(kind: &str) -> Response {
    super::unimplemented(kind)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::temp_state;

    /// 四条命令在桩阶段都必须报 `-32006`，**不能**报 `-32005 denied`。
    ///
    /// 理由：`-32005` 在前端被解释成"用户拒绝/权限不足"，会引导用户去设置页找
    /// 一个并不存在的开关；`-32006` 才是"这个能力还没装配"。这条断言的真正价值是
    /// 防止别人为了省事把桩改成 `denied`。
    #[tokio::test]
    async fn every_device_command_reports_unimplemented_while_stubbed() {
        // `temp_state` 返回 `(TempDir, AppState)`：`TempDir` 必须活到测试结束，
        // 否则库文件在断言中途就被删了（其它域的单测都这么写）。
        let (_dir, state) = temp_state();
        let bot = hbot_core::BotId::from_uuid(uuid::Uuid::new_v4());

        let commands = vec![
            (Command::DeviceStatus { bot_id: bot }, "device_status"),
            (
                Command::DeviceGrant {
                    bot_id: bot,
                    sub: vec!["screen".to_owned()],
                    allowed_apps: vec!["game.exe".to_owned()],
                    minutes: Some(5),
                },
                "device_grant",
            ),
            (Command::DeviceRevoke { bot_id: bot }, "device_revoke"),
            (
                Command::DeviceRequest {
                    bot_id: bot,
                    sub: vec!["screen".to_owned()],
                    reason: "想看看你的游戏画面".to_owned(),
                },
                "device_request",
            ),
        ];

        for (command, kind) in commands {
            let response = handle(&state, command.clone()).await;
            let error = response.clone().into_result().expect_err("桩阶段必须报错");
            assert_eq!(error.code, -32006, "{kind} 应当是 -32006：{response:?}");
            assert!(
                error.message.contains(kind),
                "{kind} 的错误文案应当点名命令，实际：{}",
                error.message
            );
        }
    }
}
