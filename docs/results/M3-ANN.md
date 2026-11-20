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

# 两个冲突事实（0 与 1 不能同时为真），查询 0 是否必然成立
tritdb repair --facts 2 --denial 0,1 --query 0
# facts=2 denials=1 query=0 answer=contingent

# 链式冲突 0-1、1-2，查询 0 且 1
tritdb repair --facts 3 --denial 0,1 --denial 1,2 --query 0,1
# answer=impossible
```

表达式语法：变量 `a`..`z` / `vN` / `N`，`NOT`/`!`、`AND`/`&&`、`OR`/`||`、括号、`TRUE`/`FALSE`。

## 3. 语义

| 域 | 说明 |
|---|---|
| K3 | SQL 三值逻辑，与 `TruthValue` 同表 |
| Belnap | `True / False / Both / Neither`，冲突可表达 |
| Łukasiewicz | `[0,1]`，neg `1-x`，meet `min`，join `max` |
| ℕ | 计数半环：meet 乘法、join 加法、无 neg |

- `count_models`：变量 ≤ 20，精确枚举，超限 `TooManyVars`；
- `under_repair`：事实 ≤ 16，修复 = 最大一致子集，返回 `Certain / Impossible / Contingent`。

## 4. 验收

```bash
cargo test -p tritdb-exec ann          # 5 passed
cargo test -p tritdb --test ann_cli    # 2 passed
```

## 5. 还没做

- SQL 语法：`SELECT COUNT MODELS ...`、`UNDER REPAIR` 还没有进 parser/binder；
- 注解列存储与执行器接线（当前只在库层与 CLI 使用）；
- `count_models` 的 DPLL/BDD 优化（当前 `2^n` 枚举，上限 20）；
- `under_repair` 的增量/组件分解（当前 `2^n * n * d`，上限 16）；
- 概率/模糊聚合与 `Łukasiewicz` 的更多 t-norm。
