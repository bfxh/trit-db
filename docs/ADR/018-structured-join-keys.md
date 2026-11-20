# ADR-018：结构化 join 键与残余谓词

状态：已实现（Accepted）；只改变 `PlanNode` 的内部表示，`EXPLAIN` 文本不变；`EXPLAIN SEXPR` 的 join 字段由 ADR-019 升级为结构化输出
日期：2026-11-20

## 背景

ADR-017 引入了 `PlanNode`，但 `JoinPlan` 仍把 join 的键和残余谓词存成已经渲染好的字符串：

```rust
JoinPlan {
    uses_hash: bool,
    kind: JoinKind,
    details: Vec<String>, // ["keys=(a.id = b.id)", "residual=yes"]
    metrics: Metrics,
}
```

这能让文本计划保持稳定，却让“计划即数据”只完成了一半：后续规则想按 join 键匹配，必须重新解析 `"keys=(a.id = b.id)"`，或者把 `details` 当成不透明字符串。规则一旦需要区分 `USING` / `NATURAL` 的共享列名和 `ON` 的显式等值对，字符串表示就不够用了。

## 决策

1. `JoinPlan` 改为持有结构化字段：

   ```rust
   JoinPlan {
       uses_hash: bool,
       kind: JoinKind,
       keys: Option<JoinKeys>,
       residual: bool,
       on_expr: bool,
       metrics: Metrics,
   }
   ```

2. `JoinKeys` 区分两种键来源：
   - `Names(Vec<String>)`：`USING` / `NATURAL` 的共享列名，文本仍渲染为 `keys=(id, code)`；
   - `Pairs(Vec<JoinKey>)`：`ON` 的显式等值对，`JoinKey { left, right }` 保留两侧限定列名，文本仍渲染为 `keys=(a.id = b.id)`。
3. `residual` 表示 `ON` hash join 是否存在等值键之外的谓词，文本仍渲染 `residual=yes`；`on_expr` 表示 nested-loop `ON` 是否显示整体谓词，文本仍渲染 `on=expr`。
4. 所有显示片段由 `JoinPlan::detail_fragments()` 从结构化字段重建，顺序固定为 keys、residual、on_expr。历史文本逐字保持不变；本切片暂不改 `EXPLAIN SEXPR` 的 `:details` 列表，ADR-019 随后将其升级为 `:keys` / `:residual` / `:on-expr`。
5. 不改变执行路径、join 选择、统计估算、错误语义或结果行集；`residual` / `on_expr` 仍只是计划可观察的布尔标志，不携带完整 `ExecExpr`。

## 后果

- 正结果：后续规则可以直接读 `JoinPlan.keys`，按 `Names` / `Pairs` 匹配，不必解析 `keys=...` 字符串。
- 正结果：`USING` / `NATURAL` 与 `ON` 的键来源在类型上分开，规则不会把共享列名误当成显式等值对。
- 正结果：本切片在内部完成结构化时，文本和 S-expression 输出不变，现有整段计划断言可以继续作为回归保护；S-expression 的 join 字段随后由 ADR-019 单独升级。
- 负结果：`residual` 只是“有残余谓词”的标志，规则还不能读取或重写残余表达式；要走到那一步，需要把绑定期 `ExecExpr` 纳入计划节点，或建立单独的谓词表示。
- 负结果：本切片当时仍让 `EXPLAIN SEXPR` 输出 `:details` 字符串列表，尚未直接暴露 `:keys` / `:residual` 字段；ADR-019 已单独完成这次格式升级，使外部消费者可以直接按字段读取。
- 负结果：`Names` 只保存共享列名，不保存右侧列名；对 `USING` / `NATURAL` 的语义这是等价的，但需要右侧列对象的规则仍要从 schema 解析。

## 被否掉的替代

- **继续保留 `details: Vec<String>`**：规则仍需解析字符串，无法完成“计划即数据”的下一步。
- **本切片同时把完整 `ExecExpr` 存进 `JoinPlan`**：会把只读 `EXPLAIN` 节点扩成谓词计划，牵动绑定、生命周期和输出；当前没有规则真正需要重写残余表达式，收益不足。
- **直接把 `EXPLAIN SEXPR` 改成 `:keys` / `:residual` 字段**：会让上一版刚写入的输出格式和测试失效；先把内部结构做对，再单独决定是否升级外部格式。
- **把 `USING` / `NATURAL` 也强行表示成 `Pairs`**：文本需要只显示共享列名，强行用左右对会引入显示模式和额外状态，当前 `Names` 更贴近语义。

## 参考

- [ADR-017：结构化计划树与只读 S-expression](017-structured-plan-ir.md)
- [ADR-010：多源 WHERE 合取项拆分与单源下推](010-conjunctive-predicate-pushdown.md)
- Goetz Graefe. *The Cascades Framework for Query Optimization*. 1995. <https://15721.courses.cs.cmu.edu/spring2023/papers/16-optimizer1/graefe-ieee1995.pdf>
