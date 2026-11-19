# ADR 004：checkpoint 用 sidecar，先写 checkpoint 再重置 WAL

状态：**Proposed**

日期：2026-11-20

## 背景

页存储的提交协议一直是“页镜像写 WAL → fsync → 写数据文件 → fsync”。这保证崩溃恢复正确，但 WAL 只增不减：

- 256 个单页事务后，data 2,097,152 字节，WAL 2,123,808 字节；
- 每次打开都要扫描并重放全部已提交页；
- WAL 里既有已经落盘的页镜像，也有恢复所需的元数据，不能简单删除。

需要在不破坏“已提交页至少有一份持久副本”的前提下，把已经落盘的页镜像从恢复路径中移除。

## 决策

新增 `<data>.ckpt` sidecar，格式为 32 字节 `TRCK`：

| offset | field |
|---|---|
| 0..4 | magic `TRCK` |
| 4 | version `0` |
| 5..8 | reserved 0 |
| 8..16 | `checkpoint_lsn`，小端 u64 |
| 16..24 | `page_count`，小端 u64 |
| 24..28 | reserved 0 |
| 28..32 | CRC32 IEEE，覆盖 `0..28` |

`PageStore::checkpoint()` 的顺序固定为：

1. 持有 WAL 写者锁，等待所有活动事务结束；
2. 对 data 文件 `sync_all`；
3. 计算 `checkpoint_lsn = wal.next_lsn() - 1` 和 `page_count = data_len / 8192`；
4. 写临时 sidecar、`sync_all`、rename 到 `<data>.ckpt`；
5. `wal.reset(checkpoint_lsn + 1)`，清空旧 WAL 并写新的段头；
6. 释放写者锁，返回 `CheckpointInfo`。

恢复时：

- 先读 sidecar；不存在则 `checkpoint_lsn = 0`；
- 扫描 WAL；
- 跳过 `lsn <= checkpoint_lsn` 的记录；
- 只重放 checkpoint 之后的已提交页；
- sidecar 的 `page_count` 大于数据文件页数时返回 `CheckpointPageCount`；
- sidecar 损坏时返回 `BadCheckpoint`，不静默回退。

WAL 的 `reset` 只重置单段；崩溃把文件留成 0 字节或不足 32 字节时，`WalWriter::open` 修复为空段。

## 理由

- **sidecar 先于 WAL reset**：崩溃在两步之间时，旧 WAL 仍在，但 sidecar 已声明数据文件覆盖到 `checkpoint_lsn`；恢复跳过旧页，不会回退。
- **WAL reset 在后**：只有数据文件已经 fsync，才能丢弃页镜像；顺序反过来会失去唯一副本。
- **短 WAL 自动修复**：reset 的 `set_len(0)` 与写段头不是原子操作；短文件不可能包含有效记录，修复为空段比报错更符合崩溃语义。
- **sidecar 是独立文件**：不改 TRWL 记录格式，不把 checkpoint 混进页镜像 WAL；恢复逻辑可以单独测试。
- **page_count 校验**：sidecar 声明“数据文件至少这么多页”，比只存 LSN 多一道完整性检查。

## 后果

正面：

- 256 个单页事务后，WAL 从 2,123,808 字节降到 32 字节；重开从 18.959 ms 降到 2.649 ms（约 7.2×）。
- 32 个单页事务后，WAL 从 265,504 字节降到 32 字节；重开从 4.933 ms 降到 3.738 ms（约 1.3×，WAL 太小，固定成本占比高）。
- checkpoint 本身约 10–16 ms 量级（32 事务轮次曾抖动到 29.738 ms），远低于继续让 WAL 增长的恢复成本。

负面：

- checkpoint 是单写者停顿点：它要等当前事务结束，并持有 WAL 锁完成 data fsync、sidecar 写和 WAL reset。
- checkpoint 只重置单段，不做段轮转；WAL 不能保留 checkpoint 之前的历史。
- sidecar 没有记录数据文件身份/代次；M0 假定 data 路径不被替换。
- sidecar rename 后没有目录 fsync；Windows 上没有等价的目录持久化原语。
- 页存储仍没有组提交；checkpoint 不解决 3–5 ms 的单事务提交延迟。键值层的提交组见 ADR 006。

## 备选方案

1. **只记录 checkpoint LSN，不截断 WAL**：恢复可以跳过旧页，但 WAL 继续增长，扫描成本仍在。放弃作为最终方案。
2. **先 reset WAL 再写 sidecar**：崩溃会同时丢失旧 WAL 和 checkpoint，已提交页没有第二副本。放弃。
3. **把 checkpoint 写进 TRWL 记录**：需要新增记录类型和恢复起点，且旧 WAL 被截断后 marker 也会消失；sidecar 更直接。
4. **用数据文件头部 superblock**：会改变 `N * 8192` 布局和页 0 语义；M0 不值得。
5. **立即做 WAL 段轮转**：需要多段命名、段选择、旧段回收和 checkpoint 协调；先把单段 reset 跑通。

## 后续

- WAL 段轮转：保留多个段，checkpoint 只回收已覆盖段。
- checkpoint 期间不停写：把 data fsync 与 WAL reset 拆成后台步骤，缩短写者停顿。
- 组提交：多事务共享一次 WAL fsync；这需要把单写者事务模型改成可并发提交。
- sidecar 增加 data 代次/长度校验，防止 data 文件被替换后误用 checkpoint。
