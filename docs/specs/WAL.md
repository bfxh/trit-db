# TRWL：trit-db Write-Ahead Log v0

状态：`已实现`（`tritdb-wal`、`tritdb-txn`、`tritdb-store` 页镜像），**未冻结**。

TRWL 是单文件、追加写、单写者的事务日志。WAL 层只做崩溃恢复，不做自动 checkpoint、段轮转、组提交、跨进程锁和 MVCC；checkpoint 由消费者负责（页存储用 `TRCK` sidecar，键值存储用 `TRSN` 快照）。

## 1. 文件布局

```text
+----------------------------+
| segment header (32 bytes)  |
+----------------------------+
| record 1 header (32 bytes) |
| record 1 payload           |
+----------------------------+
| record 2 header (32 bytes) |
| record 2 payload           |
+----------------------------+
| ...                        |
```

所有多字节整数为小端。

### 1.1 Segment header

| offset | size | field | v0 |
|---|---:|---|---|
| 0 | 4 | magic | `TRWS` |
| 4 | 1 | version | `0` |
| 5 | 3 | reserved | 必须为 `0` |
| 8 | 8 | first LSN | 非 0；新文件为 `1` |
| 16 | 12 | reserved | 必须为 `0` |
| 28 | 4 | CRC32 (IEEE) | 覆盖 header `0..28` |

## 2. Record

### 2.1 Header

| offset | size | field | v0 |
|---|---:|---|---|
| 0 | 4 | magic | `TRWL` |
| 4 | 1 | version | `0` |
| 5 | 1 | kind | 见下表 |
| 6 | 2 | flags | 必须为 `0` |
| 8 | 8 | LSN | 严格递增 |
| 16 | 8 | transaction id | |
| 24 | 4 | payload length | 最大 64 MiB |
| 28 | 4 | CRC32 (IEEE) | 覆盖 header `0..28` + payload |
| 32 | … | payload | |

### 2.2 Kind

| code | name | payload |
|---:|---|---|
| 1 | `Begin` | 空 |
| 2 | `Put` | `key_len u32`, `key`, `value_len u32`, `value` |
| 3 | `Delete` | `key_len u32`, `key` |
| 4 | `Commit` | 空 |
| 5 | `Abort` | 空 |

`Put` / `Delete` 的长度字段是 payload 内的 `u32`，不是记录头的 `payload_len`。记录头的 `payload_len` 是整段 payload 的字节数。

## 3. 写入与持久化

- `WalWriter::append(kind, txn_id, payload, sync)` 先写 32 字节头，再写 payload。
- `sync=false`：只写到 OS page cache，进程崩溃通常仍在，断电不保证。
- `sync=true`：调用 `File::sync_all`。
- `tritdb-txn` 的提交路径：
  1. `Begin`（不 fsync）
  2. 每个 `Put` / `Delete`（不 fsync）
  3. `Commit`（fsync）
  4. 只有 fsync 成功后才把操作应用到内存 map。
- `Abort` 不强制 fsync；未提交事务在没有 `Commit` 时本来就会被恢复丢弃。
- `WalWriter::reset(first_lsn)` 清空文件并写新的 32 字节段头，供 `tritdb-store` checkpoint 使用。调用方必须先保证旧记录描述的持久状态已经落盘；WAL 本身不判断这一点。
- `WalWriter::open` 遇到 0 字节或不足 32 字节的文件会修复为空段；不足 32 字节不可能包含有效记录。

这保证“已提交事务的操作在 `Commit` 之前已经写入日志”；不保证“提交返回失败时一定没落盘”（fsync 失败存在经典的不确定窗口）。

## 4. 恢复

`parse_wal` 从 offset 32 开始顺序扫描：

1. 剩余字节不足 32：尾部是 torn，停止。
2. 记录头 magic/version/kind/flags 非法：**corruption**，报错。
3. `payload_len > 64 MiB`：报错。
4. 剩余字节不足 `32 + payload_len`：尾部是 torn，停止。
5. CRC 不匹配：**corruption**，报错。
6. LSN 不严格递增：报错。
7. 记录有效，继续。

“torn”只允许出现在文件尾部；一个完整长度但 CRC 错误的记录不是 torn，是损坏。

`WalWriter::open` 在扫描后：

- `TailState::Clean`：从 `valid_end` 继续追加。
- `TailState::Torn`：`set_len(offset)` 截断 torn 记录，再追加。
- 文件不足 32 字节：重置为空段，而不是报错。
- 任何 corruption：拒绝打开。

`tritdb-txn` 恢复：

- `Begin` 建立 pending 事务。
- `Put` / `Delete` 进入 pending。
- `Commit` 把 pending 应用到状态，计数 +1。
- `Abort` 丢弃 pending，计数 +1。
- 文件结束时仍 pending 的事务：丢弃，计数 `discarded_txns`。
- `Put` / `Delete` 找不到 `Begin`、重复 `Begin`、`Commit`/`Abort` 找不到 `Begin`：拒绝打开。

## 5. 并发模型

- **单 WAL 写者**：每次 append 拿 `Mutex<WalWriter>`，但锁只包住单次 append；`Txn` 不再持有 `MutexGuard`。多个逻辑事务可以交错写 WAL。
- **多读者**：`TxnStore::get` / `iter` 只拿 `RwLock` 读锁，不碰 WAL。
- 读者看到的是最后一次成功提交组应用后的 map；没有版本链、没有快照、没有读已开始事务的中间状态。
- **单次 `get` 只保证单 key 一致**；连续多次 `get` 不构成跨 key 快照，提交可以发生在两次 `get` 之间。需要一次读一致的多 key 视图时用 `iter`（一次读锁内克隆整张 map），或等后续快照隔离。
- `TxnStore` 是 `Send + Sync`；`Txn` 只借用 store，不持有 WAL guard。
- `Txn::commit` 先追加 `Commit`（不 fsync）并在持有 WAL 锁时入队；leader 对一批 commit 做一次 `fsync`，再按 `commit_lsn` 顺序应用状态（ADR 006）。

M0 没有跨进程写锁。两个进程同时打开同一 WAL 是未定义行为；生产前必须加文件锁。

## 6. 限制

- 单文件，无段轮转；文件无限增长。
- WAL 层没有自动 checkpoint；`tritdb-store` 通过 `reset` 做页存储单段 checkpoint，`tritdb-txn` 通过 `reset` + TRSN 快照做键值单段 checkpoint。
- WAL 层不提供组提交 API；组提交是 `tritdb-txn` 的提交队列行为。
- 无跨进程锁。
- 无 MVCC / 快照隔离。
- `payload_len` 与 key/value 长度都是 `u32`；单记录 payload 上限 64 MiB。
- CRC32 只做损坏检测，不是安全校验；不防恶意篡改。

## 7. 已验证

- 7 个 WAL 单元测试：段头/记录往返、torn 尾部报告与截断、坏 CRC、坏段 magic/version、坏 kind/flags、reset 空段、短文件修复。
- 14 个事务测试：提交/恢复、未提交丢弃、abort、4 线程多读、读者不见半提交、torn 尾部截断后恢复、4 个 checkpoint 测试、活跃事务拒绝 checkpoint、16 线程组提交、同 key WAL 顺序。
- 1 个集成测试：子进程硬杀后重开，验证只恢复完整提交组。
- 读数见 `docs/results/M0-SPIKE.md`。
