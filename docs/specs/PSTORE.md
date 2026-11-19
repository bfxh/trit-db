# TRPS v0：页存储、页镜像 WAL 与 checkpoint（草案）

状态：**草案，未冻结**。实现位于 `crates/tritdb-store`；格式和语义变更必须先改本文档并补兼容性测试。

## 1. 范围

本文定义 M0 页存储的五件事：

1. 8 KiB 页数据文件的布局；
2. 页镜像如何写进 TRWL v0 WAL；
3. 单写者提交与崩溃恢复的顺序；
4. checkpoint sidecar 与 WAL reset；
5. 页缓存、锁顺序和明确不承诺的能力。

本文不定义 B+ 树页内布局、空闲页位图、WAL 段轮转、跨进程文件锁、MVCC 或快照隔离。

## 2. 数据文件

- 页大小固定为 8192 字节（`tritdb_page::PAGE_SIZE`）。
- 第 `N` 页从字节偏移 `N * 8192` 开始；没有 superblock，页 0 就是第一张数据页。
- 页内容使用 `TRPG` 页格式（16 字节头 + payload + CRC32），由 `tritdb-page` 校验。
- 正常关闭或恢复完成后，数据文件长度必须是 8192 的整数倍。
- 崩溃可能留下只写了一部分的尾页。`PageStore::open` 会把长度截断到最近的 8192 边界，并把被截掉的字节数记录为 `RecoveryInfo::data_tail_bytes`。
- 截断是安全的：提交协议保证页镜像先进入已 fsync 的 WAL，恢复会重放所有已提交页；数据文件里的半页不可能是唯一副本。
- M0 不维护空闲页位图，也不允许在文件尾部之外写入稀疏页 id。`PageTxn::write_page` 本身不禁止任意 id，但 CLI 的 `pstore put` 只允许更新已存在的页；新页必须走 `pstore alloc` / `PageTxn::allocate`。

## 3. 页镜像 WAL

页存储复用 `tritdb-wal` 的 TRWL v0 段和记录格式，不新增记录类型。

| 记录 | payload | 说明 |
|---|---|---|
| `Begin` | 空 | 开启页事务 |
| `Put` | `page_id u64 LE` + 8192 字节页 | 一张页镜像，共 8200 字节 |
| `Commit` | 空 | 事务提交；提交前 WAL 必须 fsync |
| `Abort` | 空 | 事务放弃；不写数据文件 |
| `Delete` | 不支持 | 页存储遇到 `Delete` 返回 `UnsupportedRecord` |

约束：

- 每个 `Put` 的页必须是 `Page::parse` 能通过 CRC 的完整 8192 字节页。
- `PageTxn::write_page` 对同一页 id 写多次时，WAL 保留多条 `Put`，内存 pending 只保留最后一次；恢复按 WAL 顺序重放，后者覆盖前者。
- 一笔事务的 `pages` 是 pending 中不同页 id 的数量，不是 `Put` 记录数。
- `PageTxn::allocate` 的 id 只在事务内递增，提交时才发布到 `PageStore.next_page_id`；abort 不泄漏 id、不制造空洞。
- 页镜像 payload 8200 字节，远小于 TRWL 的 64 MiB 单记录上限。
- 页存储 WAL 不使用 `Delete`；M0 也不支持删除页或回收页 id。

## 4. 提交协议

`PageTxn::commit` 的顺序固定为：

1. 追加 `Commit` 记录，并对 WAL 执行 `fsync`；
2. 持有 data 文件锁，按 pending 中的页逐张写入数据文件；
3. 对 data 文件执行 `sync_all`；
4. 更新共享页缓存；
5. 推进 `page_count` 与 `next_page_id`；
6. 释放 WAL 写者锁。

关键不变量：

- **WAL 先于数据文件持久化**。崩溃点落在 1 之后、3 之前时，恢复能从 WAL 重放整笔事务的页。
- **写者锁覆盖状态应用**。下一个写者不能在这个事务写完数据文件、更新缓存和 `page_count` 之前越过它。
- **读者不见半提交**。页只有在数据文件 fsync 和缓存更新完成后才对读者可见；提交前读者读不到 pending 页。
- **一次提交两次 fsync**：WAL 一次、data 一次。页存储 M0 不做组提交；`tritdb-txn` 的提交组见 ADR 006，页存储的提交路径仍是后续优化项。

`abort` 只追加 `Abort` 并丢弃 pending，不碰数据文件。没有 `Commit` 的 `Begin/Put` 在恢复时整笔丢弃。

