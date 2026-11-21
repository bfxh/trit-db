# M6 第六刀：窗口函数

状态：`ROW_NUMBER` / `RANK` / `DENSE_RANK` 与 `SUM/AVG/COUNT/MIN/MAX` 聚合窗口（含累计帧）的解析、分区排序求值、CLI/测试已实现；`LAG/LEAD`、窗口帧未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-sql/src/ast.rs      WindowFunction / WindowFunction::Aggregate / Expr::Window
crates/tritdb-sql/src/parser.rs   OVER (PARTITION BY ... ORDER BY ...)，聚合后接 OVER
crates/tritdb-sql/src/engine.rs   select_with_window / window_values / compare_window_keys
crates/tritdb/tests/sql_cli.rs    2 个窗口函数端到端测试
docs/ADR/043-window-functions.md
```

## 2. 用法

```sql
SELECT id, ROW_NUMBER() OVER (PARTITION BY dept ORDER BY id) AS rn
FROM t ORDER BY id;

SELECT id, RANK() OVER (ORDER BY score) AS r,
       DENSE_RANK() OVER (ORDER BY score) AS d
FROM s ORDER BY id;

SELECT id, SUM(score) OVER (PARTITION BY dept) AS total FROM t ORDER BY id;
SELECT id, SUM(score) OVER (PARTITION BY dept ORDER BY id) AS running FROM t ORDER BY id;
SELECT id, AVG(score) OVER (PARTITION BY dept) AS a,
       MIN(score) OVER (PARTITION BY dept) AS mn,
       MAX(score) OVER (PARTITION BY dept) AS mx FROM t ORDER BY id;
```

## 3. 语义

- `OVER` / `PARTITION` 是普通词；`BY` / `ORDER` 复用现有关键字。
- 窗口函数只能作为顶层 select item；复合表达式中的窗口函数报 `Unsupported`。
- 逐行计算：按 `PARTITION BY` 分组、按 `ORDER BY` 排序（NULL 升序在前，方向与 `ORDER BY` 一致），写入隐藏列 `__winN`，item 替换为列引用。
- `ROW_NUMBER`：位置 + 1；`RANK`：并列同号、跳号；`DENSE_RANK`：并列同号、不跳号。
- 聚合窗口（`SUM/AVG/COUNT/MIN/MAX`）：无 `ORDER BY` 对整分区聚合；有 `ORDER BY` 为累计帧（当前行之前含当前行），复用 `eval_aggregate`。
- 窗口函数不能与 `GROUP BY`、聚合、相关子查询同时出现。

## 4. 验收

```bash
cargo test -p tritdb --test sql_cli window_functions   # 2 passed
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

覆盖：`ROW_NUMBER` 分区升序（1,2 / 1,2,3）、分区降序、无分区升序；`RANK` / `DENSE_RANK` 并列（1,1,3 / 1,1,2）；`SUM/COUNT` 整分区、`SUM` 累计帧、`AVG/MIN/MAX` 整分区；窗口 + `GROUP BY` 被拒绝。

## 5. 还没做

- **`LAG` / `LEAD` / `NTILE` / 命名窗口**；
- **窗口帧**：`ROWS BETWEEN ...`；
- 窗口值不能用于 `WHERE` / `HAVING`（只能投影与 `ORDER BY` 别名）。
