# S3 SQLite + OS keychain

- 状态：待开始（等 `hbot-store` 落地）
- 日期：
- 环境：Windows 11 / 文件系统 NTFS / Rust release+debug 两种
- 关联：[ADR-0002](../adr/0002-agent-in-rust.md) S3、[02 架构](../02-architecture.md) §9/§11

## 1. 目的

1. `rusqlite`（bundled）在三平台可编译可运行，WAL 生效；
2. 单 writer 线程 + mpsc 的写入吞吐是否够（目标：1000 条 inbound 批量写 < 200 ms）；
3. 8 个 bot 并发写同一库不死锁、不出现 `SQLITE_BUSY` 失控；
4. OS keychain（Windows Credential Manager / macOS Keychain / Linux Secret Service）在无桌面会话时的行为，决定 Linux 后台怎么存密钥。

## 2. 方法

```powershell
cargo test -p hbot-store --release
# 吞吐：示例程序或 test 里用 std::time::Instant 计时，1 万条插入取中位
```

keychain：先只测 Windows（`keyring` crate 或直接 `windows` crate Credential API）。Linux/macOS 留到 M5 打包阶段，若 Secret Service 不可用，按 [02](../02-architecture.md) §11 的规则**拒绝保存并提示**，不静默明文。

## 3. 结果

| 项 | 目标 | 实测 |
|---|---|---|
| bundled 编译时间（首次） | — | 待测 |
| 1000 条 inbound 单事务写入 | < 200 ms | 待测 |
| 8 线程并发写 | 无死锁、无丢失 | 待测 |
| WAL 文件增长（1 万条后） | 可控、可 checkpoint | 待测 |
| Windows 凭据读写 | 成功 | 待测 |

## 4. 结论

待填。

## 5. 对设计的影响

- 若写入成为瓶颈：先批量提交、再看是否要按时序分库；不要直接换存储引擎。
- 若 keychain 在某平台不可用：该平台的密钥输入必须显式告警，并在 UI 上标明“明文存储”。