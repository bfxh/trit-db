# ADR-019：在 EXPLAIN SEXPR 中暴露结构化 join 字段

状态：已实现（Accepted）；只改变 `EXPLAIN SEXPR` 的 join 输出，`EXPLAIN` 文本逐字不变
日期：2026-11-20

## 背景

ADR-018 把 `JoinPlan` 从渲染好的 `details: Vec<String>` 改成 `JoinKeys`、`residual` 和 `on_expr`，但 `EXPLAIN SEXPR` 仍输出：

```lisp
(join hash inner :details ("keys=(a.id = b.id)" "residual=yes"))
```

这样机器消费者仍需解析 `"keys=(...)"` 字符串，结构化只发生在 Rust 内部，没有体现到对外的计划数据。既然 `EXPLAIN SEXPR` 的目标是让计划可被读取和匹配，join 的键、残余标志和 nested-loop 谓词标志应直接成为字段。

## 决策

1. `EXPLAIN SEXPR` 的 join 节点不再输出 `:details`，改为：
   - `:keys ("id" "code")`：`USING` / `NATURAL` 的共享列名，列表元素是字符串；
   - `:keys (("a.id" "b.id") ("a.code" "b.code"))`：`ON` 的显式等值对，列表元素是二元字符串列表；
   - `:residual #t`：`ON` hash join 存在等值键之外的谓词；
   - `:on-expr #t`：nested-loop `ON` join 显示整体谓词。
2. 字段只在存在/为真时输出；缺省即表示无键、无残余或不是 `on=expr`。这与现有 `:filtered`、`:cols`、`:est-rows` 的按需字段风格一致。
3. `EXPLAIN` 文本继续由 `detail_fragments()` 生成，保持 `keys=(...)` / `residual=yes` / `on=expr` 的历史格式；`EXPLAIN SEXPR` 与文本共享同一个 `JoinPlan`，不复制规划逻辑。
4. 解析器、执行路径、join 选择、统计估算和错误语义不变；本 ADR 当时仍拒绝 `EXPLAIN ANALYZE SEXPR`，该限制已由 ADR-020 撤销；`EXPLAIN ANALYZE SEXPR` 的 join 字段继续沿用本 ADR 的 `:keys` / `:residual` / `:on-expr` 形状。ADR-022 已在该形状之后追加完整 join 谓词 `:pred`，`residual` / `on-expr` 标志继续保留。
5. `:keys` 的两种列表形状是有意的：共享列名和显式等值对在语义上不同，不能强行压成同一种形状。消费者可以用“字符串 vs 二元列表”区分。

## 后果

- 正结果：机器消费者可以直接读 `:keys`，不再解析 `"keys=(a.id = b.id)"`；`USING` / `NATURAL` 与 `ON` 的形状差异也保留下来。
- 正结果：`:residual` / `:on-expr` 从字符串片段变成布尔字段，规则和脚本可以直接分支。
- 正结果：文本计划完全不变，既有 `EXPLAIN` 整段断言继续有效。
- 负结果：`EXPLAIN SEXPR` 的 join 输出格式发生了一次有意变更；上一版按 `:details` 解析的消费者需要切换到 `:keys` / `:residual` / `:on-expr`。该格式只在此前两个提交内存在，没有外部兼容承诺。
- 负结果：`:residual #t` 仍只是“有残余谓词”的标志，不携带残余表达式；ADR-021 已把 `Filter` 的残余 `WHERE` 谓词转成 `:pred` 的 `PlanExpr`，ADR-022 又把 join 的完整 `ON` / `USING` / `NATURAL` 谓词转成 `:pred`。ADR-027 新增 `:residual-pred` 承载去掉等值键后的顶层残余项，`:residual` 标志继续保留。
- 负结果：`:keys` 的 `Names` 形态只保存共享列名，不保存右侧列对象；需要右侧 schema 的规则仍要从计划节点或绑定上下文取。

## 被否掉的替代

- **保留 `:details` 并额外加字段**：输出冗余，消费者仍可能继续解析旧字符串，结构化没有真正成为唯一接口。
- **把 `USING` / `NATURAL` 也输出成 `(("id" "id"))`**：形状统一了，但丢掉了“共享列名”和“显式等值对”的语义区别，文本侧也要额外记录显示模式。
- **只改内部字段，不改 `EXPLAIN SEXPR`**：规则能用，但外部脚本仍要解析字符串，违背 `EXPLAIN SEXPR` 作为机器可读计划的初衷。
- **同时支持 `:details` 和 `:keys` 两套格式**：需要版本协商和双份测试，当前没有外部消费者，收益不足。

## 参考

- [ADR-017：结构化计划树与只读 S-expression](017-structured-plan-ir.md)
- [ADR-018：结构化 join 键与残余谓词](018-structured-join-keys.md)
- Apache Calcite. *Algebra* 与 `RelNode` 树形计划文档. <https://calcite.apache.org/docs/algebra.html>
