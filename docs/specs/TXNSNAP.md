# TRSN v0：键值存储快照

状态：**草案，未冻结**。实现位于 `crates/tritdb-txn`。

## 1. 目的

`tritdb-txn` 的 WAL 是单文件追加日志。没有 checkpoint 时，每次打开都要把全部已提交事务重放一遍，WAL 和恢复时间线性增长。TRSN v0 给已提交的 `BTreeMap` 一个持久快照，让恢复从「重放全部历史」变成「读快照 + 重放快照之后的记录」。

TRSN 只服务 `tritdb-txn` 的键值状态，不是页存储的 `TRCK` sidecar，也不是 TRBL 块格式。

## 2. 文件

- 快照路径：`<wal>.snap`。
- 临时文件：`<wal>.snap.tmp`。
- 写入顺序：临时文件 `sync_all` → rename 覆盖 `<wal>.snap`。
- 快照是整体替换，不做增量；M0 每次 checkpoint 重写全部键值。

## 3. 头

固定 32 字节：

| offset | size | field | v0 |
|---|---:|---|---|
| 0 | 4 | magic | `TRSN` |
| 4 | 1 | version | `0` |
| 5 | 3 | reserved | 必须为 `0` |
| 8 | 8 | checkpoint LSN | 小端 `u64`； `0` 表示空 WAL |
| 16 | 8 | entry count | 小端 `u64` |
| 24 | 4 | payload length | 小端 `u32` |
| 28 | 4 | CRC32 (IEEE) | 覆盖 header `0..28` + payload |

## 4. 载荷

payload 是按 key 升序排列的键值对，每个条目：

| offset | size | field |
|---|---:|---|
| 0 | 4 | key length，小端 `u32` |
| 4 | key length | key |
| … | 4 | value length，小端 `u32` |
| … | value length | value |

约束：

- `entry count` 必须等于 payload 中条目数。
- 每个 key 在快照内唯一；重复 key 视为损坏。
- payload 总长度必须等于 header 的 `payload length`；尾随字节视为损坏。
- 每个条目至少 8 字节（两个零长度 blob）；`entry count > payload_length / 8` 直接拒绝，避免小文件触发巨大循环。
- v0 的 payload 上限是 `u32::MAX` 字节。M0 在内存中构造 payload，所以峰值内存约为状态的两倍。

## 5. 恢复

`TxnStore::open(wal)`：

1. 读取 `<wal>.snap`；不存在则视为无快照。
2. 校验 magic/version/reserved/长度/CRC/payload；损坏返回 `BadSnapshot`，不静默回退。
3. 用快照的 `checkpoint LSN` 作为恢复起点，快照条目作为初始状态。
4. `WalWriter::open` 打开 WAL；WAL 层会截断 torn tail，并把短于 32 字节的文件修复为空段。
5. `WalReader` 扫描全部记录；`LSN <= checkpoint LSN` 的记录跳过，计入 `records_skipped`。
6. 只对快照之后的 `Begin/Put/Delete/Commit/Abort` 聚合事务；只有带 `Commit` 的事务会应用到状态。
7. `records` 只统计快照之后参与恢复的记录；`last_lsn = max(WAL 尾, checkpoint LSN)`。

checkpoint 要求没有活跃逻辑事务。`TxnStore::checkpoint` 先检查 `active_txns == 0`，否则返回 `ActiveTxns`；它不等待事务结束。因此不会有事务跨过 checkpoint 边界。若快照之后出现没有 `Begin` 的 `Put`，恢复按 `UnknownTxn` 拒绝。

## 6. Checkpoint

`TxnStore::checkpoint()`：

1. 持有 WAL 写者锁，并在锁内检查 `active_txns == 0`；否则返回 `ActiveTxns`；
2. 持有状态读锁，取得已提交 `BTreeMap`；
3. `checkpoint_lsn = wal.next_lsn() - 1`；
4. 序列化全部键值到 payload，写 `<wal>.snap.tmp`，`sync_all`，rename 覆盖 `<wal>.snap`；
5. `wal.reset(checkpoint_lsn + 1)`，写新的 32 字节段头；
6. 返回 `CheckpointInfo { checkpoint_lsn, entries, wal_bytes_before, wal_bytes_after, snapshot_bytes }`。

**顺序不能反**：快照必须先持久化，再 reset WAL。否则 crash 后旧 WAL 被清掉、又没有快照，键值状态无法恢复。

崩溃窗口：

