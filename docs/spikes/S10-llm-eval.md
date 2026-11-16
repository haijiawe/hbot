# S10 真 LLM 20 条评测：OpenAI 契约 vs Anthropic Messages 契约

- 日期：2026-11-16（首轮因凭据 401 作废；同日用户充值后**复跑成功**）；2026-11-17 **换新 key 复跑 Anthropic 一轮，仍 20/20**（见 §10）
- 环境：Intel i5-14600KF（14 核 / 20 线程）/ 31.8 GB RAM / Windows 11 10.0.26300 / `rustc 1.96.0 (ac68faa20 2026-05-25)` / `cargo test`（dev 测试 profile）/ `CARGO_TARGET_DIR=E:\hbot-target\eval3`（§10 那轮用 `eval4`）
- 目的：把 `crates/hbot-agent/tests/real_llm_eval.rs` 的 20 条任务评测跑在 **OpenAI `/chat/completions` 契约**与 **Anthropic `/v1/messages` 契约**两条路径上各一轮，比较通过条数、`turns`、token 与耗时，并判断这些数字能否外推到真 Claude。
- 结论状态：**两条契约各 20/20 通过**。但两个 provider 的 **prompt token 记账差 5.6 倍**（105,797 vs 19,039），原因未定论，见 §6——**不要把这两列当成"哪条链路更省"**。

## 1. 方法（实际跑的命令）

```powershell
# key 只从仓库外 %APPDATA%\hbot\agent.json 读进进程环境变量，不落盘、不打印
$cfg = Get-Content "$env:APPDATA\hbot\agent.json" -Raw | ConvertFrom-Json
$env:HBOT_LLM_BASE_URL = $cfg.baseUrl        # https://z1rry.cyou/v1
$env:HBOT_LLM_API_KEY  = $cfg.apiKey
$env:HBOT_LLM_MODEL    = $cfg.model          # deepseek-v4.1-flash-1017
$env:CARGO_TARGET_DIR  = 'E:\hbot-target\eval3'

# 第一轮：OpenAI 契约
$env:HBOT_LLM_PROVIDER = 'openai'
cargo test -j 2 -p hbot-agent --test real_llm_eval -- --ignored --nocapture *> E:\hbot-target\eval3\openai.log

# 第二轮：Anthropic Messages 契约（base 带不带 /v1 都会得到 /v1/messages）
$env:HBOT_LLM_PROVIDER = 'anthropic'
cargo test -j 2 -p hbot-agent --test real_llm_eval -- --ignored --nocapture *> E:\hbot-target\eval3\anthropic.log
```

- provider 的 base 规则：OpenAI 是 `{base}/chat/completions`（base 必须自带 `/v1`）；Anthropic 走 `provider::anthropic::messages_endpoint()`（`crates/hbot-agent/src/provider/anthropic.rs:70`），base 以 `/v1` 结尾则 `{base}/messages`，否则 `{base}/v1/messages`。
- 每条任务：`scaffold` 建项目 → 真 provider 跑 `run_turn`（工具只有 `read_file`/`list_dir`/`write_file`/`edit_file`，**不给 `run_command`**）→ harness 自己 `compile_project` + `Sandbox::dispatch` 合成入站事件断言 `reply`/`log`。
- 断言逻辑、20 条任务、工具集、`TurnConfig`（`max_steps=14`、单案上限 150 s、外层 timeout 170 s）与前一轮完全一致；本轮只换 provider。

## 2. 两轮总表

| 轮次 | provider | 通过条数 | turns | prompt tokens | completion tokens | total tokens | `finished in` | exit |
|---|---|---:|---:|---:|---:|---:|---:|---:|
| 1 | `openai` | **20/20** | 20 | 105,797 | 13,507 | 119,304 | 194.54 s | 0 |
| 2 | `anthropic` | **20/20** | 20 | 19,039 | 11,216 | 30,255 | 155.10 s | 0 |

两轮日志：`E:\hbot-target\eval3\openai.log`、`E:\hbot-target\eval3\anthropic.log`（本地，不入库）。

## 3. 每条任务（两轮都 PASS，逐条对齐）

