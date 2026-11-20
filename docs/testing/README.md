# trit-db 测试矩阵与 CI/CD

M0/M1 的测试目标不是“覆盖率数字好看”，而是把格式、引擎、跨语言、崩溃、并发和 SQL 三值/分组/JOIN/DML/谓词/CASE/源表列裁剪/投影常量折叠/结构化计划/结构化 join 键/结构化过滤谓词/结构化 join 谓词/结构化 join 残余谓词/结构化 join 非键合取项/结构化 HAVING 谓词/结构化 Aggregate 分组键与聚合项/结构化 Sort 键与方向/结构化 Project 项与表达式/结构化 Scan 表名与别名/结构化 Scan 下推谓词/解析嵌套上限/恶意输入 fuzz/执行汇总/加法 schema 迁移语义拆开验证。当前工作区有 **271 个正常测试 + 4 个 `#[ignore]` 压力测试**。

## 1. 分层与计数

| crate | 单元 | conformance / fuzz / model | 小计 | 说明 |
|---|---:|---:|---:|---|
| `tritdb-radix` | 15 | 11 + 6 properties | 32 | 全部字节双射、跨进制重打包、canonical padding、百万 trit |
| `tritdb-page` | 14 | 9 fuzz | 23 | 每种截断、逐字节 corruption、0..=8176 payload |
| `tritdb-format` | 15 | 11 conformance | 26 | 全长度/进制、逐字节 corruption、块塞满页 |
| `tritdb-wal` | 7 | 10 fuzz + 3 rotation | 20 | 每个前缀、torn tail、LSN 连续、逐字节 mutation、同文件段轮转/重开/reset |
| `tritdb-txn` | 14 | 11 model + 5 pitr + 3 time | 33 | 随机事务 + checkpoint/reopen、提交组、读者单调性、snapshot-only reopen LSN、归档 checkpoint / `recover_at` PITR、时间索引 `recover_at_time` |
| `tritdb-store` | 12 | 9 model | 21 | 页模型、abort、sidecar 截断/损坏、并发读者 |
| `tritdb-exec` | 12 | — | 12 | 三值 AND/OR/NOT 真值表、NULL 比较、算术、IN/BETWEEN/LIKE 三值语义、CASE 短路/简单式/缺省 NULL、非布尔谓词 |
| `tritdb-sql` | 30 | 55 engine/security | 85 | parser、TRSQ/TRSS 编解码、3VL WHERE、NULL 排序、GROUP BY/HAVING、INNER/CROSS/LEFT/RIGHT/FULL/NATURAL/USING JOIN、等值 hash join/非等值回退、`ANALYZE` 统计持久化与 DML 失效、`Auto` join 成本选择、`WHERE` 合取项拆分与单源下推、源表列裁剪（含无歧义未限定列裁剪、歧义回退与零列源表投影）、常量折叠与恒真条件简化、投影常量折叠（`SELECT` 常量项折叠/标签保持/投影错误保留）、结构化 `PlanNode` 与 `EXPLAIN SEXPR`（文本逐字保持、括号化计划、估算与零列、结构化过滤谓词、结构化 Scan 表名/别名/下推谓词）、结构化 `JoinKeys`（USING/NATURAL 列名、ON 等值对、residual/on-expr 标志、完整 join `:pred` 的 ON/USING/nested-loop 渲染、同源列等值的非键 `:residual-pred`）、`Having :pred`（`COUNT(*)` / `SUM` / 标量列谓词）、`EXPLAIN ANALYZE SEXPR`（执行汇总、节点 actual 字段、无 est 混入）、解析嵌套深度上限（括号/`NOT`/一元负号超限返回 parse error）、恶意输入 corpus + 2000 条确定性随机 SQL 字节汤不 panic、加法 schema 迁移（旧行 `NULL` 回填/重开持久化/统计失效/`IF NOT EXISTS` 幂等/`NOT NULL` 拒绝）、`EXPLAIN` 估算/`EXPLAIN ANALYZE` 实际行数与耗时/策略开关、逗号 `FROM`、空表、IN/BETWEEN/LIKE、CASE（投影/WHERE/聚合/JOIN/UPDATE）、UPDATE、聚合排序、DELETE/reopen |
| `tritdb`（集成） | — | — | 19 | CLI 4、SQL CLI 3、PITR CLI 2、crash matrix 1、硬杀 1、pstore 硬杀 1、cross-engine 2、vectors 5 |
| **合计** | **119** | **133** | **271** | 另有 4 个 ignored 压力测试 |

