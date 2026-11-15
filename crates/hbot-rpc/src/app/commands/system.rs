//! 系统命令：`app_info`、`project_open`。
//!
//! 这两条是 UI 的启动握手：`app_info` 拿产品名/版本/平台/数据目录，`project_open`
//! 决定后续所有文件命令的相对根。

use std::path::PathBuf;

use hbot_core::HbotError;

use super::failed;
use crate::app::AppState;
use crate::dto::{AppInfo, Response};

/// 产品名（与 `tauri.conf.json` 的 `productName`、`Cargo.toml` 的包名分开写：
/// 前者是给用户看的，后者是给 cargo 看的）。
pub const PRODUCT_NAME: &str = "hbot";

/// `app_info`：真数据，不占位。
pub fn app_info(state: &AppState) -> Response {
    let info = AppInfo {
        name: PRODUCT_NAME.to_owned(),
        version: env!("CARGO_PKG_VERSION").to_owned(),
        platform: std::env::consts::OS.to_owned(),
        data_dir: state.data_dir().display().to_string(),
    };

    // `AppInfo` 只有字符串字段，序列化不可能失败；仍然显式报错而不是 `unwrap`
    // （库代码禁 panic，`AGENTS.md` §5）。
    match serde_json::to_value(info) {
        Ok(value) => Response::with_data(value),
        Err(error) => failed(HbotError::internal(format!("序列化 AppInfo 失败：{error}"))),
    }
}

/// `project_open`：打开（或切换）项目根，返回规范化后的绝对路径。
///
/// 路径必须存在且是目录：`Workspace::open` 会 canonicalize，不存在时报 `-32001`，
/// 不是目录时报 `-32002`。不在这里 `create_dir_all`——"打开"和"新建"是两件事，
/// 自动建目录会让打错路径悄悄变成一个空项目。
pub async fn project_open(state: &AppState, path: String) -> Response {
    match open_project_root(state, path).await {
        Ok(root) => Response::with_data(serde_json::Value::String(root.display().to_string())),
        Err(error) => failed(error),
    }
}

async fn open_project_root(state: &AppState, path: String) -> Result<PathBuf, HbotError> {
    if path.trim().is_empty() {
        return Err(HbotError::invalid("project_open: path 不能为空"));
    }
    state.open_project(PathBuf::from(path)).await
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::app::test_support::temp_state;

    #[test]
    fn app_info_reports_real_data_dir() {
        let (_dir, state) = temp_state();
        let response = app_info(&state);
        let crate::dto::Response::Ok { data } = response else {
            panic!("app_info 必须成功");
        };
        let info: AppInfo = serde_json::from_value(data.expect("app_info 有 data")).unwrap();
        assert_eq!(info.name, PRODUCT_NAME);
        assert!(!info.version.is_empty());
        assert!(!info.platform.is_empty());
        assert!(std::path::Path::new(&info.data_dir).is_dir());
    }

    #[test]
    fn empty_project_path_is_invalid_not_missing() {
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        let (_dir, state) = temp_state();
        let response = runtime.block_on(project_open(&state, "   ".to_owned()));
        let crate::dto::Response::Error { error } = response else {
            panic!("空路径必须失败");
        };
        assert_eq!(error.code, -32002);
    }
}
