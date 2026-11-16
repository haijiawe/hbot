# S10 真 LLM 20 条评测：OpenAI 契约 vs Anthropic Messages 契约

- 日期：2026-11-16
- 环境：Intel i5-14600KF（14 核 / 20 线程）/ 31.8 GB RAM / Windows 11 10.0.26300 / `rustc 1.96.0 (ac68faa20 2026-05-25)` / `cargo test`（dev 测试 profile）/ `CARGO_TARGET_DIR=E:\hbot-target\eval2`
- 目的：把 `crates/hbot-agent/tests/real_llm_eval.rs` 的 20 条任务评测改成可切换 provider，在 **OpenAI `/chat/completions` 契约**与 **Anthropic `/v1/messages` 契约**两条路径上各跑一轮，比较通过条数、`turns`、`prompt/completion/total tokens` 与 `finished in`，并判断这些数字能否外推到真 Claude。
- 结论状态：**未执行（凭据失效）**。代码切换已完成且 `cargo check` / `clippy` / `fmt` 通过；两轮 20 条评测没有跑起来，因此本文件没有通过率、token 或耗时数字，也不存在任何可比较的实测结论。

## 方法（可复制的命令）

```powershell
$env:CARGO_TARGET_DIR = 'E:\hbot-target\eval2'
cd E:\HBot-Next

# 第一轮：OpenAI 契约；base 必须带 /v1，客户端拼 {base}/chat/completions
$env:HBOT_LLM_PROVIDER = 'openai'
$env:HBOT_LLM_BASE_URL = 'https://z1rry.cyou/v1'
$env:HBOT_LLM_MODEL    = 'deepseek-v4.1-flash-1017'
$env:HBOT_LLM_API_KEY  = '<从仓库外 agent.json 读取，绝不写进仓库/日志>'
cargo test -j 2 -p hbot-agent --test real_llm_eval -- --ignored --nocapture *> 'E:\HBot-Next\_recovery\eval2-openai.log'

# 第二轮：Anthropic Messages 契约；base 不带 /v1
$env:HBOT_LLM_PROVIDER = 'anthropic'
$env:HBOT_LLM_BASE_URL = 'https://z1rry.cyou'
cargo test -j 2 -p hbot-agent --test real_llm_eval -- --ignored --nocapture *> 'E:\HBot-Next\_recovery\eval2-anthropic.log'
```

- provider 的 base 规则：
  - OpenAI：`OpenAiProvider::endpoint()` = `{base}/chat/completions`，所以 base 必须自带 `/v1`。
  - Anthropic：`provider::anthropic::messages_endpoint(base)`（`crates/hbot-agent/src/provider/anthropic.rs:70`）先 trim 末尾 `/`；base 以 `/v1` 结尾时返回 `{base}/messages`，否则返回 `{base}/v1/messages`。因此 `https://z1rry.cyou` 与 `https://z1rry.cyou/v1` 都会得到 `https://z1rry.cyou/v1/messages`，不会拼出 `/v1/v1/messages`；单测已覆盖这两种输入。
- 断言逻辑、20 条任务、工具集、`TurnConfig`（`max_steps=14`、单案上限 150 s、外层 timeout 170 s）全部保持原样；本轮只改 provider 的选择与构造。

## 结果（真实执行情况）

### 两轮总表

| 轮次 | provider | 端点（运行日志打印） | 命令 exit code | 通过条数 | turns | prompt tokens | completion tokens | total tokens | finished in |
|---|---|---|---|---|---|---|---|---:|
| 1 | openai | `https://z1rry.cyou/v1/chat/completions`（冒烟） | **101**（冒烟） | **未执行** | 0 | 0 | 0 | 0 | 未采集 |
| 2 | anthropic | 未发起（凭据已确认失效） | 未执行 | **未执行** | — | — | — | — | — |

- 冒烟命令为 `HBOT_LLM_EVAL_ONLY=1`（只跑第 1 条），不是 20 条正测；它证明的是「请求已到达 provider 后被鉴权拒绝」，不是评测结果。
- 冒烟日志 `E:\HBot-Next\_recovery\eval2-smoke-openai.log` 的实际错误：
  `run_turn 失败：invalid: provider 返回 HTTP 401: {"error":{"code":"","message":"Invalid token (request id: 202611160851225916236738268d9d6yDuBbwam)","type":"new_api_error"}}`
- 直连探测（同一凭据）：`GET https://z1rry.cyou/v1/models`（`Authorization: Bearer …`）→ 401 同一错误；`POST https://z1rry.cyou/v1/messages` 分别用 `x-api-key` 与 `Authorization: Bearer` 两种认证头 → 均 401 同一错误。因此不是某一条链路的请求格式问题，而是凭据本身被端点拒绝。
- 按 lead 指示，确认失效后**停止全部真实 LLM 调用**，没有用同一凭据重试 20 条，也没有猜测其它端点或模型。

