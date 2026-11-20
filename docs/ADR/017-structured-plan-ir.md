# ADR-017：结构化计划树与只读 S-expression

状态：已实现（Accepted）；只提供 `EXPLAIN` 的只读计划表示，不引入 Lisp 运行时
日期：2026-11-20

> 后续：ADR-018 将 `JoinPlan.details` 拆成结构化 `JoinKeys` / `residual` / `on_expr`；ADR-019 进一步把 `EXPLAIN SEXPR` 的 `:details` 升级为 `:keys` / `:residual` / `:on-expr`。`EXPLAIN` 文本始终保持不变。

## 背景

`EXPLAIN` 目前由 `build_plan` 递归拼接字符串：每个节点先拼出自己的标签，再用 `indent_plan` 把子节点整体缩进。这个文本对人和现有测试都很稳定，但计划本身没有独立的数据类型，导致：

- 后续优化规则只能继续在 `Select` / `Expr` 上写特判，不能对计划树做模式匹配；
- `EXPLAIN` 的节点、行数、列裁剪、join 键和残余谓词分散在格式化代码里；
- 想让 AI/外部工具读取计划，只能解析缩进文本，信息有损；
- 想引入规则式优化，必须先有“计划即数据”的基础。

Lisp 的可借之处不是括号或运行时，而是同像性（homoiconicity）：程序/计划可以作为统一的数据结构被读取、匹配和重写。查询优化器领域已有成熟先例：Volcano 优化器生成器把优化表达为查询代数上的变换规则；Cascades 进一步用 memo 和规则搜索组织等价计划；Apache Calcite 用 `RelNode` + `RelOptRule` + `VolcanoPlanner`，DataFusion 用 `LogicalPlan` + `OptimizerRule` 递归重写计划树。它们共同的前提都是：计划先是树，文本只是树的投影。

## 决策

1. 新增 `crates/tritdb-sql/src/plan.rs`，定义只在 crate 内可见的 `PlanNode` / `PlanKind`，节点覆盖当前 `EXPLAIN` 的 `Scan`、`Join`、`Filter`、`Aggregate`、`Having`、`Project`、`Sort`、`Limit`。
2. `build_plan` 从返回 `String` 改为返回 `PlanNode`；`PlanNode::to_text()` 负责历史文本格式。现有 `EXPLAIN` 文本必须逐字保持不变，由既有的整段 `assert_eq!` 计划测试守护。
3. `PlanNode::to_sexpr()` 输出只读的括号化计划树；新增 `EXPLAIN SEXPR SELECT ...`，返回一列 `plan`、一行文本值。它不是 Lisp 解释器，也不接受用户自定义表达式或函数。
4. `EXPLAIN SEXPR` 只做静态计划；本 ADR 当时不支持 `EXPLAIN ANALYZE SEXPR`，由 parser 明确拒绝。该限制已由 ADR-020 撤销：`EXPLAIN SEXPR` 仍只输出估算（`:est-rows` / `:est-groups`），`EXPLAIN ANALYZE SEXPR` 走真实执行并只输出 actual 字段和 `(execution ...)` 汇总；`EXPLAIN ANALYZE` 文本路径不变。
5. S-expression 的字段是稳定的最小集合：节点名、扫描表名/别名/是否下推过滤/估算行数/裁剪列、join 算法/连接类型/键或残余谓词、投影列、排序键数、limit 值。join 的 `details` 当时保留为已渲染的键/残余片段，避免把当前 join 条件分析复制成第二套逻辑；ADR-018 已把该字段替换为结构化 `JoinKeys` / `residual` / `on_expr`，文本保持不变；ADR-019 随后把 S-expression 的 join 输出升级为 `:keys` / `:residual` / `:on-expr`；ADR-021 又把 `Filter` 的绑定谓词转成 `:pred` 的 `PlanExpr`；ADR-022 进一步把 join 的完整 `ON` / `USING` / `NATURAL` 谓词转成同一形状的 `:pred`；ADR-023 把 `Having` 的 `GroupExpr` 也转成 `:pred`；ADR-024 再把 `Aggregate` 的分组键与引用聚合调用转成 `:keys` / `:aggregates`；ADR-025 再把 `Sort` 的排序键表达式与升降序转成结构化 `:keys`；ADR-026 再把 `Project` 的输出名与计算表达式转成结构化项列表；ADR-027 再把 hash join 的残余谓词单独转成 `:residual-pred`；ADR-028 把 `Scan` 的表名与别名拆成 `table` / `alias`，在 `EXPLAIN SEXPR` 中输出可选 `:alias`；ADR-029 再给 `Scan` 增加可选 `:filter`，把下推过滤谓词转为 `PlanExpr`；ADR-030 让 hash join 的 `:residual-pred` 不再依赖历史 `:residual` 标志，补上同源列等值等非键合取项。
6. 明确不做：完整逻辑/物理计划分层、规则注册表、代价搜索、join 重排、表达式级列下推、Lisp/Scheme 方言、运行时宏展开、通用 `eval`、把 SQL parser 改成 s-expression reader。
7. `EXPLAIN SEXPR` 不改变执行路径、统计、错误语义、结果行集或列标签；它只在 `EXPLAIN` 的输出格式分支上生效。

