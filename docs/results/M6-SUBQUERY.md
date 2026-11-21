# M6：子查询（`IN` / `EXISTS` / `ANY` / `ALL` / 相关）

状态：`IN (SELECT ...)`、`NOT IN`、标量子查询、`EXISTS`、`ANY` / `ALL`、`WHERE` 中的相关子查询、与 `WITH`/`UPDATE`/`DELETE`/`INSERT ... SELECT` 的对接已实现；投影/`HAVING` 中的相关子查询、多列 `IN` 未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-sql/src/ast.rs      Expr::InSubquery / Expr::ScalarSubquery / Expr::Exists / Expr::Quantified
crates/tritdb-sql/src/parser.rs   IN (SELECT ...) / (SELECT ...) 标量 / EXISTS / ANY / ALL
crates/tritdb-sql/src/engine.rs   materialize_subqueries / materialize_expr
                                  evaluate_subquery_column / evaluate_scalar_subquery
                                  select_with_correlated_filter / bind_correlated_expr
                                  substitute_outer / expr_has_outer_subquery / quantified_expr
crates/tritdb-sql/src/bind.rs     子查询必须物化后才能绑定
crates/tritdb/tests/sql_cli.rs    5 个子查询端到端测试
docs/ADR/042-subqueries.md
```

## 2. 用法

```sql
-- IN / NOT IN
SELECT id FROM t WHERE id IN (SELECT id FROM u) ORDER BY id;
SELECT id FROM t WHERE id NOT IN (SELECT id FROM u) ORDER BY id;

-- 标量子查询
SELECT id, (SELECT MAX(id) FROM u) AS m FROM t ORDER BY id;

-- EXISTS / ANY / ALL
SELECT id FROM t WHERE EXISTS (SELECT 1 FROM u WHERE u.id = t.id) ORDER BY id;
SELECT id FROM t WHERE id > ANY (SELECT id FROM u) ORDER BY id;
SELECT id FROM t WHERE id <> ALL (SELECT id FROM u) ORDER BY id;

-- 把 CTE 接进 UPDATE / DELETE
WITH c AS (SELECT id FROM u) UPDATE t SET id = id WHERE id IN (SELECT id FROM c);
WITH c AS (SELECT id FROM u WHERE id = 2) DELETE FROM t WHERE id IN (SELECT id FROM c);
```

## 3. 语义

- 执行前把子查询物化：`InSubquery` → `InList { values: [Literal(v), ...] }`，`ScalarSubquery` → `Literal(v)`；binder/executor/裁剪只看到普通表达式。
- `execute_query` 统一物化，覆盖 `WITH` 的 seed/step、`INSERT ... SELECT`、`IN (SELECT ...)` 嵌套。
- `IN` 子查询必须单列；标量子查询必须单列、零或一行（0 行 → `NULL`，多行 → `Unsupported`）。
- `NOT IN` 复用既有三值 `IN` 语义：列表含 `NULL` 且无匹配时为 `UNKNOWN`。
- 相关子查询（`WHERE` 中）：子查询引用外层限定列（`outer.col`）时走 `select_with_correlated_filter`——逐行把外层列替换成字面量、执行子查询、求值 filter，再复用投影/分组/排序；逐行 nested-loop，不是为大数据集设计的。
- 投影 / `HAVING` / `ORDER BY` 中的相关子查询仍报 `Unsupported`；外层列必须限定。
- `EXISTS`：子查询有行 → `TRUE`，否则 `FALSE`；`NOT EXISTS` 由 `Not` 包裹。
- `ANY` / `ALL`：逐值比较后按三值 `OR` / `AND` 合并；空集 `ANY` → `FALSE`、`ALL` → `TRUE`。

## 4. 验收

```bash
cargo test -p tritdb --test sql_cli subquer   # 5 passed
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

覆盖：`IN` / `NOT IN` 过滤、标量投影、标量零行 `NULL`、标量多行报错、`IN` 多列报错、`WITH` + `UPDATE`/`DELETE` 通过子查询消费 CTE、相关 `IN` / 相关标量、非相关路径回归、投影相关子查询被拒绝、`EXISTS` / `NOT EXISTS` / `> ANY` / `> ALL` / `<> ALL`、空集 `ANY` → 空、空集 `ALL` → 全保留。

## 5. 还没做

- **投影 / `HAVING` / `ORDER BY` 中的相关子查询**：当前只支持 `WHERE`；
- **多列 `IN`**：`(a, b) IN (SELECT ...)`；
- **大结果集**：`IN` 物化为字面量列表，超大子查询结果会占内存。
