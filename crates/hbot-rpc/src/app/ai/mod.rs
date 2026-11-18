//! 宿主侧的智能能力（C5）：`ctx.ai.complete` 的端口实现。
//!
//! 契约在 `hbot_core::bot_ai`（**已冻结**）：[`AiClient`] 端口 trait、`AiRequest` /
//! `AiText` 类型、参数闸（`validate_*`）与 `HBOT_AI_*` 错误码。装配、每事件预算、
//! 等模型时间的抵账都在 `hbot-code` 那侧（`crates/hbot-code/src/{ai.rs,
//! runtime/ai_api.rs}`）。本模块只做三件事：
//!
//! 1. 把 [`AiRequest`] 翻译成 `hbot_agent` 的 [`CompletionRequest`]（一段 system +
//!    一段 user，不带工具）；
//! 2. 用现成的 provider 工厂发请求（与 Agent 走同一条链路，不另造 HTTP 客户端）；
//! 3. 把 `HbotError` 折成 `AiError`，**错误正文先过 `redact_text`**。
//!
//! ## 为什么请求里没有 base_url / api_key / model
//!
//! 这三样都不在 `AiRequest` 里（见 `hbot_core::bot_ai` 的模块文档）：模型选择是用户
//! 设置的事（`agent.json` / `HBOT_LLM_*`），不是项目源码的事。生成代码能自定端点
//! 等于绕过 `permissions.http` 开一个任意出网口子。
//!
//! ## 为什么是工厂（[`AiSource`]）而不是一个现成实例
//!
//! 与 [`crate::app::state::StateSource`] 同一理由，两件事的作用域不同：
//!
//! * 能力声明是**项目级**的（`hbot.json` 的 `capabilities.ai`），随时会被 Agent 改；
//! * provider 配置是**宿主级**的（`<data_dir>/agent.json` + 环境变量），一个进程一份。
//!
//! 所以 [`HostAiSource::client`] 每次调都现读清单；provider 本身（连同它的
//! `reqwest` 连接池）按配置缓存一份 —— 每条消息都新建一个 transport 会把连接池
//! 建一遍，那是纯浪费。
//!
//! ## 声明层与装配层是两道闸
//!
//! 清单没写 `capabilities.ai` → `HBOT_AI_NOT_DECLARED`（改清单）；
//! 宿主没配模型（`agent.json` 缺 baseUrl/model、或 key 空且端点要 key）→
//! [`HostAiSource::client`] 返回 `None`，`hbot-code` 报 `HBOT_AI_UNAVAILABLE`（去设置里配）。
//! 两者分开报，是为了让排障方向不同。

use std::path::Path;
use std::sync::{Arc, Mutex};
use std::time::Duration;

use hbot_agent::provider::NullSink;
use hbot_agent::{CancellationToken, ChatMessage, CompletionRequest, LlmProvider, Provider};
use hbot_core::bot_ai::{self, AiClient, AiError, AiRequest, AiText, AiUsage};
use hbot_core::redact::redact_text;

use crate::app::agent::config::ProviderConfig;

/// 一次补全的发请求预算。
///
/// 比 `hbot-code` 的 `DEFAULT_CALL_TIMEOUT`（30 s）略短：让"沙箱侧超时"永远比
/// "传输侧超时"晚一步，报出来的才是 `HBOT_AI_TIMEOUT` 而不是一条含糊的
/// `HBOT_AI_FAILED`。真正的上限仍由 `hbot-code` 从 handler 预算里算出。
const REQUEST_TIMEOUT: Duration = Duration::from_secs(28);

/// 每个 bot 项目一份智能装配：清单里的 `capabilities.ai` 决定给不给客户端。
///
/// 实现方只做两件事：读清单、按声明造客户端。**不要在实现里缓存清单**（否则
/// 热重载换掉的 `hbot.json` 不会生效）。
pub trait AiSource: Send + Sync {
    /// 为 `root` 这个项目造一个模型客户端。
    ///
    /// 返回 `None` 表示**宿主没有可用的模型配置**（对应
    /// `HBOT_AI_UNAVAILABLE`）。注意 `None` 不等于"项目没声明"——声明是 [`AiClient`]
    /// 实现自己的第一道判据，它要能报出 `HBOT_AI_NOT_DECLARED` 这个更具体的码。
    fn client(&self, root: &Path) -> Option<Arc<dyn AiClient>>;
}

