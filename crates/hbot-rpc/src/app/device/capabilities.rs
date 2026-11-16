//! 设备能力的**清单声明层**：[`hbot_core::device::CapabilityGate`] 的 RPC 侧实现。
//!
//! ## 这是哪一层
//!
//! 设备能力的判据分**两层**，缺一层都等于没判（设计稿
//! `docs/09-device-capability.md` §4）：
//!
//! 1. **静态声明层**（本模块；设计稿 §4.1"第一层：清单声明"）：这份项目代码
//!    （`hbot.json` 的 `permissions.device.sub`）有没有资格要这一类能力。它不看
//!    "现在有没有人点头"，只看清单。`ToolGateway` 的第一个判断步骤
//!    （`GatewayStep::Capability`）问的就是它 —— 一个没声明 `ocr` 的项目，连
//!    "去问用户要不要授权"这一步都不该走到（判定表见 §4.4）。
//! 2. **运行时授权层**（[`super::DeviceRegistry`] 实现的 `DeviceGate`）：这一次会话、
//!    这一刻、对谁。它判的是"用户刚点的那下确认还作不作数"。设计稿把这一层展开成
//!    两道（§4.2 宿主设置、§4.3 运行时会话），都不归本模块管。
//!
//! 谁也不能替谁：少了第 1 层，用户授权过一个 `screen` 会话，没声明 `ocr` 的 bot 就能读屏；
//! 少了第 2 层，清单里写了 `input` 就永久可用，"一键撤销"形同虚设。
//!
//! ## 为什么不缓存
//!
//! [`hbot_core::device::CapabilityGate`] 的契约要求**无状态判定**：每次 `authorize`
//! 都重新读 `hbot.json`。缓存（哪怕只缓存一秒）会让"用户把 `ocr` 从
//! `permissions.device.sub` 里删掉、下一次工具调用仍在读屏"成为可能 —— 而改清单正是
//! 用户收回权限最直接的方式。所以 [`ManifestCapabilityGate`] 只存项目根这个**地址**，
//! 不存读过之后的**结论**（`authorize_rereads_the_manifest_on_every_call` 钉住这一点）。
//!
//! ## 为什么读失败是"没声明"
//!
//! 缺清单、JSON 坏、清单校验不过（`hbot_code::manifest::load` 还会查 `entry` 指向的文件
//! 是否存在）、目录读不了 —— 一律 [`DeviceError::NotDeclared`]。闸门的方向只能是
//! "证明声明过才放行"：读不出来的清单不能被解释成"什么都可以"（那是 default-allow），
//! 也不能 panic（一个坏文件不该把整条工具调用链打崩）。

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

use hbot_core::device::{CapabilityGate, DeviceCapability, DeviceError};

/// 读 `<root>/hbot.json` 的 `permissions.device.sub`，返回清单声明的子能力。
///
/// **每次调用都重新读盘**（见模块文档"为什么不缓存"）。读失败返回**空集**，
/// 由调用方翻成各自的"未声明"形状：能力闸要 [`DeviceError::NotDeclared`]，
/// 登记表要"没声明"（两者语义一致：证明声明过才放行）。
///
/// 与 `DeviceRegistry::declared_capabilities` 同源同口径 —— 那两处曾经
/// 各读一遍清单，任何一处改了判定（比如"空的 `device` 块怎么办"）都会让
/// "闸放行、登记表不放行"或反过来。现在只有这一份实现。
pub(crate) fn declared_at(root: &Path) -> BTreeSet<DeviceCapability> {
    match hbot_code::manifest::load(root) {
        Ok(manifest) => manifest
            .permissions
            .device
            .map(|device| device.sub.into_iter().collect())
            .unwrap_or_default(),
        Err(error) => {
            tracing::debug!(
                root = %root.display(),
                %error,
                "读不到项目清单，设备子能力按未声明处理"
            );
            BTreeSet::new()
        }
    }
}

/// 按项目清单判定的能力声明闸。
///
/// `ToolGateway::with_capability_gate` 装配的就是它（见
/// `crate::app::agent::AgentManager::start_turn`）：每个回合按当时的项目根现建一个，
/// 而它自己**不存判定结果** —— 所以清单在回合中途被改，下一步调用立刻按新清单判。
pub struct ManifestCapabilityGate {
    /// 项目根（`hbot.json` 所在目录）。只存地址，不存结论。
    root: PathBuf,
}