`vectors` 的 5 个测试覆盖 `radix.txt`、`trbl.txt`、`page.txt`、`wal.txt`、`snapshot.txt`。向量由 Python 参考实现生成，Rust 只负责解码、重编码和语义恢复。

## 2. 交叉测试

### Python ↔ Rust 字节级

- `tools/crosscheck/reference.py`：不导入 Rust、不调用 Rust 的独立 Python 实现。覆盖 R3/R4/R16、TRBL、TRPG、TRWS/TRWL、TRSN、TRCK。
- `tools/crosscheck/gen_vectors.py`：确定性 LCG 生成向量；`--check` 在 CI 中重新生成并逐字节比较。
- `crates/tritdb/tests/vectors.rs`：Python 编码 → Rust 解码 → Rust 重编码；快照向量写入 `<wal>.snap` 后由 `TxnStore::open` 恢复。
- `tools/crosscheck/check_cli.py`：Rust CLI → Python 解码；覆盖 `radix/block/page/txn/pstore`，并检查错误退出码。
- `tools/crosscheck/byte_stream.py` + `tritdb crosscheck`：二进制 framed stdin/stdout bridge，避免临时文件和 shell 引号差异。Python 发送 TRBL/TRPG/键值请求，Rust 引擎 checkpoint/reopen 后返回结果，Python 再解析比较。覆盖 block、page、txn、pstore 四条路径。

### 引擎 ↔ 引擎

- `crates/tritdb/tests/cross_engine.rs`：同一逻辑 workload 同时跑 `TxnStore` 和 `PageStore`，逐 key/页比对；TRBL 块作为同一 payload 分别塞进两个引擎，checkpoint/reopen 后仍可解码。

### SQL ↔ SQLite

- `tools/crosscheck/sql_oracle.py`：同一份 `CREATE TABLE` + `INSERT` 同时灌入 `tritdb-sql` 和 Python `sqlite3`；22 条基础 + 12 条扩展 + 11 条 GROUP BY/HAVING + 18 条 JOIN + 5 条 UPDATE 后查询 + 15 条 IN/BETWEEN/LIKE + 10 条 CASE + 10 条 RIGHT/FULL JOIN + 9 条 CROSS JOIN/逗号 `FROM` + 15 条 NATURAL JOIN/USING + 5 条 hash join 共 **132 条 SELECT** 逐行一致；另对 6 条 `UPDATE` 语句比对 tritdb 返回的更新行数与 `sqlite3` 的 `rowcount`，合计 **138/138**。
- 比较前归一化 `TRUE`/`FALSE` 为 `1`/`0`、`NULL` 为 `NULL`，只比较行值不比较列名；SQLite 不支持的 `IS UNKNOWN` 由 Rust 测试单独覆盖。
- 覆盖三值 `WHERE`、`NOT`、`IS NULL`、NULL 参与比较、NULL 排序、聚合、分组、HAVING、表别名、限定列、INNER/CROSS/LEFT/RIGHT/FULL JOIN、逗号 `FROM`、自连接、空表、`LIMIT`、`WHERE` 顶层 `AND` 合取项拆分与单源下推、`UPDATE` 多列赋值/旧行求值/更新行数、`IN`/`BETWEEN`/`LIKE` 的 NULL 与通配符语义、`CASE` 的搜索式/简单式/嵌套/缺省 `NULL`。

### 格式 ↔ 引擎

- `tritdb-format/tests/conformance.rs::block_inside_page_roundtrip_is_stable` 与 `block_composes_into_a_full_page`：块页嵌套、整页容量。
- `tritdb-store/tests/model.rs`：页镜像 payload 与页文件的字节一致性。

## 3. 崩溃与故障注入

交叉测试已经抓到两个真实问题：

- Python `packed_len` 对 R3 误用 3 trits/byte；向量期望 1,366 字节而实际 820 字节，修复为 `{3:5,4:4,16:2}`。
- `TxnStore::checkpoint` 在“只有 snapshot、没有新 WAL 记录”的 reopen 后会把 `checkpoint_lsn` 从 snapshot 值降到 0；修复为 `max(wal.next_lsn-1, recovery.checkpoint_lsn)`，并加 `checkpoint_after_reopen_without_writes_preserves_lsn` 回归测试。

