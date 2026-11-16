# 红队发现清单（RT 系列）

本文件记录 `crates/hbot-redteam` 红队用例集在 M4 验收中发现的缺陷，以及它们的处置状态。

* **为什么只记录不修**：红队单元的红线是「不许改实现 crate 的源码」。所有条目都已落成 `#[ignore]` 用例，
  变红即复现；修法写在这里，交给实现方（或后续批次）处置。
* **为什么每条都要有 `#[ignore]` 用例**：M4 的判据是「红队用例全部有自动化测试」。一条不落成测试的发现，
  下一轮就只剩这段文字；而一个长期红的测试会让整个套件失去信号。`#[ignore]` 是这两者的唯一交集：
  **默认不响，但只跑 `--ignored` 就能立刻复现**。
* **已修条目要转正**：修好之后去掉 `#[ignore]`、让用例进默认套件（否则修复本身没有回归网），
  条目里保留"修复前的现场"作为取证，并补一段"修法与证据"。
* **状态**：**全部已修**：RT-01、RT-02、RT-03、RT-05、RT-06、RT-07、RT-08（2026-11-16；RT-02 / RT-03 / RT-05
  见各条「修法与证据」）。RT-06 的 `AuditEvent` 仍保留 `#[derive(Debug)]`，但它唯一可能带密钥的字段是
  `Option<ApprovalKey>`，而 `ApprovalKey` 已在 RT-03 里改用手写 `Debug`（只打工具名/键名/类型/长度），
  因此不再泄露。每条末尾给出对应的 run 命令 / 用例名。

```powershell
$env:CARGO_TARGET_DIR='E:\hbot-target\redteam'   # 别用 D 盘：本机 D 盘满过一次
cargo test -p hbot-redteam                       # 默认套件：全绿（72 passed / 1 ignored）
cargo test -p hbot-redteam -- --ignored          # 只剩非安全的 b_helper（当前 0 条未修的安全用例）
```

> `b_helper` 不是安全用例，是本套件用子进程方式拉起自己的入口函数（`--exact b_helper --ignored`），
> 别人跑 `--ignored` 时它会显示 `ok`，属预期。

---

## 严重度口径

| 级别 | 含义 |
|---|---|
| 高 | 无需额外前提即可越权/隔离失效/密钥外泄到持久化面；红队上线前必修 |
| 中高 | 需要一条前置条件（畸形输入、特定配置、一次崩溃）即可造成拒绝服务或密钥进入日志 |
| 中 | 有防护但边界不全；利用需要组合条件，或只影响可观测面 |
| 低 | 信息泄漏/信息质量类，单独利用价值低，但会帮助攻击者侦察 |

---

## RT-01 路径不存在时，错误信息回显项目根的**绝对路径**

**严重度：低**（信息泄漏）　**状态：已修（2026-11-16）**

* **位置**（行号均指**修复前**）：`crates/hbot-workspace/src/project.rs:424-425`（`canonical_within`）、
  `crates/hbot-workspace/src/project.rs:466-468`（`io_error`）、`crates/hbot-workspace/src/project.rs:260`（`resolve_entry`）
* **复现**：`Workspace::open(temp)` 后
  `ws.resolve_existing(Path::new("missing/deep/file.txt"))`，得到的错误是
  ``not found: parent directory `\\?\C:\Users\<user>\AppData\Local\Temp\hbot-redteam-a20-<pid>-<n>\missing\deep` ``。
* **期望**：错误里只出现**调用方给的相对路径**（`missing/deep/file.txt`），或完全不出现路径。
* **实际**：`HbotError::not_found(what, path.display().to_string())` 里的 `path` 是
  `self.root.join(rel)` 或 `fs::canonicalize` 之后的**绝对路径**。`resolve_entry` / `canonical_within` /
  `io_error` 三条 NotFound 分支都是同一个写法。
* **影响**：泄漏操作系统用户名、项目目录名、临时目录布局。这类错误会经工具输出、RPC 响应、运行日志
  一路带给模型和用户；对红队而言是免费的侦察信息。同时违反「拒绝信息不得出现目标绝对路径」的验收要求。
* **最小修法建议**：workspace 层所有面向调用方的错误统一用**相对路径**显示。最省事的做法是给
  `Workspace` 加一个 `fn display_rel(&self, path: &Path) -> String`（`strip_root` 后回退到文件名），
  把上面三处的 `path.display().to_string()` 换掉（3 处，各 1 行）。`ensure_within` 的两个分支
  （`:438`、`:448`、`:453`）同理。
* **红队用例**（均已从 `#[ignore]` 转正进默认套件）：
  * `crates/hbot-redteam/tests/a_path_escape.rs::a20_拒绝信息不得泄漏项目根绝对路径`
    （表驱动覆盖 22 种恶意输入 × `resolve_existing`/`resolve_new`，另加 6 个真实入口
    ——读 / 写 / 建目录 / 列举 / 改名 / 删除）
  * `crates/hbot-redteam/tests/f_secret_leak.rs::f09_路径错误不得泄漏绝对路径`
    ——**这条证明泄漏不只发生在"诡异输入"上**：普通地
    `read_file("does-not-exist.txt")` 的错误链三层都带着
    `\\?\C:\Users\<用户名>\AppData\Local\Temp\<测试目录>\does-not-exist.txt`。任何一次拼错文件名
    都会把宿主路径送给模型。
