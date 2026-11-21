# hbot-workspace

> 项目文件系统：路径守卫、读写、目录操作与文本搜索。**工作区的安全边界**。

## 概述

所有文件 API 都是 `Workspace` 的方法：它持有项目根与策略，先做路径守卫再碰磁盘。
不含 UI，也不含 Agent 决策 —— 谁要读什么、要不要给模型看，是 `hbot-agent` 的事。

## 目录

- [关键文件](#关键文件)
- [路径守卫](#路径守卫)
- [边界：能放什么 / 不能放什么](#边界能放什么--不能放什么)
- [上游与下游](#上游与下游)
- [测试](#测试)
- [已知限制](#已知限制)

## 关键文件

| 文件 | 做什么 |
|---|---|
| `src/project.rs` | 根解析与路径守卫：拒绝绝对路径、`..`、符号链接逃逸、`.git/`、`.hbot/`；Windows 保留名（`CON` / `NUL` 等） |
| `src/files.rs` | 读取（带大小上限）、原子写入（同目录 tmp → fsync → rename）、列目录、建目录、重命名、移入 `.hbot/trash` |
| `src/search.rs` | `ignore` + `grep-searcher` 的流式搜索；跳过二进制与非 UTF-8 并计数 |
| `src/lib.rs` | 再导出（`Workspace` / `FileContent` / `SearchHit` / `MAX_READ_BYTES` 等） |

## 路径守卫

判据顺序固定：字符串层（绝对路径 / `..` / NUL / Windows 保留名）→ realpath → `starts_with(root)`。
只做字符串判断会漏符号链接；只做 realpath 会在目标不存在时把合法的"新建文件"也拒掉 —— 两步都要有。

## 边界：能放什么 / 不能放什么

- **能放**：路径策略、原子写、搜索、列目录。
- **不能放**：UI、Agent 决策、命令执行（`hbot-command`）、Git 操作。

## 上游与下游

- 上游：`hbot-core`（错误、ID）。
- 下游：`hbot-agent`（工具层）、`hbot-rpc`（`files.*` 命令）、`hbot-replay`、`hbot-bench`、`hbot-parity`、`hbot-redteam`。

## 测试

`cargo test -p hbot-workspace`。路径逃逸的用例主要在两处：本 crate 的单测与 `crates/hbot-redteam/tests/a_path_escape.rs`。
Windows 专有语义（保留名、8.3 名）必须按平台门控：非 Windows 上它们只是普通文件名。

## 已知限制

- 没有文件监听：热重载靠 `hbot-rpc` 的 watcher 与指纹缓存。
- 回收站是"移进 `.hbot/trash`"，不会自动清理；磁盘用量由 `C3.file` 的配额统计兜底。
