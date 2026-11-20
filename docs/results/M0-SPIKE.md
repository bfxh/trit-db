# M0 Spike：radix + page + TRBL + WAL + 页存储/键值 checkpoint + 提交组

状态：radix 查表、page、TRBL、WAL/txn、页存储 checkpoint、键值快照 checkpoint、提交组与硬杀恢复已实现；r3/r4 二进制基线已加。
日期：2026-11-20

## 机器 / 命令

- CPU：Intel(R) Core(TM) Ultra 7 270K Plus
- OS：Microsoft Windows NT 10.0.28000.0
- 工具链：`rustc 1.99.0`、`cargo 1.99.0`
- MSRV：`cargo +1.88 fmt --check` / `clippy -D warnings` / `test --workspace` 全绿（1.88 工具链已本机验证）
- Profile：`--release`
- 轮次：6；表中为 median，括号内为 min/max。

```bash
cargo run -q -p tritdb-bench --release -- radix --trits 1000000 --rounds 6
cargo run -q -p tritdb-bench --release -- baseline --trits 1000000 --rounds 6
cargo run -q -p tritdb-bench --release -- block --trits 1000000 --rounds 6 --radix r3
cargo run -q -p tritdb-bench --release -- page --payload 4096 --rounds 6
cargo run -q -p tritdb-bench --release -- wal --records 1000 --payload 64 --rounds 6
cargo run -q -p tritdb-bench --release -- wal --records 100 --payload 64 --rounds 6 --fsync
cargo run -q -p tritdb-bench --release -- pstore --pages 1 --payload 4096 --rounds 6
cargo run -q -p tritdb-bench --release -- pstore --pages 8 --payload 4096 --rounds 6
cargo run -q -p tritdb-bench --release -- pstore-checkpoint --txns 32 --payload 4096 --rounds 6
cargo run -q -p tritdb-bench --release -- pstore-checkpoint --txns 256 --payload 4096 --rounds 3
cargo run -q -p tritdb-bench --release -- txn-checkpoint --txns 64 --keys-per-txn 4 --rounds 3
cargo run -q -p tritdb-bench --release -- txn-checkpoint --txns 256 --keys-per-txn 4 --rounds 3
cargo run -q -p tritdb-bench --release -- txn-checkpoint --txns 1024 --keys-per-txn 4 --rounds 3
cargo run -q -p tritdb-bench --release -- txn-group --txns 256 --threads 16 --keys-per-txn 4 --rounds 3
cargo run -q -p tritdb-bench --release -- txn-group --txns 1024 --threads 16 --keys-per-txn 4 --rounds 3
```

## r3 优化前后

| 版本 | pack median ms | unpack median ms |
|---|---:|---:|
| 逐 trit 乘加（旧） | 0.455 | 0.438 |
| 10-bit 查表 + 243 项 unpack 表（新） | **0.113** | **0.104** |
| 变化 | **-75.2%** | **-76.3%** |

- pack 表：`t0 | t1<<2 | t2<<4 | t3<<6 | t4<<8` → base-3 byte，1024 项（243 个合法 key）。
- unpack 表：byte `0..=242` → 5 个 trit，243 项。
- 优化后 r3 从“省空间但慢”变成“同口径下 pack/unpack 都快于 r4”。

## 全量读数

| codec | trits | bytes | B/trit | pack ms (min/med/max) | unpack ms (min/med/max) | roundtrip |
|---|---:|---:|---:|---|---|---|
| r3 | 1,000,000 | 200,000 | 0.200 | 0.112 / **0.113** / 0.113 | 0.104 / **0.104** / 0.109 | true |
| r4 | 1,000,000 | 250,000 | 0.250 | 0.165 / **0.170** / 0.179 | 0.435 / **0.436** / 0.465 | true |
| r16 | 1,000,000 | 500,000 | 0.500 | 0.423 / **0.428** / 0.433 | 0.437 / **0.439** / 0.441 | true |
| bin-u8 | 1,000,000 | 1,000,000 | 1.000 | 0.017 / **0.018** / 0.040 | 0.440 / **0.443** / 0.475 | true |
| bin-u2 | 1,000,000 | 250,000 | 0.250 | 0.165 / **0.167** / 0.184 | 0.440 / **0.440** / 0.445 | true |

基线口径：

