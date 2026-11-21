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

2. **执行前物化（不做相关子查询）**
   - `execute_statement` / `execute_query` 在执行前遍历表达式，把：
     - `InSubquery { expr, query, negated }` → `InList { expr, values: [Literal(v), ...], negated }`；
     - `ScalarSubquery { query }` → `Literal(value)`。
   - binder / executor / 常量折叠 / 列裁剪只看到 `InList` 与 `Literal`，不需要知道子查询存在。
   - `execute_query` 统一做物化，覆盖 `WITH` 的 seed/step、`INSERT ... SELECT`、`IN (SELECT ...)` 嵌套。

3. **语义**
   - `IN`：0 行 → 恒 `FALSE`（含 `NOT IN` → `TRUE`）；`NULL` 走既有三值 `IN` 规则。
   - 标量：0 行 → `NULL`；多行 → `Unsupported`；多列 → `Unsupported`。
   - 相关子查询不支持：子查询里引用外层列会解析失败并报 `UnknownColumn`。

## 理由

- **物化**是最小改动：不需要给 binder/executor 加执行上下文，也不需要改 `eval` 的纯函数签名。
- **复用 `InList`**：三值 `IN` 语义、类型 `coerce`、计划/裁剪全部自动生效。
- **上限明确**：`IN` 列表可能很大，但当前数据规模与 `MAX_RECURSIVE_ROWS` 同量级，暂不做哈希。

## 后果 / 限制

- **非相关**：相关子查询（引用外层列）不支持。
- **物化内存**：`IN` 子查询把结果全部转成字面量，超大结果集会占用内存。
- **`EXISTS` / `ANY` / `ALL`**：未实现。
- **多列 `IN`**：不支持（`(a, b) IN (SELECT ...)`）。
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
- 标量多行、`IN` 多列报错。