## 5. 恢复

`PageStore::open(data, wal)` 的步骤：

1. 打开或创建数据文件，截断不是 8192 整数倍的尾部；
2. 读取 `<data>.ckpt` checkpoint sidecar（不存在则视为无 checkpoint）；
3. `WalWriter::open` 打开 WAL；WAL 层会截断尾部 torn record，并把短于 32 字节的文件修复为空段；
4. `WalReader` 扫描全部有效记录；
5. 跳过 LSN 小于等于 `checkpoint_lsn` 的记录；
6. 按 `txn_id` 聚合剩余的 `Begin/Put/Commit/Abort`；
7. 只对带 `Commit` 的事务重放页镜像到数据文件；
8. 未提交的 `Begin/Put` 丢弃，计入 `discarded_txns`；
9. 对 data 文件 `sync_all`；
10. 用数据文件页数初始化 `page_count`，用已见最大页 id 和最大事务 id 初始化分配器。

`RecoveryInfo` 字段：

| 字段 | 含义 |
|---|---|
| `records` | checkpoint 之后参与恢复的 WAL 记录数（Begin/Put/Commit/Abort 都算） |
| `records_skipped` | 因 LSN 小于等于 checkpoint 而跳过的记录数 |
| `checkpoint_lsn` | sidecar 覆盖的最后 LSN；`0` 表示无 checkpoint |
| `committed_txns` | 有 `Commit` 并已重放的事务数 |
| `aborted_txns` | 有 `Abort` 的事务数 |
| `discarded_txns` | 有 `Begin` 但没有 `Commit`/`Abort` 的事务数 |
| `pages_replayed` | 重放到数据文件的页镜像数 |
| `torn_tail_bytes` | WAL 尾部 torn record 的字节数 |
| `data_tail_bytes` | 数据文件被截掉的半页字节数 |
| `data_pages` | 恢复后的数据文件页数 |
| `last_lsn` | 最后已知的持久 LSN：WAL 尾或 checkpoint |

有 checkpoint 时，恢复只处理 checkpoint 之后的记录；`pages_replayed` 不再包含已经被 checkpoint 覆盖的页。没有 checkpoint 时，每次打开都会重放所有已提交页。

## 6. Checkpoint

`PageStore::checkpoint()` 是单写者操作；它会等待当前 `PageTxn` 结束，期间没有活动事务。

1. 持有 WAL 写者锁；
2. 对数据文件执行 `sync_all`；
3. 令 `checkpoint_lsn = wal.next_lsn() - 1`，`page_count = data_len / 8192`；
4. 原子写 sidecar：临时文件 `sync_all` → rename 覆盖 `<data>.ckpt`；
5. `wal.reset(checkpoint_lsn + 1)`：清空旧段，写新的 32 字节段头；
6. 返回 `CheckpointInfo { checkpoint_lsn, page_count, wal_bytes_before, wal_bytes_after }`。

**顺序不能反**：sidecar 必须先于 WAL reset 持久化。否则 crash 后旧 WAL 被清掉、又没有 checkpoint，已提交页可能失去唯一副本。

Sidecar 固定 32 字节：

| offset | size | field | v0 |
|---|---:|---|---|
| 0 | 4 | magic | `TRCK` |
| 4 | 1 | version | `0` |
| 5 | 3 | reserved | 必须为 `0` |
| 8 | 8 | checkpoint LSN | 小端 `u64`；`0` 表示空 WAL |
| 16 | 8 | page count | 小端 `u64` |
| 24 | 4 | reserved | 必须为 `0` |
| 28 | 4 | CRC32 (IEEE) | 覆盖 `0..28` |

崩溃窗口：

- **sidecar 已写、WAL 未 reset**：旧 WAL 仍在；打开时读 sidecar，跳过 `LSN <= checkpoint_lsn` 的记录，不会把旧页覆盖到新数据上。
- **WAL reset 进行中**：文件可能是 0 字节或不足 32 字节；`WalWriter::open` 把短文件修复为空段。sidecar 仍然说明数据文件已覆盖到 `checkpoint_lsn`。
- **WAL reset 已完成**：新段 `first_lsn = checkpoint_lsn + 1`，没有旧记录；恢复只读数据文件和 sidecar。
- **sidecar 缺失或损坏**：缺失则全量重放；损坏返回 `BadCheckpoint`，不猜测、不静默回退。
- **sidecar page count 大于数据文件页数**：返回 `CheckpointPageCount`，说明数据文件丢了已覆盖页，不能继续。

