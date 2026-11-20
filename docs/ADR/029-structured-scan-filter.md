# ADR-029：把下推到 Scan 的过滤谓词结构化

状态：Accepted

日期：2026-11-20

## 背景

谓词下推已经在执行路径中存在：`plan_filter_pushdown` 把全内连接/交叉连接的顶层 `AND` 合取项按源拆分，`pushdown_filter(pushdown, index)` 返回第 `index` 个扫描源的下推谓词，`filter_table` 再把它绑定并应用到该源。

但计划数据只保留了：

```lisp
(scan "a" :filtered #t :est-rows 3 :cols ("id" "v"))
```

`:filtered #t` 只能说明“这个扫描有下推过滤”，不能说明过滤了什么。消费者想知道 `a.v <> 'skip'` 这类谓词，仍得回到 `Select` / `Expr` 或解析执行层状态。这是当前 Scan 节点最后一个“存在标志代替内容”的字段。

## 决策

1. `PlanKind::Scan` 增加 `filter: Option<PlanExpr>`，表示已经下推到该扫描源的谓词；没有下推过滤时为 `None`。
2. `PlanNode::scan` 增加对应参数。`filter` 与 `filtered` 来自同一个 `PushdownPlan`：`filtered` 继续用于文本渲染，`filter` 保存结构化谓词。
3. `build_plan` 在构造每个扫描节点时调用 `plan_scan_filter(pushdown, scan_index, &source.schema)`：

   - 从 `pushdown_filter` 取 `&Expr`；
   - 用完整源 schema `bind_expr`，保留原始列名与限定名；
   - 再经 `PlanExpr::from_exec` 转成只读计划表达式。

4. `EXPLAIN SEXPR` 在 `:filtered #t` 之后输出可选 `:filter`：

```lisp
(scan "a" :filtered #t :filter (<> (col "a.v") "skip") :est-rows N :cols ("id" "v"))
```

   - 没有下推谓词时不输出 `:filter`；
   - 字段顺序固定为 `:alias`、`:filtered`、`:filter`、估算/actual、`:cols`。
5. 文本 `EXPLAIN` / `EXPLAIN ANALYZE` 仍只根据 `filtered` 输出 `Scan a filtered`，不打印 `:filter` 或谓词文本；因此文本计划逐字不变。
6. `:filtered #t` 作为兼容标志保留，即使 `:filter` 已经能表达同一事实。理由是已有 `EXPLAIN SEXPR` 消费者可能只检查该字段，且当前没有版本化格式或迁移机制；删除它只减少冗余，不增加信息。后续若引入格式版本，再统一评估移除。

## 不做什么

- 不改变 `plan_filter_pushdown` 的拆分策略。单表、外连接或不满足全内连接条件的查询，仍按既有规则把整个过滤留给 `Filter` / residual，不为了填充 `:filter` 扩大下推范围。
- 不把 residual 谓词、join 谓词或 `Filter.pred` 复制到 Scan。
- 不改变执行侧的投影、列裁剪、过滤顺序或绑定 schema。
- 不引入表达式级列下推、规则 pass、代价模型或新的计划节点。
- 不删除 `:filtered`，也不改变无过滤扫描的既有形状。

## 影响

- 结构化消费者现在可以直接读取“哪个源被什么谓词过滤”，不需要回到 SQL AST。
- `EXPLAIN SEXPR` 新增一个可选字段；旧消费者忽略未知字段时继续工作。整段精确断言的期望需要同步加入 `:filter`。
- 文本计划、结果行集、错误语义和执行路径不变。
- `Scan` 同时保留 `filtered: bool` 与 `filter: Option<PlanExpr>`，两者在正常路径上同源；这是有意的兼容冗余，不是两个独立事实来源。

## 验证

- `EXPLAIN SEXPR SELECT a.v FROM a JOIN b ON a.id = b.id WHERE a.v <> 'skip' ORDER BY a.v LIMIT 1` 的扫描节点包含 `:filtered #t :filter (<> (col "a.v") "skip")`。
- 双侧下推 `WHERE a.v <> 'skip' AND b.w <> 'keep'` 时，`a` 与 `b` 的扫描节点各自输出对应的 `:filter`。
- 既有文本断言 `Scan a filtered` / `Scan a AS x` 保持不变。
- `EXPLAIN SEXPR` / `EXPLAIN ANALYZE SEXPR` 的其余整段断言保持不变；fmt、Clippy、全量测试与 SQLite 对拍通过。
