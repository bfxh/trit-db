# M5 第三刀：递归 CTE（`WITH [RECURSIVE]`）

状态：`WITH [RECURSIVE]` 的解析、working-table 不动点执行、嵌套 `WITH`、无 `FROM` 锚点、`WITH ... INSERT/UPDATE/DELETE`、`INSERT ... SELECT` 与 CLI/测试均已实现；子查询未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-sql/src/lexer.rs    WITH / RECURSIVE / UNION / ALL 关键字
crates/tritdb-sql/src/ast.rs      Query / Cte / With / Statement::With / Insert.select
crates/tritdb-sql/src/parser.rs   parse_with / parse_query（可选 UNION、嵌套 WITH、FROM 可选）
crates/tritdb-sql/src/engine.rs   with_query / materialize_cte（working table + 上限）
                                  execute_query / INSERT ... SELECT
crates/tritdb/tests/sql_cli.rs    4 个递归 CTE 端到端测试
docs/ADR/041-recursive-cte.md
```

## 2. 用法

```sql
-- 递归：环形图去重
WITH RECURSIVE reach(node) AS (
  SELECT src FROM edges WHERE src = 0
  UNION
  SELECT e.dst FROM edges e JOIN reach r ON e.src = r.node
)
SELECT node FROM reach ORDER BY node;
-- 0, 1, 2, 3

-- 无 FROM 锚点
WITH RECURSIVE nums(n) AS (
  SELECT 1 UNION ALL SELECT n + 1 FROM nums WHERE n < 3
)
SELECT n FROM nums ORDER BY n;
-- 1, 2, 3

-- 嵌套 WITH
WITH a AS (WITH b AS (SELECT id FROM t) SELECT id FROM b) SELECT id FROM a;

-- WITH + INSERT ... SELECT
WITH c AS (SELECT id FROM t WHERE id > 1) INSERT INTO u SELECT id FROM c;
```

## 3. 语义

- `seed` / `step` 是 `SELECT` 或嵌套 `WITH`；`UNION [ALL] step` 可省略（普通 CTE）。
- 无 `FROM` 的 `SELECT` 扫描零列单行虚拟表，`SELECT 1` 可作为锚点。
- 递归项只看到**上一轮新行**（working table），不是全部累积结果；这是 `UNION ALL` 能终止的前提。
- `UNION` 对新行与累积结果去重；`UNION ALL` 保留重复；下一轮无新行即不动点。
- 上限：1000 轮、10 万行；超限返回 `Unsupported`，不 OOM。
- CTE 物化为内存 `Table`；列名取显式列表或 seed 输出，类型取每列第一个非 `NULL` 值。
- body 支持 `SELECT` / 嵌套 `WITH` / `INSERT` / `UPDATE` / `DELETE`；DDL 被拒绝。
- `INSERT ... SELECT` 执行查询后按目标列做 `coerce` 与 `NOT NULL` 检查，再走与 `VALUES` 相同的约束、唯一性与落盘路径。

## 4. 验收

```bash
cargo test -p tritdb-sql --lib parses_              # 19 passed（含 2 个 WITH 解析测试）
cargo test -p tritdb --test sql_cli with_           # 4 passed
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

覆盖：环形图 `UNION` 去重到 `{0,1,2,3}`、树形 `UNION ALL` 不重复、列数不匹配报错、无不动点触发迭代上限、无 `FROM` 锚点、嵌套 `WITH`、`WITH ... INSERT/UPDATE/DELETE`、`INSERT ... SELECT`、DDL body 被拒绝。

## 5. 还没做

- **子查询**：`UPDATE`/`DELETE` 的 `WHERE` 里没有子查询，CTE 实际只能被 `SELECT` 与 `INSERT ... SELECT` 消费；
- **类型推断**：启发式（第一个非 `NULL` 值），混类型列会在后续比较时报错；
- **大规模图**：`UNION` 去重是线性扫描，CTE 不落盘、不产生统计信息。
