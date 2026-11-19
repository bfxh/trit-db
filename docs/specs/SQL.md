# SQL：M1 最小三值查询层

状态：已实现最小 parser / binder / executor / CLI / SQLite 对拍、单表 `GROUP BY`/`HAVING`、多表 `INNER JOIN` 与 `LEFT [OUTER] JOIN`（表别名 + 限定列）、`UPDATE ... SET ...`、`IN`/`BETWEEN`/`LIKE` 谓词；`RIGHT/FULL JOIN`、子查询、pgwire、优化器仍未做。

## 1. 范围

这一层不是完整 SQL 引擎，只做一条能判红的纵向切片：

```sql
CREATE TABLE t (id INT NOT NULL, name TEXT, flag BOOL);
INSERT INTO t VALUES (1, 'a', TRUE), (2, NULL, UNKNOWN);
SELECT flag, COUNT(*) AS n FROM t
GROUP BY flag
HAVING COUNT(*) >= 1
ORDER BY flag;
SELECT id, name, flag FROM t
WHERE flag IS NULL OR id >= 2
ORDER BY id DESC
LIMIT 2;
SELECT d.name, COUNT(*) AS n
FROM users AS u JOIN depts AS d ON u.dept = d.id
GROUP BY d.name
ORDER BY d.name;
SELECT u.name, d.name AS dept
FROM users AS u LEFT JOIN depts AS d ON u.dept = d.id
ORDER BY u.name;
SELECT id FROM t WHERE id IN (1, 2) AND score BETWEEN 10 AND 20 AND name LIKE 'a%';
UPDATE t SET name = 'b', flag = NULL WHERE id = 2;
DELETE FROM t WHERE id < 2;
DROP TABLE t;
```