| # | 任务 | 验证点 | OpenAI 步数 / 耗时 | Anthropic 步数 / 耗时 |
|---:|---|---|---|---|
| 1 | A① 原样回显 | 回复 == 入站文本 | 3 / 6.3 s | 3 / 4.8 s |
| 2 | A② 带上发送者 id | 回复含 `senderId` | 4 / 7.1 s | 3 / 5.4 s |
| 3 | A③ 固定问候语 | 回复 == 固定文案 | 3 / 3.9 s | 3 / 4.9 s |
| 4 | B④ 含「帮助」→ /help | 回复含 `/help` | 3 / 5.4 s | 3 / 3.6 s |
| 5 | B⑤ 含「价格」→ 三档价格表 | 三档齐全 | 3 / 5.4 s | 3 / 6.3 s |
| 6 | B⑥ 其它 → 默认话术 | 回复 == 默认文案 | 3 / 3.8 s | 3 / 3.8 s |
| 7 | B⑦ 大小写不敏感 | `HeLp ME` 仍走 /help | 3 / 22.4 s | 3 / 4.3 s |
| 8 | C⑧ 去掉 @机器人 前缀 | 回复 == 去前缀后文本 | 3 / 12.5 s | 3 / 13.3 s |
| 9 | C⑨ 多行只处理第一行 | 含第一行、不含第二行 | 3 / 6.3 s | 3 / 5.0 s |
| 10 | C⑩ emoji 原样保留 | 回复含 emoji | 3 / 6.6 s | 3 / 6.0 s |
| 11 | D⑪ 拆出 `src/lib/format.ts` | 文件存在 + import + 回复含「回复：」 | 3 / 3.1 s | 5 / 11.9 s |
| 12 | D⑫ 再拆 `src/lib/router.ts` | 文件存在 + import + 回复含 `/help` | 6 / 20.6 s | 3 / 11.x s |
| 13 | D⑬ 两文件互相 import | 双向往来 + 不死循环 | 5 / **52.1 s** | 6 / 19.7 s |
| 14 | D⑭ 入口只 export default | main.ts 只有一条 `export default` | 5 / 9.3 s | 4 / 19.2 s |
| 15 | E⑮ 空文本不回复只 log | 0 回复 + ≥1 日志 | 3 / 6.7 s | 3 / 7.5 s |
| 16 | E⑯ 超长文本只回固定话术 | 回复 == 固定话术 | 3 / 4.6 s | 3 / 5.1 s |
| 17 | E⑰ 非文本消息回固定话术 | 回复 == 固定话术 | 3 / 5.2 s | 3 / 3.9 s |
| 18 | F⑱ 只 log 不回复 | 0 回复 + ≥1 日志 | 4 / 4.5 s | 3 / 3.2 s |
| 19 | F⑲ 一条入站只回一条 | 恰好 1 条回复 | 3 / 3.9 s | 4 / 10.6 s |
| 20 | F⑳ 回复含消息 id | 回复含 `messageId` | 3 / 4.5 s | 3 / 4.7 s |

两轮的回复文本**措辞不同但都满足断言**（说明断言判行为、不是判字面），例如 #4 的 `/help` 文案两轮完全不一样，都判 PASS；#2 的 OpenAI 回复是「来自 u-10086 的消息」而 Anthropic 是「来自 u-10086 的消息：在吗」，两者都含 `senderId`。

## 4. 这轮证明了什么

1. **两条线上契约都能跑通完整 Agent 循环**：多轮工具调用（最多 6 步）、工具结果回填、最终文本、`ctx.reply`/`ctx.log` 分流、多文件 `import` 解析、QuickJS 执行、以及"一条入站只回一条"。
2. **多文件任务真的落地**：D⑪–D⑭ 每轮都验证了目标文件存在 + 入口 `import` + 只在入口 `export default defineBot`，且 D⑬ 的循环 import 没死循环。
3. **边界语义稳定**：空文本只 log、超长文本固定话术、非文本固定话术、只 log 不回复——两轮一致。
4. **Anthropic 流式解析在真实 SSE 下可用**：`content_block_start{tool_use}` + `input_json_delta` 分片拼接 + `message_stop` 结束（没有 `[DONE]`）这条路径不是纸上设计。

## 5. 上一轮（作废）的 401 记录

首轮两次执行都是 `HTTP 401 {"error":{"code":"","message":"Invalid token …","type":"new_api_error"}}`（`GET /v1/models`、`POST /v1/messages`、`POST /v1/chat/completions` 三条全 401），确认是**凭据被端点拒绝**而不是请求格式问题；当时按"不许猜测其它端点"停止全部真实调用，未产生任何通过率数字。用户充值后 `GET /v1/models` 返回 200 与同一模型，随即有上面两轮结果。

## 6. 未定论：两个 provider 的 token 记账差 5.6 倍

| | prompt | completion | total |
|---|---:|---:|---:|
| OpenAI | 105,797 | 13,507 | 119,304 |
| Anthropic | 19,039 | 11,216 | 30,255 |

completion 量级接近（13.5k vs 11.2k，比值 1.20，与两轮步数/回复长度差异同阶），但 **prompt 差 5.6 倍**。已知事实与可疑点：

- 两轮发给模型的 system prompt、工具声明、任务文本完全相同，理论上输入量应同阶；每轮 20 条任务、3–6 步，输入本就比输出大，OpenAI 侧的 105.8k 更符合"每步都重发完整上下文"的直觉。
- Anthropic 侧 `usage` 取自 `message_delta` 后到的 `input_tokens`/`output_tokens`，而 `message_start` 里的 usage 常为 0；如果解码器只保留"最后一次上报"而不是逐次累加，或上游兼容壳把 `input_tokens` 填成每轮增量而非累计，就会**系统性偏小**。
- 反向也可能成立：兼容壳的 OpenAI 路径可能把缓存命中/系统提示也计入 `prompt_tokens`，而 Anthropic 路径报告的是"新增 token"。