- `crates/tritdb/tests/crash_recovery.rs`：硬杀键值 writer，恢复后只保留完整提交组。
- `crates/tritdb/tests/crash_matrix.rs`：在 0/2/5/10/25/50/100/200 ms 八个杀点重复硬杀，断言 `committed_txns`、key group 数和每事务 key 数完全一致。
- `crates/tritdb/tests/pstore_crash_recovery.rs`：页 writer 硬杀后恢复连续页前缀，页数等于已提交事务数。
- `tritdb-wal/tests/fuzz.rs`：每个前缀解析、短段修复、torn tail 截断后 LSN 复用边界。
- `tritdb-page/tests/fuzz.rs`、`tritdb-format/tests/conformance.rs`：每种截断偏移和每个单字节 corruption 都必须结构化报错或安全解析，不允许 panic。
- `tritdb-sql` parser 安全边界：括号、连续 `NOT`、连续一元负号超过 64 层返回 `expression nesting too deep`；`tritdb/tests/sql_cli.rs` 用 512 层嵌套输入验证 CLI 子进程不栈溢出。见 [`ADR-031`](../ADR/031-parser-expression-depth-limit.md)。
- `tritdb-sql/tests/security.rs`：固定 malformed corpus、公开 API 的 512 层嵌套断言，以及 2000 条确定性随机 SQL 字符汤；只要求 parser 不 panic，不假定随机输入必须成功或失败。
- checkpoint 竞争：`txn model` 中 writer 活跃时并发 checkpoint，验证最终状态与 snapshot；checkpoint 失败返回 `ActiveTxns` 而不是破坏状态。

## 4. 压力测试（ignored，nightly）

```bash
TRITDB_STRESS_THREADS=16 \
TRITDB_STRESS_TXNS_PER_THREAD=32 \
TRITDB_STRESS_ROUNDS=64 \
cargo test -p tritdb-txn --test stress -- --ignored --nocapture

TRITDB_STRESS_THREADS=8 \
TRITDB_STRESS_PAGES=200 \
TRITDB_STRESS_READERS=4 \
TRITDB_STRESS_ROUNDS=32 \
cargo test -p tritdb-store --test stress -- --ignored --nocapture
```

- `group_commit_many_threads`：Barrier 同时起跑，验证总行数、`syncs` 上界、reopen 后 WAL 提交数。
- `checkpoint_commit_loop`：writer 持续提交，主线程循环 checkpoint；统计成功/被拒绝次数，最终 reopen 验证行数。
- `many_readers_during_commits`：多读者与页提交并发，读者只看已提交页。
- `repeated_checkpoints`：反复提交 + checkpoint，验证页数与内容。

这些测试不断言“成功 checkpoint 次数”，因为调度和 active transaction 会让它非确定；断言的是安全性和最终可恢复状态。

## 5. CI

`.github/workflows/ci.yml`：

| job | runner | 内容 |
|---|---|---|
| `lint` | Ubuntu | `cargo fmt --check`、`cargo clippy --workspace --all-targets -- -D warnings` |
| `test` | Ubuntu / Windows / macOS | `cargo test --workspace` |
| `msrv` | Ubuntu / Windows | Rust 1.88.0 `cargo test --workspace` |
| `bench-smoke` | Ubuntu | release 构建后跑 `sql-plan`、`sql-join`、`sql-exec` 最小档，并检查 JSON `bench_id` |
| `python-crosscheck` | Ubuntu / Windows / macOS | 构建 CLI、`gen_vectors.py --check`、`check_cli.py`（含 framed bridge）、`sql_oracle.py`（SQLite 3VL 对拍） |
| `coverage` | Ubuntu | `cargo llvm-cov --workspace --all-targets`，行覆盖门禁 65%，上传 `lcov.info`；M0 首轮 3253/4791 = 67.9%，SQL 3VL 71.81%，GROUP BY 后 72.39%，INNER JOIN 后 73.02%，LEFT JOIN 后 73.19%，UPDATE 后 73.65%，谓词后 74.48%，CASE 后 75.41%，RIGHT/FULL JOIN 后 75.59%，CROSS JOIN/逗号 FROM 后 **5859/7737 = 75.73%**；后续 NATURAL/USING、hash join、EXPLAIN、EXPLAIN SEXPR、EXPLAIN ANALYZE、ANALYZE、谓词下推、合取拆分、源表列裁剪（含无歧义未限定列与零列源表投影）、常量折叠、投影常量折叠、结构化计划、结构化 join 键、结构化过滤谓词与 join 谓词、结构化 join 残余谓词、结构化 join 非键合取项、结构化 HAVING 谓词、结构化 Aggregate 分组键与聚合项、结构化 Sort 键与方向、结构化 Project 项与表达式、结构化 Scan 表名与别名、结构化 Scan 下推谓词、加法 schema 迁移未重跑 |
| `docs` | Ubuntu | `cargo doc --workspace --no-deps`，`RUSTDOCFLAGS=-D warnings` |