- `bin-u8`：每个 trit 直接存 1 字节。pack 接近纯写入，是最低开销下界；但它不是可接受的存储格式。
- `bin-u2`：每个 trit 2 bit，和 r4 同布局；pack 不做 canonical padding 检查，是“最小二进制基线”。

## page 读数

| payload | page bytes | build ms (min/med/max) | parse ms (min/med/max) |
|---|---|---|---|
| 4,096 | 8,192 | 0.005 / **0.005** / 0.006 | 0.005 / **0.005** / 0.005 |

## TRBL 块读数

命令：

```bash
cargo run -q -p tritdb-bench --release -- block --trits 1000000 --rounds 6 --radix r3
```

| radix | block bytes | payload bytes | encode ms | parse ms | trits ms |
|---|---:|---:|---:|---:|---:|
| r3 | 200,020 | 200,000 | 0.493 | 0.286 | 0.108 |
| r4 | 250,020 | 250,000 | 0.688 | 0.403 | 0.439 |
| r16 | 500,020 | 500,000 | 1.412 | 0.742 | 0.443 |

口径：

- `encode` = pack + canonical 校验 + CRC + 分配块缓冲。
- `parse` = 头字段 + 长度 + CRC + canonical padding 校验；不含 trit 展开。
- `trits` = 仅 `unpack_into`，复用缓冲。
- 20 字节头在 1M trit 下是 0.01% 开销；块级 CRC 的代价主要体现在 parse 的 0.18 ms 量级。

## WAL / txn 读数

命令：

```bash
cargo run -q -p tritdb-bench --release -- wal --records 1000 --payload 64 --rounds 6
cargo run -q -p tritdb-bench --release -- wal --records 100 --payload 64 --rounds 6 --fsync
```

| mode | records | payload | file bytes | append ms (min/med/max) | replay ms (min/med/max) |
|---|---:|---:|---:|---|---|
| append（不 fsync） | 1,000 | 64 | 96,032 | 2.656 / **3.445** / 4.007 | 0.266 / **0.287** / 0.302 |
| fsync（每条记录） | 100 | 64 | 9,632 | 293.270 / **312.295** / 332.290 | 0.091 / **0.107** / 0.130 |

换算：

- append：3.445 ms / 1000 条 ≈ **3.45 µs/条**，约 290k 记录/s；96 KB / 3.445 ms ≈ 27.9 MB/s。
- fsync：312.295 ms / 100 条 ≈ **3.12 ms/条**，约 320 次 fsync/s。
- replay：0.287 ms / 1000 条 ≈ **0.287 µs/条**，约 3.48M 记录/s。

口径：

- `append` 只写 header + payload 到 page cache，不调用 `sync_all`；进程崩溃通常仍在，断电不保证。
- `fsync` 每条记录调用一次 `sync_all`，代表最坏的单事务提交路径。
- 文件字节数正好是 `32 + records * (32 + payload)`；TRWL 每条约 64 字节头开销。
- `replay` 包含读文件、CRC 校验、反序列化；不含把 payload 解码成 key/value。

硬杀恢复测试：

```bash
cargo test -p tritdb --test crash_recovery -- --nocapture
```

一次本机运行：子进程提交到第 9 个事务后被硬杀，重开得到：

```text
recovered committed=9 discarded=0 aborted=0 torn_bytes=0 keys=36 groups=9
```

- 事务数不是固定值；测试断言的是不变量：提交组完整、没有半组 key、`discarded_txns <= 1`。
- 这次没有 torn tail；torn tail 的截断路径由 WAL 单元测试和事务层单元测试覆盖。

## 页存储读数

命令：

```bash
cargo run -q -p tritdb-bench --release -- pstore --pages 1 --payload 4096 --rounds 6
cargo run -q -p tritdb-bench --release -- pstore --pages 8 --payload 4096 --rounds 6
```

| pages | payload | data bytes | WAL bytes | write+commit ms (min/med/max) | cached read ms (min/med/max) |
|---:|---:|---:|---:|---|---|
| 1 | 4,096 | 8,192 | 8,328 | 1.817 / **5.040** / 11.903 | 0.001 / **0.001** / 0.002 |
| 8 | 4,096 | 65,536 | 65,952 | 4.253 / **5.327** / 8.195 | 0.001 / **0.001** / 0.002 |

口径：

