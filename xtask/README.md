# xtask

仓库维护命令：`cargo xtask <cmd>`。**只在开发期跑**，不参与任何产品运行时。

## 目录

- [关键文件](#关键文件)
- [gen：生成 TS 契约与示例 hbot.d.ts](#gen生成-ts-契约与示例-hbotdts)
- [deps：crate 依赖方向闸门](#deps-crate-依赖方向闸门)
- [边界：能放什么 / 不能放什么](#边界能放什么--不能放什么)
- [为什么放在仓库根而不是 crates/](#为什么放在仓库根而不是-crates)
- [测试](#测试)
- [扩展点](#扩展点)
- [已知限制](#已知限制)

## 概述

两件事，两件都是 CI 闸门：

| 子命令 | 判据 | 没跑会怎样 |
|---|---|---|
| `cargo xtask gen` | 从 Rust 契约生成 `apps/web/src/generated/` 与 `examples/*/hbot.d.ts` | UI 的 TS 类型与运行时对不上（坑 30、坑 42） |
| `cargo xtask gen --check` | 只比对不写盘，不一致就非零退出 | 同上，但是 CI 判红而不是改写工作树 |
| `cargo xtask deps` | 每条内部依赖边都必须在 `ALLOWED` 白名单里 | 架构边界靠人自觉（`docs/03-directory.md` §4 的约束没人执行） |

两个都是**默认拒绝**：`gen --check` 只认"磁盘上恰好等于生成结果"，`deps` 只认"这条边被显式登记过"。新增一条边或改一个 DTO 都必须来这两个文件里做一次显式决定。

## 关键文件

| 路径 | 做什么 |
|---|---|
| `xtask/src/main.rs` | 子命令分发（`deps` / `gen [--check]`）、`workspace_root()`、示例目录发现与 `hbot.d.ts` 读写 |
| `xtask/src/deps.rs` | `ALLOWED` 白名单（每个 crate 允许依赖谁）、扫描 `crates/*/Cargo.toml` 与 `xtask/Cargo.toml`、违规报告 |
| `Cargo.toml` | 只依赖 `hbot-rpc` / `hbot-core` / `hbot-code`（生成器要读契约与 dts 生成器） |

### gen：生成 TS 契约与示例 hbot.d.ts

```bash
cargo xtask gen            # 写盘：apps/web/src/generated/*.ts + examples/*/hbot.d.ts
cargo xtask gen --check    # 只比对，退出码非零即不一致（CI 用这个）
```

示例目录**不写死名字**：扫 `examples/` 下所有含 `hbot.json` 的目录。加一个示例不需要改生成器；删光示例也不会让闸门崩。

`examples/*/hbot.d.ts` 是每个项目自己的副本 —— 用户打开项目时 Monaco 的补全读的是它，不是仓库里那份。所以它同样要生成，否则补全与运行时行为会漂移，而漂移的后果要等用户在 IDE 里写错才会暴露。

### deps：crate 依赖方向闸门

判据只有一条：**每一条内部依赖边都必须在 `/xtask/src/deps.rs` 的 `ALLOWED` 里**。没登记就是违规，不区分"看起来合理"。

为什么不写成"禁止反向边"：反向边只能覆盖已知的坏方向，新 crate 一加进来就默认放行。白名单反过来 —— 默认拒绝，加边必须是一次显式的架构决定。

扫描口径：只解析 `[dependencies]` / `[dev-dependencies]` / `[build-dependencies]` 的键名，不看版本与 feature —— 方向闸门只关心"谁依赖谁"。

## 边界：能放什么 / 不能放什么

**能放**：代码生成、依赖检查、基准脚本这类开发期工具。

**不能放**：任何产品运行时逻辑。`hbot` 二进制（`crates/hbot-cli`）、桌面壳（`crates/hbot-desktop`）、托盘、自动更新都不许调用这里的东西 —— 它的依赖方向是"开发工具看见所有实现 crate"，一旦被产品代码引用，这条方向就反了（`deps` 会红）。

**也不能放**：第二套契约定义。`gen` 只搬运 `hbot-rpc::ts` 与 `hbot-code::dts` 的产物，不自己拼 TS。

## 为什么放在仓库根而不是 crates/

它是 workspace 成员之一（根 `Cargo.toml` 的 `members = ["crates/*", "xtask"]`），但**不是产品 crate**：

- `crates/` 下每个目录都有对应的产品职责（而且是 `deps` 白名单里的一个节点），`xtask` 没有；
- 它的依赖方向是"看得见全部实现 crate"，与 `hbot-cli` 同级同向，但它是开发期工具而不是组合根；
- 放在根上让人一眼看出它不参与发布：`packaging/` 的打包脚本只认 `hbot-cli` 与 `hbot-desktop`。

改名时记得同步 `AGENTS.md` §2 目录地图与 `xtask/src/deps.rs` 的 `ALLOWED` 条目（键名是 crate 名 `xtask`，与目录位置无关）。

## 测试

`deps.rs` 末尾有 3 条单测（`:267` 起）钉住白名单判定本身：登记过的边放行、没登记的边报"未登记"、越界的边报"违反方向"。

`gen` 没有单测 —— 它的正确性由生成物自己证明：`cargo test` 里 `hbot-rpc` 与 `hbot-code` 都有一批"契约与 TS/d.ts 对齐"的测试，而 `gen --check` 在干净工作树上必须是 0。

## 扩展点

加新子命令：`xtask/src/main.rs` 的 `match cmd.as_str()` 加分支 + 同目录一个模块。规矩与别处一样 —— 开发期工具，不引产品依赖。

加新 crate：在 `deps.rs` 的 `ALLOWED` 登记它能依赖谁（空数组 = 谁都不许），否则 `cargo xtask deps` 直接报"未登记"。

## 已知限制

- **不做增量**：每次 `gen` 都全量重写 55 个文件 + 每个示例一份 `hbot.d.ts`。文件不大，没必要为了几百毫秒引入缓存失效逻辑。
- **`--check` 只比内容不比格式**：生成的 TS 由 `ts-rs` 决定排版，改 Rust 注释不会让 `--check` 变红。
- **`deps` 看不见 `dev-dependencies` 的 feature 条件**：它只读键名，所以 `[target."cfg(unix)".dev-dependencies]` 里的边也算数。这是刻意的 —— 方向约束不该因为 cfg 而绕开。