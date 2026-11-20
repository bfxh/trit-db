# ADR 036：WAL 段轮转改为多文件段

状态：**Proposed**（取代 ADR-033 的「同文件段头」）

日期：2026-11-21

## 背景

ADR-033 把段轮转实现为「同一文件内追加 32 字节段头」。它能控制逻辑段大小，但旧段仍留在同一个文件里：`reset` 只能清空整个文件，归档 GC 也只能删整个归档，**无法按段释放空间**。M2 需要真正的多段文件。

## 决策

- 活动段仍然是 `<wal>`（保持所有现有路径/工具兼容）。
- 轮转时把活动段重命名为 `<wal>.<first_lsn>.seg`，然后新建一个空的 `<wal>`（段头 `first_lsn = next_lsn`）。
- 读取时合并「所有 `<wal>.*.seg`（按 first_lsn 升序）+ `<wal>`」，校验段头 LSN 与记录 LSN 全局连续。
- `reset(first_lsn)` 删除所有 rotated 段，只保留一个新的活动段。
- `segment_paths()` 暴露全部段，供归档使用。
- **归档仍是单文件**：把各段按顺序拼接成 `<lsn>.wal`。每个段自带 `TRWS` 头，`parse_wal` 已支持一段接一段的段头序列，因此 `recover_at` 无需新格式。

## 崩溃窗口

- 轮转先 `sync` 活动段、关闭句柄，再 `rename`，再新建活动段。
- 崩在 `rename` 后、新建前：`<wal>` 不存在，但 rotated 段完整；`open` 检测到没有活动段，用 `next_lsn` 新建一个空段。
- 崩在新建段头半写：`open` 把 `< 32` 字节的活动段视为空段并重建。
- 归档拼接通过临时文件 + `rename`，不会留下半份归档。

## 理由

- **可释放空间**：`reset` 和归档 GC 能真正删掉旧段文件。
- **兼容**：活动段路径不变；旧单段文件仍然是最简单的「零 rotated 段」形态。
- **归档格式不变**：`parse_wal` 的多段头能力（ADR-033）直接复用，`recover_at` 不需要改。

## 后果

正面：

- 段文件数量可控；`tritdb-wal/tests/rotation.rs` 直接断言 `.seg` 文件存在/被删除。
- `checkpoint_archive` 归档 6 个 rotated 段 + 活动段，归档文件含 7 个段头，`WalReader` 能读出全部记录。

负面：

- 文件数增加；目录里会同时出现 `<wal>` 和 `<wal>.<lsn>.seg`。
- Windows 上必须关闭文件句柄后才能 `rename`，`WalWriter::file` 因此是 `Option<File>`。
- 零散小段会产生更多文件系统元数据；默认 8 MiB 段大小仍是权衡。
- 还没有后台 compaction / 段合并；旧段只在 `reset` 或归档 GC 时消失。

## 验收

```bash
cargo test -p tritdb-wal
cargo test -p tritdb-txn --test pitr
```

- `rotation_adds_segment_headers_and_keeps_records_readable`：40 条记录、2048 字节段上限 → 生成 `.seg`，重开后 LSN 连续。
- `reset_drops_all_segments`：reset 后 `.seg` 为 0。
- `multi_file_segments_are_archived_and_recoverable`：6 个 rotated 段 + 活动段归档成一个文件（7 个 `TRWS` 头），`recover_at` 完整恢复 40 个键。