- `write+commit` 包含 `Begin`、每页 8200 字节的 `Put`、`Commit` + WAL fsync、写数据文件、data fsync、缓存发布；不含页对象构造。
- WAL 字节数：1 页 `32（段头）+ 32（Begin）+ 8200（Put）+ 32（Commit）= 8,328`；8 页为 `32 + 32 + 8×8200 + 32 = 65,952`。
- `cached read` 是提交后对页 0 的缓存命中，不读盘。
- 1 页和 8 页的 median 只差 0.287 ms：提交成本由两次 fsync 主导，页数在 8 页以内不是主项。

页存储硬杀恢复：

```bash
cargo test -p tritdb --test pstore_crash_recovery -- --nocapture
```

连续 5 次本机运行：

```text
attempt 0: pages=5 records=15 committed=5 discarded=0 replayed=5 data_tail=0 wal_torn=0
attempt 1: pages=4 records=12 committed=4 discarded=0 replayed=4 data_tail=0 wal_torn=0
attempt 2: pages=7 records=21 committed=7 discarded=0 replayed=7 data_tail=0 wal_torn=0
attempt 3: pages=3 records=9 committed=3 discarded=0 replayed=3 data_tail=0 wal_torn=0
attempt 4: pages=6 records=18 committed=6 discarded=0 replayed=6 data_tail=0 wal_torn=0
```

- 子进程每事务分配一页并提交，父进程读到第一行后 30 ms `kill()`。
- 断言页数等于已提交事务数、页 id 连续、payload `txn=N` 与页 id 对齐、`discarded_txns <= 1`。
- 5 次都没有 torn tail；半页数据尾由单元测试 `torn_data_tail_is_truncated` 覆盖：向数据文件追加 37 字节后重开，`data_tail_bytes=37`，已提交页仍可读。

## checkpoint 读数

命令：

```bash
cargo run -q -p tritdb-bench --release -- pstore-checkpoint --txns 32 --payload 4096 --rounds 6
cargo run -q -p tritdb-bench --release -- pstore-checkpoint --txns 256 --payload 4096 --rounds 3
```

| txns | payload | data bytes | WAL before | WAL after | write total ms (min/med/max) | checkpoint ms (min/med/max) | reopen before ms (min/med/max) | reopen after ms (min/med/max) |
|---:|---:|---:|---:|---:|---|---|---|---|
| 32 | 4,096 | 262,144 | 265,504 | 32 | 183.625 / **191.310** / 236.207 | 11.943 / **16.160** / 29.738 | 3.320 / **4.933** / 6.449 | 2.617 / **3.738** / 6.464 |
| 256 | 4,096 | 2,097,152 | 2,123,808 | 32 | 1,445.700 / **1,484.495** / 1,538.066 | 4.444 / **9.940** / 14.395 | 16.130 / **18.959** / 20.719 | 1.515 / **2.649** / 4.910 |

口径：

- 每个事务一页，`write total` 是所有事务提交的总时间，不是单事务延迟；单事务仍是 5 ms 量级。
- `checkpoint` 包含 data fsync、32 字节 sidecar 写入 + fsync + rename、WAL reset + fsync。
- `reopen before` 没有 checkpoint，重放全部页；`reopen after` 读 checkpoint、跳过旧页，WAL 只有 32 字节段头。
- 256 事务：WAL 2,123,808 → 32 字节（-99.998%），重开 18.959 → 2.649 ms（约 7.2×）。
- 32 事务：WAL 265,504 → 32 字节（-99.988%），重开 4.933 → 3.738 ms（约 1.3×）；32 事务的 WAL 太小，checkpoint 的固定成本占比高，且这轮 checkpoint 本身抖动到 16 ms，不能当稳定结论。
- checkpoint 是单写者停顿点；它不把 5 ms 的单事务提交变快，只把恢复路径和 WAL 体积压下来。

## 键值 checkpoint 读数

命令：

```bash
cargo run -q -p tritdb-bench --release -- txn-checkpoint --txns 64 --keys-per-txn 4 --rounds 3
cargo run -q -p tritdb-bench --release -- txn-checkpoint --txns 1024 --keys-per-txn 4 --rounds 3
```

