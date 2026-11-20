//! 「五分钟上手」这条链的集成测试：新建项目 → 列表里就有 bot → 启动 → 注入 → 看到回复。
//!
//! 这是 `docs/quickstart-desktop.md` 的可执行版本：文档里每一句「点哪里、期望看到什么」
//! 都在这里有一条对应的命令调用。GUI 的点击没法在测试里点，所以钉住的是**点击背后的
//! 那条命令链**：少了任何一环，用户看到的都是空列表或启动失败。
//!
//! 关键前提：新建项目**必须自带** `bots.json`（`project_create` 写一条 local 条目）。
//! 没有它就只能靠用户手写文件，而 `docs/quickstart-desktop.md` 承诺的是零配置。

use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicU64, Ordering};

use hbot_adapters::SystemClock;
use hbot_core::BotStatus;
use hbot_rpc::Dispatcher;
use hbot_rpc::app::AppState;
use hbot_rpc::app::ids::UuidIds;
use hbot_rpc::dto::{BotSummary, Command, ConversationSummary, ProjectSummary, Response};

static SEQ: AtomicU64 = AtomicU64::new(0);

struct TempDir {
    path: PathBuf,
}

impl TempDir {
    fn new(label: &str) -> Self {
        let seq = SEQ.fetch_add(1, Ordering::Relaxed);
        let path = std::env::temp_dir().join(format!(
            "hbot-quickstart-{label}-{}-{seq}",
            std::process::id()
        ));
        std::fs::create_dir_all(&path).expect("创建临时目录");
        Self { path }
    }

    fn path(&self) -> &Path {
        &self.path
    }
}

impl Drop for TempDir {
    fn drop(&mut self) {
        let _ = std::fs::remove_dir_all(&self.path);
    }
}

struct Harness {
    _dir: TempDir,
    state: Arc<AppState>,
    dispatcher: Dispatcher,
}

impl Harness {
    async fn new(label: &str) -> Self {
        let dir = TempDir::new(label);
        let state = AppState::open(
            dir.path().join("data"),
            Arc::new(SystemClock),
            Arc::new(UuidIds),
        )
        .expect("打开 AppState");
        let state = Arc::new(state);
        let dispatcher = Dispatcher::new().with_state(Arc::clone(&state));
        Self {
            _dir: dir,
            state,
            dispatcher,
        }
    }

    /// 命令面板里的「新建项目…」：只给路径和名字，其余全由核心生成。
    async fn create_project(&self, name: &str) -> ProjectSummary {
        let path = self._dir.path().join("projects").join(name);
        let response = self
            .dispatcher
            .dispatch(Command::ProjectCreate {
                path: path.display().to_string(),
                name: name.to_owned(),
            })
            .await;
        match response {
            Response::Ok { data } => {
                let data = data.expect("project_create 应当有 data");
                serde_json::from_value(data).expect("ProjectSummary")
            }
            other => panic!("project_create 失败了：{other:?}"),
        }
    }

    async fn list(&self) -> Vec<BotSummary> {
        let response = self.dispatcher.dispatch(Command::BotList).await;
        match response {
            Response::Ok { data } => {
                let data = data.expect("bot_list 应当有 data");
                serde_json::from_value(data).expect("BotSummary 数组")
            }
            other => panic!("bot_list 失败了：{other:?}"),
        }
    }

    async fn start(&self, bot: hbot_core::BotId) {
        let response = self
            .dispatcher
            .dispatch(Command::BotStart { bot_id: bot })
            .await;
        assert!(
            !matches!(response, Response::Error { .. }),
            "bot_start 失败：{response:?}"
        );
    }

    async fn conversations(&self, bot: hbot_core::BotId) -> Vec<ConversationSummary> {
        let response = self
            .dispatcher
            .dispatch(Command::BotConversations { bot_id: bot })
            .await;
        match response {
            Response::Ok { data } => {
                let data = data.expect("bot_conversations 应当有 data（空列表也是 []）");
                serde_json::from_value(data).expect("ConversationSummary 数组")
            }
            other => panic!("bot_conversations 失败了：{other:?}"),
        }
    }