### 每条任务的通过情况（两轮均未执行）

> 全部 20 条都因凭据 401 未进入模型调用；下表只记录任务清单与状态，不伪造 PASS/FAIL。展开可见逐条状态。

<details>
<summary>20 条任务逐条状态（点击展开）</summary>

| # | 任务 | 验证点 | OpenAI 轮 | Anthropic 轮 |
|---:|---|---|---|---|
| 1 | A① 原样回显 | 回复 == 入站文本 | 未执行（凭据失效） | 未执行（凭据失效） |
| 2 | A② 带上发送者 id | 回复含 `senderId` | 未执行（凭据失效） | 未执行（凭据失效） |
| 3 | A③ 固定问候语 | 回复 == 固定文案 | 未执行（凭据失效） | 未执行（凭据失效） |
| 4 | B④ 含「帮助」→ /help | 回复以 `/help` 开头 | 未执行（凭据失效） | 未执行（凭据失效） |
| 5 | B⑤ 含「价格」→ 三档价格表 | 三档价格齐全 | 未执行（凭据失效） | 未执行（凭据失效） |
| 6 | B⑥ 其它 → 默认话术 | 回复 == 默认文案 | 未执行（凭据失效） | 未执行（凭据失效） |
| 7 | B⑦ 大小写不敏感 | `HeLp ME` 仍走 /help | 未执行（凭据失效） | 未执行（凭据失效） |
| 8 | C⑧ 去掉 @机器人 前缀 | 回复 == 去前缀后的文本 | 未执行（凭据失效） | 未执行（凭据失效） |
| 9 | C⑨ 多行只处理第一行 | 含第一行、不含第二行 | 未执行（凭据失效） | 未执行（凭据失效） |
| 10 | C⑩ emoji 原样保留 | 回复 == 入站文本（含 emoji） | 未执行（凭据失效） | 未执行（凭据失效） |
| 11 | D⑪ 拆出 `src/lib/format.ts` | 文件存在 + main.ts import + 回复含「回复：」 | 未执行（凭据失效） | 未执行（凭据失效） |
| 12 | D⑫ 再拆 `src/lib/router.ts` | 文件存在 + main.ts import + 回复含 `/help` | 未执行（凭据失效） | 未执行（凭据失效） |
| 13 | D⑬ 两文件互相 import | 双向往来 + 不死循环 + 回复含「前缀：」 | 未执行（凭据失效） | 未执行（凭据失效） |
| 14 | D⑭ 入口只 export default | main.ts 只有一条 export default | 未执行（凭据失效） | 未执行（凭据失效） |
| 15 | E⑮ 空文本不回复只 log | 0 条回复 + 至少 1 条日志 | 未执行（凭据失效） | 未执行（凭据失效） |
| 16 | E⑯ 超长文本只回固定话术 | 回复 ==「消息太长了，请分段发送」 | 未执行（凭据失效） | 未执行（凭据失效） |
| 17 | E⑰ 非文本消息回固定话术 | 回复 ==「暂不支持非文本消息」 | 未执行（凭据失效） | 未执行（凭据失效） |
| 18 | F⑱ 只 log 不回复 | 0 条回复 + 至少 1 条日志 | 未执行（凭据失效） | 未执行（凭据失效） |
| 19 | F⑲ 一条入站只回一条 | 恰好 1 条回复且含入站文本 | 未执行（凭据失效） | 未执行（凭据失效） |
| 20 | F⑳ 回复含消息 id | 回复含 `messageId` | 未执行（凭据失效） | 未执行（凭据失效） |

</details>

### 静态验收（与真实评测分开）

| 命令 | 结果 |
|---|---|
| `cargo check -j 2 -p hbot-agent --all-targets` | exit 0（11.00 s） |
| `cargo clippy -j 2 -p hbot-agent --all-targets -- -D warnings` | exit 0 |
| `cargo fmt -p hbot-agent -- --check` | exit 0 |
| `cargo test -j 2 -p hbot-agent --test real_llm_eval --no-run` | exit 0（8.00 s，测试二进制可编译） |

## 本次代码改动

只改测试文件 `crates/hbot-agent/tests/real_llm_eval.rs`，未改任何 `src/**` 生产代码：

| 位置 | 改动 |
|---|---|
| `crates/hbot-agent/tests/real_llm_eval.rs:10-20` | 模块文档增加 `HBOT_LLM_PROVIDER` 说明与 Anthropic base 规则 |
| `crates/hbot-agent/tests/real_llm_eval.rs:62` | `LlmConfig` 增加 `provider: LlmProvider` 字段 |
| `crates/hbot-agent/tests/real_llm_eval.rs:90-97` | 读取可选 `HBOT_LLM_PROVIDER`，缺省 `openai`；非法值 panic，不静默回退 |
| `crates/hbot-agent/tests/real_llm_eval.rs:830-856` | 按枚举构造 `OpenAiProvider` 或 `AnthropicProvider`，都 `.with_timeout(120s)`，统一装箱为 `Box<dyn Provider>` |
| `crates/hbot-agent/tests/real_llm_eval.rs:848-850` | 运行日志打印 `provider = …` 与实际 `endpoint = …` |
| `crates/hbot-agent/tests/real_llm_eval.rs:874` | `run_case(provider.as_ref(), …)`，测试主体只依赖 `Provider` trait |