* **修法与证据**（2026-11-16，已修）：
  * `crates/hbot-workspace/src/project.rs`：新增
    `pub(crate) fn display_rel(root: &Path, path: &Path) -> String` 与 `Workspace::display_path`
    ——`strip_root` 成功就回 `ui_path(rel)`（`/` 分隔、空串回 `"."`）；失败只回**文件名**，
    再失败回 `"<path>"`，**任何分支都不回显绝对路径**。`io_error` 的签名加了 `root`：
    `pub(crate) fn io_error(root: &Path, context: &str, path: &Path, err: io::Error) -> HbotError`。
    改写的构造点：`relative_path`、`resolve_new`（parent 目录 + 悬空符号链接两条）、
    `resolve_write_target`、`resolve_entry`（`not_found("path", …)`）、`resolve_new_dir`
    （canonical + `cannot resolve path` ×2）、`canonical_within`（not_found/denied/invalid 三条）、
    `ensure_within`（escapes 分支 + `..`/非相对路径分支）。
  * `crates/hbot-workspace/src/files.rs`：`read_file`（5 处文案 + 2 处 `io_error`）、`write_file`
    （3 处文案 + 2 处 `io_error`）、`list_dir`、`create_dir`（含 `is not a directory` 分支）、
    `rename`（destination/源/cross-device）、`delete_to_trash` 全部改用 `self.display_path(...)`；
    `write_atomic` 增加 `root` 参数，内部 3 处 `io_error` 一并相对化。
  * `io_error` 调用点共 **20 处**（`project.rs` 5、`files.rs` 15）全部换成带 `root` 的形态；
    「文件/目录不存在」类文案不再有第二份构造点残留（`grep 'path.display()'` 剩下的都在
    `tracing::debug!` 里，本机日志不进错误文案）。
  * **刻意保留的例外**：`open_with_config` 里"项目根不存在"的两条文案仍然显示调用方**自己传进来**的
    根路径 —— 这一步还没有可相对化的基准，而且那条路径本来就在调用方手里，不构成新的侦察信息。
    `tracing::debug!` 里的绝对路径同理保留（本机运维诊断，不出本机、不进模型上下文）。
  * 验收（2026-11-16 实测，全部 exit 0）：`cargo test -p hbot-rpc -p hbot-workspace -j 2`
    （workspace 58 passed、rpc 各测试二进制全绿）、`cargo test -p hbot-redteam -j 2`
    （A 面 20 passed / 0 ignored）、`cargo clippy -p hbot-rpc -p hbot-workspace --all-targets
    -j 2 -- -D warnings`（无告警）、`cargo fmt -p hbot-rpc -p hbot-workspace -- --check`。
  * **为什么不会再退化**：`a20` 与 `f09` 都在默认套件里，判据是
    `hbot_redteam::assert_no_path_leak`（同时比较根的原样 / 正斜杠 / `\\?\` 三种形态、
    `fs::canonicalize` 长名、以及目录叶子名），断言的是**行为**（文案里不许出现根路径），
    不是某个具体字符串。任何人再把 `path.display()` 塞回错误文案，这两条立刻变红，不需要谁记得这条约定。
  * 变异验证：把 `display_rel` 临时改成 `return path.display().to_string()`（模拟修复退回），
    `a20` 与 `f09` **双双 FAILED**（失败点 `crates/hbot-redteam/src/lib.rs:155` 的
    `assert_no_path_leak`，报错原文「错误信息泄漏了项目根的绝对路径
    `\\?\C:\Users\Administrator\AppData\Local\Temp\hbot-redteam-a20-*-0`」）；还原后两条都绿，
    `git diff --stat crates/hbot-workspace/src/project.rs` 与变异前一致（`185 insertions(+), 27 deletions(-)`）。

> **方法学教训（值得写进测试规范）**：这条第一次写的时候是**假绿**的。`%TEMP%` 在本机是 8.3 短名
> `C:\Users\ADMINI~1\...`，而工作区规范化出来的是长名 `\\?\C:\Users\Administrator\...`；
> 断言拿短名去 `contains` 长名，永远不命中。现在 `hbot_redteam::assert_no_path_leak` 同时比较
> 短名、`fs::canonicalize` 长名、`\\?\` 前缀形态，以及**目录叶子名**（短名/长名一致，且只可能出现在路径里）。
> 任何"断言某个字符串不出现"的用例都该按这个口径写。

---

## RT-02 工具参数校验不查 `enum` / `minimum` / `maximum`

**严重度：中**　**状态：已修（2026-11-16）**

* **位置**：`crates/hbot-agent/src/tools/mod.rs:301`（`validate_args`）、
  `crates/hbot-agent/src/tools/mod.rs:344`（`matches_type`）
* **复现**：schema 声明 `{ "mode": {"type":"string","enum":["read","write"]},
  "count": {"type":"integer","minimum":1,"maximum":3} }`，然后
  `validate_args("probe", &schema, &json!({"mode":"delete","count":99}))` → **`Ok(())`**。
  `count: 0` 同样放行。
* **期望**：`enum` 越界、低于 `minimum`、高于 `maximum` 都在**执行前**被拒（`HbotError::invalid`）。
* **实际**：只校验「必填参数在不在」和「JSON 基础类型对不对」（`required` + `type` +
  `additionalProperties`）。`enum` / `minimum` / `maximum` / `minLength` / `maxLength` / `pattern`
  一个都没查；`matches_type` 遇到未知类型声明还会一律放行。
* **影响**：越界参数会一路进到工具实现里。多数工具实现不会二次校验，于是「审批卡上看到的动作」与
  「实际执行的动作」可能不一致：`mode` 命中不了任何分支时实现走 fallback、`count` 被当成循环次数、
  `max_bytes` 类参数被放大到无界。审批人看到的是一个合法 schema 声明，却批准了一个 schema 之外的动作。
* **最小修法建议**：在 `validate_args` 的类型检查之后补 4 个分支（`enum` 数组成员相等、
  `minimum`/`maximum` 用 `as_f64` 比较、字符串 `minLength`/`maxLength` 按 `chars().count()`），
  约 40 行；语义按 JSON Schema draft-07 子集即可，不必引依赖。
  （2026-11-16 只补了 `enum` / `minimum` / `maximum` 三项；`minLength` / `maxLength` / `pattern`
  仍未校验，将来要补仍落在这个函数里，别另开第二条闸。）
* **红队用例**（均已从 `#[ignore]` 转正进默认套件）：
  * `crates/hbot-redteam/tests/d_approval_bypass.rs::d09_enum与上下限必须在执行前拒`
  * `crates/hbot-redteam/tests/d_approval_bypass.rs::d10_越界参数不得进入工具执行`
    （后者用一个自定义 `EnumTool` 证明**越界值真的到了 `run` 里面**，不只是错误文案问题）
* **修法与证据**（2026-11-16，已修）：
  * `crates/hbot-agent/src/tools/mod.rs:305` 的 `validate_args`：在**同一个循环里**、`type` 检查之后加一句
    `check_value_constraints(tool_name, key, property, value)?;`（`:336`）。网关仍然只有这一处参数闸 ——
    `crates/hbot-agent/src/gateway.rs:404` 的调用点一个字没动，没有新增第二条校验路径。
  * 新增 `fn check_value_constraints`（`:360`）：
    * `enum`：`choices.iter().any(|choice| choice == value)`，按 **JSON 值**判等，不做隐式类型转换
      （`"1"` 不等于 `1`），字符串与数值都生效；
    * `minimum` / `maximum`：`and_then(Value::as_f64)` 取值，**只对数值生效**（非数值交给 `type` 判定，
      这里不重复报错）；
    * 非有限数值（`NaN` / `Infinity`）一律拒：`value.as_f64().filter(|n| n.is_finite())`，即使将来打开
      `serde_json/arbitrary_precision` 也过不去；
    * 错误全部走 `HbotError::invalid` → **-32002**，文案带字段名与被拒的值，
      值回显截 `MAX_ECHO_CHARS = 120` 字符（`echo_rejected`，`:413`/`:416`），
      不会把整段消息正文抄进日志；`truncate_chars`（`:425`）按字符截断（不是字节）。
  * 单测 5 条（`crates/hbot-agent/src/tools/mod.rs:553` 起，紧挨既有 `validate_*` 用例）：
    `validate_rejects_value_outside_enum`、`validate_accepts_enum_choice_for_string_and_number`、
    `validate_rejects_number_outside_bounds`、`validate_accepts_number_on_bounds`、
    `validate_bounds_only_apply_to_numbers`。
  * 验收（2026-11-16 实测，均 exit 0）：`cargo test -p hbot-agent -j 2` → `155 passed; 0 failed`；
    `cargo clippy -p hbot-agent --all-targets -j 2 -- -D warnings` 无告警；
    `cargo fmt -p hbot-agent -- --check` 通过；`cargo test -p hbot-redteam` →
    `69 passed; 0 failed; 3 ignored`（当时 `f03`/`f04` 仍属 RT-06 的忽略用例），
    其中 `d09`、`d10` 都是 `ok`（D 面 **11 passed / 0 ignored**）；RT-06 批次随后转正
    `f03`/`f04`，最新实测 `71 passed; 0 failed; 1 ignored`（只剩非安全的 `b_helper`）。
  * **为什么不会再退化**：`d09` / `d10` 已进默认套件；`d10` 断言的是
    `error.failed_at() == Some(GatewayStep::ValidateArgs)` —— 失败必须发生在**执行前**，
    而不只是"错误文案里出现 mode"。谁把值域校验挪到工具实现里、或漏掉某一侧，
    这条会在 `ValidateArgs` 之前/之后的位置上直接红。
  * 变异验证：把上面那句 `check_value_constraints(...)` 调用临时删掉（模拟修复退回）→
    `cargo test -p hbot-redteam --test d_approval_bypass` **exit 101**：
    `d09` FAILED（`d_approval_bypass.rs:442`）、`d10` FAILED（`:515`），正对照 `d08` 仍 `ok`；
    把调用加回去后同命令 `11 passed; 0 failed`。
  * **覆盖面的实话（这条修复今天没有工具直接受益）**：当前 8 个生产 `impl Tool`
    （`read_file` / `write_file` / `edit_file` / `list_dir` / `create_dir` / `search` /
    `run_command` / `http_fetch`）的 schema **一个都没声明** `enum` / `minimum` / `maximum`
    （全仓只有红队测试自己造的两处 schema 用到）。所以这是**给后续工具准备的地基**，
    不是修好了一个正在被踩的坑。天然的第一批用户已经现成：
    `read_file.max_bytes`（现在只在 `description` 里写"默认 256 KiB，上限 2 MiB"）、
    `search.max_results`（工具里 `.clamp(1, DEFAULT_MAX_RESULTS)` 自校验）、
    `run_command.timeout_ms`（`parse_timeout` 自校验）。把边界写进 schema 后，
    越界值会在**执行前**被网关拒，而不是进到工具实现里再兜 —— 这正是"审批卡上看到的动作"
    与"实际执行的动作"一致性所依赖的那道闸。

