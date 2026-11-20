# ADR-027：把 hash join 的残余谓词变成结构化数据

状态：Accepted

日期：2026-11-20

> 后续：ADR-030 把 `:residual-pred` 的计算与历史 `:residual` 标志解耦；同源列等值等非键合取项现在也能输出，但文本 `residual=yes` 保持不变。

## 背景

ADR-022 已经让 `JoinPlan.pred` 携带完整的 `ON` / `USING` / `NATURAL` 谓词，但 hash join 仍只有 `residual: bool`：

```lisp
(join hash inner :keys (("a.id" "b.id")) :residual #t
  :pred (and (= (col "a.id") (col "b.id")) (<> (col "b.code") 0)))
```

消费者能读到完整谓词，却不知道哪一部分已经被 `:keys` 消费、哪一部分还要在桶内逐行判断。要恢复这个信息，只能重新实现 `equi_join_pairs` 的拆分规则；这会让计划数据本身不可直接比较。

## 决策

1. `JoinPlan` 新增 `residual_pred: Option<PlanExpr>`，只承载 hash join 中**未被等值键覆盖的顶层合取项**。
   - `:pred` 仍是完整 join 谓词，含义不变；
   - `:keys` 仍是执行器实际使用的桶键；
   - `:residual-pred` 只在 `:residual #t` 时输出。
2. 拆分从绑定后的 `ExecExpr` 开始，只沿顶层 `AND` 递归：
   - 若某个 `Eq` 是左右两侧列的等值且该列对已在 `hash_pairs` 中，该合取项被移除；
   - 其余合取项原样保留，包括非等值比较、`OR` 子树、同源列比较和常量；
   - 保留项重新用 `AND` 连接，尽量维持原有结合形状；整棵树都被键覆盖时为空，不输出字段。
3. `EXPLAIN SEXPR` 的字段顺序固定为 `:keys`、`:residual`、`:residual-pred`、`:pred`，再输出估算或 actual 指标：

```lisp
(join hash inner :keys (("a.id" "b.id")) :residual #t
  :residual-pred (<> (col "b.code") 0)
  :pred (and (= (col "a.id") (col "b.id")) (<> (col "b.code") 0))
  child
  child)
```

4. `USING` / `NATURAL` 的谓词全部由共享列等值对生成，不产生残余谓词；没有键的 nested-loop `ON` join 继续只有 `:on-expr #t` 和完整 `:pred`，不输出 `:residual-pred`。
5. `EXPLAIN` / `EXPLAIN ANALYZE` 的历史文本继续只由 `keys` / `residual` / `on_expr` 生成，新增字段不参与文本渲染；文本输出逐字不变。
6. `residual` 的判定沿用现有 `is_pure_equi_conjunction`，本切片不重新定义历史标志。因此 `:residual-pred` 只在既有 `:residual #t` 时出现，保持文本与结构化标志的兼容。

## 不做什么

- 不改变 join 执行路径、hash 桶键、桶内谓词判断、join 算法选择或统计估算；`residual_pred` 只是同一棵绑定谓词的观察视图。
- 不把 `:pred` 改写成 `(and :keys :residual-pred)` 的新树；完整谓词与拆分视图并列存在。
- 不为 `USING` / `NATURAL` 合成残余谓词，也不把共享列名还原成显式列对。
- 不修复 `is_pure_equi_conjunction` 对同源列等值的宽松判定；该边界会同时影响历史文本，留到单独兼容性决策。
- 不引入规则注册表、规则 pass、join 重排或表达式规范化。

## 影响

- 规则和脚本可以直接读取残余条件，不必再从 `:pred` 和 `:keys` 反推。
- `JoinPlan` 多一个可选字段，`PartialEq` 与调试输出随之扩展；计划结构仍为 crate 内可见。
- 结构化格式新增字段；按旧字段解析的消费者可以忽略它，文本消费者完全不受影响。
- 由于拆分只在既有 `residual` 为真时执行，同源列等值这类历史边界不会在本切片中改变文本或 `:residual`。

## 验证

- `EXPLAIN SEXPR SELECT a.id FROM a JOIN b ON a.id = b.id AND b.code <> 0` 输出
  `:residual #t :residual-pred (<> (col "b.code") 0)`，同时保留完整 `:pred`。
- 复合键 `ON a.id = b.id AND a.code = b.code AND b.code <> 0` 输出两个 `:keys` 对，并只把 `(<> (col "b.code") 0)` 放进 `:residual-pred`。
- `ON a.id = b.id AND (b.code <> 0 OR a.code <> 1)` 的 `:residual-pred` 保留整个 `OR` 子树。
- 纯等值 `ON a.id = b.id AND a.code = b.code`、`USING` 和 nested-loop `ON` 都不输出 `:residual-pred`。
- `EXPLAIN ANALYZE SEXPR` 的 hash join 同样带 `:residual-pred`，并继续输出 actual 指标。
- `EXPLAIN` / `EXPLAIN ANALYZE` 文本逐字断言保持通过；fmt、Clippy、全量测试与 SQLite 对拍通过。