| txns | keys | WAL before | WAL after | snapshot bytes | write total ms (min/med/max) | checkpoint ms (min/med/max) | reopen before ms (min/med/max) | reopen after ms (min/med/max) |
|---:|---:|---:|---:|---:|---|---|---|---|
| 64 | 256 | 16,848 | 32 | 4,560 | 177.394 / **202.237** / 211.893 | 5.828 / **6.048** / 6.477 | 0.278 / **0.387** / 0.394 | 0.280 / **0.283** / 0.284 |
| 256 | 1,024 | 68,784 | 32 | 19,632 | 793.941 / **797.623** / 804.327 | 6.173 / **6.302** / 57.534 | 0.587 / **0.600** / 0.631 | 0.457 / **0.459** / 0.461 |
| 1,024 | 4,096 | 277,872 | 32 | 81,264 | 2,928.654 / **2,954.827** / 2,971.021 | 4.763 / **6.157** / 6.209 | 1.516 / **1.665** / 1.670 | 0.996 / **1.066** / 1.072 |

口径：

- 每个事务 4 个 key；`write total` 是所有事务提交的总时间，单事务仍是 fsync 主导的毫秒级。
- checkpoint 包含状态读锁、整包 TRSN 序列化、快照 fsync + rename、WAL reset + fsync。
- 64 事务的 WAL 太小，重开 0.387 → 0.283 ms 基本无收益；1,024 事务重开 1.665 → 1.066 ms（约 1.56×）。
- 快照 81,264 字节比 WAL 277,872 字节小，但仍是状态全量副本；状态更大时快照体积和内存峰值会先成为问题。
- 快照只保留当前状态，不保留旧代际；checkpoint 前历史不可回放。

## 组提交读数

命令：

```bash
cargo run -q -p tritdb-bench --release -- txn-group --txns 256 --threads 16 --keys-per-txn 4 --rounds 3
cargo run -q -p tritdb-bench --release -- txn-group --txns 1024 --threads 16 --keys-per-txn 4 --rounds 3
```

| txns | threads | keys | WAL bytes | write total ms (min/med/max) | fsyncs (min/med/max) | fsync/txn |
|---:|---:|---:|---:|---|---|---:|
| 256 | 16 | 1,024 | 73,248 | 174.282 / **242.826** / 309.929 | 70 / **73** / 76 | 0.285 |
| 1,024 | 16 | 4,096 | 296,736 | 836.818 / **867.780** / 968.245 | 243 / **260** / 281 | 0.254 |

对比同一代码的顺序提交（`txn-checkpoint` 的写事务阶段，无 checkpoint）：

| txns | threads | write total median ms | fsync/txn |
|---:|---:|---:|---:|
| 256 | 1 | 797.623 | 1.000 |
| 1,024 | 1 | 2,954.827 | 1.000 |

- 256 事务：797.623 → 242.826 ms，约 **3.29×**；fsync 256 → 73，约 **3.5×** 减少。
- 1,024 事务：2,954.827 → 867.780 ms，约 **3.40×**；fsync 1,024 → 260，约 **3.9×** 减少。
- 收益来自“一批 Commit 共用一次 `sync_all`”，不是 WAL append 变快；WAL 仍被单写者锁串行化。
- 16 线程下 fsync/txn 约 0.25–0.29，说明组大小约 3–4；提高并发或让 leader 让出更多调度点可以增大组，但会增加单批延迟。
- 没有冲突检测：并发写同一 key 时按 WAL 顺序最后写入者获胜；这不是快照隔离。

## 负结果 / 观察