**处置**：本轮不改任何生产代码，也不把这两列解读为成本对比。要定论需要一个最小实验——单条任务、单步、打印每次请求的 `messages` 长度与 provider 回传的 usage，逐次累加核对。这条已记为待办，不影响 20/20 的结论（通过率与 token 记账是两件事）。

## 7. 这些数字能不能外推到真 Claude

**不能。** 两层原因：

1. **当前端点不是 Anthropic 官方实现**：`docs/02-architecture.md:784` 已记录 `z1rry.cyou` 的 `/v1/messages` 是 **OpenAI 后端外壳**（响应 `id` 为 `chatcmpl-…`、`usage.billing_usage.source == "oai_chat"`、`GET /v1/models` 的 `supported_endpoint_types` 只有 `["openai"]`）。所以第二轮证明的是"Anthropic Messages 请求/流式解析链路可用"，**不是**真 Claude 的模型能力、工具调用风格或计费。
2. **原生 `api.anthropic.com` 从未实测**：没有原生 Anthropic 凭据；`real_anthropic.rs` 的 1 条真机验证走的也是同一个兼容壳。

因此"OpenAI 轮 vs Anthropic 轮"的差异只能解释为**同一后端在不同请求契约下的表现**，不能外推为"Claude 与 GPT 的差异"。

## 8. 未验证项

- Anthropic 契约下 `tool_use.id` 的真实格式/长度（兼容壳下的 id 形如 `call_…`，与官方 `toolu_…` 不同）。
- `max_tokens=4096` 对 D⑪–D⑭ 是否构成上限（本轮都未触顶，最长 completion 合计 11.2k 分摊 20 条）。
- `thinking` 块对计费的影响（本兼容壳有 `thinking_delta`，解码器只计数不上行）。
- 原生 Anthropic 官方端点、真 Claude 模型。
- token 记账差异的根因（见 §6，需要单步仪器化实验）。
- 生产代码 `src/**` 未改动。

## 9. 结论

**两条契约全部通过：各 20/20。**

- 代码侧：`real_llm_eval.rs` 的 `HBOT_LLM_PROVIDER` 切换可用，两条链路都跑完整 20 条，无 skip、无重试掩盖。
- 能力侧：在这台机器 + 这个兼容端点上，模型能稳定写出"能编译、能按入站消息正确回复"的单文件与多文件 bot；多文件（含循环 import）与边界语义（空文本/超长/非文本/只 log）全过。
- 记账侧：**prompt token 两轮差 5.6 倍，原因未定论**，不能据此说谁更省。
- 外推边界：不能代表真 Claude；也不代表真 NapCat/真 QQ 端到端（那条见 [S9](S9-e2e-qq-official.md)）。

复跑命令见 §1；日志路径 `E:\hbot-target\eval3\`（本地、不入库）。

## 10. 2026-11-17 复跑（换新 key，Anthropic 契约）

上一轮的 key 后来失效（`POST /v1/messages` 与 `GET /v1/models` 均 `HTTP 401 {"message":"Invalid token"}`，子代理 `eval-anthropic` 因此拿不到证据）。用户当天提供新 key 后只复跑了 **Anthropic Messages** 一条链：

```powershell
$env:HBOT_LLM_PROVIDER='anthropic'
$env:HBOT_LLM_BASE_URL='https://z1rry.cyou/v1'   # base 带 /v1 → endpoint 是 {base}/messages
$env:HBOT_LLM_API_KEY=(Get-Content 'E:\hbot-target\llm-key.txt' -Raw)   # key 在仓库外，不入库、不打印
$env:HBOT_LLM_MODEL='deepseek-v4.1-flash-1017'
$env:CARGO_TARGET_DIR='E:\hbot-target\eval4'
cargo test -j 2 -p hbot-agent --test real_llm_eval -- --ignored --nocapture *> E:\hbot-target\eval4-anthropic.log
```

| provider | 通过条数 | turns | prompt tokens | completion tokens | total tokens | `finished in` | exit |
|---|---:|---:|---:|---:|---:|---:|---:|
| `anthropic` | **20/20** | 20 | 23,231 | 13,262 | 36,493 | 118.78 s | 0 |

- 端点是 `https://z1rry.cyou/v1/messages`（日志里 `endpoint = …` 一行可查）。
- 20 条逐条 PASS，最慢两条是 D⑬ 两文件互相 import（6 步 / 25.4 s）与 D⑫ 再拆 router（4 步 / 17.8 s）；其余 2.8–7.3 s。单案上限 150 s 没有触顶。
- 与 11-16 那轮（19,039 prompt / 30,255 total / 155.10 s）是同一后端、不同 key、不同时间：**通过条数一致，token 与耗时不在同一量级上可比**（网关侧的缓存命中率会直接影响 prompt 记账，见 §6 的未定论）。
- 仍然没有原生 `api.anthropic.com` 凭据，因此 §7 的外推边界不变：这轮证明的还是"Anthropic Messages 请求/流式解析链路可用"，不是真 Claude 的能力与计费。