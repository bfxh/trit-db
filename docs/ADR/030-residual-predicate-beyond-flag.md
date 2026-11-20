# ADR-030：让 hash join 的非键合取项不再依赖历史 residual 标志

状态：Accepted

日期：2026-11-20

## 背景

ADR-027 把 hash join 的残余谓词变成 `:residual-pred`，但只在既有 `residual` 为真时计算。`residual` 的判定来自 `is_pure_equi_conjunction`，它把任何“两列相等”都视为纯等值，包括同一源表内的两列：

```sql
ON a.id = b.id AND a.id = a.code
```

这条 join 的 hash 键只有 `("a.id" "b.id")`，`a.id = a.code` 是未被键消费的合取项；执行器仍会在桶内应用完整谓词。但 `is_pure_equi_conjunction` 看到 `a.id = a.code` 也是列-列相等，判定整棵树为纯等值，`residual` 为假，于是 `:residual-pred` 被跳过。结构化消费者只能从 `:pred` 自己再拆一遍，才能恢复这个非键条件。

ADR-027 明确把这个边界留给单独的兼容性决策：直接收紧 `is_pure_equi_conjunction` 会改变历史文本的 `residual=yes` 标记。本 ADR 只解决结构化数据的缺口，不改文本。

## 决策

1. `residual_pred` 的计算从“`residual == true` 时”改为“该 hash join 存在绑定谓词时”：
   - 仍只沿顶层 `AND` 拆分；
   - 仍移除已由 `hash_pairs` 消费的左右列等值；
   - 其余合取项，包括同源列等值，继续进入 `:residual-pred`。
2. `JoinPlan.residual` 保持原义：历史文本的 `residual=yes` 标志，由 `is_pure_equi_conjunction` 决定，不因本切片改变。
3. `EXPLAIN SEXPR` 的 `:residual-pred` 因此可能在没有 `:residual #t` 时出现：

```lisp
(join hash inner :keys (("a.id" "b.id"))
  :residual-pred (= (col "a.id") (col "a.code"))
  :pred (and (= (col "a.id") (col "b.id")) (= (col "a.id") (col "a.code")))
  ...)
```

   - `:residual #t` 仍只在历史标志为真时输出；
   - `:residual-pred` 表示“`residual_join_predicate` 实际留下的非键合取项”，不表示历史标志；
   - `:pred` 仍是完整 join 谓词。
4. 纯等值且全部键都已命中、`USING`、`NATURAL`、以及 nested-loop `ON` join 都仍不输出 `:residual-pred`；`residual_join_predicate` 对它们返回 `None`。
5. 文本 `EXPLAIN` / `EXPLAIN ANALYZE` 的 join 片段继续只由 `keys` / `residual` / `on_expr` 生成，不读取 `residual_pred`；历史文本逐字不变。
6. 执行路径不变：hash 桶键、桶内完整谓词判断、join 算法选择和统计估算都不受本切片影响。

## 不做什么

- 不收紧 `is_pure_equi_conjunction`，不把同源列等值改判为 residual，也不改变文本中的 `residual=yes`。
- 不把 `:residual` 字段删除或改名；它继续是历史文本标志。
- 不把 `:pred` 重写成 `(and :keys :residual-pred)`；完整谓词和拆分视图并列存在。
- 不为 `USING` / `NATURAL` 合成同源列谓词，也不改变共享列键的表示。
- 不引入规则注册表、规则 pass、join 重排或表达式规范化。

## 影响

- 结构化消费者现在能直接看到 hash join 中所有未被键消费的顶层合取项，包括历史分类器漏掉的同源列等值。
- `:residual-pred` 与 `:residual` 不再是一一对应关系：前者是谓词拆分结果，后者是文本兼容标志。读计划格式的脚本需要按字段语义而非字段共现来理解。
- 既有纯等值、`USING`、`NATURAL`、nested-loop 和常规 residual 查询的输出不变；文本计划、结果行集、错误语义不变。
- `JoinPlan` 的字段不变，只是 `residual_pred` 的填充条件更宽；`PartialEq` 对同源列等值 join 的调试表示会多出该字段。

## 验证

- `EXPLAIN SEXPR SELECT a.id FROM a JOIN b ON a.id = b.id AND a.id = a.code` 输出
  `:keys (("a.id" "b.id")) :residual-pred (= (col "a.id") (col "a.code")) :pred (and ...)`，且不输出 `:residual #t`。
- 同一查询的文本 `EXPLAIN` 仍包含 `HashJoin (inner) keys=(a.id = b.id)`，不包含 `residual=yes`。
- 既有 `ON a.id = b.id AND b.code <> 0`、复合键、`OR` 子树仍输出 `:residual #t` 与对应 `:residual-pred`。
- 纯等值 `ON a.id = b.id AND a.code = b.code`、`USING`、`NATURAL` 和 nested-loop 仍不输出 `:residual-pred`。
- fmt、Clippy、全量测试与 SQLite 对拍通过。