M0 的 checkpoint 只重置单个 WAL 段，不做段轮转；旧 WAL 在 reset 时整体丢弃。checkpoint 也不清理页缓存。

## 7. 缓存与并发

- 缓存是 `RwLock<HashMap<PageId, Page>>`；命中直接返回克隆页，不读盘。
- 未命中时先释放 cache 读锁，再拿 data 锁读整页，最后拿 cache 写锁填入；避免 writer 持 data 等 cache、reader 持 cache 等 data 的锁环。
- 写者锁顺序是 `WAL -> data -> cache`；读者只在 cache 命中时拿 cache 读锁。
- 缓存无上限、无淘汰、无页版本；M0 不是 buffer pool。
- `PageStore` 是 `Send + Sync`；`PageTxn` 持有 WAL `MutexGuard`，不是 `Send`。
- 没有跨进程文件锁。两个进程同时打开同一对 data/WAL 会互相破坏，M0 不支持。
- `checkpoint` 要拿 WAL 写者锁；同一线程持有 `PageTxn` 时再调用 `checkpoint` 会自锁。必须在事务外调用。
- `checkpoint` 不会刷新 `PageStore::recovery()` 里的旧报告；要看 checkpoint 后的恢复状态，重开 store 或读 CLI `pstore dump`。
- 没有快照隔离。单次 `read_page` 只保证该页来自某个已提交版本；M0 没有跨页快照或批量一致读 API，调用方不能把多次 `read_page` 拼成一个一致视图。

## 8. 错误

`StoreError`：

- `Io`：底层文件错误；
- `Page`：页格式/CRC 错误；
- `Wal`：TRWL 格式/恢复错误；
- `BadDataLength`：保留给显式长度校验；当前打开路径改为截断半页尾；
- `MalformedPagePayload`：页镜像 payload 不是 8200 字节；
- `PageIdOverflow`：页 id 乘 8192 溢出 `u64`；
- `UnsupportedRecord`：页存储 WAL 出现 `Delete`，或空 payload 记录带了 payload；
- `UnknownTxn`：`Put` 没有对应 `Begin`；
- `DuplicateBegin`：同一事务重复 `Begin`；
- `MissingBegin`：`Commit`/`Abort` 没有对应 `Begin`；
- `BadCheckpoint`：sidecar 长度、magic、version、reserved 或 CRC 错误；
- `CheckpointPageCount`：sidecar 的页数大于恢复后的数据文件页数；
- `Poisoned`：锁中毒。

## 9. 已验证

- 12 个单元测试：提交/重开、未提交不应用、abort 丢弃、abort 不泄漏页 id、缓存命中、多读者、页 id 单调、半页尾截断、`Send + Sync`、checkpoint 截断 WAL、checkpoint 跳过残留旧 WAL、checkpoint 覆盖已有 sidecar。
- `crates/tritdb/tests/pstore_crash_recovery.rs`：子进程每事务分配一页，父进程读到第一行后 30 ms 硬杀，连续 5 次；断言页数等于已提交事务数、页 id 连续、payload 与页 id 对齐、`discarded_txns <= 1`。
- 一次硬杀读数：`pages=5 records=15 committed=5 discarded=0 replayed=5 data_tail=0 wal_torn=0`，其余 4 次为 4/7/3/6 页，全部通过。
- 基准（release，6 轮 median）：1 页 4096 payload，`write_commit_ms=5.040`、`read_hit_ms=0.001`、data 8192 字节、WAL 8328 字节；8 页 4096 payload，`write_commit_ms=5.327`、data 65536 字节、WAL 65952 字节。两次 fsync 主导提交成本，8 页相对 1 页只多约 0.29 ms。
- checkpoint 基准（release）：32 个单页事务、6 轮 median，WAL 265,504 → 32 字节，checkpoint 16.160 ms，重开 4.933 → 3.738 ms；256 个单页事务、3 轮 median，WAL 2,123,808 → 32 字节，checkpoint 9.940 ms，重开 18.959 → 2.649 ms。32 事务的 WAL 太小，收益受固定成本和抖动影响；256 事务约 7.2×。

## 10. 未做

- WAL 段轮转（当前 checkpoint 只重置单段）；
- 组提交；
- 页分配位图、页回收、vacuum；
- 跨进程文件锁；
- 页级 MVCC/快照隔离；
- B+ 树页内布局、索引页分裂/合并；
- 多文件表空间和页校验和分层。