- 支持表别名、多表 `INNER JOIN` 和 `LEFT [OUTER] JOIN`；不支持 `RIGHT`/`FULL`/`CROSS`/`NATURAL JOIN`、`USING`、逗号 join 或子查询。
- `GROUP BY` 只接受列引用（可带表限定），不接受表达式、位置序号或 GROUPING SETS。
- 每个语句自动提交；脚本不是一个大事务。
- 表整表保存在 `TxnStore` 的一个 value 里；每次 INSERT/DELETE/UPDATE 重写整张表。
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
FROM name [AS alias]
[[INNER | LEFT [OUTER]] JOIN name [AS alias] ON predicate]...
[WHERE predicate]
[GROUP BY column, ...]
[HAVING predicate]
[ORDER BY expr [ASC|DESC], ...]
[LIMIT n];
```

- `FROM` 后可带 `AS alias` 或裸别名；别名定义限定名命名空间。
- `JOIN` 支持 `INNER`（`JOIN` 等价于 `INNER JOIN`）和 `LEFT [OUTER] JOIN`，可连续多个，按左到右累积；每个 `ON` 看到当前已连接的全部列。
- `ON` 是逐行谓词，只保留结果为 `TRUE` 的组合；`FALSE` 和 `UNKNOWN` 都不产生匹配行。`ON` 中不接受聚合或子查询。
- `INNER JOIN` 只输出匹配组合；`LEFT JOIN` 对每个左行，若没有任何 `ON=TRUE` 的右行，就输出该左行并把右侧所有列置为 `NULL`。右表为空时，左表每一行都以右侧全 `NULL` 输出。
- `LEFT JOIN` 的右侧列在输出 schema 中一律标记为可空；`WHERE` 在 join 之后执行，因此 `WHERE right.col IS NULL` 可筛出未匹配左行。
- 限定列写作 `alias.column`；非限定列先精确匹配，再按唯一列名解析，多表同名列必须限定，否则报 `ambiguous column`。
- 别名/表名在同一 `FROM` 子句内必须唯一；自连接必须给同一张表分配不同别名，否则报 `duplicate table name or alias`。
- JOIN 使用嵌套循环实现，不引入 hash join 或优化器；左右表都物化为临时结果，内存随行数乘积增长。
- 输出列标签去掉限定前缀；`SELECT *` 在多表 join 中遇到同名列会产生重复标签。

- `WHERE` 在 join 之后执行，只保留谓词为 `TRUE` 的行；`FALSE` 和 `UNKNOWN` 都过滤掉。
- `GROUP BY` 只支持直接列引用（可带表限定）；`NULL` 作为同一个分组键；空输入在有 `GROUP BY` 时产生零组，无 `GROUP BY` 的聚合产生一行（`COUNT` 为 0，`SUM`/`MIN`/`MAX` 为 `NULL`）。
- `HAVING` 在分组聚合之后、投影/排序之前执行，只保留 `TRUE`；可以引用分组键和聚合表达式，不能引用未分组列。
- `SELECT` 聚合项支持 `COUNT(*)`、`COUNT(expr)`、`SUM(expr)`、`MIN(expr)`、`MAX(expr)`；分组查询中的裸列必须是 `GROUP BY` 列，标量表达式只能引用分组列。
- `ORDER BY` 支持源列、SELECT 别名、聚合表达式；分组查询按分组键/聚合结果排序。`NULL` 在 ASC 时排最前，DESC 时排最后。
- `LIMIT` 在排序和投影之后截断。
- `SELECT *` 不能与 `GROUP BY` 或聚合同时使用；`GROUP BY` 表达式、聚合查询中的未分组裸列会被 binder 拒绝。

### 谓词

```sql
expr IN (expr, ...)
expr NOT IN (expr, ...)
expr BETWEEN low AND high
expr NOT BETWEEN low AND high
expr [NOT] LIKE pattern
```

- `IN` 对每个候选值做等值比较：任一为 `TRUE` 则结果 `TRUE`；否则只要有 `NULL` 参与（探针或候选）就为 `UNKNOWN`；否则 `FALSE`。`NOT IN` 对结果做三值取反；`NULL NOT IN (...)` 仍是 `UNKNOWN`。
- `IN` 列表至少一个元素，不支持子查询、`IN` 空列表或 `IN` 后的行构造器。
- `BETWEEN` 等价于 `expr >= low AND expr <= high`，两端都求值并按三值 `AND` 组合；`NOT BETWEEN` 对结果取反。边界含等号。
- `LIKE` 左值和模式都必须是 `TEXT`（`NULL` 传播为 `NULL`）；`%` 匹配任意长度（含空串），`_` 匹配任意单个字符；ASCII 字母大小写不敏感，非 ASCII 按原字符比较。不支持 `ESCAPE`、`GLOB` 或正则。
- 这些谓词可用于 `WHERE`、`ON`、`HAVING` 和非聚合投影表达式；`UPDATE`/`DELETE` 的 `WHERE` 复用同一求值路径。

### UPDATE

```sql
UPDATE name SET column = expr [, column = expr]... [WHERE predicate];
```

- 目标列必须是不带限定的列名；未知列、重复目标列直接报错。
- 所有 `SET` 右值都针对**更新前的整行**求值，再统一写回；`UPDATE t SET a = b, b = a` 是交换，不会级联看到新值。
- 右值按目标列类型严格转换；把 `NULL` 写进 `NOT NULL` 列会拒绝整条语句。
- 只有 `WHERE` 结果为 `TRUE` 的行被更新；`FALSE`/`UNKNOWN` 与 `DELETE` 一样跳过。
- 先在整个表的副本上算完；任一行的求值或约束失败时不会落盘，表保持原状。
- 返回消息 `updated N row(s) in <table>`，`N` 是谓词为 `TRUE` 的行数；零行不重写表值。
- 不支持 `UPDATE ... FROM`、多表更新、子查询或限定目标列。

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

- Rust 单元/集成测试覆盖 parser、存储编解码、3VL、NULL 排序、聚合、GROUP BY/HAVING、INNER/LEFT JOIN、IN/BETWEEN/LIKE、UPDATE、DELETE/重开。
- Python `sqlite3` 对拍 `tools/crosscheck/sql_oracle.py`，当前 89/89 项通过（83 条 SELECT + 6 条 UPDATE 行数）。
- CI 三平台 `python-crosscheck` job 同时跑向量、CLI 反校验和 SQL 对拍。
