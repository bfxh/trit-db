# ADR-020：EXPLAIN ANALYZE SEXPR 输出执行汇总

状态：已实现（Accepted）；只新增 `EXPLAIN ANALYZE SEXPR`，`EXPLAIN` / `EXPLAIN ANALYZE` 文本与 `EXPLAIN SEXPR` 静态输出不变
日期：2026-11-20

## 背景

ADR-017 引入 `EXPLAIN SEXPR` 时明确不支持 `EXPLAIN ANALYZE SEXPR`，理由是 S-expression 只做静态计划。ADR-019 又把这个接口的 join 字段结构化，`PlanNode` 已经能携带静态估算，也能携带执行期 `actual` 指标；但机器消费者仍无法从同一种计划格式读取实际行数和耗时，只能回到文本计划里解析 `actual rows` 和末行 `Execution: rows=N time=Xms`。

真正需要“计划即数据”的场景不只是静态计划：规则验证、回归对比和基准观察都需要把“这次执行发生了什么”与计划树放在同一个值里。继续拒绝 `ANALYZE SEXPR` 会让 S-expression 只覆盖一半计划数据。

## 决策

1. 新增 `EXPLAIN ANALYZE SEXPR SELECT ...`，复用 `EXPLAIN ANALYZE` 的执行路径、`ExecutionStats` 和同一棵 `PlanNode` 树；只改变输出格式，不改变执行、扫描、join 选择、统计或错误语义。
2. 顶层增加执行汇总：
   ```lisp
   (execution :rows 1 :time-ms 0.149
     (limit :limit 1 :actual-rows 1
       ...))
   ```
   `:rows` 是最终输出行数，`:time-ms` 是从构建 FROM 到完成投影/排序/截断的总墙钟时间，与文本格式末行 `Execution: rows=N time=Xms` 同源。`execution` 是信封，不是新的计划节点。
3. 节点只输出实际字段：`Scan` 为 `:actual-rows`；`Join` / `Filter` / `Project` / `Sort` 为 `:actual-rows` 和 `:time-ms`；`Aggregate` / `Having` 为 `:actual-groups` 和 `:time-ms`；`Limit` 只有 `:actual-rows`。`Scan` 不单独计时，与文本 `EXPLAIN ANALYZE` 一致。
4. `EXPLAIN ANALYZE SEXPR` 不输出 `:est-rows` / `:est-groups`。估算和实际是两类数据，混在同一棵树会让消费者无法判断字段来源；静态 `EXPLAIN SEXPR` 继续只输出估算。
5. join 的 `:keys` / `:residual` / `:on-expr` 保持 ADR-019 的形状；执行时实际选中的 join 算法仍体现在节点名 `hash` / `nested-loop`。
6. `EXPLAIN ANALYZE SEXPR` 返回一列 `plan`、一行文本，与 `EXPLAIN SEXPR` 的返回形状一致；文本内部保留换行和缩进。

## 后果

- 正结果：静态计划和实际执行可以用同一种括号化格式读取，脚本不再需要解析 `actual rows=` 或 `Execution:` 行。
- 正结果：`execution` 信封明确区分“整次执行”与“计划节点”，不会把总行数误当成根节点输出。
- 正结果：文本 `EXPLAIN` / `EXPLAIN ANALYZE` 逐字不变，既有整段断言继续有效；静态 `EXPLAIN SEXPR` 也不变。
- 负结果：`EXPLAIN ANALYZE SEXPR` 会真实执行查询，因此与 `EXPLAIN SEXPR` 的只读语义不同；使用者必须知道它会触发扫描和计算。
- 负结果：节点耗时仍是单次墙钟测量，受负载影响；没有多次采样、置信区间或 per-operator CPU 时间。
- 负结果：`execution` 是输出信封，不是可回喂执行器的计划节点；规则引擎不能把它当作 `PlanNode` 的根。
- 负结果：`Scan` 和 `Limit` 没有 `:time-ms`，总耗时不能由节点耗时相加得到。

## 被否掉的替代

- **继续拒绝 `ANALYZE SEXPR`**：静态计划已经结构化，实际指标却只能回到文本，计划数据的两种视图不对称。
- **把 `:actual-*` 直接追加到静态 `EXPLAIN SEXPR` 的根节点**：没有执行就没有 actual；把总行数塞进根节点会与根算子的 `:actual-rows` 混淆。
- **在 `PlanNode` 上增加 `execution` 变体**：会让执行信封进入计划树，规则遍历必须特判它，违反“节点只描述算子”的边界。
- **同时输出估算和实际字段**：消费者无法区分来源，也违背 `EXPLAIN ANALYZE` 文本“有 ANALYZE 统计时也不混入 est”的既有口径。
- **输出 JSON 而不是 S-expression**：当前仓已有 `PlanNode` 的括号化序列化，新增 JSON 会引入第二套格式和转义规则；JSON 工具协议仍不属于这个切片。

## 参考

- [ADR-017：结构化计划树与只读 S-expression](017-structured-plan-ir.md)
- [ADR-018：结构化 join 键与残余谓词](018-structured-join-keys.md)
- [ADR-019：在 EXPLAIN SEXPR 中暴露结构化 join 字段](019-structured-join-sexpr.md)
- PostgreSQL. *EXPLAIN* 文档中 `ANALYZE` 与 `FORMAT` 的组合语义. <https://www.postgresql.org/docs/current/sql-explain.html>
