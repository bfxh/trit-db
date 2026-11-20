# ADR-015：无歧义未限定列的源表裁剪

状态：已实现（Accepted）；扩展 ADR-011 的列裁剪边界
日期：2026-11-20

## 背景

ADR-011 为了避免改变 binder 的后缀解析和歧义规则，要求所有列引用都带显式 qualifier；只要出现一个未限定列，整条查询就退回完整 schema。这样 `SELECT v FROM wide` 或 `... WHERE v = 'x'` 即使 `v` 只属于一个源表，也无法裁掉旁边的宽列。

binder 的 `resolve_column`（`bind.rs`；join 布局里的 `find_join_column` 使用同一后缀规则）已经按“未限定列在限定 schema 中必须唯一”来解析：没有匹配报 `unknown column`，多个匹配报 `ambiguous column`。因此，在收集阶段先按源表 schema 做同样的唯一性判断，可以安全地放开一部分未限定列，而不改变最终解析结果。

## 决策

1. 收集列需求时，同时记录每个源表 schema 的列名集合。
2. 显式 qualifier 仍按源表 qualifier 归属；未限定列 `name` 只有在所有源表中恰好一个 schema 包含 `name` 时，才归给该源表并记录保留列。
3. 未限定列匹配零个或多个源表时，整条查询退回完整 schema，不在优化阶段报错；后续仍由 binder 按原规则报 `unknown column` 或 `ambiguous column`。
4. `SELECT *`、`USING` / `NATURAL` join 仍整条回退。每个源表仍只在保留列构成严格子集时裁剪；空集合或全列不裁剪。
5. 裁剪仍只作用于 join 前的源表副本和中间结果宽度。源表加载、统计、join 选择、输出列顺序、输出标签和值都不变。
6. 明确不做：零列源表、表达式级投影下推、跨 join 的列需求传播、`SELECT *` / `USING` / `NATURAL` 的按列裁剪、join 重排或逻辑计划。

## 后果

- 正结果：`SELECT v FROM a` 这类单表未限定列查询可以只扫描 `v`，不再携带同表的 `id` / `spare` 等宽列。
- 正结果：join 中的未限定列如果只属于一个源表（例如 `WHERE v = 'a2'`），可以和显式限定列一起参与裁剪，计划显示更窄的 `cols=...`。
- 正结果：歧义列 `id`（两个源表都有）仍退回完整 schema，binder 继续报 `ambiguous column: id`；未知列也继续报 `unknown column`，错误时机不变。
- 正结果：查询结果、列标签、`ORDER BY` / `GROUP BY` 的别名解析保持不变；变化只在 `EXPLAIN` / `EXPLAIN ANALYZE` 的 `Scan ... cols=...` 和中间行宽度上可观测。
- 负结果：只有当未限定列在源表 schema 中唯一时才会被归属；多源同名或未知名字仍整条回退。
- 负结果：如果 `ORDER BY` / `GROUP BY` 的名字是投影别名而不是真实源列，收集阶段可能保留一个多余的源列；这是保守的，不改变结果。
- 负结果：源表仍完整加载到内存，扫描 I/O 不减少；零列源表和表达式级列需求仍不做。

## 被否掉的替代

- **不检查唯一性，直接裁剪所有未限定列**：会把歧义列错误地归给某一个源，或让 binder 看不到冲突；ADR-015 只接受唯一归属。
- **先执行 binder 再裁剪**：需要重排 `select` 的绑定、扫描和计划构建顺序，范围大于本切片；当前用源表 schema 集合即可做与 binder 等价的唯一性判断。
- **同时做零列源表/表达式级列下推**：会把行多重性和表达式需求传播一起引入；先闭合未限定列的唯一归属。