---

## RT-03 审批被拒时，错误文案回显**动作参数原文**

**严重度：中**　**状态：已修（2026-11-16）**

* **位置**：`crates/hbot-agent/src/approvals.rs:112`（`ApprovalKey::describe`）、
  `crates/hbot-agent/src/gateway.rs:446`（策略拒绝）、`crates/hbot-agent/src/gateway.rs:465`（用户拒绝）
* **复现**：`write_file` 的 `content` 里带 `API_KEY=sk-test-DEADBEEF`，审批人拒绝 →
  `GatewayError::describe()` 里出现
  `用户拒绝执行：write_file {"content":"API_KEY=sk-test-DEADBEEF",…} [workspace_write]`。
* **期望**：拒绝文案只说明**哪个工具、什么原因、什么沙箱档**，不回显参数值。
* **实际**：`describe()` 是 `format!("{tool} {canonical_args} [{sandbox}]")`，`canonical_args`
  是完整 JSON（截 300 字符）。错误会进 UI、RPC 响应、运行日志，模型也会读到。
* **影响**：被拒的动作恰恰是"模型想做但没被批准"的那一个 —— 里面可能正好是它刚刚从环境变量/文件里
  读到的密钥，或者用户隐私文本。拒绝路径反而成了泄漏路径。此外它还把完整动作写进日志，
  与 `hbot-rpc` 的 `TracingAudit`（`crates/hbot-rpc/src/app/agent/mod.rs:679`，注释明写「刻意不打印
  `event.key` 的规范化参数」）是同一类风险的两种结果。
* **最小修法建议**：`describe()` 改成 `format!("{tool}（参数：{键名列表}）[{sandbox}]")`，
  只列键名不列值；若要保留可观测性，值统一截断到 16 字符并替换成 `<redacted>`。
  `ApprovalKey` 的 `Ord/Eq/Hash` 语义不受影响（它们用的是 `canonical_args` 字段本身，不是 `describe`）。
* **红队用例**：`crates/hbot-redteam/tests/f_secret_leak.rs::f08_审批拒绝文案不得回显参数原文`
  （已从 `#[ignore]` 转正进默认套件）
* **修法与证据**（2026-11-16，已修）：
  * `crates/hbot-agent/src/approvals.rs:235` 的 `ApprovalKey::describe()` 改成
    `format!("{} {} [{}]", tool, summarize_args(&canonical_args), sandbox)` ——
    **只出「哪个工具、哪些参数、值有多长」**，参数值一个字都不出。
    拒绝文案现在长这样：
    `用户拒绝执行：write_file content: string(24 字符), path: string(9 字符) [workspace_write]`。
  * 新增 `fn summarize_args`（`:73`）：把 `canonical_args` 反序列化后逐键输出 `key: <摘要>`；
    空对象回 `无参数`；**解析失败整串替换成 `<参数不可解析，已隐藏>`**（宁可少信息也不冒险原样输出）。
    摘要按 JSON 类型给：`string(N 字符)` / `number(N 位)` / `array(N 项)` / `object(N 键)` /
    boolean / null（`fn summarize_value`，`:96`）—— 永远不出现值本身。
  * 密钥类键名一律 `<redacted>`：`fn is_secret_key`（`:112`）先把键名按非字母数字切词、
    再按驼峰切一次（`apiKey` / `accessToken` / `clientSecret` / `X-Api-Key` / `API_KEY` / `api_key`
    都命中），词表 `SECRET_WORDS`（`token` / `secret` / `key` / `password` / `passwd` /
    `authorization` / `credential`，`:113`）+ `SECRET_COMPOUNDS`（`apikey` / `accesskey` /
    `privatekey` / `secretkey` / `clientsecret` / `accesstoken` / `refreshtoken` / `bearertoken` …，`:122`）。
    反向不误伤：`monkey` / `keyboard` / `tokenizer` / `path` / `content` / `query` 保持原样（有单测钉住）。
  * 总长仍是 `truncate_chars(…, MAX_DESCRIBE_ARGS_CHARS = 300)`（`:66`），
    原有 `describe_truncates_long_args` 仍绿。
  * **同一批把 `ApprovalKey` 的 `Debug` 也改成手写脱敏**（`:183`，`derive` 里的 `Debug` 去掉）：
    只改 `describe()` **不够**。`f08` 的第二条断言走 `hbot_redteam::error_chain`，它会
    `format!("{item:?}")`，而 `GatewayError` 是 `#[derive(Debug)]` 且含
    `pub key: Option<ApprovalKey>` —— 派生 `Debug` 会把 `canonical_args` 原样打出来。
    手写 `impl std::fmt::Debug for ApprovalKey` 复用 `summarize_args`，这条泄漏路径才算堵住
    （`Clone` / `PartialEq` / `Eq` / `PartialOrd` / `Ord` / `Hash` 仍 derive，审批键的比较与哈希语义不变）。
  * 单测 6 条（`crates/hbot-agent/src/approvals.rs:530` 起，原 `describe_truncates_long_args` 保留）：
    `describe_never_echoes_argument_values`（`sk-test-DEADBEEF` 与 `notes.txt` 都不出现，
    但 `path` / `content` / `string(` 必须在）、`describe_redacts_secret_key_names`（4 个密钥键打码，
    键名仍可见）、`describe_does_not_redact_innocent_names`、`debug_never_echoes_argument_values`、
    `describe_hides_unparseable_canonical_args`、`secret_key_names_cover_underscore_and_camel_case`
    （下划线 / 驼峰 / 全大写 / 连字符 12 种写法 + 6 种双关词反向断言）。
  * 验收（2026-11-16 实测，exit 0）：`cargo test -p hbot-agent -j 2` → `155 passed; 0 failed`
    （含上面 6 条）；`cargo test -p hbot-redteam` → `69 passed; 0 failed; 3 ignored`，
    `f08` 为 `ok`（当时 F 面 **8 passed / 2 ignored**，那两条是 RT-06 的 `f03`、`f04`，
    随后已由 RT-06 批次转正，最新实测 F 面 **10 passed / 0 ignored**）。
  * **为什么不会再退化**：`f08` 已进默认套件，它同时断言**两条路径** —— `error.describe()`
    与 `error_chain(&error)`（后者含 `{item:?}` 的 `Debug` 路线）。只回退 `describe()` 或只回退
    `Debug`，两条断言各自会红（下一条变异验证的两次都证明了这一点）。
  * 变异验证（各做一次，均 `exit 101`，随后完全还原）：
    1. 把 `describe()` 里 `summarize_args(&self.canonical_args)` 换回
       `truncate_chars(&self.canonical_args, MAX_DESCRIBE_ARGS_CHARS)` →
       `cargo test -p hbot-redteam --test f_secret_leak` **exit 101**，`f08` FAILED，
       失败点 `f_secret_leak.rs:28` 的 `assert_no_secret`，报错原文是「审批拒绝文案 里出现了密钥原文：
       … write_file {"content":"API_KEY=sk-test-DEADBEEF",…} …」。
    2. 把 `Debug` 里的 `.field("args", &summarize_args(...))` 换回
       `.field("args", &self.canonical_args)` →
       `cargo test -p hbot-redteam --test f_secret_leak` **exit 101**，`f08` FAILED，
       这次红在「网关错误链」那条断言，泄漏出现在
       `key: Some(ApprovalKey { …, args: "{\"content\":\"API_KEY=sk-test-DEADBEEF\",…}" })`。
    3. 两次改动都还原后：`f_secret_leak` → `8 passed; 0 failed; 2 ignored`，
       `d_approval_bypass` → `11 passed; 0 failed`，`git status --short` 里
       `crates/hbot-agent/src/{approvals.rs,tools/mod.rs}` 与我备份的副本逐字节一致
       （备份在 `E:\hbot-target\secb\backup\`，比对命令见报告）。

---

## RT-04 （编号空缺）

首轮勘察时预留的编号，候选项实测**防护成立**（命令超时确实杀整棵进程树，见
`b_command_injection.rs::b04_超时必须杀掉整棵进程树`），未形成缺陷条目。
编号不复用，避免与已有用例里的 `RT-0x` 引用错位。

---

## RT-05 允许清单里放 `cmd` / `powershell` 等于把 shell 装回来

**严重度：中**（配置面风险，非代码缺陷）　**状态：已修（2026-11-16）**

* **位置（修复前）**：`crates/hbot-agent/src/tools/shell.rs:77-160`（`RunCommandTool::with_allowed_programs`）、
  `crates/hbot-command/src/program.rs`（白名单解析，允许绝对路径条目）；配置入口在
  `crates/hbot-rpc/src/app/agent/config.rs`（`HBOT_COMMAND_ALLOW` / `agent.json` 的
  `allowedPrograms`，经 `SandboxConfig::resolve_with` 解析）。
* **复现（修复前的现场）**：`RunCommandTool::with_allowed_programs(["cmd"])` + `danger` 档，命令
  `echo PWNED>marker.txt` → `marker.txt` **真的被写出来了**。这条现场保留为可执行证据：
  `b03_允许cmd等于把shell装回来自证`（**故意断言"能跑通"**、不是 `#[ignore]`）。它钉的是**命令层**
  的事实 —— `hbot-command` 只回答"这份清单里有没有这个名字"，不判断名字危险不危险。
