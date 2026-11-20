# trit-db

多进制列存 + 多值语义数据库的 **M0 骨架 + M1 最小 SQL 3VL/分组/JOIN/谓词/CASE 切片**。当前十一个 crate 只做这些事：

1. `tritdb-radix`：三值序列的 3/4/16 进制打包与解包；r3 走 10-bit 查表；逐位往返、非法输入结构化报错。
2. `tritdb-page`：8 KiB 页 + 头 + CRC32；坏 magic/版本/长度/校验和/半写检测。
3. `tritdb-format`：TRBL 块格式；20 字节头 + CRC32 + canonical padding 校验。
4. `tritdb-wal`：TRWL 日志；32 字节段头/记录头 + CRC32；torn tail 检测与截断；LSN 单调性；段达到大小上限时在同一文件内追加新段头（归档仍是单文件）。
5. `tritdb-txn`：单 WAL 写者 + 多个并发逻辑事务；`Begin/Put/Delete/Commit/Abort`；提交组让一批 `Commit` 共享一次 fsync；恢复只应用已提交事务；checkpoint 把已提交键值写进 `<wal>.snap` 后重置 WAL；`checkpoint --archive` 把 snapshot/WAL 归档到 `<wal>.history/<lsn>.{snap,wal}`，`txn recover --to-lsn` 可只读地回到历史 LSN；`<wal>.timeidx` 记录 LSN→墙钟毫秒，`txn recover --to-time-ms` 可按时间点恢复。
6. `tritdb-store`：8 KiB 页文件 + 页缓存（M0 无淘汰、无上限）+ WAL 页镜像 redo；提交先 fsync WAL 再写页文件；只重放已提交页；崩溃留下的半页尾会被截断；checkpoint 把已落盘页写入 `<data>.ckpt` 后重置单段 WAL。
7. `tritdb` / `tritdb-bench`：CLI 与微基准；6 轮交错、min/median/max；带 `bin-u8` / `bin-u2` 二进制基线。
8. `tritdb-testkit`：零依赖测试工具；唯一临时目录、确定性 xorshift64*、hex 编解码。
9. `tools/crosscheck/` + `testdata/vectors/`：独立 Python 字节级参考实现、确定性向量、Rust→Python 反向校验和二进制 framed bridge。
10. `tritdb-exec`：SQL 值、schema、三值逻辑真值表、比较/算术表达式求值。
11. `tritdb-sql`：最小 lexer/parser/binder/executor；`CREATE/INSERT/SELECT/UPDATE/DELETE/DROP`、加法 schema 迁移 `ALTER TABLE ... ADD [COLUMN]`、`WHERE/ORDER BY/LIMIT`、`IN/BETWEEN/LIKE` 谓词、`CASE` 表达式、`GROUP BY`/`HAVING`、多表 `INNER JOIN` / `CROSS JOIN` / `LEFT [OUTER] JOIN` / `RIGHT [OUTER] JOIN` / `FULL [OUTER] JOIN`、`NATURAL [INNER|LEFT|RIGHT|FULL] JOIN`、`JOIN ... USING (col, ...)`、等值 JOIN 的 hash join 执行（非等值条件回退 nested loop）、基于 `ANALYZE` 统计的 `Auto` join 代价选择、内连接/交叉连接的 `WHERE` 合取项拆分与单源下推、无 `*` / `USING` / `NATURAL` 且列引用可唯一归属时的源表列裁剪（含无歧义未限定列与零列源表投影）、计划入口的常量折叠、投影常量折叠与恒真条件简化、结构化 `PlanNode`（含 join 键/残余标志/非键合取项/过滤谓词/完整 join 谓词/HAVING 谓词/Scan 表名、别名与下推谓词）、只读 `EXPLAIN SEXPR` 与带执行汇总的 `EXPLAIN ANALYZE SEXPR`、逗号分隔 `FROM`（表别名 + 限定列）、分组聚合与聚合结果排序、`ANALYZE` 表/列统计、`EXPLAIN SELECT` 静态计划与统计估算；表以 `TRSQ` 整表 value 存进 `TxnStore`，统计以 `TRSS` value 存在 `sql/stats/` 下。`ALTER TABLE ADD COLUMN` 只允许追加可空列，旧行回填 `NULL`，整表在单事务中重写并在成功后使统计失效；不支持 `DROP` / `RENAME` / `ALTER TYPE`。

