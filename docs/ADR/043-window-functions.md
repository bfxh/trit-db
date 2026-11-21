# ADR 043：窗口函数（`ROW_NUMBER` / `RANK` / `DENSE_RANK`）

状态：**Proposed**

日期：2026-11-21

## 背景

SQL 生态需要按分区排序的序号能力；当前引擎只有聚合与排序，没有窗口函数。最小可用的一组是 `ROW_NUMBER`、`RANK`、`DENSE_RANK`，它们不依赖聚合值，只需要分区与排序键。

## 决策

1. **语法**
   ```sql
   ROW_NUMBER() OVER (PARTITION BY dept ORDER BY id)
   RANK()       OVER (ORDER BY score)
   DENSE_RANK() OVER (PARTITION BY dept ORDER BY score DESC)
   SUM(score)   OVER (PARTITION BY dept)
   AVG(score)   OVER (PARTITION BY dept ORDER BY id)
   COUNT(*)     OVER (PARTITION BY dept)
   LAG(score)   OVER (PARTITION BY dept ORDER BY id)
   LEAD(score)  OVER (PARTITION BY dept ORDER BY id)
   ```
   - `OVER` 与 `PARTITION` 是普通词（不是保留字）；`BY` / `ORDER` 复用现有关键字。
   - 窗口函数只能作为顶层 select item（`SELECT ... FROM ...`），不能嵌在复合表达式里。

2. **执行**
   - `select` 检测 select list 中的 `Expr::Window`，走 `select_with_window`：
     1. `build_from` 得到过滤后的外层表；
     2. 对每个窗口 item 按 `PARTITION BY` 分组、按 `ORDER BY` 排序，逐行计算序号；
     3. 序号写入隐藏列 `__winN`，item 替换为对该列的引用；
     4. 复用 `execute_select` 做最终投影/排序。
   - 分区键与排序键用 `bind_expr` + `eval` 求值；排序复用与 `ORDER BY` 一致的 NULL 规则（NULL 升序在前）和方向。
   - `ROW_NUMBER`：排序位置 + 1；`RANK`：并列同号、下一个跳号；`DENSE_RANK`：并列同号、不跳号。
   - 聚合窗口（`SUM/AVG/COUNT/MIN/MAX`）：无 `ORDER BY` 时对整分区聚合；有 `ORDER BY` 时为累计帧（当前行之前含当前行），复用 `eval_aggregate`。
   - `LAG` / `LEAD`：取排序后前一行 / 后一行的 `input` 值；越界返回 `NULL`。

3. **限制**
   - 窗口函数不能与 `GROUP BY`、聚合、相关子查询同时出现。
   - 窗口函数必须是顶层 select item；复合表达式中的窗口函数报 `Unsupported`。
   - 聚合窗口只支持 `SUM/AVG/COUNT/MIN/MAX`；`LAG/LEAD` 只支持默认偏移 1；`NTILE`、命名窗口、显式窗口帧未做。

## 理由

- **隐藏列**复用现有投影/排序路径，不需要改 `execute_select` 的求值模型。
- **逐行求值**与投影相关子查询的机制一致，实现简单。
- **NULL/方向规则**直接复用 `ORDER BY` 的比较，语义一致。

## 后果

- 窗口值不参与 `WHERE`/`HAVING`；`ORDER BY` 可以引用窗口别名。
- 不支持窗口帧（`ROWS BETWEEN ...`）与聚合窗口。
- 分区键比较是 `Vec<Value>` 线性查找，分组数大时退化为 O(n·g)；当前规模可接受。

## 验收

```bash
cargo test -p tritdb --test sql_cli window_functions
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

- `ROW_NUMBER` 分区升序/降序、无分区升序；
- `RANK` / `DENSE_RANK` 并列（1,1,3 / 1,1,2）；
- `SUM/COUNT` 整分区、`SUM` 累计帧、`AVG/MIN/MAX` 整分区；
- `LAG` / `LEAD` 相邻行与越界 `NULL`；
- 窗口 + `GROUP BY` 被拒绝。
