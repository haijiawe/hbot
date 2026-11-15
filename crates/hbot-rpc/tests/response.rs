//! 响应/错误契约测试：ApiError 的错误码映射、Response 包装、核心 ID 可达性。

mod support;

use hbot_core::HbotError;
use hbot_rpc::{ApiError, Response};
use serde_json::json;
use support::{BOT_UUID, PROJECT_UUID};

#[test]
fn api_error_maps_from_hbot_error() {
    let cases: [(HbotError, i32); 6] = [
        (HbotError::not_found("project", PROJECT_UUID), -32001),
        (HbotError::invalid("bad glob"), -32002),
        (HbotError::conflict("hash mismatch"), -32003),
        (HbotError::timeout("llm_request", 1_000), -32004),
        (HbotError::denied("approval rejected"), -32005),
        (HbotError::internal("bug"), -32006),
    ];

    for (error, code) in cases {
        let expected_message = error.to_string();
        let mapped = ApiError::from(error);
        assert_eq!(mapped.code, code);
        assert_eq!(mapped.message, expected_message);
    }

    // NotFound / Timeout 带结构化 data；其余为 null。
    let not_found = ApiError::from(HbotError::not_found("bot", BOT_UUID));
    assert_eq!(
        not_found.data,
        Some(json!({ "entity": "bot", "id": BOT_UUID }))
    );

    let timeout = ApiError::from(HbotError::timeout("run_command", 5_000));
    assert_eq!(
        timeout.data,
        Some(json!({ "operation": "run_command", "timeoutMs": 5000 }))
    );

    assert_eq!(ApiError::from(HbotError::internal("bug")).data, None);
    assert_eq!(ApiError::from(HbotError::invalid("x")).data, None);
    assert_eq!(ApiError::from(HbotError::conflict("x")).data, None);
    assert_eq!(ApiError::from(HbotError::denied("x")).data, None);
}

#[test]
fn api_error_json_shape() {
    let error = ApiError {
        code: -32005,
        message: "denied by sandbox".to_string(),
        data: None,
    };
    assert_eq!(
        serde_json::to_value(&error).expect("ser"),
        json!({
            "code": -32005,
            "message": "denied by sandbox",
            "data": null,
        })
    );
    assert_eq!(
        serde_json::from_value::<ApiError>(json!({ "code": -32002, "message": "bad" }))
            .expect("de")
            .data,
        None,
        "旧 UI 不传 data 时按 None 处理"
    );
}

#[test]
fn response_wraps_ok_and_error() {
    assert_eq!(
        serde_json::to_value(Response::ok()).expect("ser"),
        json!({ "type": "ok", "data": null })
    );
    assert_eq!(
        serde_json::to_value(Response::with_data(json!({ "version": "0.1.0" }))).expect("ser"),
        json!({ "type": "ok", "data": { "version": "0.1.0" } })
    );
    assert_eq!(
        serde_json::to_value(Response::error(ApiError {
            code: -32006,
            message: "boom".to_string(),
            data: None,
        }))
        .expect("ser"),
        json!({
            "type": "error",
            "error": { "code": -32006, "message": "boom", "data": null },
        })
    );

    // 往返：UI 侧按 type 收窄后字段可直接用。
    let parsed: Response = serde_json::from_value(json!({ "type": "ok" })).expect("de");
    assert_eq!(parsed, Response::ok());
}

#[test]
fn response_converts_to_result_for_transports() {
    assert_eq!(Response::ok().into_result(), Ok(None));
    assert_eq!(
        Response::with_data(json!({ "version": "0.1.0" })).into_result(),
        Ok(Some(json!({ "version": "0.1.0" })))
    );

    let error = ApiError {
        code: -32001,
        message: "not found: project".to_string(),
        data: None,
    };
    assert_eq!(
        Response::error(error.clone()).into_result(),
        Err(error),
        "错误必须能被传输层原样取出"
    );
}
