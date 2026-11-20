# M2 恢复：归档 checkpoint 与 PITR

状态：`checkpoint_archive` / `recover_at` 已实现；WAL 段轮转、后台 checkpoint、跨进程锁仍未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-txn/src/lib.rs        checkpoint_archive / recover_at / PitrView
crates/tritdb/src/main.rs           txn history / txn recover / txn checkpoint --archive --keep
crates/tritdb-txn/tests/pitr.rs      5 个库级测试
crates/tritdb-txn/tests/pitr_time.rs 3 个时间索引测试
crates/tritdb/tests/pitr_cli.rs      2 个 CLI 端到端测试
docs/ADR/032-pitr-wal-archive.md     归档与 PITR
docs/ADR/033-wal-segment-rotation.md WAL 段轮转
docs/ADR/034-time-index.md           时间索引
```

## 2. 用法

```bash
# 归档 checkpoint：写 snapshot + 把旧 WAL / snapshot 复制到 <wal>.history/<lsn>.{snap,wal}
# 默认只保留最新 8 个归档；--keep 0 关闭自动清理
tritdb txn checkpoint --wal test.wal --archive --keep 5

# 列出可用历史
tritdb txn history --wal test.wal
tritdb txn history --wal test.wal --prune --keep 5

# 恢复到指定 LSN（只读；不修改 live store）
tritdb txn recover --wal test.wal --to-lsn 42
```

## 3. 语义

- 选择 `checkpoint_lsn <= target_lsn` 的最大归档 snapshot 作为 base；
- 重放 `base < lsn <= target` 的记录，来源是后续归档 WAL + 当前 WAL，按 LSN 排序去重；
- `Commit` 在 target 之后的事务整个丢弃，不会半应用；
- target 早于最早归档 → `PitrBeforeHistory`；
- 归档缺 snap/wal → `PitrIncompleteArchive`；
- `<wal>.timeidx` 记录 LSN→墙钟毫秒，`recover_at_time` / `txn recover --to-time-ms` 选择 `ts <= target` 的最大 LSN，再走 `recover_at`；
- 时间索引缺失或没有更早条目 → `TimeIndexUnavailable`，不静默退化成按 LSN 恢复。

## 4. 验收

```bash
cargo test -p tritdb-txn --test pitr
cargo test -p tritdb-txn --test pitr_time
cargo test -p tritdb --test pitr_cli
```

本机结果：

```text
pitr: 5 passed
pitr_time: 3 passed
pitr_cli: 2 passed
```

## 5. 还没做

- 自动 GC 的保留数默认 8（`checkpoint_archive_keep` / `--keep` 可改，0 关闭）；
- 时间索引只能写入时维护，无法从 WAL 重建；崩溃可能落后最后几条；
- WAL 段轮转已在同一文件内实现（ADR-033），但不能按段释放文件空间；
- 并发 append 下 `recover_at` 读安全前缀，并对 checkpoint 截断重试 3 次；需要跨 checkpoint 的线性一致视图时用 `TxnStore::recover_at_consistent`（持 WAL 锁）。`pitr_concurrent` 覆盖两种压力。