    async fn inject(
        &self,
        bot: hbot_core::BotId,
        conversation: hbot_core::ConversationId,
        text: &str,
    ) {
        let response = self
            .dispatcher
            .dispatch(Command::BotInject {
                bot_id: bot,
                conversation_id: conversation,
                text: text.to_owned(),
            })
            .await;
        assert!(
            !matches!(response, Response::Error { .. }),
            "bot_inject 失败：{response:?}"
        );
    }

    fn log_lines(&self, bot: hbot_core::BotId) -> Vec<String> {
        self.state
            .store()
            .list_logs(Some(&bot), 1024)
            .expect("读日志")
            .into_iter()
            .map(|row| row.line)
            .collect()
    }
}

/// 25 ms × 240 ≈ 6 秒：足够慢机器编译 + 跑一次 isolate，又不至于让失败变成三分钟挂起。
async fn wait_for<F, Fut>(label: &str, mut predicate: F)
where
    F: FnMut() -> Fut,
    Fut: std::future::Future<Output = bool>,
{
    for _ in 0..240 {
        if predicate().await {
            return;
        }
        tokio::time::sleep(std::time::Duration::from_millis(25)).await;
    }
    panic!("等不到 {label}：超过 6 秒没有任何变化");
}

/// 零配置路径：新建出来的项目立刻有一个能启动的 bot，发一句话就能看到回复。
#[tokio::test]
async fn a_brand_new_project_can_be_started_and_talked_to() {
    let harness = Harness::new("flow").await;
    let summary = harness.create_project("我的第一个机器人").await;
    assert_eq!(summary.name, "我的第一个机器人");

    // ① 试跑面板的数据来源：列表里必须有这一条，且状态是「停着」。
    let listed = harness.list().await;
    assert_eq!(
        listed.len(),
        1,
        "新建项目应当自带一个可试跑的 bot：{listed:?}"
    );
    let bot = listed[0].bot_id;
    assert_eq!(listed[0].platform, "local", "零配置入口只能是本地试跑");
    assert_eq!(listed[0].status, BotStatus::Stopped);
    assert_eq!(
        listed[0].name, "我的第一个机器人 本地试跑",
        "展示名要能认出这是哪个项目的试跑 bot"
    );

    // ② 点「开始试跑」：local 平台会同步建好 local:default 会话。
    harness.start(bot).await;
    let conversations = harness.conversations(bot).await;
    assert_eq!(conversations.len(), 1, "local 平台启动后必须有一条会话");
    assert_eq!(conversations[0].external_id, "local:default");
    let conversation = conversations[0].conversation_id;

    // ③ 输入一条模拟消息，回车。
    harness.inject(bot, conversation, "你好").await;

    // ④ 「回复」区与日志面板的同一个数据源：outbound 的 [reply] 行。
    let bot_for_logs = bot;
    wait_for("回复落地", || {
        let lines = harness.log_lines(bot_for_logs);
        async move { lines.iter().any(|line| line.contains("[reply]")) }
    })
    .await;

    let lines = harness.log_lines(bot);
    let reply = lines
        .iter()
        .find(|line| line.contains("[reply]"))
        .expect("必须有 [reply] 行")
        .clone();
    assert!(
        reply.contains("收到：你好"),
        "回复必须是 handler 真的算出来的，实际：{reply}"
    );
    assert!(
        !lines.iter().any(|line| line.contains("HBOT_")),
        "零配置路径不该有任何能力码报错：{lines:?}"
    );
}

/// 同名项目不能建两次：用户点两下「创建」时看到的是冲突提示，而不是两个项目。
#[tokio::test]
async fn creating_the_same_project_twice_is_a_conflict() {
    let harness = Harness::new("dupe").await;
    harness.create_project("重复项目").await;
    let path = harness._dir.path().join("projects").join("重复项目");
    let response = harness
        .dispatcher
        .dispatch(Command::ProjectCreate {
            path: path.display().to_string(),
            name: "重复项目".to_owned(),
        })
        .await;
    match response {
        Response::Error { error, .. } => assert_eq!(error.code, -32003, "必须是冲突码：{error:?}"),
        other => panic!("第二次创建必须失败，实际 {other:?}"),
    }
}
