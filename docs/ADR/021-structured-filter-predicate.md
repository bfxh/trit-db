# ADR-021：把过滤谓词变成结构化 `PlanExpr`

状态：已实现（Accepted）；只改变 `EXPLAIN SEXPR` 的 `Filter` 节点，`EXPLAIN` / `EXPLAIN ANALYZE` 文本不变
日期：2026-11-20

## 背景

ADR-019 已经把 join 键从字符串拆成 `JoinKeys`，但 `Filter` 节点仍只有 `Metrics`：

```lisp
(filter :est-rows 2
  child)
```

`Filter` 的谓词在 binder 之后是 `tritdb_exec::Expr`，列引用是行内索引，`EXPLAIN` 文本只输出 `Filter` 标签。机器消费者能读到“有几行被过滤”，却读不到“按什么条件过滤”；要做规则匹配、计划对比或回归诊断，只能回到 SQL 原文或 `EXPLAIN` 文本猜测。计划树因此仍不是完整的“计划即数据”。

## 决策

1. 新增 crate 内 `PlanExpr`，镜像当前 `tritdb_exec::Expr` 的谓词形状，但把 `Column(usize)` 换成 `Column(String)`。绑定后的行索引按 `Filter` 所在节点的 schema 还原为列名；越界索引返回 `SqlError::UnknownColumn`，不静默丢弃。
2. `PlanKind::Filter` 从 `Filter(Metrics)` 改为 `Filter { pred: PlanExpr, metrics: Metrics }`。`Filter` 只在实际存在残余 `WHERE` 时构造，因此 `pred` 不是 `Option`。
3. `EXPLAIN SEXPR` 的 `Filter` 输出改为：
   ```lisp
   (filter :pred (or (<> (col "a.v") "skip") (like (col "b.w") "b%"))
     child)
   ```
   字段顺序固定为 `:pred`、actual/est 指标、子节点。谓词形状：
   - `(col "name")`
   - 字面量：整数、`"text"`、`#t` / `#f` / `null`
   - `(<op> left right)`：`and` / `or` / `=` / `<>` / `<` / `<=` / `>` / `>=` / `+` / `-` / `*` / `/`
   - `(not expr)`
   - `(is-null expr)` / `(is-not-null expr)`
   - `(is-truth expr true|false|unknown)` / `(is-not-truth ...)`
   - `(in expr (value ...))` / `(not-in ...)`
   - `(between expr low high)` / `(not-between ...)`
   - `(like expr pattern)` / `(not-like ...)`
   - `(case [:operand expr] (when cond then) ... [:else expr])`
4. `EXPLAIN` / `EXPLAIN ANALYZE` 文本继续只显示 `Filter` 和指标，不追加谓词文本；现有整段断言不变。
5. 本切片只覆盖 `Filter`。`Having`、join 的 `residual` / `on-expr` 仍只有布尔标志；它们的绑定表达式分别是 `GroupExpr` 和 join 两侧 schema，转换规则不同，留到后续切片。ADR-022 已把 join 的完整绑定谓词转成 `:pred`，ADR-023 又把 `Having` 的 `GroupExpr` 转成同一形状的 `:pred`，当前三类谓词节点都已结构化。

## 后果

- 正结果：`Filter` 节点从“过滤了几行”变成“按什么条件过滤”，脚本可以直接匹配谓词结构。
- 正结果：谓词中的列名是 schema 名（如 `a.v` / `b.w`），不再暴露行内索引；机器消费者不需要重建 join 布局。
- 正结果：谓词转换只发生在 `build_plan` 的 explain 路径，不改变执行器、绑定结果、过滤语义或错误路径。
- 负结果：`PlanExpr` 与 `tritdb_exec::Expr` 是两套形状，后续增加表达式变体时必须同步维护；当前没有自动派生或共享序列化。
- 负结果：`PlanExpr` 只保存绑定后的结构，不保存原始 SQL 的括号、别名或文本；它用于数据匹配，不是 SQL 反解析器。
- 负结果：`Filter` 的 `pred` 在 `EXPLAIN ANALYZE SEXPR` 中同样输出，因此实际执行计划会包含完整谓词；大谓词会让单行文本变长，但不会拆成多行节点。
- 负结果：`Having` 仍未结构化；聚合查询的分组过滤仍只能看到 `Having` 标签和组数。join 谓词已由 ADR-022 补上 `:pred`，`Having` 随后由 ADR-023 补上 `:pred`，但 join 的 `residual` 仍不单独拆出。

## 被否掉的替代

- **继续输出 `:pred #t` 或谓词文本字符串**：仍然不可匹配，只是把“有谓词”换成“有字符串”。
- **直接 `Debug` 打印 `tritdb_exec::Expr`**：会暴露 `Column(3)` 行索引和 Rust 调试格式，不是稳定数据协议。
- **把 `PlanExpr` 提升为公共 API**：当前只有 `EXPLAIN SEXPR` 消费，扩大公共 API 会固定一套尚未稳定的谓词形状。
- **同时结构化 `Having` / join residual**：需要分别处理 `GroupExpr` 的聚合项和 join 两侧 schema，混在一个切片里会放大验证面；先做 `Filter` 这个最常用、边界最清楚的节点。
- **把 `PlanExpr` 回喂执行器**：绑定后的 `Expr` 已经可直接执行，`PlanExpr` 只用于观察；建立双向转换会引入第二套执行语义。

## 参考

- [ADR-017：结构化计划树与只读 S-expression](017-structured-plan-ir.md)
- [ADR-018：结构化 join 键与残余谓词](018-structured-join-keys.md)
- [ADR-019：在 EXPLAIN SEXPR 中暴露结构化 join 字段](019-structured-join-sexpr.md)
- [ADR-020：EXPLAIN ANALYZE SEXPR 输出执行汇总](020-explain-analyze-sexpr.md)
- [ADR-022：把 join 谓词变成结构化数据](022-structured-join-predicate.md)
- [ADR-023：把 HAVING 谓词变成结构化数据](023-structured-having-predicate.md)
