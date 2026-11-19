# SQL：M1 最小三值查询层

状态：已实现最小 parser / binder / executor / CLI / SQLite 对拍；pgwire、优化器、GROUP BY、JOIN、子查询仍未做。

## 1. 范围

这一层不是完整 SQL 引擎，只做一条能判红的纵向切片：

```sql
CREATE TABLE t (id INT NOT NULL, name TEXT, flag BOOL);
INSERT INTO t VALUES (1, 'a', TRUE), (2, NULL, UNKNOWN);
SELECT id, name, flag FROM t
WHERE flag IS NULL OR id >= 2
ORDER BY id DESC
LIMIT 2;
DELETE FROM t WHERE id < 2;
DROP TABLE t;
```

- 单表、无 JOIN、无子查询、无 GROUP BY。
- 每个语句自动提交；脚本不是一个大事务。
- 表整表保存在 `TxnStore` 的一个 value 里；每次写重写整张表。
- 没有 pgwire、没有优化器、没有索引、没有 MVCC。
- 类型严格：不会把 TEXT 隐式转成 INT。

## 2. 词法

- 关键字大小写不敏感；未引用标识符转小写。
- `"quoted ident"` 保留原样，支持 `""` 转义。
- 字符串用单引号，支持 `''` 转义。
- 支持 `--` 行注释和 `/* ... */` 块注释。
- 整数只接受十进制 `i64`。

## 3. 语句

### CREATE TABLE

```sql
CREATE TABLE [IF NOT EXISTS] name (
  column type [NOT NULL],
  ...
);
```

类型：`INT` / `INTEGER` / `BIGINT`、`TEXT` / `VARCHAR` / `STRING`、`BOOL` / `BOOLEAN`。
`INT` 全部落入 `i64`，`VARCHAR` 不带长度。

### INSERT

```sql
INSERT INTO name [(column, ...)] VALUES (expr, ...) [, (expr, ...)];
```

- 省略列清单时按 schema 顺序。
- 省略的列填 `NULL`；若该列 `NOT NULL` 则报错。
- 值表达式只能是常量/算术，不允许引用列。

### SELECT

```sql
SELECT * | expr [AS alias], ...
FROM name
[WHERE predicate]
[ORDER BY expr [ASC|DESC], ...]
[LIMIT n];
```

- `WHERE` 只保留谓词为 `TRUE` 的行；`FALSE` 和 `UNKNOWN` 都过滤掉。
- `ORDER BY` 支持源列和 SELECT 别名；`NULL` 在 ASC 时排最前，DESC 时排最后。
- `LIMIT` 在排序和投影之后截断。
- 聚合查询只支持单组：`COUNT(*)`、`COUNT(expr)`、`SUM(expr)`、`MIN(expr)`、`MAX(expr)`；聚合查询不能混入裸列引用。
- `ORDER BY` 与聚合同时使用暂不支持。

### DELETE / DROP

```sql
DELETE FROM name [WHERE predicate];
DROP TABLE [IF EXISTS] name;
```

## 4. 三值逻辑

`NULL` 在布尔上下文就是 `UNKNOWN`；`UNKNOWN` 字面量也映射到 `NULL`。

| AND | T | F | U |
|---|---|---|---|
| T | T | F | U |
| F | F | F | F |
| U | U | F | U |

| OR | T | F | U |
|---|---|---|---|
| T | T | T | T |
| F | T | F | U |
| U | T | U | U |

`NOT U = U`。比较运算任一侧为 `NULL` 时结果为 `NULL`。

- `IS NULL` / `IS NOT NULL` 永远返回 `TRUE`/`FALSE`。
- `IS UNKNOWN` / `IS NOT UNKNOWN` 对布尔/`NULL` 表达式返回 `TRUE`/`FALSE`。
- `IS TRUE` / `IS FALSE` 同理；非布尔类型报错。

## 5. 存储格式

表 key：

```text
sql/table/<table-name>
```

value 是 `TRSQ` v0：

```text
0..4    "TRSQ"
4       version = 0
5..9    u32 column_count
per column:
  u8    type: 0=INT, 1=TEXT, 2=BOOL
  u8    not_null
  u32   name_len
  bytes name
u64 LE  row_count
per row:
  per column:
    u8  value tag: 0=NULL, 1=INT, 2=TEXT, 3=BOOL
        INT:  i64 LE
        TEXT: u32 len + bytes
        BOOL: u8
```

- 编码时检查行宽、列类型；解码时检查 magic/version/截断/trailing bytes/NULL 进入 `NOT NULL` 列。
- 表值走 `TxnStore` 的 `Begin/Put/Commit`，因此复用 WAL、提交组、快照 checkpoint 和崩溃恢复。
- 每次写重写整表；大表会产生明显写放大，这是 M1 语义切片的已知边界。

## 6. CLI

```bash
tritdb sql --db sql.db --sql "CREATE TABLE t (id INT); INSERT INTO t VALUES (1);"
tritdb sql --db sql.db --sql "SELECT * FROM t;"
tritdb sql --db sql.db --file script.sql
```

输出为 TSV：第一行列名，之后每行一条记录；`NULL` 显示为 `NULL`，布尔显示 `TRUE`/`FALSE`。

## 7. 验收门

- Rust 单元/集成测试覆盖 parser、存储编解码、3VL、NULL 排序、聚合、DELETE/重开。
- Python `sqlite3` 对拍 `tools/crosscheck/sql_oracle.py`，当前 34/34 通过。
- CI 三平台 `python-crosscheck` job 同时跑向量、CLI 反校验和 SQL 对拍。