impl ManifestCapabilityGate {
    /// 指向一个项目根。
    ///
    /// 不在这里读清单：构造必须便宜且不能失败（构造失败会让 `agent_send` 直接报错，
    /// 而"清单暂时读不到"应当在**用它的时候**表现为一次 `NotDeclared`，不是让整个
    /// 回合打不开）。
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    /// 这个闸读的是哪个项目根的清单。
    pub fn root(&self) -> &Path {
        &self.root
    }
}

impl CapabilityGate for ManifestCapabilityGate {
    fn authorize(&self, capability: DeviceCapability) -> Result<(), DeviceError> {
        // 重读，不快照：这是 `CapabilityGate` 的契约，也是"改清单立刻生效"的唯一保证。
        if declared_at(&self.root).contains(&capability) {
            return Ok(());
        }
        Err(DeviceError::NotDeclared { capability })
    }
}

#[cfg(test)]
mod tests {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicU64, Ordering};

    use hbot_agent::{
        ApprovalStore, CancellationToken, DenyAllApprover, GatewayStep, NullAudit, ToolCallRequest,
        ToolGateway, ToolRegistry,
    };
    use hbot_core::{DeviceBackend, HbotError, SandboxMode, SessionId};
    use hbot_workspace::Workspace;

    use super::*;

    /// 每个测试自己的临时目录（与 `device::tests` 的做法一致，不引 dev-dependency）。
    struct TempDir {
        path: PathBuf,
    }

    impl TempDir {
        fn new(label: &str) -> Self {
            static SEQ: AtomicU64 = AtomicU64::new(0);
            let seq = SEQ.fetch_add(1, Ordering::Relaxed);
            let path = std::env::temp_dir().join(format!(
                "hbot-capability-gate-{label}-{}-{seq}",
                std::process::id()
            ));
            let _ = std::fs::remove_dir_all(&path);
            std::fs::create_dir_all(&path).expect("建临时目录");
            Self { path }
        }

        /// 写一份合法清单。`sub` 是原样的 JSON 数组正文（如 `"[\"ocr\"]"`），
        /// 调用方测试的正是"清单里写了什么"，所以这里不做序列化，保留拼写自由度。
        fn write_manifest(&self, sub: &str) {
            // 清单校验要求 `entry` 指向的文件真的存在（默认入口是 `src/main.ts`）。
            std::fs::create_dir_all(self.path.join("src")).expect("建 src");
            std::fs::write(self.path.join("src").join("main.ts"), "export {};\n")
                .expect("写入口文件");
            std::fs::write(
                self.path.join("hbot.json"),
                format!(
                    r#"{{"name":"capability-gate","platform":{{"kind":"local"}},"permissions":{{"device":{{"sub":{sub}}}}}}}"#
                ),
            )
            .expect("写清单");
        }
    }

    impl Drop for TempDir {
        fn drop(&mut self) {
            let _ = std::fs::remove_dir_all(&self.path);
        }
    }

    fn gate(dir: &TempDir) -> ManifestCapabilityGate {
        ManifestCapabilityGate::new(&dir.path)
    }

    /// 清单里写了 `ocr` → 放行。
    #[test]
    fn a_declared_capability_is_authorized() {
        let dir = TempDir::new("declared");
        dir.write_manifest(r#"["screen","ocr"]"#);

        let gate = gate(&dir);
        assert_eq!(gate.authorize(DeviceCapability::Ocr), Ok(()));
        // 同一份清单里的另一项也应放行：闸答的是"这份清单声明了什么"，不是单点开关。
        assert_eq!(gate.authorize(DeviceCapability::Screen), Ok(()));
        // 项目根只是地址，要能取出来给日志看。
        assert_eq!(gate.root(), dir.path.as_path());
    }

    /// 清单里没写 `ocr` → `NotDeclared`（不是 `NotEnabled`、不是 `Failed`：
    /// "清单没声明"与"设置没开"必须分得开，前者不给开关提示）。
    #[test]
    fn an_undeclared_capability_is_refused_as_not_declared() {
        let dir = TempDir::new("undeclared");
        dir.write_manifest(r#"["screen"]"#);

        let error = gate(&dir)
            .authorize(DeviceCapability::Ocr)
            .expect_err("没声明的能力必须拒绝");
        assert_eq!(
            error,
            DeviceError::NotDeclared {
                capability: DeviceCapability::Ocr
            }
        );
        assert_eq!(error.code(), "HBOT_DEVICE_NOT_DECLARED");
        assert_eq!(error.toggle(), None, "清单未声明时不许给开关提示");
    }

    /// **不给快照**：同一个 gate 实例，先把 `ocr` 写进清单（Ok），再把 `ocr` 删掉，
    /// 下一次调用必须立刻变成 `NotDeclared`；再写回来又要立刻变回 Ok。
    ///
    /// 这条是 `CapabilityGate` 契约里"每次调用都查当前声明、不许构造时快照"的钉子：
    /// 任何形式的缓存（构造时读一次、按 mtime 缓存、只读一次存字段）都会让它变红。
    #[test]
    fn authorize_rereads_the_manifest_on_every_call() {
        let dir = TempDir::new("reread");
        dir.write_manifest(r#"["ocr"]"#);

        let gate = gate(&dir);
        assert_eq!(
            gate.authorize(DeviceCapability::Ocr),
            Ok(()),
            "第一次读到的清单里有 ocr"
        );

        // 用户收回权限：从清单里删掉 ocr。同一个实例、同一个 root。
        dir.write_manifest(r#"["screen"]"#);
        assert_eq!(
            gate.authorize(DeviceCapability::Ocr),
            Err(DeviceError::NotDeclared {
                capability: DeviceCapability::Ocr
            }),
            "改清单后下一次调用必须立刻拒绝（缓存结论会在这里变红）"
        );

        // 再写回来：立刻恢复放行（两个方向都证明是现读的）。
        dir.write_manifest(r#"["ocr"]"#);
        assert_eq!(gate.authorize(DeviceCapability::Ocr), Ok(()));
    }

    /// 清单坏 / 缺失 / 项目根不存在 → 一律 `NotDeclared`，且**不 panic**（fail-closed）。
    #[test]
    fn a_broken_or_missing_manifest_is_not_declared() {
        // ① JSON 坏。
        let corrupt = TempDir::new("corrupt");
        corrupt.write_manifest(r#"["ocr"]"#);
        std::fs::write(corrupt.path.join("hbot.json"), b"{ not json").expect("写坏清单");
        assert_eq!(
            gate(&corrupt).authorize(DeviceCapability::Ocr),
            Err(DeviceError::NotDeclared {
                capability: DeviceCapability::Ocr
            }),
            "JSON 坏不能当成声明过"
        );

        // ② 没有 hbot.json。
        let missing = TempDir::new("missing");
        assert_eq!(
            gate(&missing).authorize(DeviceCapability::Ocr),
            Err(DeviceError::NotDeclared {
                capability: DeviceCapability::Ocr
            }),
            "没清单不能当成声明过"
        );

        // ③ 清单语法对、契约不对（`sub` 里是没听说过的能力名）：`manifest::load`
        //    在解析阶段就报错，同样按未声明处理 —— 拼错的能力名不能静默变成别的能力。
        let unknown = TempDir::new("unknown");
        unknown.write_manifest(r#"["ocrr"]"#);
        assert_eq!(
            gate(&unknown).authorize(DeviceCapability::Ocr),
            Err(DeviceError::NotDeclared {
                capability: DeviceCapability::Ocr
            }),
            "不认识的能力名不能放行任何一项"
        );

        // ④ 项目根根本不存在（路径指向一个没建出来的目录）。
        let nowhere = TempDir::new("nowhere");
        let missing_root = ManifestCapabilityGate::new(nowhere.path.join("no-such-project"));
        assert_eq!(
            missing_root.authorize(DeviceCapability::Ocr),
            Err(DeviceError::NotDeclared {
                capability: DeviceCapability::Ocr
            })
        );
    }

    /// 五个能力名都按 `DeviceCapability::as_str()` 的拼写判定（`screen`/`ocr`/`input`/
    /// `window`/`clipboard`），不是一份本地映射表：清单里写全了，五个全放行。
    #[test]
    fn every_capability_name_matches_the_wire_spelling() {
        let dir = TempDir::new("all");
        dir.write_manifest(r#"["screen","ocr","input","window","clipboard"]"#);

        let gate = gate(&dir);
        for capability in DeviceCapability::all() {
            assert_eq!(
                gate.authorize(*capability),
                Ok(()),
                "`{}` 已在清单里声明，应放行",
                capability.as_str()
            );
        }
    }

    /// 声明的子能力集合与本模块 `declared_at` 的返回完全一致（登记表与闸共用这一份读取）。
    #[test]
    fn declared_at_returns_the_manifest_set() {
        let dir = TempDir::new("declared-at");
        dir.write_manifest(r#"["ocr","clipboard"]"#);

        let declared = declared_at(&dir.path);
        assert_eq!(declared.len(), 2);
        assert!(declared.contains(&DeviceCapability::Ocr));
        assert!(declared.contains(&DeviceCapability::Clipboard));

        // 空数组与"没写 device 块"等价：都是空集。
        dir.write_manifest("[]");
        assert!(declared_at(&dir.path).is_empty());
    }

    /// 网关级（第五条）：清单没声明 `ocr` 时 `read_screen` 在**能力步**就被拒 ——
    /// 不进参数校验、不进审批，更不会真去读屏；把 `ocr` 补进清单后，**同一个网关
    /// 实例**的下一次调用立刻改判（能力步放行，改由审批挡下来）。
    ///
    /// 这条钉的是"闸真接在网关上"：只测 `authorize` 只能证明闸本身对，证明不了
    /// `AgentManager::start_turn` 那条构造链装的是它、也证明不了网关第一步会调它。
    #[tokio::test]
    async fn the_gateway_refuses_read_screen_at_the_capability_step() {
        let dir = TempDir::new("gateway");
        dir.write_manifest(r#"["screen"]"#);

        // 与 `app/agent/mod.rs` 的构造链一致（只少了审批/编辑/命令三个端口，
        // 它们与设备能力无关）。
        let workspace = Arc::new(Workspace::open(dir.path.clone()).expect("打开工作区"));
        let registry = Arc::new(ToolRegistry::default_tools_with_allowed_programs(Vec::<
            String,
        >::new(
        )));
        let gateway = ToolGateway::new(
            registry,
            workspace,
            SandboxMode::WorkspaceWrite,
            Arc::new(ApprovalStore::new()),
            Arc::new(NullAudit),
        )
        .with_capability_gate(Arc::new(ManifestCapabilityGate::new(&dir.path)))
        .with_device(Arc::new(hbot_device::platform_backend()) as Arc<dyn DeviceBackend>);

        let cancel = CancellationToken::new();
        let session_id: SessionId = "00000000-0000-4000-8000-000000000001"
            .parse()
            .expect("会话 id");
        let call = || ToolCallRequest {
            id: "call-1".to_string(),
            name: "read_screen".to_string(),
            arguments: serde_json::json!({}),
        };

        let error = gateway
            .call(session_id, call(), &cancel, &DenyAllApprover)
            .await
            .expect_err("清单没声明 ocr 时不得读屏");
        assert_eq!(
            error.failed_at(),
            Some(GatewayStep::Capability),
            "失败点必须是能力步：{:?}",
            error.steps
        );
        assert!(
            !error.reached(GatewayStep::Approval),
            "能力步就拒了，不该走到审批：{:?}",
            error.steps
        );
        assert!(
            !error.reached(GatewayStep::Execute),
            "能力步就拒了，不该执行工具：{:?}",
            error.steps
        );
        let message = error.error.to_string();
        assert!(matches!(error.error, HbotError::Denied { .. }), "{message}");
        assert!(message.contains("项目没有声明能力 `ocr`"), "{message}");

        // 补上声明：同一个网关（闸与后端都没重建）下一次调用立刻改判。
        dir.write_manifest(r#"["screen","ocr"]"#);
        let error = gateway
            .call(session_id, call(), &cancel, &DenyAllApprover)
            .await
            .expect_err("审批出口一律拒绝，所以仍然失败");
        assert_eq!(
            error.failed_at(),
            Some(GatewayStep::Approval),
            "能力步已放行、挡在审批：{:?}",
            error.steps
        );
        let message = error.error.to_string();
        assert!(
            !message.contains("项目没有声明能力"),
            "清单改过后不该再报未声明：{message}"
        );
        assert!(message.contains("用户拒绝执行"), "{message}");
    }
}