/// 用 `agent.json` / `HBOT_LLM_*` 的 provider 配置支撑的 [`AiSource`]。
pub struct HostAiSource {
    data_dir: std::path::PathBuf,
    /// provider 实例（连同连接池）缓存，按 `(provider, base_url, model, api_key 指纹)`
    /// 复用。用 `Mutex` 而不是 `OnceLock`：用户能在设置页换模型，换完要立刻生效。
    cached: Mutex<Option<CachedProvider>>,
}

/// 缓存的一条 provider 与其配置指纹。
struct CachedProvider {
    /// `(provider, base_url, model)` —— 三者任一变了就要重建。
    identity: (LlmProvider, String, String),
    provider: Arc<dyn Provider>,
}

impl HostAiSource {
    /// 用数据目录（里面放着 `agent.json`）造工厂。
    #[must_use]
    pub fn new(data_dir: impl Into<std::path::PathBuf>) -> Self {
        Self {
            data_dir: data_dir.into(),
            cached: Mutex::new(None),
        }
    }

    /// 现读配置；配置不全（或合法但无法建 provider）就返回 `None`。
    fn config(&self) -> Option<ProviderConfig> {
        // 读配置是同步 IO，调用点（`SandboxHandler::worker`）已经在 `spawn_blocking`
        // 或等价的阻塞上下文里，这里不再包一层。
        match ProviderConfig::resolve(&self.data_dir) {
            Ok(config) => Some(config),
            Err(error) => {
                // 不是错误，是"还没配"：`HBOT_AI_UNAVAILABLE` 由沙箱侧报。
                tracing::debug!(error = %error, "没有可用的模型配置，ctx.ai 将报 HBOT_AI_UNAVAILABLE");
                None
            }
        }
    }

    /// 取（或重建）provider。配置变了就重建，否则复用连接池。
    fn provider(&self, config: &ProviderConfig) -> Result<Arc<dyn Provider>, String> {
        let identity = (
            config.provider(),
            config.base_url().to_owned(),
            config.model().to_owned(),
        );
        let mut cached = self
            .cached
            .lock()
            .unwrap_or_else(std::sync::PoisonError::into_inner);
        if let Some(existing) = cached.as_ref()
            && existing.identity == identity
        {
            return Ok(Arc::clone(&existing.provider));
        }
        // 与 Agent 同一条链路：不跨 provider 重试，失败如实上报。
        let provider = crate::app::agent::default_provider_factory(config)
            .map_err(|error| redact_text(&error.to_string()))?;
        let provider: Arc<dyn Provider> = Arc::from(provider);
        *cached = Some(CachedProvider {
            identity,
            provider: Arc::clone(&provider),
        });
        Ok(provider)
    }
}

impl AiSource for HostAiSource {
    fn client(&self, root: &Path) -> Option<Arc<dyn AiClient>> {
        // 读不到清单 = 全不声明（fail-closed）。**不缓存**：热重载要能换声明。
        let declared = hbot_code::manifest::load(root)
            .map(|manifest| manifest.capabilities.ai())
            .unwrap_or(false);
        let config = self.config()?;
        let provider = match self.provider(&config) {
            Ok(provider) => provider,
            Err(reason) => {
                tracing::warn!(reason = %reason, "建不出模型 provider，ctx.ai 将报 HBOT_AI_FAILED");
                return None;
            }
        };
        let client: Arc<ProviderAiClient> = Arc::new(ProviderAiClient {
            declared,
            api: provider,
            model: config.model().to_owned(),
        });
        Some(client)
    }
}

