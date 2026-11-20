# ADR-024：把 Aggregate 的分组键与引用聚合项变成结构化数据

状态：Accepted  
日期：2026-11-20

> 后续：ADR-025 把 `Sort` 的排序键表达式与升降序转成结构化 `:keys`。

## 背景

ADR-023 把 `Having` 的 `GroupExpr` 转成了 `PlanExpr`，但分组查询里的 `Aggregate` 节点仍只有执行指标：

```lisp
(aggregate :est-groups 2
  (scan "a"))
```

计划读者能知道这里做了聚合、产生了几个组，却看不到「按什么分组」，也看不到查询引用了哪些聚合调用。`GROUP BY a.v`、`SELECT COUNT(*)`、`SELECT a.v, SUM(a.id) ... HAVING SUM(a.id) > 2` 在计划树里长得一样。`Aggregate` 因此是当前计划里最后一个只有标签和计数的核心节点。

## 决策

1. `PlanKind::Aggregate` 从只携带 `Metrics` 改为携带 `AggregatePlan`：

```rust
pub(crate) struct AggregatePlan {
    pub(crate) keys: Vec<PlanExpr>,
    pub(crate) aggregates: Vec<PlanExpr>,
    pub(crate) metrics: Metrics,
}
```

2. `keys` 来自 `BoundSelect.group_by`。每个 `ExecExpr` 用分组输入表的 schema 走 `PlanExpr::from_exec`，把列索引还原为列名。全局聚合没有 `GROUP BY`，因此 `keys` 为空。
3. `aggregates` 是查询引用到的去重聚合调用，按 SELECT、HAVING、ORDER BY 的首次出现顺序收集：
   - 只收 `GroupExpr::Aggregate`，其输入仍按分组输入表的 schema 还原；
   - 同一个聚合表达式在 SELECT 和 HAVING 中重复出现时只保留一次，按 `PlanExpr` 的结构相等判重；
   - 它是查询级清单，不是执行器预计算出的物理聚合槽位；执行路径仍按每个出现点求值。
4. `EXPLAIN SEXPR` 的 `aggregate` 节点在指标之前输出两个字段：

```lisp
(aggregate :keys ((col "a.v"))
           :aggregates ((count :all) (sum (col "a.id")))
  (scan "a"))
```

   - 全局聚合输出 `:keys ()`；
   - 只有 `GROUP BY`、没有聚合调用时输出 `:aggregates ()`；
   - 两个字段始终出现，空列表用 `()` 表示，避免消费者区分「字段缺失」和「列表为空」。
5. `to_text()` 仍只输出 `Aggregate` 和分组指标，不显示分组键或聚合列表；`EXPLAIN` / `EXPLAIN ANALYZE` 文本逐字不变。
6. `EXPLAIN ANALYZE SEXPR` 复用同一棵 `PlanNode`，因此 `aggregate` 节点同样带 `:keys` / `:aggregates`，指标改为 `:actual-groups` / `:time-ms`。
7. `AggregatePlan` 仍是 crate 内类型；`PlanExpr` 不新增变体，`Aggregate` 继续复用 ADR-023 已定义的 `PlanExpr::Aggregate`。

## 不做什么

- 不把 `aggregates` 当作执行器已经物化的聚合槽位，也不做公共子表达式消除；重复聚合仍按现有路径分别求值。
- 不暴露 `ORDER BY` 的排序方向、空值顺序或排序表达式本身；`Sort` 节点仍只有键数（该限制已由 ADR-025 撤销）。
- 不做分组键裁剪、分组键下推、聚合下推或规则 pass。
- 不把分组键或聚合列表渲染成 SQL 文本，也不改变 `EXPLAIN` 文本格式。
- 不引入新的聚合函数或改变聚合的三值/NULL 语义。

## 影响

- 分组查询的计划现在能读出「按哪些表达式分组」和「引用了哪些聚合调用」，`COUNT(*)`、`SUM`、`MIN`、`MAX` 共用 ADR-023 的渲染形状。
- `keys` 与 `aggregates` 的列名都来自分组输入表的 schema，和 `Filter`、join、`Having` 使用同一命名空间。
- 聚合清单按结构相等去重，顺序稳定；它描述查询引用了什么，不承诺执行器只计算一次。
- 文本计划、结果行集、执行路径、统计和错误语义不变；既有测试只扩展断言，不新增测试计数。

## 验证

- `EXPLAIN SEXPR SELECT a.v, COUNT(*) FROM a GROUP BY a.v HAVING COUNT(*) > 1` 输出
  `(aggregate :keys ((col "a.v")) :aggregates ((count :all)) ...)`。
- `EXPLAIN SEXPR SELECT a.v, SUM(a.id) FROM a GROUP BY a.v HAVING SUM(a.id) > 2` 输出
  `(aggregate :keys ((col "a.v")) :aggregates ((sum (col "a.id"))) ...)`，SELECT 与 HAVING 中的同一聚合只保留一次。
- `EXPLAIN SEXPR SELECT COUNT(*) FROM a` 输出 `(aggregate :keys () :aggregates ((count :all)) ...)`。
- `EXPLAIN SEXPR SELECT a.v FROM a GROUP BY a.v` 输出 `(aggregate :keys ((col "a.v")) :aggregates () ...)`。
- `EXPLAIN` / `EXPLAIN ANALYZE` 文本逐字断言保持通过。
- `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、SQLite 对拍全通过。
