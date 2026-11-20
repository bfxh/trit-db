# ADR 035：标注关系代数第一刀——四域注解、精确计数、最小修复

状态：**Proposed**

日期：2026-11-21

## 背景

M0/M1 的 SQL 只有 `TRUE / FALSE / NULL` 三值，无法表达：

- **冲突**：同一条知识同时有正反证据（Belnap `Both`）；
- **可信度**：模糊/概率式的 `[0, 1]` 标注；
- **计数**：一个条件有多少个模型；
- **修复**：数据违反约束时，在所有最小修复下查询是否必然成立。

M3 的目标是「标注关系代数」。完整实现需要 parser/binder/executor 全链接线，周期很长；这一刀先交付**可独立验证的语义核心**。

## 决策

在 `tritdb-exec` 增加 `ann` 模块，不碰 SQL parser：

### 1. 四域注解

`Annotation::{K3, Belnap, Lukasiewicz, Nat}`，每个域定义 `neg` / `meet` / `join`：

| 域 | neg | meet (AND) | join (OR) |
|---|---|---|---|
| K3 | `not` | SQL 三值 AND（`False` 吸收） | SQL 三值 OR（`True` 吸收） |
| Belnap | `True<->False`，`Both/Neither` 固定 | 真值格 meet | 真值格 join |
| Łukasiewicz | `1 - x` | `min` | `max` |
| ℕ | 未定义 | 乘法（独立事件计数） | 加法（并集计数） |

跨域运算返回 `DomainMismatch`；Łukasiewicz 构造拒绝 `[0, 1]` 之外的值。

### 2. 精确模型计数（`COUNT MODELS` 的语义核）

`BoolExpr::{Var, Const, Not, And, Or}` + `count_models(expr, vars)`：

- 变量数 ≤ 20，枚举 `2^n` 个赋值，逐个求值；
- 精确，不抽样；
- 超过上限返回 `TooManyVars`，不静默降级。

### 3. 最小修复下的一致性查询（`UNDER REPAIR` 的语义核）

`under_repair(facts, denials, query)`：

- 事实用索引 `0..facts` 表示；
- 每个 denial 是一组不能同时为真的事实；
- 修复 = 不满足任何 denial 的**最大**事实子集（MCS）；
- 返回 `Certain`（所有修复满足 query）、`Impossible`（所有修复都不满足）、`Contingent`（部分满足，对应 Belnap `Both`）；
- 事实数 ≤ 16，枚举 `2^n` 个子集，每个子集检查是否能再加一个事实仍一致。

## 理由

- **语义先行**：注解/计数/修复的语义可以用小规模穷举验证，不需要先把 SQL 方言接完。
- **可判红**：每个结论都有精确期望值（模型数、`Certain/Impossible/Contingent`），不是「看起来对」。
- **上限显式**：20 个变量 / 16 个事实是硬边界，超限报错。真正的 #SAT/BDD/CQA 优化留到后续。

## 后果

正面：

- `COUNT MODELS` 与 `UNDER REPAIR` 有了可复用的库 API 和 CLI（`tritdb models` / `tritdb repair`）。
- 四域注解为后续「注解列 / 标注关系代数」提供类型基础。
- 测试覆盖 Belnap 真值表、跨域错误、精确计数、CQA 三种答案、越界输入。

负面 / 未做：

- **SQL parser 只接了 `count_models`**：`SELECT count_models('<expr>'[, vars])` 已可用；`UNDER REPAIR` 还没有语法，因为需要先有约束（UNIQUE/FD/denial）才能定义修复语义。
- `count_models` 是 `2^n` 枚举，不是 DPLL/BDD；变量上限 20。
- `under_repair` 是 `2^n * n * d` 枚举，事实上限 16；没有增量/组件分解。
- 注解还没有进入存储层（列存没有注解列），也没有进入查询执行器。

## 验收

```bash
cargo test -p tritdb-exec ann
cargo test -p tritdb --test ann_cli
cargo run -p tritdb -- models --vars 3 --expr "a AND (NOT b OR c)"
cargo run -p tritdb -- repair --facts 2 --denial 0,1 --query 0
```

- `models` 输出 `vars=3 models=3`；
- `repair` 输出 `answer=contingent`（两个冲突事实只保留一个）。
