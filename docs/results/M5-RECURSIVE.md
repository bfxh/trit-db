# M5 第三刀：递归 CTE（`WITH [RECURSIVE]`）

状态：`WITH [RECURSIVE]` 的解析、working-table 不动点执行、CLI/测试已实现；嵌套 `WITH`、无 `FROM` 锚点、`WITH ... DML` 未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-sql/src/lexer.rs    WITH / RECURSIVE / UNION / ALL 关键字
crates/tritdb-sql/src/ast.rs      Cte / With / Statement::With
crates/tritdb-sql/src/parser.rs   parse_with（seed UNION [ALL] step）
crates/tritdb-sql/src/engine.rs   with_query / materialize_cte（working table + 上限）
crates/tritdb/tests/sql_cli.rs    2 个递归 CTE 端到端测试
docs/ADR/041-recursive-cte.md
```

## 2. 用法

```sql
CREATE TABLE edges (src INT NOT NULL, dst INT NOT NULL);
INSERT INTO edges VALUES (0,1),(1,2),(2,3);

WITH RECURSIVE reach(node) AS (
  SELECT src FROM edges WHERE src = 0
  UNION
  SELECT e.dst FROM edges e JOIN reach r ON e.src = r.node
)
SELECT node FROM reach ORDER BY node;
-- 0, 1, 2, 3
```

```bash
cargo run -p tritdb -- sql --db rec.db --sql "WITH RECURSIVE ... SELECT ..."
```

## 3. 语义

- `seed` / `step` 都是完整 `SELECT`（必须有 `FROM`）。
- 递归项只看到**上一轮新行**（working table），不是全部累积结果；这是 `UNION ALL` 能终止的前提。
- `UNION` 对新行与累积结果去重；`UNION ALL` 保留重复。
- 下一轮无新行即到不动点。
- 上限：1000 轮、10 万行；超限返回 `Unsupported`，不 OOM。
- CTE 物化为内存 `Table`；列名取显式列表或 seed 输出，类型取每列第一个非 `NULL` 值。
- 执行期间临时注入 `SqlEngine.tables`，语句结束后恢复同名真实表。

## 4. 验收

```bash
cargo test -p tritdb-sql --lib parses_                 # 19 passed（含 2 个 WITH 测试）
cargo test -p tritdb --test sql_cli with_recursive      # 2 passed
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

覆盖：环形图 `UNION` 去重到 `{0,1,2,3}`、树形 `UNION ALL` 不重复、列数不匹配报错、无不动点触发迭代上限。

## 5. 还没做

- **嵌套 `WITH`**：递归项内部或子查询里不能再写 `WITH`；
- **无 `FROM` 锚点**：`SELECT 1` 作为 seed 暂不支持；
- **`WITH ... INSERT/UPDATE/DELETE`**：只支持引导 `SELECT`；
- **类型推断**：启发式（第一个非 `NULL` 值），混类型列会在后续比较时报错；
- **大规模图**：`UNION` 去重是线性扫描，CTE 不落盘、不产生统计信息。
