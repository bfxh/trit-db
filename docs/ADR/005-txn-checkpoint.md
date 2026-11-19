# ADR 005：键值存储 checkpoint 与 TRSN 快照

状态：**Proposed**

## 背景

`tritdb-store` 已经有页存储 checkpoint：32 字节 `TRCK` sidecar + WAL reset。`tritdb-txn` 是另一条 WAL 消费者，状态在内存 `BTreeMap`，没有页文件，也没有快照；WAL 会一直增长，每次打开都要重放全部已提交事务。两条消费者的恢复成本和持久化边界不一致。

需要一个不引入页管理、也不改 TRWL 记录格式的最小方案：把已提交键值状态持久化，再重置 WAL。

## 决策

- 新增 TRSN v0 快照文件 `<wal>.snap`，32 字节头 + 按 key 升序的键值 payload + CRC32。
- `TxnStore::checkpoint()` 的顺序固定为：
  1. 持有 WAL 写者锁；
  2. 持有状态读锁；
  3. 序列化全部键值，写 `.snap.tmp`，`sync_all`，rename 覆盖 `.snap`；
  4. `wal.reset(checkpoint_lsn + 1)`；
  5. 返回 `CheckpointInfo`。
- 恢复：先读快照，再从 WAL 中跳过 `LSN <= checkpoint_lsn` 的记录，只重放之后的记录。
- 快照损坏硬失败 `BadSnapshot`；快照缺失时走全量 WAL 恢复；WAL reset 留下的短文件由 WAL 层修复。

## 理由

- **快照先于 WAL reset**：这是页存储 checkpoint 已经验证过的顺序。反序会在 crash 后出现“旧 WAL 已清、状态没有副本”的窗口。
- **LSN 跳过而不是重放全部旧记录**：快照已经覆盖到 `checkpoint_lsn`，旧记录重放只会做无用功；跳过还能让恢复读数直接反映 checkpoint 的效果。
- **独立 TRSN，不复用 TRCK**：TRCK 只有页数，没有键值；把键值塞进页 sidecar 会把两种恢复单位混在一起。
- **整包快照**：M0 状态在内存，整包序列化最简单，也能直接量出快照大小和 checkpoint 成本。增量/分块留到状态大到不能整包复制时再做。
- **checkpoint 要求没有活跃事务**：checkpoint 会 reset WAL；如果还有逻辑事务持有未提交的 `Begin`/`Put`，reset 后再提交会留下没有 `Begin` 的 `Commit`，恢复报 `MissingBegin`。因此 `TxnStore::checkpoint` 先检查 `active_txns == 0`，否则返回 `TxnError::ActiveTxns`；它不等待活跃事务结束。提交组见 ADR 006。

## 后果

正面：

- `tritdb-txn` 的 WAL 可以被截到 32 字节，恢复不再随历史线性增长。
- 1,024 事务 × 4 key：WAL 277,872 → 32 字节，snapshot 81,264 字节，checkpoint 6.157 ms，重开 1.665 → 1.066 ms。
- 64 事务 × 4 key：WAL 16,848 → 32 字节，snapshot 4,560 字节，checkpoint 6.048 ms；此时 WAL 太小，重开 0.387 → 0.283 ms 几乎无收益。

负面：

- checkpoint 是单写者停顿点，持有 WAL 锁和状态读锁；活跃事务存在时直接拒绝。
- 快照整包在内存构造，峰值内存约状态的两倍；大状态会先撞内存。
- 每次 checkpoint 重写全部键值，没有增量。
- 快照缺失且 WAL 已 reset 时无法恢复；M0 不提供回退命令。
- 没有父目录 fsync；Windows 上 rename 覆盖已通过重复 checkpoint 测试，断电目录项持久性未验证。
- TRSN 只服务键值层，和页存储的 TRCK 是两套格式，未来若要统一需要另立 ADR。

## 备选方案

1. **只重放 WAL，不做快照**：实现最简单，但 WAL 和恢复时间无限增长；放弃。
2. **把键值状态写进页存储**：可以复用页文件和 checkpoint，但 `tritdb-txn` 会依赖 `tritdb-store`，把两条本应独立的 WAL 消费者合并；推迟到表堆/索引阶段再评估。
3. **逻辑 WAL 压缩/截断**：需要知道哪些 key 的旧记录可以被覆盖，还要处理删除墓碑；M0 没有版本链，放弃。
4. **先 reset WAL 再写快照**：crash 窗口会丢状态；放弃。
5. **快照增量写**：需要代际、父快照和合并规则；当前状态不大，推迟。

## 后续

- 组提交已由 ADR 006 实现；checkpoint 仍要求没有活跃事务。
- 快照流式写入，避免整包复制。
- 增量/差分快照与代际保留。
- WAL 段轮转与后台 checkpoint。
- 跨进程文件锁。
- 页存储与键值存储是否共享一套快照/恢复框架，等表堆落地后再决定。