1. **r3 现在三项都赢**：比 r4 少 20% 字节，pack 快 1.50×（0.113 vs 0.170），unpack 快 4.19×（0.104 vs 0.436）。查表把逐 trit 的 `/3`、`%3` 全部消掉了。
2. **r3 vs bin-u2**：同样 20% 空间优势；pack 快 1.48×（0.113 vs 0.167），unpack 快 4.23×（0.104 vs 0.440）。r3 的 5-trit/byte 在扫描侧已经不只是“省空间”，也是更快路径。
3. **r3 vs bin-u8**：r3 省 80% 空间，但 pack 慢 6.28×（0.113 vs 0.018）。`bin-u8` 的 0.018 ms 是纯字节写入下界，不能拿来直接否定 r3；真实存储还要算 I/O、页、压缩和扫描。
4. **r4 现在偏尴尬**：同字节数下 bin-u2 pack 0.167，r4 0.170；但 r3 更小更快。r4 只在“不想维护 1024 项表 / 极简解码器”时有意义。
5. **r16 没有性能理由**：0.428 ms pack、0.439 ms unpack，字节是 r4 的 2×。只适合调试或 nibble 直读。
6. **TRBL 头很便宜，校验不便宜**：20 字节头在 1M trit 下占 0.01%；但 r3 `encode` 0.493 ms，比裸 pack 0.113 ms 多约 0.38 ms，主要是 canonical 二次校验 + CRC + 分配。
7. **TRBL parse 的固定成本**：0.286 ms vs 裸 `trits` 0.108 ms；头/长度/CRC/padding 校验约 0.18 ms。当前是“先校验再展开”，没有零拷贝流式 CRC。
8. **page 固定开销约 5 µs**：单页、无 I/O、无并发；不是 fsync 后的持久化数字。
9. **单机单次**：只代表这台机器、这个 commit、这个 release profile；没有 criterion、没有固定 CPU 频率、没有交错 A/B。下一轮要补齐。
10. **fsync 是提交路径的绝对瓶颈，提交组把它摊薄约 3.5×**：逐条 fsync 3.12 ms/次提交，比不 fsync 的 3.45 µs/条慢约 900×。M0 的提交组把 1,024 次事务 fsync 降到 260 次；没有组提交时单事务单 fsync 只能用于功能验证。
11. **不 fsync 不等于持久**：append 的 3.45 µs/条是 page cache 数字；进程崩溃通常还在，断电不保证。不要把 290k 记录/s 当成提交吞吐。
12. **replay 很便宜但不可扩展**：0.287 µs/条的前提是把整个 WAL 读进 `Vec` 再解析。页存储和键值存储的 checkpoint 都已把重放范围压到 checkpoint 之后，但 WAL 本身仍单段、无轮转；没有 checkpoint 时恢复时间和内存仍随历史线性上升。
13. **写者锁的粒度变了，但 append 仍串行**：`Txn` 不再持有 `MutexGuard`，多个逻辑事务可以交错 append/commit；WAL append 仍被单个 `Mutex<WalWriter>` 串行化。16 线程组提交验证了 fsync 摊薄，但没有证明 WAL 写入带宽可扩展。
14. **跨 key 读不是快照**：release 下 4 次独立 `get` 读同一组 4 个 key，可能读到 3 个旧值 + 1 个新值。这不是提交被打断，而是“每次 `get` 各自拿读锁”的 API 语义；一次读一致的多 key 视图要用 `iter`。快照隔离未实现。
15. **硬杀测试不是确定性的**：一次跑出 9 个已提交事务、0 丢弃；测试只断言“不出现半组”和“最多一个未提交事务”，不锁定具体条数。
16. **页存储的提交瓶颈仍是 fsync**：1 页 median 5.040 ms、8 页 5.327 ms；页数从 1 到 8，数据量 8×、WAL 8×，延迟只多 0.287 ms。键值层已用提交组摊薄 fsync（ADR 006），页存储的提交路径还没有组提交；页拷贝不是首要优化点。
17. **WAL 放大明显**：每页 8,200 字节页镜像，8 页事务 WAL 65,952 字节，和 data 65,536 字节几乎 1:1。checkpoint 把已落盘页从 WAL 移除；256 个单页事务后 WAL 2,123,808 → 32 字节。
18. **checkpoint 前重开是全量重放**：`pages_replayed` 等于所有已提交页数，不是“缺失页数”。硬杀测试里 5/4/7/3/6 页都全量重放；checkpoint 后只处理之后的记录。
19. **半页尾截断是必要语义**：数据文件长度不是 8192 整数倍时，直接报错会让“写到一半被 kill”无法恢复。截断后重放已提交页，才是 WAL 该有的行为；这条已写成 `PSTORE.md` 的不变量。
20. **页缓存没有上限**：M0 只验证命中路径 0.001 ms；长跑内存会随页数线性增长，不能当成 buffer pool。
21. **checkpoint 不是免费的**：32 事务 16.160 ms、256 事务 9.940 ms；它把恢复成本从 4.933/18.959 ms 降到 3.738/2.649 ms。32 事务的 WAL 太小，固定成本和抖动占比高，不能把 1.3× 当稳定收益；WAL 越大，收益越明显，但 checkpoint 本身仍是单写者停顿。
22. **checkpoint 只 reset 单段**：256 事务的旧 WAL 被整体丢弃，不能保留 checkpoint 前的历史；段轮转仍未做。
23. **键值 checkpoint 的收益依赖 WAL 规模**：1,024 事务 WAL 277,872 → 32 字节，重开 1.665 → 1.066 ms（约 1.56×）；64 事务 WAL 只有 16,848 字节，重开 0.387 → 0.283 ms，固定成本盖过收益。
24. **TRSN 是全量快照**：1,024 事务 × 4 key 的快照 81,264 字节；状态再大时，序列化内存约状态两倍，checkpoint 停顿和内存峰值会先成为瓶颈。没有增量、压缩、分块或代际保留。
25. **组提交把提交时间降到约 1/3，但不是线性扩展**：1,024 事务、16 线程 median 867.780 ms，fsync 260 次；同规模顺序提交 2,954.827 ms。WAL append、leader 的 `sync_all` 和状态写锁仍在关键路径上。
26. **没有冲突检测**：并发写同一 key 是最后 WAL 顺序获胜。组提交只保证“已 fsync 的 Commit 按 WAL 顺序应用”，不提供快照隔离、可串行化或写冲突回滚。`checkpoint` 还要求没有活跃事务。

