# ADR-025：把 Sort 的排序键与方向变成结构化数据

状态：Accepted  
日期：2026-11-20

> 后续：ADR-026 把 `Project` 的输出名与计算表达式结构化。

## 背景

ADR-024 补上 `Aggregate` 的分组键和引用聚合项后，`Sort` 成了计划树里唯一仍只暴露计数的节点：

```lisp
(sort :keys 1
  child)
```

`ORDER BY a.v`、`ORDER BY a.v DESC`、`ORDER BY SUM(a.id)`、`ORDER BY a.v, b.w` 在计划里都只是「1 个或 N 个键」，看不到排的是哪个表达式、每个键是升序还是降序。执行器已经持有 `BoundOrder { expr, ascending }`，只是构建 `PlanNode` 时把它压成了 `usize`。

## 决策

1. `PlanKind::Sort` 从 `Sort { keys: usize, metrics }` 改为 `Sort(SortPlan)`：

```rust
pub(crate) struct SortPlan {
    pub(crate) keys: Vec<SortKey>,
    pub(crate) metrics: Metrics,
}

pub(crate) struct SortKey {
    pub(crate) expr: PlanExpr,
    pub(crate) ascending: bool,
}
```

2. `keys` 按 `BoundSelect.order_by` 的顺序构造，每个 `BoundOrder.expr` 走 `PlanExpr::from_group_expr`，列索引按排序输入表的 schema 还原为列名。这样同一套转换同时支持：
   - 源列与别名；
   - 分组键；
   - `SUM` / `COUNT` / `MIN` / `MAX` 等聚合表达式；
   - 由这些表达式组成的算术和 `CASE`。
3. `EXPLAIN SEXPR` 的 `sort` 节点把原来的数字 `:keys` 换成有序键列表，每个键是 `(asc <PlanExpr>)` 或 `(desc <PlanExpr>)`：

```lisp
(sort :keys ((desc (sum (col "a.id"))) (asc (col "a.v")))
  child)
```

   `Sort` 节点只在 `order_by` 非空时构造，因此列表不会为空。
4. `to_text()` 继续输出历史格式 `Sort (N key(s))`，不追加表达式或方向；`EXPLAIN` / `EXPLAIN ANALYZE` 文本逐字不变。
5. `EXPLAIN ANALYZE SEXPR` 复用同一棵 `PlanNode`，因此 `sort` 节点同样带结构化 `:keys`，指标改为 `:actual-rows` / `:time-ms`。
6. `SortKey` 只携带表达式和升降序。`NULL` 的位置仍是执行器的固定规则（ASC 排最前，DESC 排最后），不是每个键上的可配置字段。

## 不做什么

- 不增加 `NULLS FIRST` / `NULLS LAST` 语法，也不把空值位置变成计划字段。
- 不引入 collation、locale 或按列类型的自定义比较器；比较仍走现有的 `compare_values`。
- 不暴露排序算法、内存上限、溢出写盘或稳定排序标记。
- 不做 `ORDER BY` 消除、排序下推、top-N 改写或规则 pass。
- 不改变排序结果、空值顺序、错误语义或文本计划格式。

## 影响

- 计划消费者现在能区分 `ORDER BY a.v` 与 `ORDER BY a.v DESC`，也能看到聚合排序键和复合排序键。
- 排序键的列名按执行器排序输入（投影前）的 schema 还原；计划树的 `Sort -> Project` 顺序沿用历史 `EXPLAIN` 文本，表达的是可观察节点顺序，不承诺 `Sort` 消费 `Project` 的输出列。
- `EXPLAIN SEXPR` 的 `sort :keys` 从整数变成列表，是格式变更；当前只有仓库内测试和文档消费该格式，没有外部下游。
- `SortPlan` / `SortKey` 仍是 crate 内类型，不扩大公共 API。
- 文本计划、结果行集、执行路径、统计和错误语义不变；既有测试扩展断言，不新增测试计数。

## 验证

- `EXPLAIN SEXPR SELECT a.v FROM a ORDER BY a.v` 输出 `(sort :keys ((asc (col "a.v"))) ...)`。
- `EXPLAIN SEXPR SELECT a.v FROM a ORDER BY a.v DESC` 输出 `(sort :keys ((desc (col "a.v"))) ...)`。
- `EXPLAIN SEXPR SELECT a.v, SUM(a.id) FROM a GROUP BY a.v ORDER BY SUM(a.id) DESC, a.v ASC` 输出
  `(sort :keys ((desc (sum (col "a.id"))) (asc (col "a.v"))) ...)`。
- `EXPLAIN SEXPR SELECT a.v FROM a ORDER BY a.id DESC` 输出 `(sort :keys ((desc (col "a.id"))) ...)`，即使 `a.id` 不在投影列中，也按排序输入 schema 还原。
- `EXPLAIN ANALYZE SEXPR` 的 `sort` 节点同时带结构化 `:keys` 与 `:actual-rows` / `:time-ms`。
- `EXPLAIN` / `EXPLAIN ANALYZE` 文本逐字断言保持通过。
- `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、SQLite 对拍全通过。