* **期望**：白名单只放行"某一类程序"，不放行"解释器"。
* **实际（修复前）**：实现是 **argv 直传**（不经 shell），所以 `echo hi; rm -rf /` 里的 `;` 是普通字符 ——
  这一点是对的（`b02` 证明）。但 `cmd /c "<任意字符串>"` 的 `/c` 之后**就是 shell**，
  于是"允许 cmd"等价于"允许任意命令"。同理 `powershell`/`pwsh`/`bash`/`sh`/`wscript`/`cscript`/
  `mshta`/`rundll32`/`node -e`。当时**没有**在配置解析处做拦截，也没有在文档里警告；这是
  "看起来像收紧、实际等于放开"的典型陷阱。

### 修法与证据（2026-11-16）

* **决策：默认拒绝，不采用"只告警"**。理由：只告警等于把一个 RCE 面留在默认配置里，而日志在生产
  里最容易被忽略；拒绝是 fail-closed，且错误在 `agent_send` 第一回合就以 `-32002` 回到 UI
  （`HbotError::invalid`），用户当场知道要改哪一行。逃生阀保留为环境变量 `HBOT_ALLOW_SHELL`：
  只认 `1` / `true` / `yes` / `on`（大小写不敏感），其余值（含 `0`、空串、拼错）**一律算关**；
  打开时**放行并保留名字**（不删条目，命令层要拿完整清单），同时 `tracing::warn!` ——
  **每进程每条清单只告警一次**（`agent.json` 每回合都会被重读，不去重就是一回合一条）。
