# ADR 046：ASOF JOIN 的 SQL 语法与语义

状态：**Proposed**（实现随本 ADR 落地）

日期：2026-11-22

## 背景

M10 第一刀已在 `tritdb-exec::batch` 提供 `asof_join(left, right)` 库原语（右表有序、
每左行取最后一个 `right <= left`），并对暴力实现与 10 万行规模对拍通过。缺的是 SQL 表面：
时序分析（q 的 `aj`）需要在查询里表达「按时间取最近一行」。

## 决策

### 语法

```sql
SELECT ... FROM left ASOF JOIN right ON left.ts >= right.ts;
```

- 只支持单时间键（`ON <left_col> >= <right_col>`）；
- `NATURAL ASOF`、`ASOF ... USING`、`ASOF JOIN` 无 `ON` 都是错误；
- `ON` 只接受 `>=`，且两侧都是列引用；其他形式报 `0A000`（feature_not_supported）。

### 语义

1. **相等时间取最后一条**（q 的 `aj` 约定）；
2. **右表在执行时按时间列做稳定排序**——trit-db 的表没有排序保证，SQL 里也无法表达
   「FROM 有序」，因此由执行器排序（O(n log n)），并在计划里体现；
3. **时间列为 `NULL` 的行不参与匹配**：右表 NULL 行不进时间序列；左表 NULL 行匹配不到
   任何右行（右列全 `NULL`）；
4. **左外语义**：每个左行都保留；无匹配时右列填 `NULL`（与 `LEFT JOIN` 的可空规则一致）；
5. 时间列必须是 `INT`（Unix 毫秒/秒由调用方约定）；类型不符报 `42804`，
   列不存在报 `42703`。

### SQLSTATE

| 情况 | SQLSTATE |
|---|---|
| 时间列不存在 | `42703` |
| 时间列不是 `INT` | `42804` |
| `ON` 不是 `left >= right` 形式 / 缺 `ON` | `0A000` |

## 理由

- **单时间键**覆盖「按 symbol 分组再 asof」的绝大多数需求（外层 `GROUP BY` 或后续多键扩展）；
  第一版不引入多键的排序/分组复杂性；
- **执行时排序**让语法不依赖物理有序性，代价可预测（O(n log n)），且与现有 `ORDER BY` 的
  语义一致；
- **NULL 不匹配**与 q 的 `0N` 语义一致，也避免「NULL 时间被当成最小时间」这类静默错误；
- **左外**是时序补齐的默认需求（保留左表时间轴）。

## 后果

正面：

- `SELECT ... ASOF JOIN ...` 可直接使用 M10 的库原语，SQL 与库层共享同一实现；
- 计划输出新增 `asof` join 类型，`EXPLAIN SEXPR` 可读；
- 错误路径全部走既有结构化错误（SQLSTATE + hint）。

负面 / 未做：

- 不支持多时间键、不支持 `symbol` 分组内 asof（外层 `GROUP BY` 可用，但不是同一语义）；
- 不做右表「已排序」假设，因此每次执行都排序；
- `ASOF JOIN` 只能作为 `JOIN` 的一种，不能与 `NATURAL` 组合。

## 验收

```bash
cargo test -p tritdb-sql asof      # 集成测试
cargo test --workspace             # 全量
```

集成测试覆盖：基础匹配、相等时间取最后、右表乱序输入、NULL 时间不匹配、
无匹配右列 NULL、`ON` 形式错误、时间列类型错误。