/// 把 [`AiRequest`] 走一次 provider 补全的 [`AiClient`] 实现。
///
/// 不带工具（`tools: Vec::new()`）：生成代码的 `ctx.ai.complete` 是"要一段文本"，
/// 不是"让模型调工具"——工具网关在 Agent 那一侧，不该被 bot 代码借用。
pub struct ProviderAiClient {
    /// 清单是否声明了 `capabilities.ai`（第一道闸）。
    declared: bool,
    api: Arc<dyn Provider>,
    model: String,
}

impl AiClient for ProviderAiClient {
    fn complete<'a>(
        &'a self,
        request: &'a AiRequest,
    ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Result<AiText, AiError>> + Send + 'a>>
    {
        Box::pin(async move {
            if !self.declared {
                return Err(AiError::NotDeclared);
            }

            let mut messages = Vec::with_capacity(2);
            if let Some(system) = &request.system {
                messages.push(ChatMessage::system(system.clone()));
            }
            messages.push(ChatMessage::user(request.prompt.clone()));

            let req = CompletionRequest {
                model: self.model.clone(),
                messages,
                tools: Vec::new(),
                temperature: request.temperature,
                max_tokens: Some(request.max_tokens),
                cancel: CancellationToken::new(),
            };

            let mut sink = NullSink;
            let completion = match tokio::time::timeout(
                REQUEST_TIMEOUT,
                self.api.complete(req, &mut sink),
            )
            .await
            {
                Ok(Ok(completion)) => completion,
                Ok(Err(error)) => {
                    return Err(AiError::Failed {
                        message: redact_text(&error.to_string()),
                    });
                }
                Err(_elapsed) => {
                    return Err(AiError::Timeout {
                        ms: u64::try_from(REQUEST_TIMEOUT.as_millis()).unwrap_or(u64::MAX),
                    });
                }
            };

            Ok(AiText {
                // `completion.text()` 在纯工具调用回合是空串；这里不发工具，所以空串
                // 就是"模型什么都没说"，照实返回而不是编一句。
                text: completion.text().to_owned(),
                usage: AiUsage {
                    prompt_tokens: completion.usage.prompt_tokens,
                    completion_tokens: completion.usage.completion_tokens,
                },
                model: self.model.clone(),
            })
        })
    }
}

