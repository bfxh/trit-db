# M3 第一刀：四域注解、COUNT MODELS、UNDER REPAIR

状态：语义核心、CLI、SQL 接线（`count_models` / `UNDER REPAIR ON` / `LOGIC` 列 / `truth()`）已实现；第二刀补上 t-norm 家族、truth 阈值投影与单元传播；增量 MCS 枚举未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-exec/src/ann.rs        Annotation / B4 / BoolExpr / count_models / under_repair
crates/tritdb/src/main.rs            models / repair 子命令 + 布尔表达式 parser
crates/tritdb/tests/ann_cli.rs       2 个 CLI 测试
docs/ADR/035-annotated-relations.md  决策与边界
```

## 2. 用法

```bash
# 精确模型计数：a AND (NOT b OR c)
tritdb models --vars 3 --expr "a AND (NOT b OR c)"
# vars=3 models=3

# SQL 形式：无 FROM，规则级计数
tritdb sql --db ann.db --sql "SELECT count_models('a AND (NOT b OR c)');"
# models
# 3

# 两个冲突事实（0 与 1 不能同时为真），查询 0 是否必然成立
tritdb repair --facts 2 --denial 0,1 --query 0
# facts=2 denials=1 query=0 answer=contingent

# 链式冲突 0-1、1-2，查询 0 且 1
tritdb repair --facts 3 --denial 0,1 --denial 1,2 --query 0,1
# answer=impossible
```

表达式语法：变量 `a`..`z` / `vN` / `N`，`NOT`/`!`、`AND`/`&&`、`OR`/`||`、括号、`TRUE`/`FALSE`。

## 3. 注解列

```sql
CREATE TABLE reading (id INT NOT NULL, status LOGIC K3);
INSERT INTO reading VALUES (1, TRUE), (2, FALSE), (3, NULL);
SELECT id, truth(status) AS s FROM reading;
-- 1 TRUE / 2 FALSE / 3 UNKNOWN
SELECT id FROM reading WHERE status IS UNKNOWN;
```

`LOGIC K3` 列按 `TRUE`/`FALSE`/`NULL` 存储；`LOGIC BELNAP` 列存 `TRUE`/`FALSE`/`BOTH`/`NEITHER`（另有 `NULL`），`IS UNKNOWN` 把 `BOTH`/`NEITHER` 投影为未知；`LOGIC LUKASIEWICZ` 用千分位整数存 `0.000–1.000`，`LOGIC NAT` 存非负整数。`truth(expr)` 是把三值投影成 `TRUE`/`FALSE`/`UNKNOWN` 的语法糖（内部展开为 `CASE`）。

## 4. 语义

| 域 | 说明 |
|---|---|
| K3 | SQL 三值逻辑，与 `TruthValue` 同表 |
| Belnap | `True / False / Both / Neither`，冲突可表达 |
| Łukasiewicz | `[0,1]`，neg `1-x`，meet `min`，join `max` |
| ℕ | 计数半环：meet 乘法、join 加法、无 neg |

- `count_models`：变量 ≤ 32，组件分解 + BDD 默认，DPLL 保留为对拍；超限 `TooManyVars`；
- `under_repair`：总事实 ≤ 64；按 denial 连通分量分解，**单元传播**把单元素 denial 的事实先钉掉，枚举只在存活事实上展开（单分量上限 16 指存活事实），返回 `Certain / Impossible / Contingent`；
- `TNorm`：Gödel / Łukasiewicz / Product 的 t-norm、t-conorm 与残差蕴涵，`Annotation::meet_with` / `join_with` / `implication_with` 可选家族（默认 Gödel）；
- `truth(x [, threshold])`：K3/Belnap 走三值投影，Ł 按千分位与阈值（默认 0.5）比较，ℕ 与阈值（默认 1）比较。

## 5. 验收

```bash
cargo test -p tritdb-exec ann          # 20 passed（含 t-norm 与单元传播）
cargo test -p tritdb --test ann_cli    # 6 passed
cargo test -p tritdb --test sql_cli truth_  # 2 passed
```

## 6. 还没做

- SQL 支持 `UNDER REPAIR ON (a, b, ...)` 多列函数依赖；列级/表级 `UNIQUE` 与 `CHECK` 在 `INSERT` / `UPDATE` 时生效（CHECK 的 `UNKNOWN` 通过）；Ł/ℕ 的 `SUM/AVG/MIN/MAX` 已支持；
- `truth()` 已支持 Ł/ℕ 的阈值投影（第二参数为 `Int`：Ł 用千分位、ℕ 用计数）；`PROB_SUM` 已提供；
- `count_models` 默认走组件分解 + BDD（独立合取项相乘、自由变量乘 `2^k`）；DPLL 保留为对拍 oracle；
- `under_repair` 的增量修复与更优的 MCS 枚举仍未做（单元传播只是剪枝，不是增量）；
- 概率/模糊聚合的标量函数（`PROB_AND` / `PROB_OR`）与 `PROB_SUM` 之外的聚合；t-norm 家族目前只在库层。
