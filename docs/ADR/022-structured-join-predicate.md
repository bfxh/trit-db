# ADR-022：把 join 谓词变成结构化数据

状态：Accepted  
日期：2026-11-20

> 后续：ADR-027 在不改变 `:pred` 含义的前提下，新增 `:residual-pred` 承载等值键之外的顶层合取项。

## 背景

ADR-018 把 `JoinPlan.details` 拆成了 `JoinKeys`、`residual`、`on_expr`，ADR-019 又把这些字段暴露到 `EXPLAIN SEXPR`。但 join 自身的谓词仍只有布尔标志：

- `:residual #t` 只说「等值键之外还有条件」；
- `:on-expr #t` 只说「nested-loop 显示了整体条件」；
- 条件具体是什么，仍只存在于 `EXPLAIN` 的历史文本里。

ADR-021 已经把 `Filter` 的绑定谓词转成 `PlanExpr`，因此 `Filter :pred` 可以按列名、运算符、字面量读取和比较。join 是查询计划里最需要被规则操作的位置之一，`residual` / `on_expr` 只有标志会让后续规则无法看到实际条件。

## 决策

1. `JoinPlan` 新增 `pred: Option<PlanExpr>`，表示该 join 的**完整绑定谓词**。
   - `ON` join：保存 `ON` 表达式绑定后的结果；
   - `USING` / `NATURAL`：保存由共享列等值对生成的合取谓词；
   - 没有谓词的 `CROSS JOIN` / 无公共列的 `NATURAL JOIN` 为 `None`。
2. 谓词按左右表拼接后的 schema 绑定：左表列索引不变，右表列索引偏移左表列数。列索引继续通过 `PlanExpr::from_exec` 还原为 schema 列名；越界仍返回 `SqlError::UnknownColumn`，不静默丢弃。
3. `EXPLAIN SEXPR` 的 join 节点在 `:keys` / `:residual` / `:on-expr` 之后输出 `:pred`，再输出估算或 actual 指标：

```lisp
(join hash inner :keys (("a.id" "b.id")) :residual #t
  :pred (and (= (col "a.id") (col "b.id")) (<> (col "b.code") 0))
  child
  child)
```

```lisp
(join hash left :keys ("id")
  :pred (= (col "a.id") (col "b.id"))
  child
  child)
```

4. `:pred` 是**完整 join 谓词**，不是只含残余项。等值键会同时出现在 `:keys` 和 `:pred` 中；两者用途不同：`:keys` 是执行器实际使用的桶键，`:pred` 是语义上完整的连接条件。后续若要单独暴露「残余谓词」，应新增字段，而不是改变 `:pred` 的含义。
5. `EXPLAIN` / `EXPLAIN ANALYZE` 的文本输出保持逐字不变；`detail_fragments()` 继续只由 `keys` / `residual` / `on_expr` 生成历史片段，不读取 `pred`。
6. `EXPLAIN ANALYZE SEXPR` 复用同一棵 `PlanNode` 树，因此 join 节点同样带 `:pred`，actual 字段仍只输出本次执行指标。
7. `JoinPlan` / `PlanExpr` 仍为 crate 内可见，不扩大公共 API。

## 不做什么

- 不把 `:pred` 拆成「等值键部分 / 残余部分」；`pred` 本身保持完整。ADR-027 另加 `:residual-pred` 作为并列的残余视图，而不是改变 `:pred` 的含义。
- 不改变 join 执行路径：`pred` 只用于计划观察，hash / nested-loop 的选择、桶键和行匹配仍走原逻辑。
- 本切片不结构化 `HAVING` 谓词，也不引入子查询或通用表达式求值；该限制已由 ADR-023 撤销。
- 不把 `:pred` 渲染成 SQL 文本，也不把 `EXPLAIN` 文本改成 `PlanExpr` 渲染；文本格式是兼容边界。
- 不引入规则注册表、规则 pass 或 join 重排。

## 影响

- `PlanNode` 的 join 节点现在同时携带 `keys`、`residual` / `on_expr` 标志和完整 `pred`，消费者可以读取实际条件而不用解析历史文本。
- `PlanExpr` 的维护面扩大：`Filter` 和 join 共用同一套形状；新增执行器表达式变体时两处都要同步。
- `USING` / `NATURAL` 的 `:pred` 是生成的等值合取，和用户写的 `ON` 谓词在语义上等价，但不保留原始书写形式；这是绑定后计划的预期。
- 文本计划、结果行集、错误语义和测试计数不变；既有 `EXPLAIN SEXPR` 断言只多出 `:pred` 字段。

## 验证

- `EXPLAIN SEXPR SELECT a.id FROM a JOIN b ON a.id = b.id AND b.code <> 0` 输出
  `:keys (("a.id" "b.id")) :residual #t :pred (and (= (col "a.id") (col "b.id")) (<> (col "b.code") 0))`。
- `EXPLAIN SEXPR SELECT * FROM a LEFT JOIN b USING (id)` 输出
  `:keys ("id") :pred (= (col "a.id") (col "b.id"))`。
- nested-loop `ON a.id < b.id` 输出
  `:on-expr #t :pred (< (col "a.id") (col "b.id"))`。
- `EXPLAIN` / `EXPLAIN ANALYZE` 文本逐字断言保持通过。
- `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、SQLite 对拍全通过。
