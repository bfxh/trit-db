# ADR-026：把 Project 的输出项与表达式变成结构化数据

状态：Accepted  
日期：2026-11-20

## 背景

ADR-025 之后，计划树里只剩下 `Project` 仍把计算过程藏在列名后面：

```lisp
(project ("v" "total")
  child)
```

`SELECT a.v`、`SELECT a.v + 1 AS v`、`SELECT SUM(a.id) AS total`、`SELECT CASE ... END AS c` 在计划里都可能只是「输出列 `v` / `total`」，看不到这一列由哪个绑定表达式算出。执行器和 binder 已经持有 `BoundItem { name, expr }`，但构建 `PlanNode` 时只保留了 `name`。

## 决策

1. `PlanKind::Project` 从 `Project { columns: Vec<String>, metrics }` 改为 `Project(ProjectPlan)`：

```rust
pub(crate) struct ProjectPlan {
    pub(crate) items: Vec<ProjectItem>,
    pub(crate) metrics: Metrics,
}

pub(crate) struct ProjectItem {
    pub(crate) name: String,
    pub(crate) expr: PlanExpr,
}
```

2. `items` 按 `BoundSelect.items` 的顺序构造，每项同时保留调用方可见的输出名和绑定表达式。`name` 是别名或默认标签；`expr` 走 `PlanExpr::from_group_expr`，列索引按投影输入表的 schema 还原。这样同一套转换支持：
   - 源列与常量；
   - 算术、`CASE`、谓词类标量表达式；
   - `COUNT` / `SUM` / `MIN` / `MAX` 等聚合调用；
   - 分组键上的标量表达式。
3. `EXPLAIN SEXPR` 的 `project` 节点把原来的纯列名列表换成 `("name" <PlanExpr>)` 对：

```lisp
(project (("v" (col "a.v"))
          ("total" (sum (col "a.id"))))
  child)
```

   列表顺序与输出列顺序一致；没有输出项时输出 `()`。
4. `to_text()` 继续输出历史格式 `Project [v, total]`，不追加表达式；`EXPLAIN` / `EXPLAIN ANALYZE` 文本逐字不变。
5. `EXPLAIN ANALYZE SEXPR` 复用同一棵 `PlanNode`，因此 `project` 节点同样带结构化项列表，指标改为 `:actual-rows` / `:time-ms`。
6. `name` 与 `expr` 在同一个 `ProjectItem` 内绑定，不再用两个等长数组维持隐式对应关系；文本标签和 S-expression 都从同一组 items 派生。

## 不做什么

- 不推导输出列的类型、排序规则或可空性；`PlanExpr` 仍只是表达式形状。
- 不做投影表达式重写、公共子表达式消除、常量传播之外的新优化，也不把 `PlanExpr` 回喂执行器。
- 不把表达式渲染成 SQL 文本，也不增加别名解析表或输出 schema 节点。
- 不改变投影顺序、列标签、结果行集、错误语义或文本计划格式。
- 不改变 `Sort -> Project` 的历史节点顺序；投影表达式按执行器输入行/组求值，不要求与子节点输出列一致。

## 影响

- 计划消费者现在能读出每个输出列的名字和计算表达式，`SELECT a.v` 与 `SELECT a.v + 1 AS v` 不再在计划里同形。
- `project` 的 S-expression 从字符串列表变成 `(name expr)` 列表，是格式变更；当前只有仓库内测试和文档消费该格式，没有外部下游。
- `ProjectPlan` / `ProjectItem` 仍是 crate 内类型，不扩大公共 API。
- 文本计划、结果行集、执行路径、统计和错误语义不变；既有测试扩展断言，不新增测试计数。

## 验证

- `EXPLAIN SEXPR SELECT a.v FROM a` 输出 `(project (("v" (col "a.v"))) ...)`。
- `EXPLAIN SEXPR SELECT 1 AS one FROM a` 输出 `(project (("one" 1)) ...)`。
- `EXPLAIN SEXPR SELECT a.v, SUM(a.id) AS total FROM a GROUP BY a.v` 输出
  `(project (("v" (col "a.v")) ("total" (sum (col "a.id")))) ...)`。
- `EXPLAIN ANALYZE SEXPR` 的 `project` 节点同时带结构化项列表与 `:actual-rows` / `:time-ms`。
- `EXPLAIN` / `EXPLAIN ANALYZE` 文本逐字断言保持通过。
- `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、SQLite 对拍全通过。
