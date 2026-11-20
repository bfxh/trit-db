# M1 SQL 3VL：最小纵向切片读数

状态：parser / binder / executor / CLI / 持久化 / SQLite 对拍 / 单表 GROUP BY / 多表 INNER JOIN / CROSS JOIN / 逗号 FROM / LEFT JOIN / RIGHT JOIN / FULL JOIN / NATURAL JOIN / USING / 等值 JOIN 的 hash join / `ANALYZE` 表/列统计 / `EXPLAIN SELECT` 统计估算 / `EXPLAIN ANALYZE` / JOIN 策略开关 / UPDATE / IN·BETWEEN·LIKE 谓词 / CASE 表达式已实现。
日期：2026-11-20

## 1. 交付

```text
crates/tritdb-exec/   Value、Schema、表达式、三值逻辑
crates/tritdb-sql/    lexer / parser / binder / engine / TRSQ 表存储
crates/tritdb/        sql 子命令
tools/crosscheck/sql_oracle.py
docs/specs/SQL.md
```

支持：

- `CREATE TABLE [IF NOT EXISTS]`
- `INSERT INTO ... VALUES`
- `SELECT`：`WHERE`、`GROUP BY`、`HAVING`、`ORDER BY`、`LIMIT`、别名、`*`
- `GROUP BY` 直接列引用、`NULL` 键同组、分组聚合、分组结果排序
- 多表 `INNER JOIN`：表别名、`alias.column` 限定列、非限定唯一列、自连接、JOIN 后分组/排序
- `CROSS JOIN` / 逗号分隔 `FROM`：左右行笛卡尔积、空表、与显式 JOIN 混用、`WHERE/GROUP BY`
- `LEFT [OUTER] JOIN`：未匹配左行保留、右侧列补 `NULL`、空右表、LEFT JOIN 后 `WHERE/GROUP BY`
- `RIGHT [OUTER] JOIN` / `FULL [OUTER] JOIN`：未匹配右行/双侧行保留、另一侧补 `NULL`、空表、`WHERE/GROUP BY`
- `NATURAL [INNER|LEFT|RIGHT|FULL] JOIN`：公共列自动匹配、合并列只输出一次、双限定名访问、连续链式合并、无公共列退化为 `CROSS JOIN`
- `JOIN ... USING (col, ...)`：指定列匹配、合并列排序、重复/缺失/类型不一致校验、`LEFT/RIGHT/FULL` 的非 `NULL` 侧合并
- 等值 JOIN 执行：`USING`/`NATURAL` 与 `ON` 顶层 `AND` 的列等值走 hash join；支持复合键、残余 `ON` 谓词、`NULL` 键不匹配；`OR`、非等值比较、表达式键回退 nested loop
- `EXPLAIN SELECT`：绑定但不执行，输出 `Scan` / `Join` / `Filter` / `Aggregate` / `Having` / `Project` / `Sort` / `Limit` 文本计划；Join 节点标出 `HashJoin` 或 `NestedLoop`、键和残余谓词；有 `ANALYZE` 统计时节点追加 `est rows` / `est groups`
- `ANALYZE [table]`：扫描表并持久化精确行数、每列 `NULL` 数和 NDV；DML 在同一事务删除旧统计，重开后可继续供 `EXPLAIN` 使用
- `EXPLAIN ANALYZE SELECT`：执行一次查询，在计划节点上追加 `actual rows`（聚合节点为 `actual groups`）和墙钟耗时；末行给出 `Execution: rows=N time=Xms`，不返回查询结果行
- JOIN 策略开关：`JoinStrategy::{Auto, Hash, NestedLoop}`，供基准和差分测试强制指定执行路径
- `UPDATE ... SET column = expr [, ...] [WHERE ...]`：旧行求值、多列赋值、类型/NOT NULL 校验
- `IN` / `NOT IN`、`BETWEEN` / `NOT BETWEEN`、`LIKE` / `NOT LIKE`：三值逻辑、NULL 传播、ASCII 大小写不敏感
- `CASE WHEN` 搜索式与 `CASE operand WHEN` 简单式、`ELSE`/缺省 `NULL`、分支短路、聚合/分组内使用
- `DELETE`、`DROP TABLE`
- `COUNT(*)`、`COUNT(expr)`、`SUM`、`MIN`、`MAX`
- `INT` / `TEXT` / `BOOL`，`NOT NULL`
- `NULL` / `UNKNOWN`、三值 `AND` / `OR` / `NOT`、`IS [NOT] NULL/TRUE/FALSE/UNKNOWN`

## 2. 验收命令

```bash
cargo test -p tritdb-exec
cargo test -p tritdb-sql
cargo test -p tritdb --test sql_cli
cargo build -p tritdb
python tools/crosscheck/sql_oracle.py --tritdb target/debug/tritdb
```

本机结果：

```text
sql oracle: 138/138 passed (132 SELECT, 6 UPDATE row counts)
```

### sql-join 规模基准

