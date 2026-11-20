# ADR-016：零列源表投影

状态：已实现（Accepted）；扩展 ADR-011/ADR-015 的源表列裁剪
日期：2026-11-20

## 背景

ADR-011 的列裁剪在“某源表没有被引用任何列”时保留完整 schema，因为当时把空保留集视为无法安全裁剪。这样 `SELECT 1 FROM wide`、`SELECT COUNT(*) FROM wide`，以及只靠 `CROSS JOIN` 提供行多重性的源表，仍会把所有列复制进扫描和 join 中间结果，尽管后续根本不读取它们。

源表行的数量本身就是查询语义的一部分：常量投影和 `COUNT(*)` 需要按行求值，`CROSS JOIN` 需要行数乘积。只需要保留“行”而不保留“列”时，可以用空 schema 表示该源表，不需要引入新的表类型。

## 决策

1. `ScanProjection` 的 `Some(columns)` 允许为空：`Some([])` 表示该源表只保留行多重性，不保留任何列；`None` 仍表示无法安全裁剪、保留完整 schema。
2. 计算保留列时，若某源表的保留集为空且该表原本有列，则生成 `Some([])`；若保留集覆盖全部列，仍生成 `None`。`SELECT *`、`USING` / `NATURAL`、未限定列无法唯一归属等既有回退条件不变。
3. `qualify_table_projected` 对 `Some([])` 构造空 schema，并为每条源表行生成一个空 `Row`；行数不变。`None` 仍走完整 schema 的原有路径。
4. `EXPLAIN SELECT` / `EXPLAIN ANALYZE` 对空投影显示 `Scan ... cols=()`，与普通严格子集的 `cols=a,b` 区分。
5. join 继续复用既有布局和行构造：零列输入仍然保留源表行数，`CROSS JOIN` / `ON TRUE` 产生行数乘积，外连接通过既有 NULL 行路径保留被保留侧的行数。零列 schema 不新增 join 分支。
6. 明确不做：行数专用表类型、零行源表优化、表达式级投影下推、跨 join 列需求传播、`SELECT *` / `USING` / `NATURAL` 的按列裁剪、join 重排或逻辑计划。

## 后果

- 正结果：`SELECT 1 FROM a` 和 `SELECT COUNT(*) FROM a` 的扫描只保留行数，不再复制 `a` 的宽列；计划显示 `Scan a cols=()`。
- 正结果：`CROSS JOIN` / `ON TRUE` 中只提供行多重性的源表可以变成零列，join 输出仍按行数乘积构造。
- 正结果：外连接、`FULL JOIN` 的 NULL 填充路径在空 schema 上仍保持原有行数语义；结果列标签、值和行集不变。
- 正结果：源表仍完整加载到内存，扫描 I/O 不减少；优化只缩小扫描副本和 join 中间行的宽度。
- 负结果：零列源表仍需要逐行扫描和 join 行构造，不能跳过行数计算；没有把查询改写成常量结果。
- 负结果：`EXPLAIN` 中的 `cols=()` 是物理列清单，不表示源表没有 schema；持久 schema、统计和 join 选择仍使用完整表定义。
- 负结果：含 `*`、`USING` / `NATURAL` 或无法唯一归属的未限定列时仍回退完整 schema，可能继续携带未使用列。

## 被否掉的替代

- **把空保留集继续当作完整 schema**（旧行为）：会为常量投影和 `COUNT(*)` 复制所有列，失去本切片的主要收益。
- **引入专门的 `RowCount` / 零列表类型**：需要改表表示、持久格式和所有执行节点；空 schema 已能表达同样的行多重性。
- **同时做表达式级投影下推或跨 join 列需求传播**：会把表达式需求和 join 布局传播一起引入；先闭合“只保留行、不保留列”的物理表示。
