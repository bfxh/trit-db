# ADR 038：快照隔离（读快照 + first-committer-wins）

状态：**Proposed**

日期：2026-11-21

## 背景

`TxnStore` 的读者一直看「最后一次已应用提交」的全局 map：同一个事务里读两次，可能第二次看到别的事务刚提交的新值。M2 的退出判据要求快照隔离测试。

## 决策

- `Txn::begin` 在 WAL 锁内记录 `begin_lsn = wal.next_lsn()`，并克隆当前提交 map 到 `Arc<BTreeMap>`。
- `Txn::get` / `Txn::iter` 只读这个快照，再合并本事务的写集（读己之写）。
- `TxnStore` 维护 `write_lsn: BTreeMap<key, commit_lsn>`，记录每个 key 最后一次提交的 LSN。
- `commit_inner` 在追加 `Commit` 之前检查：本事务写过的 key 若在 `write_lsn` 中的 LSN `>= begin_lsn`，返回 `TxnError::Conflict`，不写 WAL。
- `run_commit_leader` 应用提交时更新 `write_lsn`。
- `TxnStore::get/iter` 仍是当前状态的普通读（非事务读）。

## 语义

- **读快照**：同一事务内多次读取看到同一版本。
- **读己之写**：`Txn::get/iter` 先看本事务缓冲的 `Put`/`Delete`。
- **写-写冲突**：同一 key 的并发事务，先提交者赢；后提交者收到 `Conflict`。
- **不检测写偏斜**：没有序列化图检测；跨 key 的写偏斜仍可能。

## 后果

正面：

- 事务内读一致；并发写同 key 不再静默覆盖。
- `Conflict` 是显式错误，调用方可以重试。

负面 / 限制：

- **每个活跃事务克隆整个 map**：内存 O(活跃事务 × 数据量)；后续可改为 `Arc` 写时复制。
- `write_lsn` 只按 key 记录最后提交 LSN，没有版本链；不支持历史读。
- 不检测写偏斜（真正的可串行化）。
- 冲突检查在 WAL 锁内，冲突多时会增加提交延迟。

## 验收

```bash
cargo test -p tritdb-txn --test snapshot
```

- 读者在另一事务提交后仍看到旧快照；
- 读己之写；
- 同 key 并发提交，第二个返回 `Conflict`；
- 不同 key 并发提交都成功；
- `iter` 合并快照与写集。
