# ADR-023：把 HAVING 谓词变成结构化数据

状态：Accepted  
日期：2026-11-20

## 背景

ADR-021 把 `Filter` 的 `WHERE` 谓词转成 `PlanExpr`，ADR-022 又把 join 自身的完整谓词转成同一形状。`Having` 仍只有 `Metrics`：

```lisp
(having :est-groups 1
  (aggregate
    (scan "t")))
```

`EXPLAIN` 文本和 `EXPLAIN SEXPR` 都能看到 `Having` 标签，但看不到「按什么条件过滤组」。`HAVING` 是分组查询里唯一还不可读的谓词节点，补齐它之后，当前计划里的 `Filter`、join、`Having` 三类谓词都成为 `PlanExpr` 数据。

## 决策

1. `PlanKind::Having` 从 `Having(Metrics)` 改为：

```rust
Having {
    pred: PlanExpr,
    metrics: Metrics,
}
```

`Having` 节点只在 `bound.having.is_some()` 时构造，所以 `pred` 不是 `Option`。

2. `PlanExpr` 新增 `Aggregate { kind: AggregateKind, input: Option<Box<PlanExpr>> }`，用于 `HAVING` 中的聚合调用。
   - `COUNT(*)` 输出 `(count :all)`；
   - `COUNT(expr)` / `SUM(expr)` / `MIN(expr)` / `MAX(expr)` 输出 `(count expr)` / `(sum expr)` / `(min expr)` / `(max expr)`；
   - `input` 为 `None` 只用于 `COUNT(*)`；其他聚合必须带输入。
3. 新增 `PlanExpr::from_group_expr`，按 `GroupExpr` 递归转换：
   - `Scalar(ExecExpr)` 走既有 `PlanExpr::from_exec`，列索引按分组输入表的 schema 还原为列名；
   - `Aggregate { kind, input }` 的 `input` 是标量 `ExecExpr`，同样按该 schema 还原；
   - `IsNull` / `IsTruth` / `Not` / `InList` / `Between` / `Like` / `Case` / `Binary` 递归转换，形状与 `Filter` 的 `PlanExpr` 一致。
4. `EXPLAIN SEXPR` 的 `having` 节点输出 `:pred`，位置在 `:est-groups` / `:actual-groups` 之前、子节点之前：

```lisp
(having :pred (and (> (count :all) 1)
                   (is-not-null (col "t.k")))
  (aggregate
    (scan "t")))
```

5. `to_text()` 继续只输出 `Having` 和分组指标，不追加谓词文本；`EXPLAIN` / `EXPLAIN ANALYZE` 的文本逐字不变。
6. `EXPLAIN ANALYZE SEXPR` 复用同一棵 `PlanNode`，因此 `having` 节点同样带 `:pred`，指标改为 `:actual-groups` / `:time-ms`。
7. `PlanExpr` 仍是 crate 内类型，不扩大公共 API；`AggregateKind` 只作为计划数据的标签，不引入新的执行路径。

## 不做什么

- 不结构化 `GROUP BY` 的分组键、聚合列表或聚合输入之外的排序信息。
- 不把 `Aggregate` 变成可执行的通用表达式，也不把 `PlanExpr` 回喂 `eval_group_expr`。
- 不改变 `HAVING` 的三值过滤语义、聚合求值顺序或短路行为。
- 不把 `HAVING` 的 `:pred` 渲染成 SQL 文本，也不改变 `EXPLAIN` 文本格式。
- 不做 `HAVING` 谓词下推、分组键裁剪或规则 pass。

## 影响

- 当前计划的三类谓词节点现在都可以按列名、字面量、运算符和聚合调用读取。
- `PlanExpr` 出现第一个只可能来自 `GroupExpr` 的变体；`from_exec` 不会构造 `Aggregate`，`from_group_expr` 不会构造没有聚合的 `Filter` 变体。两者共享写序列化。
- `HAVING` 的列索引按分组输入表的 schema 还原，和 `Filter` / join 的列名保持同一命名空间。
- 文本计划、结果行集、执行路径和错误语义不变；既有测试只扩展断言，不新增测试计数。

## 验证

- `EXPLAIN SEXPR SELECT a.v, COUNT(*) FROM a GROUP BY a.v HAVING COUNT(*) > 1 AND a.v IS NOT NULL` 输出
  `(having :pred (and (> (count :all) 1) (is-not-null (col "a.v"))) ...)`。
- `EXPLAIN SEXPR SELECT k, SUM(v) FROM t GROUP BY k HAVING COUNT(*) > 1 AND SUM(v) BETWEEN 2 AND 5` 输出
  `(having :pred (and (> (count :all) 1) (between (sum (col "t.v")) 2 5)) ...)`。
- `EXPLAIN` / `EXPLAIN ANALYZE` 文本逐字断言保持通过。
- `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、SQLite 对拍全通过。
