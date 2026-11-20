# M3 第一刀：四域注解、COUNT MODELS、UNDER REPAIR

状态：语义核心与 CLI 已实现；SQL 语法接线、注解列存储、CQA 优化未做

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

- `count_models`：变量 ≤ 32，DPLL 精确计数（部分求值 + 短路 + 自由变量一次乘回），超限 `TooManyVars`；
- `under_repair`：总事实 ≤ 64；按 denial 连通分量分解，每个分量 ≤ 16；修复 = 最大一致子集，返回 `Certain / Impossible / Contingent`。

## 5. 验收

```bash
cargo test -p tritdb-exec ann          # 5 passed
cargo test -p tritdb --test ann_cli    # 2 passed
```

## 6. 还没做

- SQL 支持 `UNDER REPAIR ON (a, b, ...)` 多列函数依赖，冲突组整组丢弃；列级 `UNIQUE` 在 `INSERT` 时拒绝重复非 `NULL` 值；表级多列 UNIQUE/否定约束仍未做；
- Ł / ℕ 的专用聚合与 `truth()` 投影（这两类不是布尔，`truth()` 会拒绝）；
- BDD 与组件级计数仍未做（当前 DPLL，变量上限 32）；
- `under_repair` 的组件分解已做（按 denial 连通分量）；增量修复与更优的 MCS 枚举仍未做；
- 概率/模糊聚合与 `Łukasiewicz` 的更多 t-norm。