`tritdb-bench sql-join` 构造两张 `rows` 行的表，键和 payload 都是 `0..rows-1`，重复执行 `SELECT COUNT(*) FROM a JOIN b ON a.id = b.id;`。查询在计时循环前解析一次，循环内只调用 `execute_statement`，因此读数不包含 SQL 解析。每档 3 轮，取 min/median/max。

```bash
cargo run -p tritdb-bench --release -- sql-join --rows 1024 --rounds 3 --strategy auto
cargo run -p tritdb-bench --release -- sql-join --rows 1024 --rounds 3 --strategy nested
cargo run -p tritdb-bench --release -- sql-join --rows 2048 --rounds 3 --strategy auto
cargo run -p tritdb-bench --release -- sql-join --rows 2048 --rounds 3 --strategy nested
```

本机 Windows x86_64、release 构建：

| rows | strategy | join_ms(min/med/max) | matches |
|---:|---|---:|---:|
| 1024 | auto | 0.667 / 0.688 / 0.951 | 1024 |
| 1024 | nested | 67.153 / 67.560 / 67.909 | 1024 |
| 2048 | auto | 1.048 / 1.176 / 1.187 | 2048 |
| 2048 | nested | 269.805 / 270.439 / 270.572 | 2048 |
| 4096 | auto | 2.147 / 2.148 / 2.327 | 4096 |

1024 行时 hash 比 nested 快约 98 倍；2048 行时约 230 倍。行数翻倍，nested 约 4 倍增长，hash 约 1.7 到 1.8 倍增长。读数只用于本机回归对比，不作为跨机器绝对性能承诺。

对拍口径：

- 两边执行同一份 `CREATE TABLE` + `INSERT`；
- 每条 SELECT 单独跑，比较行值，不比较列名；
- `TRUE`/`FALSE` 归一化为 `1`/`0`，`NULL` 归一化为 `NULL`；
- SQLite 不支持 `IS UNKNOWN`，该语义由 Rust 测试单独覆盖。

## 3. 测试增量

| 层 | 新增 |
|---|---:|
| `tritdb-exec` 单元测试 | 12 |
| `tritdb-sql` 单元测试 | 25 |
| `tritdb-sql` 引擎集成测试 | 42 |
| `tritdb` SQL CLI 集成测试 | 2 |
| **合计** | **81** |

其中 26 个来自初始 SQL 3VL 切片，6 个来自 GROUP BY/HAVING 切片，10 个来自 JOIN 切片（3 个 parser + 7 个 engine），5 个来自 UPDATE 切片（1 个 parser + 4 个 engine），7 个来自谓词切片（3 个 exec + 1 个 parser + 3 个 engine），5 个来自 CASE 切片（1 个 exec + 1 个 parser + 3 个 engine），4 个来自 RIGHT/FULL JOIN 切片（1 个 parser + 3 个 engine），5 个来自 CROSS JOIN/逗号 FROM 切片（2 个 parser + 3 个 engine），5 个来自 NATURAL JOIN/USING 切片（3 个 parser + 2 个 engine，替换 1 个旧 unsupported join 测试，净增 4），1 个来自 hash join 切片（复合键/NULL 键/残余谓词/非等值回退），2 个来自 EXPLAIN 切片（1 个 parser + 1 个 engine），1 个来自 EXPLAIN ANALYZE 切片（计划实际行数/分组数/耗时），3 个来自 ANALYZE 统计切片（1 个 parser + 1 个 stats 单元 + 1 个 storage 编解码），2 个来自 EXPLAIN 估算切片（统计持久化/重开与 DML 失效）。工作区测试从 158 增至 **239 个正常测试 + 4 个 ignored 压力测试**。

## 4. 负结果 / 边界

