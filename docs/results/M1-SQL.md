# M1 SQL 3VL：最小纵向切片读数

状态：parser / binder / executor / CLI / 持久化 / SQLite 对拍已实现。
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
- `SELECT`：`WHERE`、`ORDER BY`、`LIMIT`、别名、`*`
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
sql oracle: 34/34 passed
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
| `tritdb-sql` 单元测试 | 9 |
| `tritdb-sql` 引擎集成测试 | 7 |
| `tritdb` SQL CLI 集成测试 | 2 |
| **合计** | **26** |

工作区测试从 158 增至 **184 个正常测试 + 4 个 ignored 压力测试**。

## 4. 负结果 / 边界

1. **整表重写**：每次 INSERT/DELETE 都把整张表重新编码成一个 KV value。数据量增大后写放大和内存峰值会先撞墙，M1 不解决。
2. **没有 GROUP BY / JOIN / 子查询 / 优化器**：当前只是语义切片；SQLite 对拍覆盖的是单表表达式和单组聚合。
3. **没有 pgwire**：`psql` 还连不上；CLI 是当前唯一入口。
4. **每个语句自动提交**：脚本不是一个大事务；`INSERT` 多行在一个事务内，但 `CREATE`/`INSERT`/`SELECT` 之间不共享事务。
5. **没有跨进程锁**：两个进程写同一个 SQL WAL 仍是未定义行为，和 M0 一致。
6. **没有统计信息/索引**：SELECT 全表扫描，ORDER BY 全量排序。
7. **类型严格**：不做 SQLite 式隐式转换；`INSERT INTO t (id) VALUES ('x')` 会拒绝。
8. **NULL 排序按 SQLite**：ASC 最前、DESC 最后；PostgreSQL 默认相反，后续要做方言开关。

## 5. 下一步

- 多表 JOIN 与 GROUP BY，先做 hash join / hash aggregate。
- `tritdb-plan`：逻辑计划、列裁剪、谓词下推、常量折叠。
- `tritdb-proto`：pgwire v3 simple query + extended query，先让 `psql` 能连。
- 表存储从整表重写改成页/行格式，接 `tritdb-page`。
- SQL 3VL 套件扩到 `trit-clean` 的完整 34 条原始语义用例（当前 34 条为本仓自定义对拍集）。
- 并发 SQL 压力测试：多连接 SELECT + 单写者 INSERT，测读者 P99。