## 测试计数

- radix：15 单元 + 11 conformance + 6 属性 = 32
- page：14 单元 + 9 fuzz = 23
- format：15 单元 + 11 conformance = 26
- wal：7 单元 + 10 fuzz = 17
- txn：14 单元 + 11 model = 25
- store：12 单元 + 9 model = 21
- `tritdb` 集成：CLI 4 + crash matrix 1 + 硬杀 1 + 页存储硬杀 1 + cross-engine 2 + vectors 5 = 14
- 合计 **158 个正常测试**

另有 4 个 `#[ignore]` 压力测试（txn 2、store 2），由 nightly `stress.yml` 在 release 下执行。

> M1 最小 SQL 切片与 GROUP BY/HAVING、INNER/CROSS/LEFT/RIGHT/FULL/NATURAL/USING JOIN、等值 hash join、EXPLAIN/EXPLAIN ANALYZE、EXPLAIN ANALYZE SEXPR、ANALYZE 统计与估算、Auto join 代价、`WHERE` 合取项拆分与单源下推、源表列裁剪（含无歧义未限定列与零列源表投影）、常量折叠与恒真条件简化、投影常量折叠、结构化计划树与 `EXPLAIN SEXPR`、结构化 join 键与残余标志、结构化过滤谓词、结构化 join 谓词、加法 schema 迁移、逗号 `FROM`、UPDATE、IN/BETWEEN/LIKE、CASE 切片在此基线之上追加 97 个测试（`tritdb-exec` 12、`tritdb-sql` 83、SQL CLI 2），当前全量为 **255 个正常测试 + 4 个 ignored**。SQL 语义与边界见 [`M1-SQL.md`](M1-SQL.md)。

测试分层、交叉矩阵、崩溃注入与 CI/CD 细节见 [`docs/testing/README.md`](../testing/README.md)。

2026-11-20 复验：158 个测试已在 debug、release 下全工作区通过；`fmt --check` 与 `clippy --workspace --all-targets -- -D warnings` 通过。Python `gen_vectors.py --check`、`check_cli.py` 与 framed bridge 通过。Rust 1.88 的全量复验见 CI `msrv` job。

## 下一步

- 用 criterion 替换手写计时；固定输入、交错 A/B、报告置信区间。
- 检查 1024 B pack 表 + 1215 B unpack 表在多线程/多核下的 cache 压力；单线程 L1 友好，不代表 32 线程友好。
- 给 r3 表加编译期断言：243 个合法 key 必须双射到 `0..=242`（已有测试）。
- 把 `radix` / `baseline` / `block` / `wal` / `pstore` / `pstore-checkpoint` / `txn-checkpoint` 的 raw JSON 包成 `bench-result.schema.json` 的单指标记录；当前 schema 是“一条记录一个指标”，不是整张表。
- 调提交组窗口与 leader 策略：当前 `yield_now` 32 次，16 线程平均组大小约 3–4；量 1/4/16/64 事务批次、单批延迟和读者 P99，再决定是否换取更大的组。
- WAL 段轮转：checkpoint 已能 reset 单段；下一步保留多段并只回收已覆盖段。
- 后台 checkpoint：把 data fsync 与 WAL reset 拆开，缩短写者停顿。
- 跨进程文件锁：没有它，两个进程写同一 WAL 仍是未定义行为。
- torn tail 注入 fuzz：在 header/payload 每个偏移制造截断，验证恢复不 panic、只丢尾部。
- 32 线程多读 + 写者排队 + fsync 期间读者 P99；当前只测了 4 线程。
- 页存储/索引落盘后，再评估 MVCC 与快照隔离；M0 的 `RwLock` 全量 map 不是最终形态。