**还没有做**：子查询、窗口函数、全局优化器、pgwire、MVCC、Parquet/Iceberg、复制、WAL 段轮转、后台 checkpoint、归档自动清理、时间戳到 LSN 的索引、跨进程文件锁、带默认值的非空列迁移、`DROP/RENAME/ALTER COLUMN` 等破坏性 schema 变更、schema 迁移版本历史。不要把这个骨架当成数据库。

## 发布

M0 首个版本 **v0.0.1**：<https://github.com/bfxh/trit-db/releases/tag/v0.0.1>。提供 Linux x86_64、Windows x86_64、macOS arm64 三个平台的 `tritdb` / `tritdb-bench` 压缩包；由 `v*` tag 触发的 GitHub Actions release 工作流构建和发布。

## 快速开始

```bash
cargo test --workspace
cargo test --workspace --release
python tools/crosscheck/gen_vectors.py --check
python tools/crosscheck/check_cli.py
cargo test -p tritdb-txn --test stress -- --ignored --nocapture
cargo test -p tritdb-store --test stress -- --ignored --nocapture
cargo run -p tritdb -- radix --radix r3 --trits 1000000
cargo run -p tritdb -- block make --out block.trbl --radix r3 --trits 1000000
cargo run -p tritdb -- block check --in block.trbl
cargo run -p tritdb -- page make --out page.bin --kind data --text "hello"
cargo run -p tritdb -- page check --in page.bin
cargo run -p tritdb -- txn put --wal test.wal --key alpha --value one
cargo run -p tritdb -- txn get --wal test.wal --key alpha
cargo run -p tritdb -- txn dump --wal test.wal
cargo run -p tritdb -- txn checkpoint --wal test.wal
cargo run -p tritdb -- txn checkpoint --wal test.wal --archive
cargo run -p tritdb -- txn history --wal test.wal
cargo run -p tritdb -- txn history --wal test.wal --prune --keep 5
cargo run -p tritdb -- txn recover --wal test.wal --to-lsn 10
cargo run -p tritdb -- txn recover --wal test.wal --to-time-ms 1700000000000
cargo run -p tritdb -- wal inspect --in test.wal
cargo run -p tritdb -- pstore alloc --data pstore.pages --wal pstore.wal --text "hello"
cargo run -p tritdb -- pstore get --data pstore.pages --wal pstore.wal --id 0
cargo run -p tritdb -- pstore dump --data pstore.pages --wal pstore.wal
cargo run -p tritdb -- pstore checkpoint --data pstore.pages --wal pstore.wal
cargo test -p tritdb --test pstore_crash_recovery
python tools/crosscheck/sql_oracle.py --tritdb target/debug/tritdb
cargo run -p tritdb -- sql --db sql.db --sql "CREATE TABLE t (id INT NOT NULL, flag BOOL); INSERT INTO t VALUES (1, TRUE), (2, NULL);"
cargo run -p tritdb -- sql --db sql.db --sql "SELECT id, flag FROM t WHERE flag IS NULL;"
cargo run -p tritdb -- sql --db sql.db --sql "SELECT flag, COUNT(*) AS n FROM t GROUP BY flag HAVING COUNT(*) >= 1 ORDER BY flag;"
cargo run -p tritdb -- sql --db sql.db --sql "CREATE TABLE d (id INT NOT NULL, name TEXT); INSERT INTO d VALUES (1, 'eng'); SELECT t.id, d.name FROM t JOIN d ON t.id = d.id;"
cargo run -p tritdb -- sql --db sql.db --sql "EXPLAIN SELECT t.id FROM t JOIN d ON t.id = d.id;"
cargo run -p tritdb -- sql --db sql.db --sql "EXPLAIN SEXPR SELECT t.id FROM t JOIN d ON t.id = d.id;"
cargo run -p tritdb -- sql --db sql.db --sql "ANALYZE; EXPLAIN SELECT t.id FROM t JOIN d ON t.id = d.id;"
cargo run -p tritdb -- sql --db sql.db --sql "ALTER TABLE t ADD COLUMN note TEXT; SELECT id, note FROM t;"
cargo run -p tritdb -- sql --db sql.db --sql "EXPLAIN ANALYZE SELECT t.id FROM t JOIN d ON t.id = d.id;"
cargo run -p tritdb -- sql --db sql.db --sql "EXPLAIN ANALYZE SEXPR SELECT t.id FROM t JOIN d ON t.id = d.id;"
cargo run -p tritdb-bench --release -- radix --trits 1000000 --rounds 6
cargo run -p tritdb-bench --release -- baseline --trits 1000000 --rounds 6
cargo run -p tritdb-bench --release -- block --trits 1000000 --rounds 6 --radix r3
cargo run -p tritdb-bench --release -- page --payload 4096 --rounds 6
cargo run -p tritdb-bench --release -- wal --records 1000 --payload 64 --rounds 6
cargo run -p tritdb-bench --release -- wal --records 100 --payload 64 --rounds 6 --fsync
cargo run -p tritdb-bench --release -- pstore --pages 1 --payload 4096 --rounds 6
cargo run -p tritdb-bench --release -- pstore --pages 8 --payload 4096 --rounds 6
cargo run -p tritdb-bench --release -- pstore-checkpoint --txns 32 --payload 4096 --rounds 6
cargo run -p tritdb-bench --release -- pstore-checkpoint --txns 256 --payload 4096 --rounds 3
cargo run -p tritdb-bench --release -- txn-checkpoint --txns 64 --keys-per-txn 4 --rounds 3
cargo run -p tritdb-bench --release -- txn-checkpoint --txns 1024 --keys-per-txn 4 --rounds 3
cargo run -p tritdb-bench --release -- txn-group --txns 256 --threads 16 --keys-per-txn 4 --rounds 3
cargo run -p tritdb-bench --release -- txn-group --txns 1024 --threads 16 --keys-per-txn 4 --rounds 3
cargo run -p tritdb-bench --release -- sql-join --rows 1024 --rounds 3 --strategy auto
cargo run -p tritdb-bench --release -- sql-join --rows 1024 --rounds 3 --strategy nested
cargo run -p tritdb-bench --release -- sql-join --rows 2048 --rounds 3 --strategy auto
cargo run -p tritdb-bench --release -- sql-join --rows 2048 --rounds 3 --strategy nested
cargo run -p tritdb-bench --release -- sql-plan --rows 1024 --rounds 5
cargo run -p tritdb-bench --release -- sql-exec --rows 1024 --rounds 5
```

