# ADR 034：时间索引用 sidecar，写入时追加 LSN→时间戳

状态：**Proposed**

日期：2026-11-21

## 背景

PITR 已经能按 LSN 恢复，但调用方通常只有墙钟时间（「恢复到 10:30 之前」）。TRWL v0 的记录头没有时间戳字段；改格式会破坏冻结的格式向量和跨语言参考实现。

选择点：

1. 改 WAL 记录格式加时间戳，还是单独维护索引？
2. 索引丢了能不能从 WAL 重建？
3. checkpoint/reset 后索引怎么处理？

## 决策

新增 sidecar `<wal>.timeidx`：

```text
header: "TRTI" (4) + version 0 (1) + reserved (3)
entry : lsn u64 LE + ts_ms u64 LE   （16 字节，可重复）
```

- `WalWriter::open_with_time_index` 打开 WAL 时同时打开索引；
- 每次 `append` 成功后在索引末尾追加 `(lsn, now_ms())`，**不 fsync**；
- 时间戳做单调钳制（`ts = max(ts, last_ts)`），时钟回拨不会破坏查找；
- 读取时忽略尾部不完整条目；magic/version 不合法则视为空索引；
- `reset(checkpoint_lsn + 1)` **保留**索引（LSN 继续增长，旧条目仍有效）；只有 LSN 回退（主要是测试）才清空重建；
- `lookup_time(wal_path, ts_ms)` 返回 `ts <= ts_ms` 的最大 LSN；
- `TxnStore::recover_at_time(path, ts_ms)` 先 `lookup_time`，再复用 `recover_at`；没有条目时返回 `TimeIndexUnavailable`，不猜。

## 理由

- **不动 WAL 格式**：格式向量、Python 参考实现、跨语言对拍全部保持不变。
- **写入路径只多一次顺序写**：索引不 fsync，提交延迟不变；最坏丢几条时间映射。
- **缺失索引必须报错**：从 WAL 无法重建时间戳，静默退化成「按 LSN 恢复」会让调用方以为按时间恢复了。
- **保留跨 checkpoint**：LSN 在 reset 后继续增长，旧时间条目仍然指向有效的已归档 LSN。

## 后果

正面：

- 可以用 `--to-time-ms` 恢复到某个墙钟时间点。
- 索引损坏/缺失有明确错误，不会返回错误状态。
- 与归档 GC、WAL 段轮转互不影响。

负面 / 限制：

- **索引不能从 WAL 重建**：WAL 文件被删除或时间索引丢失后，只能按 LSN 恢复。
- **崩溃可能落后**：索引比 WAL 少最后几条记录，恢复点是「最后一条已索引的 Commit」，偏保守。
- **时间戳精度取决于系统时钟**：同一毫秒内的多个事务会映射到同一个恢复点。
- 索引没有 CRC；只校验 magic/version 和 LSN 单调，尾部损坏靠截断。

## 验收

```bash
cargo test -p tritdb-txn --test pitr_time
cargo test -p tritdb --test pitr_cli
```

- 6 个事务、每 10 ms 一个 → 恢复到第 4 个时间戳时恰好前 4 个键。
- 删除 `.timeidx` → `TimeIndexUnavailable`。
- 普通 checkpoint 后索引仍能恢复到 checkpoint 前的时间点。
- CLI：`txn recover --to-time-ms <unix_ms>` 端到端。