- **快照已写、WAL 未 reset**：旧 WAL 仍在；恢复读快照，跳过 `LSN <= checkpoint_lsn` 的旧记录，状态正确。
- **只写了 `.tmp`，rename 未完成**：旧快照或“无快照”仍然有效，旧 WAL 未 reset；恢复走旧路径，不丢数据。残留 `.tmp` 被下次 checkpoint 覆盖。
- **WAL reset 进行中**：WAL 可能是 0 字节或不足 32 字节；`WalWriter::open` 修复为空段；快照已持久化。
- **WAL reset 已完成**：WAL 只有 32 字节段头，恢复只读快照。
- **快照缺失但 WAL 已 reset**：状态无法恢复；这是破坏性操作，M0 不提供回退。
- **快照损坏**：`BadSnapshot` 硬失败，不猜测、不跳过。

## 7. 锁与并发

- 锁顺序：`WAL -> commit queue -> state`；checkpoint 是 `WAL -> state`；提交等待者只拿 `commit queue` 和 condvar。
- commit 在持有 WAL 锁时把 work 放入队列；leader 拿 WAL 锁后取走队列并 fsync，再按 `commit_lsn` 顺序应用状态。这个顺序防止“Commit 已 fsync、但 work 还没入队”的窗口。
- checkpoint 是单写者停顿点：它持有 WAL 锁和状态读锁，阻塞写者和状态写者；读者不被阻塞。
- 同一线程持有 `Txn` 时调用 `checkpoint` 不会自锁，而是返回 `ActiveTxns`；checkpoint 必须在没有活跃事务时调用。
- checkpoint 不会刷新 `TxnStore::recovery()` 里的旧报告；要看 checkpoint 后的恢复状态，重开 store 或读 CLI `txn dump`。
- 不是 MVCC；没有跨 key 快照读 API。并发写同一 key 时，最后 WAL 顺序获胜。
- 没有跨进程文件锁；两个进程同时打开同一对 WAL/snapshot 会互相破坏。

## 8. 错误

`TxnError` 新增：

- `Io`：快照或 WAL 文件 I/O 错误；
- `BadSnapshot { reason }`：头、长度、CRC、重复 key、尾随字节或条目数不合法；
- `SnapshotTooLarge { len, max }`：payload 超过 v0 的 `u32` 上限；
- `ActiveTxns { count }`：checkpoint 时仍有活跃逻辑事务；
- `Writer { reason }`：提交组 leader 失败后 store 进入终止状态。

原有 WAL 错误仍由 `TxnError::Wal` 包装。

## 9. 已验证

- `crates/tritdb-txn` 14 个单元测试，其中 4 个覆盖快照：
  - checkpoint 写快照、WAL 截到 32 字节、重开后状态和读数正确；
  - 快照写好后把旧 WAL 放回，恢复跳过 3 条旧记录；
  - 连续两次 checkpoint 覆盖旧快照，两个 key 都在；
  - 篡改快照 CRC 后 `TxnStore::open` 返回 `BadSnapshot`。
- 并发/checkpoint 测试：活跃事务下 checkpoint 返回 `ActiveTxns`；16 线程并发提交共享 fsync，重开后 16 个事务全部恢复。
- CLI 手工两键验证：`records=6`、`checkpoint_lsn=6`、`entries=2`、WAL 244 → 32 字节、snapshot 52 字节；重开 `records=0`、`checkpoint_lsn=6`、`last_lsn=6`，两个 key 可读。
- `txn-checkpoint` 基准（release）：
  - 64 事务 × 4 key，3 轮 median：WAL 16,848 → 32 字节，snapshot 4,560 字节，checkpoint 6.048 ms，重开 0.387 → 0.283 ms。
  - 256 事务 × 4 key，3 轮 median：WAL 68,784 → 32 字节，snapshot 19,632 字节，checkpoint 6.302 ms，重开 0.600 → 0.459 ms。
  - 1,024 事务 × 4 key，3 轮 median：WAL 277,872 → 32 字节，snapshot 81,264 字节，checkpoint 6.157 ms，重开 1.665 → 1.066 ms（约 1.56×）。
- 64 事务的 WAL 太小，快照固定成本占主导；WAL 越大，快照对恢复的收益越明显。

## 10. 未做

- 增量/差分快照；
- 快照压缩、分块、流式写入（当前整包在内存构造）；
- 快照代际保留与 PITR；
- 与页存储共享快照格式；
- WAL 段轮转；
- 跨进程文件锁；
- MVCC/快照隔离。