## 测试与 CI

当前 **258 个正常测试 + 4 个 ignored 压力测试**，按格式、引擎、故障注入、跨语言、跨引擎、SQL 3VL/分组聚合/JOIN/DML/谓词/CASE/源表列裁剪/投影常量折叠/结构化计划/结构化 join 键/结构化过滤谓词/结构化 join 谓词/结构化 join 残余谓词/结构化 join 非键合取项/结构化 HAVING 谓词/结构化 Aggregate 分组键与聚合项/结构化 Sort 键与方向/结构化 Project 项与表达式/结构化 Scan 表名与别名/结构化 Scan 下推谓词/解析嵌套上限/恶意输入 fuzz/执行汇总/加法 schema 迁移分层；完整矩阵见 [`docs/testing/README.md`](docs/testing/README.md)。

- `ci.yml`：`fmt` / `clippy` / 三平台 `test` / Ubuntu+Windows MSRV 1.88 / `sql-plan`+`sql-join`+`sql-exec` bench smoke / 三平台 Python 交叉校验（向量 + CLI + SQLite SQL 对拍）/ coverage / docs。
- `stress.yml`：每天 04:00 UTC 跑 release 模式压力测试与 crash matrix。
- `release.yml`：`v*` tag 构建 Linux x86_64、Windows x86_64、macOS arm64 的 `tritdb`/`tritdb-bench`，创建 GitHub Release。