/// 参数闸的一次性包装（给集成测试用：断言"坏参数由 `hbot-code` 挡下"）。
///
/// 生产路径不会调用它；放在这里是为了让测试不必重建 `hbot_core` 的那四条判据。
// 不加 `#[must_use]`：`Result` 本身已经是 `#[must_use]`，再叠一层会被
// `clippy::double_must_use` 判红（本仓 clippy 开 `-D warnings`）。
pub fn validate_request(request: &AiRequest) -> Result<(), AiError> {
    bot_ai::validate_prompt(&request.prompt)?;
    bot_ai::validate_system(request.system.as_deref())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    use std::path::PathBuf;
    use std::sync::atomic::{AtomicUsize, Ordering};

    use hbot_agent::FinishReason;

    /// 假 provider：按脚本返回，并记录收到几次请求。
    struct FakeProvider {
        calls: AtomicUsize,
        result: Result<String, String>,
        delays: Duration,
    }

    #[async_trait::async_trait]
    impl Provider for FakeProvider {
        async fn complete(
            &self,
            req: CompletionRequest,
            _sink: &mut dyn hbot_agent::StreamSink,
        ) -> Result<hbot_agent::Completion, hbot_core::HbotError> {
            self.calls.fetch_add(1, Ordering::SeqCst);
            if !self.delays.is_zero() {
                tokio::time::sleep(self.delays).await;
            }
            assert!(
                req.tools.is_empty(),
                "ctx.ai 不许借用工具网关：{:?}",
                req.tools.len()
            );
            match &self.result {
                Ok(text) => Ok(hbot_agent::Completion {
                    content: Some(text.clone()),
                    tool_calls: Vec::new(),
                    usage: hbot_agent::Usage {
                        prompt_tokens: 5,
                        completion_tokens: 3,
                        total_tokens: 8,
                    },
                    finish_reason: FinishReason::Stop,
                }),
                Err(message) => Err(hbot_core::HbotError::internal(message.clone())),
            }
        }
    }

    fn request() -> AiRequest {
        AiRequest::new("说一句")
    }

    #[tokio::test]
    async fn an_undeclared_project_never_reaches_the_provider() {
        let provider = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            result: Ok("不该被调用".to_owned()),
            delays: Duration::ZERO,
        });
        let client = ProviderAiClient {
            declared: false,
            api: provider.clone(),
            model: "m".to_owned(),
        };
        let error = client
            .complete(&request())
            .await
            .expect_err("未声明必须失败");
        assert_eq!(error.code(), "HBOT_AI_NOT_DECLARED");
        assert_eq!(
            provider.calls.load(Ordering::SeqCst),
            0,
            "未声明时不许碰 provider"
        );
    }

    #[tokio::test]
    async fn a_declared_project_gets_the_text_and_the_usage() {
        let provider = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            result: Ok("收到".to_owned()),
            delays: Duration::ZERO,
        });
        let client = ProviderAiClient {
            declared: true,
            api: provider.clone(),
            model: "fake".to_owned(),
        };
        let text = client.complete(&request()).await.expect("声明后应成功");
        assert_eq!(text.text, "收到");
        assert_eq!(text.usage.prompt_tokens, 5);
        assert_eq!(text.usage.completion_tokens, 3);
        assert_eq!(text.model, "fake");
        assert_eq!(provider.calls.load(Ordering::SeqCst), 1);
    }

    #[tokio::test]
    async fn an_upstream_error_is_redacted_and_kept_as_failed() {
        let provider = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            result: Err("401 api_key=sk-abcdefghijklmnop".to_owned()),
            delays: Duration::ZERO,
        });
        let client = ProviderAiClient {
            declared: true,
            api: provider,
            model: "m".to_owned(),
        };
        let error = client.complete(&request()).await.expect_err("上游错要上报");
        assert_eq!(error.code(), "HBOT_AI_FAILED");
        let AiError::Failed { message } = error else {
            panic!("应是 Failed");
        };
        assert!(
            !message.contains("sk-abcdefghijklmnop"),
            "错误正文必须脱敏，实际：{message}"
        );
    }

    #[tokio::test]
    async fn a_slow_provider_reports_timeout_not_failed() {
        let provider = Arc::new(FakeProvider {
            calls: AtomicUsize::new(0),
            result: Ok("太慢了".to_owned()),
            delays: REQUEST_TIMEOUT + Duration::from_secs(5),
        });
        let client = ProviderAiClient {
            declared: true,
            api: provider,
            model: "m".to_owned(),
        };
        let error = client.complete(&request()).await.expect_err("应超时");
        assert_eq!(error.code(), "HBOT_AI_TIMEOUT");
    }

    // -----------------------------------------------------------------------
    // AiSource：清单声明 + 宿主配置两道闸
    // -----------------------------------------------------------------------

    use std::fs;

    /// 临时项目 + 数据目录。
    struct Fixture {
        dir: PathBuf,
    }

    impl Fixture {
        fn new(label: &str, manifest: &str, agent_json: Option<&str>) -> Self {
            use std::sync::atomic::AtomicU64;
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let dir = std::env::temp_dir()
                .join(format!("hbot-rpc-ai-{label}-{}-{seq}", std::process::id()));
            let _ = fs::remove_dir_all(&dir);
            fs::create_dir_all(dir.join("project/src")).expect("建目录");
            fs::create_dir_all(dir.join("data")).expect("建目录");
            fs::write(dir.join("project/src/main.ts"), "export default {};").expect("写 main.ts");
            fs::write(dir.join("project/hbot.json"), manifest).expect("写清单");
            if let Some(agent) = agent_json {
                fs::write(dir.join("data/agent.json"), agent).expect("写 agent.json");
            }
            Self { dir }
        }

        fn project(&self) -> PathBuf {
            self.dir.join("project")
        }

        fn data(&self) -> PathBuf {
            self.dir.join("data")
        }
    }

    impl Drop for Fixture {
        fn drop(&mut self) {
            let _ = fs::remove_dir_all(&self.dir);
        }
    }

    const CONFIG: &str =
        r#"{"provider":"openai","baseUrl":"http://127.0.0.1:9/v1","apiKey":"k","model":"m"}"#;

    #[test]
    fn a_missing_manifest_means_not_declared() {
        let fixture = Fixture::new("no-manifest", "{}", Some(CONFIG));
        // 清单非法 → 读不到 → 按"全不声明"处理。
        fs::write(fixture.project().join("hbot.json"), "not json").expect("写坏清单");
        let source = HostAiSource::new(fixture.data());
        let client = source
            .client(&fixture.project())
            .expect("宿主配置在，应给出客户端");
        // 客户端存在，但一道声明闸在 complete() 里 —— 用 tokio 跑一次确认它报 NotDeclared。
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(client.complete(&request()))
            .expect_err("坏清单 = 未声明");
        assert_eq!(error.code(), "HBOT_AI_NOT_DECLARED");
    }

    #[test]
    fn a_project_that_does_not_declare_ai_gets_a_client_that_refuses() {
        let manifest = r#"{"name":"b","platform":{"kind":"local"}}"#;
        let fixture = Fixture::new("undeclared", manifest, Some(CONFIG));
        let source = HostAiSource::new(fixture.data());
        let client = source
            .client(&fixture.project())
            .expect("配置在，客户端就有");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let error = runtime
            .block_on(client.complete(&request()))
            .expect_err("没声明 ai 必须拒");
        assert_eq!(error.code(), "HBOT_AI_NOT_DECLARED");
    }

    #[test]
    fn without_host_config_the_source_returns_none_so_the_sandbox_says_unavailable() {
        let manifest = r#"{"name":"b","platform":{"kind":"local"},
            "capabilities":{"ai":true}}"#;
        let fixture = Fixture::new("no-config", manifest, None);
        let source = HostAiSource::new(fixture.data());
        assert!(
            source.client(&fixture.project()).is_none(),
            "没配模型时工厂返回 None，沙箱侧据此报 HBOT_AI_UNAVAILABLE"
        );
    }

    #[test]
    fn a_declared_project_with_config_gets_a_client() {
        let manifest = r#"{"name":"b","platform":{"kind":"local"},
            "capabilities":{"ai":true}}"#;
        let fixture = Fixture::new("happy", manifest, Some(CONFIG));
        let source = HostAiSource::new(fixture.data());
        assert!(source.client(&fixture.project()).is_some());
    }

    #[test]
    fn the_manifest_is_reread_on_every_call() {
        let unknown = r#"{"name":"b","platform":{"kind":"local"}}"#;
        let declared = r#"{"name":"b","platform":{"kind":"local"},
            "capabilities":{"ai":true}}"#;
        let fixture = Fixture::new("reread", unknown, Some(CONFIG));
        let source = HostAiSource::new(fixture.data());

        // 第一次：没声明。
        let manifest_path = fixture.project().join("hbot.json");
        fs::write(&manifest_path, unknown).expect("写清单");
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("runtime");
        let before = source.client(&fixture.project()).expect("客户端在");
        let error = runtime
            .block_on(before.complete(&request()))
            .expect_err("未声明");
        assert_eq!(error.code(), "HBOT_AI_NOT_DECLARED");

        // 像 Agent 一样只改清单（一个 .ts 字节都不动），下一次就该放行。
        fs::write(&manifest_path, declared).expect("改写清单");
        let after = source.client(&fixture.project()).expect("客户端在");
        // provider 打的是 127.0.0.1:9（拒绝连接），所以声明通过了、网络失败如实上报。
        let error = runtime
            .block_on(after.complete(&request()))
            .expect_err("网络应失败");
        assert_eq!(
            error.code(),
            "HBOT_AI_FAILED",
            "声明闸过了才会走到网络，失败应是 FAILED 而不是 NOT_DECLARED：{error}"
        );
    }
}
