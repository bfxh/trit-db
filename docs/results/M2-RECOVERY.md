# M2 恢复：归档 checkpoint 与 PITR

状态：`checkpoint_archive` / `recover_at` 已实现；WAL 段轮转、后台 checkpoint、跨进程锁仍未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-txn/src/lib.rs        checkpoint_archive / recover_at / PitrView
crates/tritdb/src/main.rs           txn history / txn recover / txn checkpoint --archive
crates/tritdb-txn/tests/pitr.rs     4 个库级测试
crates/tritdb/tests/pitr_cli.rs     1 个 CLI 端到端测试
docs/ADR/032-pitr-wal-archive.md    决策与边界
```

## 2. 用法

```bash
# 归档 checkpoint：写 snapshot + 把旧 WAL / snapshot 复制到 <wal>.history/<lsn>.{snap,wal}
tritdb txn checkpoint --wal test.wal --archive

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
- 归档缺 snap/wal → `PitrIncompleteArchive`。

## 4. 验收

```bash
cargo test -p tritdb-txn --test pitr
cargo test -p tritdb --test pitr_cli
```

本机结果：

```text
pitr: 4 passed
pitr_cli: 1 passed
```

## 5. 还没做

- 自动触发的归档 GC（当前只提供手动 `prune_history` / `txn history --prune`）；
- 时间戳 → LSN 索引（当前只接受 `--to-lsn`）；
- WAL 段轮转（归档仍是单段文件）；
- 在 `TxnStore` 打开状态下提供一致性快照读（当前 `recover_at` 读取 live WAL 时假设没有并发写者）。