* **判据在配置解析层**：`crates/hbot-rpc/src/app/agent/config.rs` 的 `SandboxConfig::resolve_with`
  （环境变量 `HBOT_COMMAND_ALLOW` 与 `agent.json` 的 `allowedPrograms` 都经过它）。
  `normalize_program_name` 四步：取 **basename**（按 `/` 与 `\`）→ 去首尾空白 → 去尾部点 →
  `to_ascii_lowercase` → 去**最后一个扩展名**；然后与 `INTERPRETER_PROGRAMS` **精确**比较，
  **不做子串匹配**。所以全路径 `C:\Windows\System32\WindowsPowerShell\v1.0\powershell.exe`、
  `CMD.EXE`、`cmd.` 都会命中；而 `ssh`（SSH 客户端）、`shellcheck`（对 shell 脚本做静态检查、
  不执行参数）、`bashful-tool`、`node_modules` 都不命中 —— **`ssh` 明确不拦、`shellcheck`
  也明确不拦**，它们只做自己被指定的那件事。
* **名单**：`cmd`、`conhost`、`powershell`、`pwsh`、`sh`、`bash`、`zsh`、`fish`、`dash`、`ksh`、
  `csh`、`tcsh`、`wsl`、`cscript`、`wscript`、`mshta`、`rundll32`、`regsvr32`、`python`、`python3`、
  `py`、`node`、`nodejs`、`deno`、`bun`、`ruby`、`perl`、`php`、`java`、`osascript`；
  另加 `python` + 纯数字/点（覆盖 `python311` / `python3.11` 这类带版本号的可执行名）。
* **为什么不改 `hbot-command`**：那一层只认"这份清单里有没有这个名字"，不该知道"哪些名字危险"
  （`docs/adr/0003-command-isolation.md` §5）。所以命令层行为不变，`b03` 仍然成立。
* **证据（可直接跑）**：
  * `crates/hbot-rpc/tests/config_warnings.rs::interpreter_in_allowlist_is_rejected_with_32002_and_a_warn_free_path`
    —— 拒绝 + `-32002` + 逐个回显被拒名字 + 拒绝路径不打 warn；
  * `crates/hbot-rpc/tests/config_warnings.rs::shell_hatch_warns_once_per_process_when_it_lets_interpreters_through`
    —— 逃生阀放行 + 名字保留 + **同进程两次解析只留一条 WARN**；
  * `crates/hbot-rpc/src/app/agent/config.rs` 单测：`interpreter_names_normalise_to_a_bare_basename`、
    `windows_full_path_interpreter_is_recognised_from_the_env_list`、
    `windows_full_path_with_spaces_is_recognised_from_agent_json`、
    `interpreter_matching_is_case_insensitive_and_extension_agnostic`、
    `concrete_programs_are_not_flagged`（`ssh`/`shellcheck`/`bashful-tool`/`node_modules` 反向）、
    `escape_hatch_keeps_the_names_and_only_accepts_explicit_truthy_values`；
  * 红队：`crates/hbot-redteam/tests/b_command_injection.rs::b10_配置层必须拒绝解释器类允许清单`。
* **没修的部分（如实写）**：这是一张**判据表**，不是完备集合。`env` / `busybox` / `xargs` /
  `npm exec` 这类"给个参数就能起别的程序"的启动器不在表里 —— 它们也是正常开发工具，逐个拉黑
  会把白名单变成黑名单的补集；这一层靠"清单只放具体程序"的纪律（ADR-0003 §5）与 `b03` 的可执行
  证据兜。仍然是一个**产品决策**：是否允许清单整体出现"运行时/包管理器"（`npm`、`pnpm`、`uv`）
  需要产品明确，见 `docs/02-architecture.md` §11.2 与下方"仍需人工决定"。
* **当时的建议 vs 实际**：原建议是在 `hbot-command::resolve_allowed_entry` 里维护黑名单；实际修法
  改到了更上层（配置解析），因为"哪些名字危险"是策略与配置解释，不属于"清单里有没有这个名字"
  的实现层（ADR-0003 §5）。

### 仍需人工决定

* 用户**确实要跑 `python` / `node`** 时的正确姿势：默认答案不是把它们塞进 `allowedPrograms`，而是
  ① 把要跑的东西固化成具体程序或项目脚本（例如编译产物、`cargo` 子命令、已安装的 CLI）；
  ② 确实需要解释器时，显式设 `HBOT_ALLOW_SHELL=1` 启动进程 —— 逃生阀会 `warn` 一次，让这件事在
  日志里可见；③ 不接受解释器又必须跑脚本的场景，应由产品层提供"固定参数的内置工具"，而不是
  放开整个解释器。是否要为此加一个 per-program 的显式开关（而不是一个全局布尔），留给后续产品决策。

---

## RT-06 四个核心类型的 `Debug` 会把密钥 / 入站原文打出来

**严重度：中**　**状态：已修（2026-11-16）**

* **位置**（修复前都是 `#[derive(Debug)]`）：
  * `crates/hbot-core/src/command.rs:79` `CommandSpec`（`env: Vec<(String,String)>`、`args` 里的 `--token=…`）
  * `crates/hbot-core/src/domain.rs:155` `InboundMessage`（`payload: Value` 原文）；同文件的 `ToolCall`（`args` / `result`）一并修了
  * `crates/hbot-agent/src/provider/mod.rs:144` `ToolCallRequest`（`arguments: Value`）
  * `crates/hbot-adapters/src/normalize.rs` `NormalizedInbound`（`raw: Value`，适配器层的入站原始事件）
  * `crates/hbot-agent/src/gateway.rs:129` `AuditEvent` —— **本轮仍未改**（写域之外，见「残余风险」）
* **复现**（修复前）：`format!("{:?}", spec)` 出现 `env: [("API_TOKEN", "sk-live-1234")]`、
  `args: ["--token=sk-live-1234"]`；`format!("{:?}", inbound)` 出现 payload 里的
  `"Authorization": "QRBT abc"` 与 `message_scene.ext` 的 `auth_token=xyz`；`format!("{:?}", call)` 出现完整 `arguments`。
* **影响**：这些 `Debug` 会经 `tracing::warn!(?spec)`、`thiserror` 的 `#[source]` 链、以及工具输出进入
  **持久化日志**（`logs` 表 / tracing 文件）。用户一旦在 `agent.json` 配了 key，或入站事件带
  `Authorization` 头（QQ 官方 `message_scene.ext` 里就有 `auth_token=`），密钥就永久落库；
  `InboundMessage` 还会把**整条入站原文**复制到每一处打印它的地方。
* **修复**：新增**唯一一份判据** `crates/hbot-core/src/redact.rs`，五处手写 `Debug` 全部经它打印
  （`CommandSpec` / `InboundMessage` / `ToolCall` / `ToolCallRequest` / `NormalizedInbound`）：
  * `redact_arg(&str) -> String`：文本级（`--token=…`、`Authorization: Bearer …`、`api_key=…`、`"password":"…"`）
  * `redact_value(&Value) -> Value`：JSON 递归（嵌套对象 / 数组里的键名命中即**整值** `<redacted>`）
  * `redact_args(&[String])`：`--api-key <value>` 这种**分开写**的 argv（下一个元素也抹）
  * `RedactedArgs` / `RedactedEnv` / `RedactedJson`：给手写 `Debug` 用的包装，**字段名全保留**、非敏感值原样
  * `REDACTED = "<redacted>"`、`is_secret_key(&str)`、`redact_text(&str)`（= `redact_arg` + 裸 32+ 位 hex 抹除，日志侧用）
* **判据矩阵**（`is_secret_key`，切词后**精确命中**，不是 `contains`）：
  | 输入 | 结果 | 判据 |
  |---|---|---|
  | `token` / `auth_token` / `access_token` / `X-Auth-Token` / `access-token` | 抹 | 词 `token` |
  | `api_key` / `apikey` / `OPENAI_API_KEY` / `clientSecret` / `secretKey` / `private-key` | 抹 | 词 `apikey` / `key` / `secret` |
  | `secret` / `client_secret` / `password` / `passwd` / `db-password` | 抹 | 词 `secret` / `password` / `passwd` |
  | `authorization` / `Authorization` / `X-Authorization` | 抹 | 词 `authorization` |
  | `credential(s)` / `cookie(s)` / `passphrase` | 抹 | 词表命中 |
  | `max_tokens` / `prompt_tokens` / `token_count` / `max_keys` | **不抹** | 凭据词被计量词（`max`/`min`/`limit`/`count`/`prompt`/`size`…）修饰 → 是限额 |
  | `tokenizer` / `monkey` / `keyboard` / `keywords` | **不抹** | 切词后是**一个词**，与 `token`/`key` 不相等 |
* **为什么 `max_tokens` 不能抹**：它是数字限额（`CompletionRequest.max_tokens`、tracing 里的用量日志），
  抹掉会让"为什么这次请求被截断 / 花了多少 token"这类调参日志直接失效，而它本身**不装密钥**。
  上一份实现（`hbot-rpc` 的 `contains` 判据）会把 `max_tokens` 判成密钥 —— 这是本轮的刻意分歧，见下。
* **注意"递归"不等于"阉割"**：payload 只是把**命中的值**换成 `<redacted>`，键名、非敏感字段、数字、
  普通字符串全部原样保留；没有一处把整个 payload 打成 `<redacted>`（那样日志就没有排查价值了）。