1. **整表重写**：每次 INSERT/DELETE/UPDATE 都把整张表重新编码成一个 KV value。数据量增大后写放大和内存峰值会先撞墙，M1 不解决。
2. **JOIN 等值走 hash、非等值走 nested loop，仍然全内存**：不支持子查询；`CROSS JOIN`/逗号 `FROM` 直接产生行数乘积。`ON` 只从顶层 `AND` 提取列等值，`OR`/非等值/表达式键回退嵌套循环；hash 桶按右表行号构建，没有分区、批处理或 spill；`ANALYZE` 统计不参与策略选择。`USING` 只接受不带限定的直接列名，`NATURAL` 只按同名公共列匹配；两者不能与 `ON` 混用，`CROSS JOIN` 不能带 `ON`/`USING`。`GROUP BY` 只支持列引用，不支持分组表达式、位置序号、GROUPING SETS/CUBE/ROLLUP。SQLite 对拍已覆盖单表表达式、分组聚合、`HAVING`、INNER/CROSS/LEFT/RIGHT/FULL/NATURAL/USING JOIN、复合键/NULL 键 hash join、非等值回退、逗号 `FROM`、空表、UPDATE、IN/BETWEEN/LIKE 谓词和 CASE。
3. **没有 pgwire**：`psql` 还连不上；CLI 是当前唯一入口。
4. **每个语句自动提交**：脚本不是一个大事务；`INSERT` 多行在一个事务内，但 `CREATE`/`INSERT`/`SELECT` 之间不共享事务。
5. **没有跨进程锁**：两个进程写同一个 SQL WAL 仍是未定义行为，和 M0 一致。
6. **统计是手动快照，索引仍没有**：`ANALYZE` 精确扫描并持久化行数、NULL 数、NDV；DML 删除旧统计但不自动重算，必须重新 `ANALYZE`。SELECT 仍全表扫描，ORDER BY 全量排序，`EXPLAIN` 的 `est` 是启发式估算，不是代价模型。
7. **类型严格**：不做 SQLite 式隐式转换；`INSERT INTO t (id) VALUES ('x')` 会拒绝。
8. **分组全内存**：`GROUP BY` 用 `BTreeMap<Vec<GroupKeyPart>, Vec<&Row>>`，没有 hash aggregate、分区或 spill；组数/行数受内存限制。
9. **NULL 排序按 SQLite**：ASC 最前、DESC 最后；PostgreSQL 默认相反，后续要做方言开关。
10. **JOIN 全内存**：hash 桶保存右表行号，每个 join 仍构造新的临时表并复制行，没有流式执行、分区或 spill；多表/大表会快速放大内存。
11. **谓词只做标量列表**：`IN` 不支持子查询或行构造器；`LIKE` 仅支持 `TEXT`，没有 `ESCAPE`、`GLOB` 或正则，大小写折叠只覆盖 ASCII。
12. **CASE 只做标量分支**：不支持子查询条件/结果；简单式 `CASE operand WHEN` 用本引擎的类型严格等值比较，探针与候选类型不同会报 `type mismatch`，而 SQLite 会落到 `ELSE`。未选中分支不求值，因此其中的除零/类型错误不会触发。
13. **EXPLAIN 不是优化器**：`EXPLAIN SELECT` 只绑定不执行，有 `ANALYZE` 统计时显示启发式 `est rows` / `est groups`，没有统计时退回无数字计划；`EXPLAIN ANALYZE` 会真实执行一次，显示本次 `actual` 和墙钟耗时，不返回查询结果。两者都不做候选计划比较或代价决策。`JoinStrategy` 是进程内 API 开关，不是 SQL hint。

## 5. CI 与覆盖

- CROSS JOIN/逗号 FROM 切片 CI run：<https://github.com/bfxh/trit-db/actions/runs/44639115915>，11 个 job 全绿（lint、三平台 test、两平台 MSRV、三平台 Python 对拍、coverage、docs）。
- NATURAL JOIN/USING 切片：本地 `sql_oracle.py` 133/133（127 SELECT + 6 UPDATE 行数）；等值 hash join 切片当前为 **138/138**（132 SELECT + 6 UPDATE 行数）。三平台 CI 待推送后补。上一轮 CROSS JOIN CI 的三平台对拍为 118/118。
- 行覆盖：SQL 3VL 切片 4710/6559 = 71.81%，GROUP BY/HAVING 后 4906/6777 = 72.39%，INNER JOIN 后 5080/6957 = 73.02%，LEFT JOIN 后 5117/6991 = 73.19%，UPDATE 后 5215/7081 = 73.65%，谓词后 5531/7426 = 74.48%，CASE 后 5766/7646 = 75.41%，RIGHT/FULL JOIN 后 5814/7692 = 75.59%，CROSS JOIN/逗号 FROM 后 **5859/7737 = 75.73%**；NATURAL/USING、hash join、EXPLAIN、EXPLAIN ANALYZE、ANALYZE 统计与估算切片尚未重跑覆盖率，65% 门禁仍有余量。
- 本机无法用 GNU/MSVC 工具链跑 `llvm-cov`（缺 profiler runtime / MSVC linker），覆盖数字取自 CI 的 `coverage-lcov` artifact。

## 6. 下一步

- 等值 hash join、可复现 `sql-join` 基准、`EXPLAIN SELECT`、`EXPLAIN ANALYZE` 与 `ANALYZE` 统计已做：右表建桶、左表探测，`USING`/`NATURAL` 与 `ON` 顶层 `AND` 的列等值可命中；非等值/OR/表达式键回退 nested loop；`JoinStrategy` 可强制路径，`EXPLAIN` 直接显示 `HashJoin` / `NestedLoop`、键与启发式 `est rows`，`EXPLAIN ANALYZE` 显示实际行数/分组数与耗时。下一步做基于这些统计的 join 代价比较，让 `Auto` 能在 hash 与 nested loop 之间做决策；再下一步是逻辑计划/列裁剪/谓词下推。子查询需要新的作用域和计划层，成本更高。
- `tritdb-plan`：逻辑计划、列裁剪、谓词下推、常量折叠。
- `tritdb-proto`：pgwire v3 simple query + extended query，先让 `psql` 能连。
- 表存储从整表重写改成页/行格式，接 `tritdb-page`。
- SQL 3VL + GROUP BY + JOIN + DML + 谓词 + CASE 套件扩到 `trit-clean` 的完整原始语义用例（当前 132 条 SELECT + 6 条 UPDATE 行数对拍为本仓自定义对拍集）。
- 并发 SQL 压力测试：多连接 SELECT + 单写者 INSERT，测读者 P99。
