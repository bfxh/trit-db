# ADR 033：WAL 段轮转在同文件内追加段头

状态：**Superseded by ADR-036**（本文保留「段头链」的解析能力，但轮转已改为多文件）

日期：2026-11-21

## 背景

TRWL v0 的文件结构是「一个 32 字节段头 + 一串记录」。写入端没有段边界：WAL 会一直增长，归档、恢复和外部工具都只能把整个文件当成一个段处理。M2 需要「段」这个单位来控制单段大小，并为后续按段归档/删除留出边界。

选择点：

1. 轮转产生新文件（`<wal>.<lsn>.seg`），还是同文件内追加段头？
2. 归档是否跟着变成多文件/段包？
3. 崩溃窗口怎么处理？

## 决策

**同文件内追加段头**。`WalWriter` 维护三个字段：

- `segment_first_lsn`：当前段的起始 LSN；
- `segment_bytes`：当前段已写字节数；
- `segment_limit`：段大小上限，默认 [`DEFAULT_SEGMENT_LIMIT`] = 8 MiB，测试可用 `open_with_limit` 指定。

`append` 在写入前检查：若当前段非空且 `segment_bytes + record_len > segment_limit`，先 `sync`，再向文件末尾写一个新的 32 字节段头（`first_lsn = next_lsn`），然后继续写记录。

`parse_wal` 在任何偏移遇到 `TRWS` 就按段头解析并继续；段头之间的记录 LSN 仍然严格递增。`WalScan` 新增 `segments` / `last_segment_offset` / `last_segment_first_lsn`，恢复端据此计算当前段大小。

`reset(first_lsn)` 仍然清空整个文件并写一个段头，所有旧段一起消失。

归档保持**单文件**：`checkpoint_archive` 继续复制整个 WAL，`recover_at` 继续用 `WalReader` 逐段解析。PITR 的归档布局不变。

## 理由

- **归档/PITR 零改动**：段只是文件内的逻辑边界，归档复制、`parse_wal`、`recover_at` 都不需要新的段包格式。
- **崩溃语义简单**：追加段头和追加记录一样是顺序写；半写的段头落在文件尾部，被 `parse_wal` 标为 torn tail，`open` 截断后从最后一条有效 LSN 继续。
- **避免 Windows 文件句柄与 rename 的坑**：多文件轮转需要在写入中途关闭/重命名活动文件，崩溃窗口和句柄共享都更复杂。
- **LSN 仍是全局单调**：段头不重置 LSN 计数器，恢复端不需要拼段。

## 后果

正面：

- 单段大小受控，`parse_wal` / 归档 / 恢复都有明确的段边界。
- `WalScan.segments` 可以直接观测轮转；测试用小 `segment_limit` 覆盖轮转、重开、reset、torn 段头。
- 现有 WAL/PITR 测试与格式向量不变（单段文件仍然合法）。

负面：

- **不能按段释放文件空间**：旧段仍在同一个文件里，只能靠 `reset` 或归档 GC 清理。真正的「多文件段 + 按段删除」留给后续 ADR。
- `WalScan.valid_end` / `file_len` 仍是整个文件的字节数，不是当前段；需要当前段大小时用 `last_segment_offset` 计算。
- 段大小上限是写入端策略，不是格式强约束；外部工具可以写入任意大的单段，`parse_wal` 仍接受。

## 验收

```bash
cargo test -p tritdb-wal
cargo test -p tritdb-wal --test rotation
```

- `rotation_adds_segment_headers_and_keeps_records_readable`：40 条记录、2048 字节段上限 → 多个段头，重开后 LSN 从 41 继续。
- `reset_drops_all_segments`：reset 后只剩一个段头，`first_lsn = 100`。
- `torn_new_segment_header_is_repaired`：手工追加半个段头 → `TailState::Torn`，重开后从 21 继续。