## 后果

- 正结果：`EXPLAIN` 的节点和子节点关系第一次成为 Rust 数据，后续可以在 `PlanNode` 上写规则匹配，而不必解析缩进文本。
- 正结果：现有文本计划由 `to_text` 统一生成，格式化逻辑从 `build_plan` 中抽离；既有整段文本断言仍通过。
- 正结果：`EXPLAIN SEXPR` 给 AI/脚本一个明确的树形输入，例如：
  ```lisp
  (project (("one" 1)) :est-rows 3
    (scan "a" :est-rows 3 :cols ()))
  ```
- 正结果：零列投影、谓词下推、列裁剪、join 算法和估算都能在同一棵树里被观察，不需要额外解析规则。
- 负结果：`PlanNode` 目前只覆盖 `EXPLAIN` 的物理可观察节点，不是完整逻辑计划；ADR-018 已把 `JoinPlan` 的键和残余标志结构化为 `JoinKeys` / `residual` / `on_expr`，ADR-019 在 `EXPLAIN SEXPR` 暴露这些字段，ADR-022 补上完整 join `:pred`，ADR-023 补上 `Having :pred`，ADR-024 补上 `Aggregate :keys` / `:aggregates`、ADR-025 补上 `Sort :keys`、ADR-026 补上 `Project` 的输出项与表达式、ADR-027 补上 hash join 的 `:residual-pred`、ADR-028 补上 `Scan` 的 `:alias`、ADR-029 补上 `Scan` 的 `:filter`、ADR-030 让 `:residual-pred` 覆盖历史 residual 标志漏掉的同源列等值，谓词与节点身份的主要观察面已经齐备；但计划树仍不承载规则、代价模型或逻辑/物理分层。
- 负结果：没有规则引擎和代价模型，`PlanNode` 只是“可被规则处理的输入”，不会自动产生更优计划。
- 负结果：新增 `Statement::ExplainSexpr` 会让外部对 `Statement` 做穷举匹配的调用方需要处理新分支；当前仓库没有外部下游，先接受这个兼容成本。
- 负结果：`EXPLAIN SEXPR` 返回单行多行文本，CLI 打印时仍按一个单元格显示；这是有意的机器可读格式，不追求和文本计划逐行对齐。

## 被否掉的替代

- **直接引入 Lisp/Scheme 运行时或 SQL 方言**：需要 reader、求值器、GC、FFI、错误模型和安全边界；会绕开现有 Rust 类型系统和 SQLite 对拍，收益远小于成本。
- **继续用字符串 + 正则/缩进解析做规则匹配**：计划文本是给人看的，缩进和标签会变；解析文本会复制一套脆弱的计划语法。
- **只加 `EXPLAIN SEXPR`，不抽 `PlanNode`**：会把文本计划再序列化一次，得到“带括号的字符串”，无法支撑后续规则。
- **先做规则引擎，再考虑计划树**：没有稳定节点类型时，规则只能在 `Select`/`Expr` 上特判，范围和风险都更大。
- **把 `PlanNode` 直接公开为完整逻辑计划 API**：当前执行路径仍是单一物理路径，先保持 crate 内可见，等规则和逻辑/物理分层稳定后再评估公开面。

## 参考

- Goetz Graefe, William J. McKenna. *The Volcano Optimizer Generator: Extensibility and Efficient Search*. 1993. <https://cs.ubc.ca/~rap/teaching/504/2006/readings/volcano.pdf>
- Goetz Graefe. *The Cascades Framework for Query Optimization*. 1995. <https://15721.courses.cs.cmu.edu/spring2023/papers/16-optimizer1/graefe-ieee1995.pdf>
- Surajit Chaudhuri. *An Overview of Query Optimization in Relational Systems*. 1998. <https://cs.stanford.edu/people/chrismre/cs345/rl/chaudhuri98.pdf>
- Apache Calcite. *Algebra* 与 `RelNode` / `RelOptRule` / `VolcanoPlanner` 文档. <https://calcite.apache.org/docs/algebra.html>
- Apache DataFusion. *Query Optimizer* 与 `LogicalPlan` / `OptimizerRule` / `TreeNode` 文档. <https://datafusion.apache.org/library-user-guide/query-optimizer.html>
- James Cheney, Sam Lindley, Philip Wadler. *The Essence of Language-Integrated Query*. 2013. <https://homepages.inf.ed.ac.uk/jcheney/linq/essence-linq.pdf>