`.github/workflows/stress.yml`：每天 04:00 UTC + 手动触发；release 模式跑 txn/store ignored 压力测试，以及 crash matrix / 两个硬杀恢复测试。

首轮扩展后 CI：<https://github.com/bfxh/trit-db/actions/runs/44595436280>（11 个 job 全绿，覆盖 Ubuntu/Windows/macOS + MSRV + Python + coverage + docs）。加入 65% 覆盖门禁后的复跑：<https://github.com/bfxh/trit-db/actions/runs/44595879346>。
SQL 切片 CI：<https://github.com/bfxh/trit-db/actions/runs/44610635876>（GROUP BY 基线，三平台 `sql_oracle.py` 均 45/45）。INNER JOIN CI：<https://github.com/bfxh/trit-db/actions/runs/44613405869>（11 个 job 全绿，三平台 `sql_oracle.py` 均 57/57，覆盖 73.02%）。LEFT JOIN CI：<https://github.com/bfxh/trit-db/actions/runs/44615053085>（11 个 job 全绿，三平台 `sql_oracle.py` 均 63/63，覆盖 73.19%）。UPDATE CI：<https://github.com/bfxh/trit-db/actions/runs/44617974358>（11 个 job 全绿，三平台 `sql_oracle.py` 均 74/74，覆盖 73.65%）。谓词 CI：<https://github.com/bfxh/trit-db/actions/runs/44620142871>（11 个 job 全绿，三平台 `sql_oracle.py` 均 89/89，覆盖 74.48%）。CASE CI：<https://github.com/bfxh/trit-db/actions/runs/44622362833>（11 个 job 全绿，三平台 `sql_oracle.py` 均 99/99，覆盖 75.41%）。RIGHT/FULL JOIN CI：<https://github.com/bfxh/trit-db/actions/runs/44635704677>（11 个 job 全绿，三平台 `sql_oracle.py` 均 109/109，覆盖 75.59%）。CROSS JOIN/逗号 FROM CI：<https://github.com/bfxh/trit-db/actions/runs/44639115915>（11 个 job 全绿，三平台 `sql_oracle.py` 均 118/118，覆盖 75.73%）。NATURAL/USING + hash join：本地 `sql_oracle.py` 138/138，三平台 CI 待推送后补。

nightly stress 手动首跑：<https://github.com/bfxh/trit-db/actions/runs/44596129132>（crash matrix、txn store 压力测试 3 个 job 全绿）。

## 6. CD

`.github/workflows/release.yml` 在 `v*` tag 上：

1. 在 Linux x86_64、Windows x86_64、macOS arm64 原生 runner 构建 release 二进制。
2. 打包 `tritdb` 和 `tritdb-bench` 为 `tritdb-<platform>.tar.gz`。
3. 上传 artifact，由 Ubuntu job 用 `gh release create` 创建 GitHub Release 并附带自动 release notes。

本地验证 release 构建：

```bash
cargo build --release --locked -p tritdb -p tritdb-bench
```

首次发布 `v0.0.1` 已实际跑通：

- Release：<https://github.com/bfxh/trit-db/releases/tag/v0.0.1>
- Release run：<https://github.com/bfxh/trit-db/actions/runs/44603091304>
- 产物：`tritdb-linux-x86_64.tar.gz`、`tritdb-windows-x86_64.tar.gz`、`tritdb-macos-arm64.tar.gz`

首跑暴露并修复了两个 workflow 问题：macOS job 使用的 `macos-14` runner image 已退役，改为 `macos-15`；publish job 缺 `actions/checkout`，导致 `gh release create --verify-tag` 找不到 git 仓库上下文。修复后三个平台构建和 Release 发布全部成功，Windows 产物已下载并做过 `radix` / `txn put/get/dump` 冒烟验证。

## 7. 已知边界

- 跨语言向量不覆盖真实磁盘 fsync 时序；崩溃测试只覆盖“进程被硬杀”和“WAL torn tail/short file”，不模拟断电、磁盘写缓存重排。
- `PageStore` 没有跨页快照 API，页级并发测试只断言单页来自已提交版本。
- 提交组没有冲突检测，同一 key 的最后 WAL 顺序获胜；模型测试按这个语义断言。
- SQL 表每次写都重写整个 value；对拍只覆盖小表，不覆盖大表写放大、并发 SQL 写者或跨进程锁。
- SQL `IS UNKNOWN` 没有进 SQLite 对拍（SQLite 不支持该语法），由 `tritdb-exec` / `tritdb-sql` 单元测试覆盖。
- `checkpoint_commit_loop` 在 debug 下可能需要数分钟；nightly 用 release，本地调试请降低 `TRITDB_STRESS_ROUNDS`。
