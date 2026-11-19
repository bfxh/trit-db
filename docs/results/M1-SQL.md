# M1 SQL 3VL：最小纵向切片读数

状态：parser / binder / executor / CLI / 持久化 / SQLite 对拍 / 单表 GROUP BY / 多表 INNER JOIN / LEFT JOIN 已实现。
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
- `LEFT [OUTER] JOIN`：未匹配左行保留、右侧列补 `NULL`、空右表、LEFT JOIN 后 `WHERE/GROUP BY`
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
sql oracle: 63/63 passed
```

对拍口径：

- 两边执行同一份 `CREATE TABLE` + `INSERT`；
- 每条 SELECT 单独跑，比较行值，不比较列名；
- `TRUE`/`FALSE` 归一化为 `1`/`0`，`NULL` 归一化为 `NULL`；
- SQLite 不支持 `IS UNKNOWN`，该语义由 Rust 测试单独覆盖。

## 3. 测试增量

| 层 | 新增 |
|---|---:|
| `tritdb-exec` 单元测试 | 8 |
| `tritdb-sql` 单元测试 | 13 |
| `tritdb-sql` 引擎集成测试 | 19 |
| `tritdb` SQL CLI 集成测试 | 2 |
| **合计** | **42** |

其中 26 个来自初始 SQL 3VL 切片，6 个来自 GROUP BY/HAVING 切片，10 个来自 JOIN 切片（3 个 parser + 7 个 engine）。工作区测试从 158 增至 **200 个正常测试 + 4 个 ignored 压力测试**。

## 4. 负结果 / 边界

1. **整表重写**：每次 INSERT/DELETE 都把整张表重新编码成一个 KV value。数据量增大后写放大和内存峰值会先撞墙，M1 不解决。
2. **JOIN 只做 INNER/LEFT，且是嵌套循环**：不支持 `RIGHT/FULL/CROSS/NATURAL JOIN`、`USING`、逗号 join 或子查询；左右表都物化，复杂度是行数乘积，后续再引入 hash join/优化器。`GROUP BY` 只支持列引用，不支持分组表达式、位置序号、GROUPING SETS/CUBE/ROLLUP。SQLite 对拍已覆盖单表表达式、分组聚合、`HAVING`、INNER JOIN 和 LEFT JOIN。
3. **没有 pgwire**：`psql` 还连不上；CLI 是当前唯一入口。
4. **每个语句自动提交**：脚本不是一个大事务；`INSERT` 多行在一个事务内，但 `CREATE`/`INSERT`/`SELECT` 之间不共享事务。
5. **没有跨进程锁**：两个进程写同一个 SQL WAL 仍是未定义行为，和 M0 一致。
6. **没有统计信息/索引**：SELECT 全表扫描，ORDER BY 全量排序。
7. **类型严格**：不做 SQLite 式隐式转换；`INSERT INTO t (id) VALUES ('x')` 会拒绝。
8. **分组全内存**：`GROUP BY` 用 `BTreeMap<Vec<GroupKeyPart>, Vec<&Row>>`，没有 hash aggregate、分区或 spill；组数/行数受内存限制。
9. **NULL 排序按 SQLite**：ASC 最前、DESC 最后；PostgreSQL 默认相反，后续要做方言开关。
10. **JOIN 全内存**：每个 join 都构造新的临时表并复制行，没有流式执行、批处理或 spill；多表/大表会快速放大内存。

## 5. CI 与覆盖

- INNER JOIN 切片 CI run：<https://github.com/bfxh/trit-db/actions/runs/44613405869>，11 个 job 全绿（lint、三平台 test、两平台 MSRV、三平台 Python 对拍、coverage、docs）。
- 三平台 `python-crosscheck` 均执行 `sql_oracle.py`，均为 **57/57**。
- 行覆盖：SQL 3VL 切片 4710/6559 = 71.81%，GROUP BY/HAVING 后 4906/6777 = 72.39%，INNER JOIN 后 **5080/6957 = 73.02%**，65% 门禁仍有余量。
- 本机无法用 GNU/MSVC 工具链跑 `llvm-cov`（缺 profiler runtime / MSVC linker），覆盖数字取自 CI 的 `coverage-lcov` artifact。

## 6. 下一步

- INNER JOIN 已做嵌套循环；LEFT JOIN 复用同一循环并补未匹配行。下一步做 hash join、`RIGHT/FULL JOIN`，并把 JOIN 对拍扩到更多 NULL/复合键组合。
- `tritdb-plan`：逻辑计划、列裁剪、谓词下推、常量折叠。
- `tritdb-proto`：pgwire v3 simple query + extended query，先让 `psql` 能连。
- 表存储从整表重写改成页/行格式，接 `tritdb-page`。
- SQL 3VL + GROUP BY + JOIN 套件扩到 `trit-clean` 的完整原始语义用例（当前 57 条为本仓自定义对拍集）。
- 并发 SQL 压力测试：多连接 SELECT + 单写者 INSERT，测读者 P99。