* **两份实现（应该合并）**：现在有**两份**脱敏实现 —— `crates/hbot-core/src/redact.rs`（本轮新增的通用版，
  `Debug` / JSON 用）与 `crates/hbot-rpc/src/app/agent/log.rs`（原有的日志版，`describe_arguments` /
  `redact_json` / `redact_text` 用）。**应该合并成一份（留 core 的），但 `crates/hbot-rpc/**` 不归本轮写域**，
  所以没动它。已知差异（合并时以 core 版为准）：
  ① rpc 版用 `contains` 判键名，会误抹 `max_tokens`；② rpc 版不认 `--token value` 分开写、也不认驼峰 `clientSecret`。
  此外还有**第三份**临时实现：`crates/hbot-adapters/src/platforms/qq/normalize.rs` 的 `redact_auth_token`
  （QQ 批自带，只抹 `auth_token=`），同样应合并过来。
* **测试**（新增 11 条 + 红队 2 条转正）：
  * `crates/hbot-core/tests/debug_redaction.rs`：`debug_of_command_spec_never_prints_secrets`、
    `debug_of_inbound_redacts_authorization`、`ordinary_words_containing_key_are_not_redacted`、
    `debug_of_domain_tool_call_redacts_args`、`sensitive_key_variants_are_recognised`、
    `redact_value_recurses_into_arrays_and_objects`
  * `crates/hbot-agent/tests/debug_redaction.rs`：`debug_of_tool_call_redacts_nested_arguments`、
    `debug_of_tool_call_redacts_flat_credential_keys`、`debug_of_tool_call_keeps_ordinary_words`
  * `crates/hbot-adapters/tests/debug_redaction.rs`：`debug_of_normalized_inbound_redacts_raw_credentials`、
    `debug_of_normalized_inbound_keeps_ordinary_words`
  * `crates/hbot-redteam/tests/f_secret_leak.rs::f03_command_spec的debug输出必须脱敏` 与
    `::f04_inbound_message的debug输出必须脱敏` —— **去掉了 `#[ignore]`**，现为默认套件（10 passed / 0 ignored）
* **变异验证**：把 `CommandSpec` 临时换回 `#[derive(Debug)]`（同时删掉手写 impl）后
  `cargo test -j 2 -p hbot-core --test debug_redaction debug_of_command_spec_never_prints_secrets` → **FAILED**：
  ```
  thread 'debug_of_command_spec_never_prints_secrets' panicked at crates\hbot-core\tests\debug_redaction.rs:48:5:
  CommandSpec 的 Debug 泄漏了密钥：CommandSpec { program: "cmd", args: ["--token=sk-live-1234", "--verbose", "status"],
  cwd: "", ..., env: [("API_TOKEN", "sk-live-1234"), ("RUST_LOG", "info")], ... }
  ```
  还原后同一条 → `ok`（6 passed），`git status --porcelain -- crates/hbot-core crates/hbot-agent
  crates/hbot-adapters crates/hbot-redteam docs/security` 为空。
  坑：`Copy-Item` 还原会保留备份的旧 mtime，cargo 直接复用变异产物（第一次还原后仍 FAILED 即是此因），必须 touch 文件再跑。
* **验收**（2026-11-16，`CARGO_TARGET_DIR=E:\hbot-target\rt06`）：
  ```
  cargo test   -j 2 -p hbot-core -p hbot-agent -p hbot-adapters -p hbot-redteam → exit 0
  cargo clippy -j 2 -p hbot-core -p hbot-agent -p hbot-adapters --all-targets -- -D warnings → exit 0
  cargo fmt    -p hbot-core -p hbot-agent -p hbot-adapters -- --check → exit 0
  cargo test   -j 2 -p hbot-rpc --lib → exit 0（193 passed）
  ```
* **残余风险**（仍是会装密钥的派生 `Debug`，本轮不修，仅登记）：
  * `crates/hbot-agent/src/gateway.rs:129` `AuditEvent`（`key: Option<ApprovalKey>`；`ApprovalKey` 自己的 `Debug` 已脱敏，故只**间接**安全）
  * `crates/hbot-core/src/domain.rs` 的 `Bot`（`config: Value`，里面可能就是 `api_key`）、`OutboundMessage`（`payload: Value`）
  * `crates/hbot-agent/src/provider/mod.rs` 的 `ChatMessage`（含 `tool_calls`，现已间接安全）、`Completion`、`ToolSpec`（`parameters: Value`）
  * `crates/hbot-adapters/src/platforms/*` 各 DTO（OneBot / QQ 原始事件结构体里的 `Value` 字段）
  * 任何第三方 crate 对这五类类型的 `#[derive(Debug)]` 再包装

---

## RT-07 handler 一旦 panic，那个会话的 worker 就死了，会话**永久**不可用

**严重度：中高**　**状态：已修（2026-11-16）**

* **位置**：`crates/hbot-bot/src/queue.rs` 的 `run_worker`（修复前 `:538-601`，修复后从 `:568` 起）
* **修复前的现场**（行号均指修复前）：
  * 关键行 `:561` `let outcome = shared.handler.on_message(ctx).await;` —— handler 直接 await，
    一 panic，展开冲出 worker 任务，`JoinHandle` 落成**没人 await** 的 `Err(JoinError::panic)`；
  * `:583-584` `depth.fetch_sub(1)` / `shared.total.fetch_sub(1)` 因展开**永远不执行**。
* **复现**：`SessionQueues::new(...)` 的 handler 在**第一条**消息上 panic（模拟插件里一次 `unwrap` 踩空），
  之后：
  1. 该会话 `depth` 永远 ≥ 1（`:583` 不执行）；
  2. `total` 同样不归还，`wait_drained()` 永不返回；
  3. worker 的 `Receiver` 随任务一起被丢弃 → 之后每次 `try_enqueue` 都在
     `crates/hbot-bot/src/queue.rs` 的 `tx.try_send` 错误分支拿到 `QueueError::Closed`
     （→ `HbotError::Conflict`）；
  4. 日志里**一行都没有**（panic 走 panic hook，`tracing` 拿不到）。
* **期望**：一次崩溃被隔离在**一条消息**上，与该会话下一条消息无关；失败要被记账并留日志。
* **实际（修复前）**：会话级永久失效，重启进程才恢复。
* **影响**：会话级永久 DoS。触发条件只是一条能让 handler panic 的畸形消息 —— 而 handler 处理的是
  **不可信入站内容**（这正是 C 面提示注入的载体），"输入能触发 panic"在插件代码里是常态而非例外。
  跨 bot 隔离仍然成立（`e07` 通过：另一个 bot 的队列正常排空），所以这不是"整个宿主挂掉"，
  而是"一个用户从此收不到回复，且运维看不到任何原因"。
