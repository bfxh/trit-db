# M6 第一刀：非相关子查询（`IN (SELECT ...)` / 标量子查询）

状态：`IN (SELECT ...)`、`NOT IN`、标量子查询、与 `WITH`/`UPDATE`/`DELETE`/`INSERT ... SELECT` 的对接已实现；相关子查询、`EXISTS`、多列 `IN` 未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-sql/src/ast.rs      Expr::InSubquery / Expr::ScalarSubquery
crates/tritdb-sql/src/parser.rs   IN (SELECT ...) / (SELECT ...) 标量
crates/tritdb-sql/src/engine.rs   materialize_subqueries / materialize_expr
                                  evaluate_subquery_column / evaluate_scalar_subquery
crates/tritdb-sql/src/bind.rs     子查询必须物化后才能绑定
crates/tritdb/tests/sql_cli.rs    3 个子查询端到端测试
docs/ADR/042-subqueries.md
```

## 2. 用法

```sql
-- IN / NOT IN
SELECT id FROM t WHERE id IN (SELECT id FROM u) ORDER BY id;
SELECT id FROM t WHERE id NOT IN (SELECT id FROM u) ORDER BY id;

-- 标量子查询
SELECT id, (SELECT MAX(id) FROM u) AS m FROM t ORDER BY id;

-- 把 CTE 接进 UPDATE / DELETE
WITH c AS (SELECT id FROM u) UPDATE t SET id = id WHERE id IN (SELECT id FROM c);
WITH c AS (SELECT id FROM u WHERE id = 2) DELETE FROM t WHERE id IN (SELECT id FROM c);
```

## 3. 语义

- 执行前把子查询物化：`InSubquery` → `InList { values: [Literal(v), ...] }`，`ScalarSubquery` → `Literal(v)`；binder/executor/裁剪只看到普通表达式。
- `execute_query` 统一物化，覆盖 `WITH` 的 seed/step、`INSERT ... SELECT`、`IN (SELECT ...)` 嵌套。
- `IN` 子查询必须单列；标量子查询必须单列、零或一行（0 行 → `NULL`，多行 → `Unsupported`）。
- `NOT IN` 复用既有三值 `IN` 语义：列表含 `NULL` 且无匹配时为 `UNKNOWN`。
- 相关子查询不支持：子查询引用外层列会报 `UnknownColumn`。

## 4. 验收

```bash
cargo test -p tritdb --test sql_cli subquer   # 3 passed
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

覆盖：`IN` / `NOT IN` 过滤、标量投影、标量零行 `NULL`、标量多行报错、`IN` 多列报错、`WITH` + `UPDATE`/`DELETE` 通过子查询消费 CTE。

## 5. 还没做

- **相关子查询**：引用外层列的子查询；
- **`EXISTS` / `ANY` / `ALL`**；
- **多列 `IN`**：`(a, b) IN (SELECT ...)`；
- **大结果集**：`IN` 物化为字面量列表，超大子查询结果会占内存。
