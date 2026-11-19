# ADR 002：单 WAL 写者 + 多读者 + 提交组

状态：Proposed（提交组模型由 ADR 006 修订）
日期：2026-11-20

## 背景

M0 需要一个可恢复的最小事内核，验证三件事：

1. 提交路径能不能做到“先日志、后状态”。
2. 进程被硬杀后，能不能只恢复已提交事务。
3. 读路径能不能不经过 WAL，支持多线程并发读。

完整 MVCC、快照隔离、跨进程锁都不在 M0 范围；提交组由 ADR 006 补上。

## 决策

- 事务层用 **单 WAL 写者 + 多读者 + 多个并发逻辑事务**。
- `TxnStore::begin` 只在追加 `Begin` 记录时持有 `Mutex<WalWriter>`；`Txn` 不持有 WAL 锁。
- `TxnStore` 的已提交状态用 `RwLock<BTreeMap<Vec<u8>, Vec<u8>>>`；读者只拿读锁。
- 单次 `get` 只保证单 key 一致；跨 key 的一次一致视图用 `iter`（一次读锁内克隆），不承诺快照隔离。
- 写事务把 `Begin` / `Put` / `Delete` 先追加到 WAL；`Commit` 记录先追加、不立即 fsync，然后进入提交队列。leader 对一批 `Commit` 做一次 `fsync`，成功后才按 WAL 顺序改内存 map（ADR 006）。
- 恢复只应用有 `Commit` 记录的事务；未提交的 `Begin` / `Put` 全部丢弃。
- WAL 尾部允许 torn record；打开时截断到最后一条完整记录。完整记录 CRC 错误视为损坏，不静默跳过。
- M0 不实现跨进程写锁；同一 WAL 只允许一个进程写。

## 备选方案

### 直接上 MVCC

需要版本链、事务 ID 可见性、快照、GC、回滚段。M0 没有页存储和索引，先做 MVCC 会把“能不能恢复”这个核心问题淹没。推迟到有页/索引之后。

### 先写状态再写日志

崩溃后可能留下“状态已改、日志没有”的半事务，恢复无法判断。违反 WAL 的基本顺序，不采纳。

### 每条记录都 fsync

实现简单，但 Windows 实测约 3.12 ms/次提交（100 条逐条 fsync，median 312.295 ms）。OLTP 不可接受。M0 先用“每条 commit 一次 fsync”，再用 ADR 006 的提交组摊薄；1,024 事务、16 线程实测 260 次 fsync，约 3.40× 提速。

### 用外部数据库/嵌入式引擎

会把“多进制存储 + 多值语义”的核心假设外包出去，也无法验证 TRBL/TRWL 自己的格式。M0 保持零外部依赖。

## 后果

好的方面：

- 提交语义清晰：一批 commit 的 `fsync` 是该批的持久化点。
- 读路径完全不碰 WAL，4 线程读测试通过。
- 崩溃恢复可判定：未提交事务丢弃，完整提交组保留。
- 多个逻辑事务可以交错写 WAL，提交组把 fsync 从“每事务一次”降到“每批一次”。
- 格式和恢复逻辑可独立测试，不依赖页/索引。

代价：

- WAL append 仍被单个 `Mutex<WalWriter>` 串行化；组提交摊薄 fsync，但不增加 WAL 写入带宽。
- 没有快照隔离；写事务之间没有冲突检测，最后提交者按 WAL 顺序覆盖。
- 连续多次 `get` 可能跨越一次提交，不是跨 key 快照；跨 key 一致视图要用 `iter`。
- `checkpoint` 要求没有活跃逻辑事务；它不是后台 checkpoint。
- WAL 无限增长；`tritdb-txn` 现在用 TRSN 快照做单段 checkpoint（ADR 005），但 WAL 段轮转仍未做（页存储的 checkpoint 见 ADR 004）。
- 没有跨进程互斥；生产前必须补文件锁。
- `BTreeMap` 全量内存状态；不是页存储。

## 验证

- `cargo test -p tritdb-wal -p tritdb-txn`：21 个测试（WAL 7、txn 14）。
- `cargo test -p tritdb --test crash_recovery`：子进程硬杀后重开，只恢复完整提交组。
- WAL 读数：append 3.445 ms / 1000 条，replay 0.287 ms / 1000 条，逐条 fsync 312.295 ms / 100 条。
- 提交组读数：1,024 事务 × 4 key、16 线程、3 轮，写总时间 median 867.780 ms，fsync 260 次（0.254 次/事务）；同规模顺序提交 2,954.827 ms。
- 全工作区 fmt/clippy/test 与 MSRV 1.88 门禁见 M0 报告。

## 下一步

1. WAL 段轮转；页存储的 checkpoint 已由 ADR 004 实现，`tritdb-txn` 的键值 checkpoint 已由 ADR 005 实现。
2. 跨进程文件锁。
3. 页存储 + 索引后，再评估 MVCC。
