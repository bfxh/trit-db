# ADR 032：PITR 用归档 checkpoint，按 LSN 重放

状态：**Proposed**

日期：2026-11-21

## 背景

`checkpoint()` 把已提交状态写进 `<wal>.snap`，然后 `reset` WAL。reset 之后，旧记录被丢弃，只能恢复到**最新**状态，无法回答「第 N 个事务提交之后数据库长什么样」。

M2 的恢复目标里除了崩溃一致性，还包括时间点恢复（PITR）。PITR 的最小要求是：

- 能回到某个历史 LSN；
- 不改变现有 `open` / `checkpoint` 的行为；
- 归档损坏或缺失时明确报错，不静默返回错的状态。

## 决策

新增两条 API：

1. `TxnStore::checkpoint_archive()`
   - 与 `checkpoint()` 相同的 snapshot + reset 流程；
   - 在 reset **之前**把 snapshot 和 WAL 复制到 `<wal>.history/<checkpoint_lsn>.{snap,wal}`；
   - 归档文件先写 `.tmp` 再 `rename`，避免崩溃留下半份归档；
   - 归档失败时直接返回错误，WAL 不 reset（live store 仍可用）。

2. `TxnStore::recover_at(path, target_lsn) -> PitrView`
   - 从 `<wal>.history` 里选 `checkpoint_lsn <= target_lsn` 的最大 snapshot 作为 base；
   - 没有可用归档时，退回到 live snapshot（仅当它的 `checkpoint_lsn <= target_lsn`）；
   - 重放所有 `base.checkpoint_lsn < lsn <= target_lsn` 的记录，来源是后续归档 WAL + 当前 WAL，按 LSN 合并去重；
   - 只读，不打开或修改 live store。

`checkpoint()` 本身不变，不产生历史。

## 语义

- **按 LSN，不按墙钟**。调用方需要自己把时间戳映射到 LSN；当前没有时间戳索引。
- **事务不半应用**。如果 `Commit` 在 `target_lsn` 之后，整个事务被丢弃（`discarded_txns` 计数），不会出现「Put 生效但 Commit 没生效」。
- **恢复是只读视图**。返回 `PitrView { target_lsn, checkpoint_lsn, applied_lsn, entries, committed_txns, discarded_txns }`，不写回任何文件。

## 错误

| 情况 | 错误 |
|---|---|
| `target_lsn` 早于最早归档 | `PitrBeforeHistory { target_lsn, oldest_lsn }` |
| 归档缺 snapshot 或 WAL | `PitrIncompleteArchive { checkpoint_lsn, reason }` |
| 归档 snapshot 损坏 | 复用 `BadSnapshot` / `SnapshotTooLarge` |
| WAL 损坏 | 复用 `WalError` |

## 限制

- 归档有自动保留策略：`checkpoint_archive` 默认保留最新 8 个（`DEFAULT_ARCHIVE_KEEP`），`checkpoint_archive_keep(n)` / `txn checkpoint --archive --keep n` 可改；`n = 0` 关闭自动清理。
- 归档 WAL 仍是 TRWL v0 单段文件；没有 WAL 段轮转。
- 没有时间戳索引；CLI 只提供 `--to-lsn`。
- `recover_at` 重放时最多读取所有归档 + 当前 WAL，复杂度与历史总量线性相关。
- `prune_history` 只删除 `<wal>.history` 里的归档，不动 live snapshot 和 WAL。

## 验收

```bash
cargo test -p tritdb-txn --test pitr
cargo test -p tritdb --test pitr_cli
```

- `pitr.rs`：两次归档 checkpoint 之后分别恢复到两个 checkpoint；恢复到两个 checkpoint 之间的某个 `Commit` LSN；target 早于历史被拒绝；普通 checkpoint 不产生 `<wal>.history`。
- `pitr_cli.rs`：`txn checkpoint --archive` → `txn history` → `txn recover --to-lsn` 端到端；`--to-lsn 0` 非零退出。