* **修法与证据**（2026-11-16）：
  * `run_worker` 改成 `AssertUnwindSafe(handler.on_message(ctx)).catch_unwind().await`（`futures-util`：
    `:595-600`），**只 catch、不回滚**——handler 内部共享状态在 panic 后可能不一致，回滚只能由 handler
    自己实现，所以这里也不丢弃会话重建（那会把"没处理"变成"静默丢失"）。
  * 名额归还全部搬进 RAII 收尾：`:594` 拿到许可后立刻 `InFlightSlot::new(...)`，`:633` 显式 `drop(slot)`；
    `InFlightSlot`（`:644`）的 `Drop` 依次 `drop(permit)` → `depth.fetch_sub` → `total.fetch_sub` →
    `last_active.store`（D7）→ `notify_waiters`（D6），与修复前顺序一致。展开路径与正常路径走同一条
    收尾，不再依赖"panic 只在 handler 里发生"的侥幸。
  * panic 计入失败：`:618` 的 `Err(payload)` 分支调 `record_failure()`（所以连续失败降级对 panic 同样生效，
    降级日志与 `Err` 路径共用 `log_degraded`），并记一条
    `tracing::error!(bot, conversation, consecutive_failures, panic, "handler panicked; session worker survives and keeps draining the queue")`；
    payload 由 `panic_description`（`:700`）downcast 成 `&str` / `String` 打印，否则打印
    `<non-string panic payload>`。日志**不含**消息正文或 payload 里的入站内容。
  * 回归网：`crates/hbot-bot/tests/worker_panic.rs` 四条（`a_panicking_handler_does_not_kill_the_session`、
    `a_panicking_handler_returns_the_session_slot`、`a_panicking_handler_counts_as_a_failure`、
    `panic_does_not_log_message_content`）；红队 `e08` 去掉 `#[ignore]` 转正。
  * 变异验证：把 `queue.rs` 临时换回修复前版本，四条新用例**全红**（第 1 条红在
    `a_panicking_handler_does_not_kill_the_session` 的 `try_enqueue` 断言，实际拿到
    `Err(Closed{..})`；第 2/3/4 条红在 `tests/common/mod.rs` 的 `wait_until` 5s 超时——
    `depth` 卡在 1、永不降级、一行日志都没有），还原后全绿。
* **红队用例**：`crates/hbot-redteam/tests/e_resource_exhaustion.rs::e08_同会话里一次handler崩溃不得让该会话永久失效`

---

## RT-08 `redact_text` 漏抹**下划线连接**的键名（`api_key=`、`access_token=`）

**严重度：中高**　**状态：已修（2026-11-16）**

* **位置**（行号均指**修复前**）：`crates/hbot-rpc/src/app/agent/log.rs:286-368`（`redact_named_values`），
  边界判定在 `:302-311`：
  `let boundary = start == 0 || !text[..start].chars().next_back().is_some_and(|ch| ch.is_alphanumeric() || ch == '_');`
* **复现**（`redact_text` 的实测矩阵）：

  | 输入 | 结果 |
  |---|---|
  | `Authorization: Bearer sk-test-DEADBEEF` | `<redacted>` ✅ |
  | `x-api-key: sk-test-DEADBEEF` | `<redacted>` ✅ |
  | `token=sk-test-DEADBEEF` | `<redacted>` ✅ |
  | `apiKey=sk-test-DEADBEEF` | `<redacted>` ✅ |
  | `api_key=sk-test-DEADBEEF` | **原样返回** ❌ |
  | `api_key: sk-test-DEADBEEF` | **原样返回** ❌ |
  | `access_token=sk-test-DEADBEEF` | **原样返回** ❌ |
  | `OPENAI_API_KEY=sk-test-DEADBEEF` | **原样返回** ❌ |
  | `client_secret=sk-test-DEADBEEF` | **原样返回** ❌ |
  | `my_api_key=sk-test-DEADBEEF` | **原样返回** ❌ |

* **根因**：键名命中后，`_` 被算作"词内字符"，于是 `api_key` 里 `key` 的命中位置前一个字符是 `_`
  → `boundary = false` → 跳过；`access_token` 里 `token` 同理。`is_under_longer_word`（`:363-368`）
  也把 `_` 当词内字符。
* **期望**：下划线连接的键名与裸 `token`、`x-api-key` 一样被抹掉。
* **实际**：漏抹。注意 `redact_json`（按键名，走 `is_secret_key` 的 `contains`）**不受影响** ——
  命中的是结构化参数路径；漏的只有**自由文本**路径。
* **影响**：`redact_text` 正是给自由文本兜底的那一层，而它存在的原因写在 `:282-285` 的注释里：
  「provider 的报错经常长成 `401: key=sk-live-...`」。而现实中 provider 报错与环境变量转储更常见的
  形状恰恰是 `api_key=…` / `access_token=…` / `OPENAI_API_KEY=…`。于是整轮错误日志
  （`turn_failure_line`）与工具输出（`describe_output`）会把密钥原样写进 `logs` 表。
* **最小修法建议（红队原文）**：把 `:302-306` 与 `:363-368` 两处的词内字符判据从
  `ch.is_alphanumeric() || ch == '_'` 收紧为 `ch.is_alphanumeric()`（`_` 当分隔符），
  并把 `is_under_longer_word` 里对 `_` 的容忍去掉（保留 `-`，否则 `x-api-key` 会失效）。
  改完必须回归 `log.rs` 自带的关键词双关用例：`tokenizer`、`keywords`、`monkey` 仍不该被抹。
* **按建议改只修一半**：只去掉 `_` 之后，下划线族确实好了，但**驼峰族仍然漏** ——
  `clientSecret=` / `accessToken=` / `dbPassword=` 命中的是 `secret` / `token` / `Password`，
  前一个字符是字母，词首判据依然为假。所以实际修复另外补了词首判据（见下）。
* **修法与证据**（2026-11-16，已修）：
  * `crates/hbot-rpc/src/app/agent/log.rs`：新增 `fn is_key_start(text: &str, start: usize) -> bool`，
    三种词首成立条件：① 串首；② 前一个字符**不是字母数字**（`_` / `-` / `=` / 空格 / 引号都算分隔符
    —— 这正是 RT-08 的根因：`_` 曾被当成词内字符）；③ 驼峰词首（前一个字符小写/数字、当前字符大写，
    且必须看**原文**大小写，`lower` 里已经没有驼峰信息）。
    `redact_named_values` 的命中处改为
    `if !is_key_start(text, start) || is_under_longer_word(&lower, key_end) { …原样放行… }`；
    `is_under_longer_word` 的词内字符判据收成 `ch.is_alphanumeric() || ch == '-'`
    （`_` 当词尾，`-` 保留以维持 `x-api-key` 的既有行为）。
  * 配套写法矩阵测试（同文件）：`underscore_hyphen_and_camel_key_names_are_all_redacted`
    —— 18 种写法（下划线 9、连字符 3、驼峰 4、裸写法与 Bearer 2）逐条断言被抹成 `<redacted>`，
    另配 5 条**反向**断言（`tokenizer=slow`、`keywords=rust`、`monkey=banana`、`secret_santa`、
    `passwords_are_hashed` 必须原样保留），把"漏抹"和"误抹"两个方向同时钉住。
  * 验收（2026-11-16 实测，均 exit 0）：`cargo test -p hbot-rpc --lib log:: -j 2`
    → `10 passed; 0 failed; 175 filtered out`；`cargo test -p hbot-redteam --test f_secret_leak -j 2`
    → `f06` / `f06b` ok；`cargo clippy -p hbot-rpc -p hbot-workspace --all-targets -j 2 -- -D warnings`
    无告警；`cargo fmt -p hbot-rpc -p hbot-workspace -- --check` 通过。
  * **为什么不会再退化**：`f06` 已从 `#[ignore]` 转正进默认套件（判据是 `assert_no_secret` +
    必须出现 `<redacted>`，表驱动 7 种下划线写法），`f06b` 作为正对照证明脱敏本身仍在跑；
    `log.rs` 自带的矩阵再把连字符、驼峰两种风格与三个双关词的反向断言补齐。
    以后谁要"收紧词首判据"，会被这两侧同时夹住：漏抹让 `f06`/矩阵红，误抹让反向断言红。
  * 变异验证（2026-11-16 10:24 复跑）：把判据临时退回修复前形态（`boundary` 把 `_` 当词内字符、
    `is_key_start` 不参与，且 `is_under_longer_word` 再容忍 `_` 当词尾），
    `cargo test -p hbot-rpc --lib log::` 与 `cargo test -p hbot-redteam --test f_secret_leak` **双双
    exit 101**（前者红在 `log.rs:604` 的矩阵用例，后者红在 `f06`，失败点 `f_secret_leak.rs:28`
    的 `assert_no_secret`）；还原后两者都绿，`log.rs` 的 SHA256 与变异前一致（`8F5EE97F…9E6BC27`）。
  * 顺带修掉的同文件缺陷（与 RT-08 无关，但会随机打红验收）：`store_backed_sink_writes_rows_that_logs_tail_can_read`
    原本**随机失败** —— 并行连跑 8 次 3 过 5 挂、`--test-threads=1` 连跑 6 次仍 4 过 2 挂。
    根因是 `StoreRunLogSink::append` 每次调用都 `handle.spawn` 一个独立任务，两次 `spawn_blocking`
    谁先落库不确定，而 `logs_tail` 按 `id DESC` 读 → 同一毫秒的两行顺序颠倒。
    修法：把同一 sink 的落库串成 FIFO 链（新增
    `tail: std::sync::Mutex<Option<tokio::task::JoinHandle<()>>>`，新任务先 `await` 前一个句柄再写），
    **没有改动任何断言**；修后连跑 10 次 10 过。