## 两侧契约与已知差异（实现事实，不是本轮实测）

| 维度 | OpenAI 兼容 | Anthropic Messages |
|---|---|---|
| 端点 | `POST {base}/chat/completions`（base 自带 `/v1`） | `POST {base}/v1/messages`（base 自带 `/v1` 时也只会得到 `/v1/messages`） |
| 认证头 | `Authorization: Bearer <key>` | `x-api-key: <key>` + `anthropic-version: 2023-06-01` |
| system | 一条 `role:"system"` 消息 | 顶层 `system` 字符串（多个 system 用 `\n\n` 拼接，空则省略） |
| 工具声明 | `{type:"function", function:{name,description,parameters}}` | `{name, description, input_schema}` |
| 工具调用上行 | `delta.tool_calls[]` 分片、按 `index` 聚合 | `content_block_start{tool_use}` + `input_json_delta.partial_json` 按 `index` 拼接，`content_block_stop` 才完整 |
| 结束标记 | `data: [DONE]` | `message_stop`（没有 `[DONE]`） |
| 必发字段 | `stream:true` | `stream:true` + `max_tokens`（默认 4096） |
| 不发 | — | `tool_choice`（兼容壳可能把未知字段判 400） |

流式解析的已知边界（来自 `crates/hbot-agent/src/provider/anthropic.rs` 模块文档）：

- `thinking_delta` 只计数并写 `debug!` 日志，**不进正文、不进 UI**；因此若真实 Claude 产生大量 thinking，token 计费可能增加而回复文本不变。
- `usage` 取 `message_delta` 后到的 `input_tokens`/`output_tokens`；`message_start` 里常为 0，必须后到覆盖。
- `message_delta` 可能没有 `stop_sequence`；末帧 `choices` 可能是空数组，解析器要先取 usage 再判 choices。
- `message_stop` 是结束事件，不能等待 OpenAI 的 `[DONE]`。

## 这些数字能不能外推到真 Claude

**不能。** 原因有三层，按证据强度排列：

1. **本轮根本没有 20 条数字**：两轮都在鉴权阶段被 401 拒绝，`turns=0`、`tokens=0`，任何「20/20」或 token/耗时的说法都没有实测依据。
2. **即便跑通，当前端点也不是 Anthropic 官方实现**：`docs/02-architecture.md:784` 已记录 `z1rry.cyou` 的 `/v1/messages` 是 **OpenAI 后端外壳**——响应 `id` 为 `chatcmpl-…`、`usage.billing_usage.source == "oai_chat"`。它只证明「Anthropic Messages 请求/流式解析链路能工作」，不能证明真 Claude 的模型能力、工具调用风格或 token 计费。
3. **原生 `api.anthropic.com` 从未实测**：本轮没有原生 Anthropic 凭据/端点；`real_anthropic.rs` 此前的 1 条真机验证同样走的是这个兼容壳，不是官方 API。

因此「OpenAI 轮 vs Anthropic 轮」的差异即便将来跑出来，也只能解释为**同一 OpenAI 后端在不同请求契约下的表现**，不能外推为「真 Claude 与 OpenAI 模型的差异」。

## 未验证项

- 两轮各 20 条任务的通过率、逐条失败原因分类（MODEL / HARNESS / PROVIDER）——缺有效凭据，全部未执行。
- Anthropic 契约下 `tool_use.id` 的真实格式/长度、`max_tokens=4096` 对多文件任务（D⑪–D⑭）是否截断、thinking 块对 token 统计的影响——均未在真机验证。
- 原生 Anthropic 官方端点（`api.anthropic.com`）或真 Claude 模型：从未测试。
- 两轮的实际耗时与花费：未采集（无任何一次成功模型调用）。
- 生产代码 `src/**` 未改动；若将来在真机发现 provider bug，本 Spike 只记录现象、不在本轮修复。

## 结论

**有条件通过（仅限代码路径）/ 真机评测未执行。**

- `real_llm_eval.rs` 已支持 `HBOT_LLM_PROVIDER=openai`（缺省）与 `anthropic`，两条构造函数都收敛到 `Box<dyn Provider>`；断言、任务集、工具集未变；静态验收全绿。
- 20 条评测没有执行：`z1rry.cyou` 凭据返回 HTTP 401 `Invalid token`，等待用户提供新的有效 key（或换用可用的兼容端点）后重跑。
- 在拿到有效凭据前，本文件不提供任何通过率、token 或耗时数字，也不把兼容壳结果当作真 Claude 结论。
