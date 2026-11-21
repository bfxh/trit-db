# ADR 042：非相关子查询（`IN (SELECT ...)` / 标量子查询）

状态：**Proposed**

日期：2026-11-21

## 背景

`WITH ... UPDATE/DELETE` 虽然语法可用，但 `UPDATE`/`DELETE` 的 `WHERE` 里没有子查询，CTE 实际上无法被它们消费；投影里也不能写 `(SELECT ...)`。最小子查询能把 CTE 与 DML 真正接起来。

## 决策

1. **语法**
   ```sql
   expr [NOT] IN (SELECT ... | WITH ...)
   (SELECT ... | WITH ...)            -- 标量位置
   ```
   - `IN` 子查询必须返回单列；标量子查询必须返回单列、零或一行。
   - 子查询可以是 `SELECT` 或 `WITH`（复用 [`Query`]）。

2. **非相关：执行前物化**
   - `execute_statement` / `execute_query` 在执行前遍历表达式，把：
     - `InSubquery { expr, query, negated }` → `InList { expr, values: [Literal(v), ...], negated }`；
     - `InSubqueryTuple { exprs, query, negated }` → 每行生成 `exprs[0] = row[0] AND ...` 的 `AND` 链，再按行 `OR` 合并；空集 `IN` → `FALSE`、`NOT IN` → `TRUE`；
     - `ScalarSubquery { query }` → `Literal(value)`；
     - `Exists { query }` → `Literal(Bool(rows > 0))`；
     - `Quantified { expr, op, kind, query }` → 对每个结果值生成 `expr op value`，按 `ANY`→`OR` / `ALL`→`AND` 串起来（空集：`ANY`→`FALSE`、`ALL`→`TRUE`）。
   - binder / executor / 常量折叠 / 列裁剪只看到 `InList` 与 `Literal`，不需要知道子查询存在。
   - `execute_query` 统一做物化，覆盖 `WITH` 的 seed/step、`INSERT ... SELECT`、`IN (SELECT ...)` 嵌套。

3. **相关：逐行替换后执行**
   - `WHERE` 或投影中出现引用外层限定列（`outer.col`）的子查询时，`select` 走 `select_with_correlated`：
     1. `build_from` 得到外层表；
     2. `WHERE` 中的相关子查询：每行把 `outer.col` 替换成 `Literal(row[col])`，执行子查询，求值 filter，保留 `TRUE` 行；
     3. 投影中的相关子查询：每行求值整个投影表达式，结果写入隐藏列 `__corrN`，投影项替换成对该列的引用；
     4. 只清空已逐行求值的相关 filter，用 `execute_select` 做投影/排序。
   - 投影相关子查询要求没有 `GROUP BY`、聚合、`HAVING`；`ORDER BY` 中的相关子查询仍报 `Unsupported`。
   - 外层列必须用外层表名或别名限定；未限定列按内层列解析。

4. **语义**
   - `IN`：0 行 → 恒 `FALSE`（含 `NOT IN` → `TRUE`）；`NULL` 走既有三值 `IN` 规则。
   - `EXISTS`：子查询有行 → `TRUE`，否则 `FALSE`；`NOT EXISTS` 由 `Not` 包裹。
   - `ANY` / `ALL`：逐值比较后按三值 `OR` / `AND` 合并；空集 `ANY` → `FALSE`、`ALL` → `TRUE`。
   - 标量：0 行 → `NULL`；多行 → `Unsupported`；多列 → `Unsupported`。
   - 相关子查询支持 `WHERE`、投影、`HAVING` 与 `ORDER BY`；外层列必须限定。
   - `HAVING` 中的相关标量子查询按 `GROUP BY` 键生成 `CASE WHEN group_key = value THEN <subquery value> ... ELSE NULL END`，要求 `GROUP BY` 非空；非标量相关子查询仍报 `Unsupported`。
   - `ORDER BY` 中的相关子查询逐行求值成隐藏列 `__ordN`，排序键替换为对该列的引用。

## 理由

- **物化**是最小改动：不需要给 binder/executor 加执行上下文，也不需要改 `eval` 的纯函数签名。
- **复用 `InList`**：三值 `IN` 语义、类型 `coerce`、计划/裁剪全部自动生效。
- **上限明确**：`IN` 列表可能很大，但当前数据规模与 `MAX_RECURSIVE_ROWS` 同量级，暂不做哈希。

## 后果 / 限制

- **相关子查询只在 `WHERE`、投影、`HAVING` 与 `ORDER BY`**：带 `GROUP BY`/聚合的相关投影、`HAVING` 中的非标量相关子查询报 `Unsupported`；外层列必须限定。
- **逐行执行**：相关子查询是 nested-loop 求值（外层行数 × 子查询成本），不是为大数据集设计的。
- **物化内存**：`IN` 子查询把结果全部转成字面量，超大结果集会占用内存。
- **多列 `IN`**：已支持；左侧元组必须和子查询列数一致，右侧不支持值列表形式。
- 子查询的 `NULL` 语义依赖既有 `IN` 三值实现，`NOT IN` 遇到 `NULL` 会得到 `UNKNOWN` 而不是 `TRUE`。

## 验收

```bash
cargo test -p tritdb --test sql_cli subquer
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

- `IN` / `NOT IN` 过滤、标量投影、标量零行 `NULL`；
- `WITH c AS (...) UPDATE ... WHERE id IN (SELECT id FROM c)` 与 `DELETE`；
- 标量多行、`IN` 多列报错；
- 相关 `IN` / 相关标量（`WHERE`）、非相关路径回归、投影相关子查询与普通 filter 组合、聚合场景被拒绝；
- 多列 `(a, b) IN` / `NOT IN` / 相关多列 `IN`、`HAVING` 标量相关子查询；
- `EXISTS` / `NOT EXISTS` / `> ANY` / `> ALL` / `<> ALL`、空集 `ANY` → 空、空集 `ALL` → 全保留。