* **红队用例**（均已从 `#[ignore]` 转正）：
  * `crates/hbot-redteam/tests/f_secret_leak.rs::f06_日志脱敏必须覆盖下划线连接的键名`
  * 正对照：`crates/hbot-redteam/tests/f_secret_leak.rs::f06b_日志脱敏对无下划线写法必须生效`
    （证明脱敏确实在跑，避免"整体失效"和"覆盖不全"混为一谈）

---

## 实测**成立**的防护（不是缺陷，记录以免被误删）

这些都是本次红队用真进程/真文件系统验证过的，改动相关代码时应当保持：

| 防护 | 证据用例 |
|---|---|
| `..`（含 `%2e%2e`、`....//`、全角句点、`..\`）、绝对路径、`C:x` 盘符、UNC、`\\?\`、`\\?\GLOBALROOT` 全部拒绝 | `a01`–`a08` |
| NTFS 备用数据流（`f:stream`、`f::$DATA`）、尾随点/空格、Windows 保留设备名（`CON`/`NUL`/`COM1`/`CON.txt`/`a/nul.txt`）全部拒绝 | `a09`–`a11` |
| 符号链接：指向根外、最后一段是链接、目录链接后再 `..` 全部拒绝；指向根内的链接仍可用 | `a12`–`a15` |
| `.git` / `.hbot` 及其大小写变体不可写；大小写不敏感文件系统上的 `Src/../OUT` 仍拒绝 | `a16`–`a17` |
| 写入 / 重命名 / 删除入口同样过路径守卫（不只是读） | `a18`–`a19` |
| 空白名单 fail-closed；argv 直传（`;`、`$()`、反引号、`&&`、`|`、`>` 都是普通字符） | `b01`、`b02` |
| 超时杀**整棵**进程树（孙进程也死）；无限输出被硬上限截断且及时返回 | `b04`、`b05` |
| 环境变量默认不继承（`PATH`/`APPDATA`/`USERPROFILE` 全不可见）；`cwd` 出项目即拒 | `b06`、`b07` |
| 非法超时预算（0 / 超上限 / 负数）执行前拒绝；`.cmd`/`.bat` 不能当可执行程序 | `b08`、`b09` |
| 入站消息里的指令被围在 `<untrusted_data>` 内，伪造闭合标签被转义 | `c01`–`c03` |
| 提示注入不能绕过审批闸：approver 被调用、无 approver 时拒绝而不是自动执行 | `c05`–`c07` |
| `always` 只对「同工具 + 同规范化参数 + 同沙箱档」生效；换文件/换内容/降档都重新问 | `d01`–`d04`、`d07` |
| 只读档下 `write_file`/`edit_file` 在**审批之前**就被拒（不给"点了就能写"的机会） | `d05`、`d06` |
| 被拒的动作也留审计记录 | `d11` |
| QuickJS：死循环超时中断、递归栈溢出、堆内存超限、字符串过长、无限 Promise 链 —— 宿主均存活 | `e01`–`e05` |
| 队列满 → 背压且消息**已落库**（不丢）；单 bot 崩溃不拖累另一 bot | `e06`、`e07` |
| handler panic 被隔离在**一条消息**上：worker 存活、名额归还、计入 `FailureTracker`、日志带 `bot`/`conversation` 且不含正文 | `e08`、`crates/hbot-bot/tests/worker_panic.rs` 四条 |
| `BotConfig` / `ProviderConfig` 的 `Debug` 已脱敏；配置解析错误不回显原文 | `f01`、`f02`、`f05` |
| 结构化日志脱敏 `redact_json`（按键名）与 `describe_arguments` 有效 | `f06b`、`f07` |
| 自由文本脱敏 `redact_text` 覆盖下划线 / 连字符 / 驼峰三种键名写法，且 `tokenizer`/`keywords`/`monkey` 不被误抹 | `f06`、`f06b`、`log.rs` 的 `underscore_hyphen_and_camel_key_names_are_all_redacted` |

---

## 用例集覆盖矩阵（`crates/hbot-redteam`）

| 面 | 文件 | 用例数 | 默认通过 | `#[ignore]` | 对应发现 |
|---|---|---|---|---|---|
| A 路径逃逸 | `tests/a_path_escape.rs` | 20 | 20 | 0（`a20` 已随 RT-01 转正） | RT-01（已修） |
| B 命令注入 | `tests/b_command_injection.rs` | 11（10 安全 + 1 helper） | 10 | 1（`b_helper`，非安全用例） | RT-05（已修） |
| C 提示注入 | `tests/c_prompt_injection.rs` | 8 | 8 | 0 | — |
| D 审批绕过 | `tests/d_approval_bypass.rs` | 11 | 11 | 0（`d09`、`d10` 已随 RT-02 转正） | RT-02（已修） |
| E 资源耗尽 | `tests/e_resource_exhaustion.rs` | 8 | 8 | 0 | RT-07（已修） |
| F 密钥泄漏 | `tests/f_secret_leak.rs` | 10 | 10 | 0（`f03`、`f04`、`f06`、`f08`、`f09` 均已转正） | RT-01/03/06/08 |
| 夹具自测 | `src/lib.rs`（`#[cfg(test)]`） | 5 | 5 | 0 | — |
| **合计** | | **73** | **72** | **1** | |

六面共 **67 条安全用例**（不含 `b_helper` 与 5 条夹具自测），其中 **67 条默认通过**、**0 条
指向已登记发现**。全部数字以 `cargo test -p hbot-redteam` 的 `test result:` 行为准；
本行是 2026-11-16 17:07 实测值（`72 passed / 1 ignored`，唯一被忽略的是非安全用例 `b_helper`）。

> A、D、F 三行的「默认通过 / `#[ignore]`」列已由各自修复批次回填：
> `a20`（RT-01）、`d09`/`d10`（RT-02）、`f03`/`f04`（RT-06）、`f06`/`f08`/`f09`（RT-03 / RT-08）
> 均已转正并通过，实测见各条「修法与证据」。

> `crates/hbot-bot/tests/worker_panic.rs` 另有 4 条 RT-07 的回归用例，属于实现 crate 的测试，
> 不计入上表。