公开仓：[`bfxh/trit-db`](https://github.com/bfxh/trit-db)



- 不是“三进制数据库”，是“多进制列存 + 多值语义数据库”。
- 不是 PostgreSQL 替代品；v1 目标是“单写者 + 快照隔离 + 只读副本”，M0 只到“单 WAL 写者 + 并发逻辑事务 + 已提交读”。
- 三值只做逻辑/位图/压缩/语义；算术走二进制。
- 依赖主库时走 git pin；不复制 `trit-codec` / `trit-texture` / `trit-ops` 源码。
- WAL 是单文件、单 WAL 写者、无段轮转；页存储用 `TRCK` sidecar、键值存储用 `TRSN` 快照，两者都能单段 checkpoint；无跨进程锁。
- 页存储与 `tritdb-txn` 是两个独立 WAL 消费者：前者是页镜像 redo，后者是 key/value 事务；M0 不合并。
- checkpoint 是单写者停顿点：要求没有活跃逻辑事务，再持久化 sidecar/快照、重置 WAL；不做后台 checkpoint。
- 键值事务允许并发逻辑事务交错写 WAL；提交组共享 fsync，但不做冲突检测，同一 key 最后 WAL 顺序获胜。
- 页存储每次提交 fsync WAL 和 data 两次；缓存无上限、无淘汰；没有页分配位图；没有快照隔离。
- 页存储没有跨页快照 API；单次 `read_page` 只保证该页来自某个已提交版本；跨页一致视图未实现。
- `tritdb-store` 的页 id 由单写者分配，提交时才推进分配器，abort 不泄漏 id；CLI `pstore put` 只允许更新已存在页，避免数据文件出现空洞。

## 目录

```
crates/tritdb-radix/   3/4/16 进制打包
crates/tritdb-page/    8 KiB 页 + CRC
crates/tritdb-format/  TRBL 块格式 + CRC
crates/tritdb-wal/     TRWL 日志 + 恢复扫描
crates/tritdb-txn/     并发逻辑事务 + 提交组 + TRSN 快照
crates/tritdb-store/   8 KiB 页文件 + 缓存 + WAL 页 redo
crates/tritdb-exec/    SQL 值/表达式/三值逻辑
crates/tritdb-sql/     最小 SQL parser/binder/executor + TRSQ 表存储
crates/tritdb-bench/   微基准
crates/tritdb/         CLI
crates/tritdb-testkit/ 测试工具（TempDir / 确定性 RNG / hex）
tools/crosscheck/      Python 独立参考实现、向量生成、反向 CLI 校验
testdata/vectors/      由 Python 参考实现生成的跨语言向量
docs/testing/          测试矩阵、CI/CD、故障注入与压力测试说明
docs/ADR/              架构决策
docs/specs/            TRBL 块格式、TRWL 日志、页存储、键值快照、提交组、SQL 子集（草案）
docs/results/          M0/M1 读数与负结果
```

## 状态词

- `已实现`：有代码、有测试、有命令。
- `已测`：有基线、有 6 轮、有机器/命令。
- `已设计`：只有文档。
- `未做`：没有代码。

当前：radix/page/format/wal/txn/store/页 checkpoint/键值 checkpoint/提交组/CLI/bench/最小 SQL 3VL/单表 GROUP BY/多表 INNER JOIN/CROSS JOIN/逗号 FROM/LEFT JOIN/RIGHT JOIN/FULL JOIN/NATURAL JOIN/USING/等值 hash join/`ANALYZE` 表统计/`Auto` join 代价选择/`WHERE` 合取项拆分与单源下推/源表列裁剪（含无歧义未限定列与零列源表投影）/常量折叠与投影常量折叠/结构化计划树与 `EXPLAIN SEXPR`/结构化 join 键/结构化过滤谓词与 join 谓词/结构化 join 残余谓词/结构化 join 非键合取项/结构化 HAVING 谓词/结构化 Aggregate 分组键与聚合项/结构化 Sort 键与方向/结构化 Project 项与表达式/结构化 Scan 表名与别名/结构化 Scan 下推谓词/解析嵌套深度上限/`EXPLAIN SELECT` 估算/`EXPLAIN ANALYZE`/JOIN 策略开关/UPDATE/IN·BETWEEN·LIKE/CASE 为 `已实现`；子查询、全局优化器/pgwire/MVCC/WAL 段轮转/后台 checkpoint/跨进程锁 为 `未做`。
