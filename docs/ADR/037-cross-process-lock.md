# ADR 037：跨进程写锁用 `<wal>.lock` + PID

状态：**Proposed**

日期：2026-11-21

## 背景

`TxnStore` 的「单 WAL 写者」只在进程内由 `Mutex` 保证。两个进程同时打开同一个 WAL 会交错追加、互相截断 torn tail，恢复语义直接崩掉。需要跨进程排他。

`std::fs::File::try_lock` 在 Rust 1.89 才稳定，而本仓库 MSRV 是 1.88；项目也不想为了一个锁引入 `unsafe`。

## 决策

- 打开 WAL 时获取 `<wal>.lock`。
- 用 `OpenOptions::create_new(true)` 原子创建锁文件，写入当前 PID。
- 锁文件已存在时读取 PID：
  - 进程仍存活 → 返回 `WalError::Locked { pid }`；
  - 进程已不存在 → 视为 stale，删除后重试一次。
- `ProcessLock` 在 `Drop` 时删除锁文件。
- 锁覆盖整个 `WalWriter` 生命周期；`rotate` / `reset` 不重新加锁（锁的是 WAL 路径，不是某个段文件）。
- `WalReader` 不加锁：只读扫描允许与写者并发（读到的是一致前缀）。

## 理由

- **零依赖**：锁文件用标准库即可，不需要 `fs2`/`libc`/`winapi`。
- **跨平台**：`create_new` 在所有平台原子；PID 存活检测用 `tasklist`（Windows）或 `kill -0`（Unix）。
- **崩溃可恢复**：进程崩溃留下的锁文件会在下一次 `open` 时被判定为 stale 并清理。

## 限制

- **PID 复用**：操作系统复用 PID 时，stale 锁可能被误判为存活，需要人工删 `<wal>.lock`。
- **不是内核锁**：`create_new` 是文件系统原语，网络文件系统上的原子性取决于实现。
- **读者不互斥**：两个进程可以同时读，也可以读者与写者并发；这是刻意的。
- **锁文件与 WAL 同目录**：需要目录可写。

## 验收

```bash
cargo test -p tritdb-wal --test lock
```

- 第一个 `WalWriter::open` 成功；
- 第二个返回 `WalError::Locked`；
- 第一个 drop 后再打开成功。
