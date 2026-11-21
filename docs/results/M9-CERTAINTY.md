# M9：`certainty(expr)` 答案等级

状态：标量函数与四域语义、CLI 端到端、与 `UNDER REPAIR` 的对拍均已完成。

日期：2026-11-21。

## 1. 交付

```text
crates/tritdb-exec/src/ann.rs       Certainty / Annotation::certainty / RepairAnswer::certainty
crates/tritdb-exec/src/lib.rs       Expr::Certainty / certainty_of_value
crates/tritdb-sql/src/{ast,parser,bind,plan,engine}.rs   certainty(expr [, threshold]) 接线
crates/tritdb/tests/sql_cli.rs      certainty 端到端 + UNDER REPAIR 对拍
```

## 2. 为什么需要它

SQL 的 `NULL` 同时表示「缺失」和「未知」，查询跑通但结论不确定时，智能体拿不到任何信号。
`certainty(expr)` 把「这个结论有多确定」变成一个显式列：

```sql
CREATE TABLE t (id INT NOT NULL, v LOGIC K3);
INSERT INTO t VALUES (1, TRUE), (2, FALSE), (3, NULL);
SELECT id, certainty(v) AS c FROM t ORDER BY id;
-- 1 CERTAIN / 2 IMPOSSIBLE / 3 CONTINGENT
```

## 3. 语义

| 域 | CERTAIN | CONTINGENT | IMPOSSIBLE |
|---|---|---|---|
| K3 | `TRUE` | `UNKNOWN`（含 `NULL`） | `FALSE` |
| Belnap | `TRUE` | `BOTH` / `NEITHER` | `FALSE` |
| Łukasiewicz | `p ≥ threshold`（默认 0.5） | `0 < p < threshold` | `p = 0` |
| ℕ | `n ≥ threshold`（默认 1） | `0 < n < threshold` | `n = 0` |

- Ł/ℕ 保留 `CONTINGENT` 中间带：低于阈值不是「假」，而是「部分成立」；
- 第二个参数是 `Int` 阈值：Ł 按千分位（`700` = 0.7），ℕ 按计数；
- 阈值越界（Ł 不在 `[0,1]`、ℕ 为负）报 `TypeMismatch`，不静默钳制；
- 非注解值（如 `TEXT`、`INT`）报 `NonBooleanPredicate`，不做隐式转换。

## 4. 与 `UNDER REPAIR` 共用同一词汇

`certainty()` 与 `UNDER REPAIR` 回答同一个问题：「这个结论是否被数据强制？」，
因此两者在库层共用 `Certainty::{Certain, Contingent, Impossible}`：

- `RepairAnswer::certainty()` 把 CQA 三种答案映射到同一枚举；
- `Annotation::certainty()` 把单值注解映射到同一枚举；
- CLI 输出统一为大写 `CERTAIN` / `CONTINGENT` / `IMPOSSIBLE`。

对拍测试：无 FD 冲突时 `UNDER REPAIR` 保留全部行且 `certainty` 全为 `CERTAIN`；
冲突时整组被丢弃且出现 `IMPOSSIBLE`。

## 5. 验收

```bash
cargo test -p tritdb-exec ann              # 22 passed（含 Certainty 两组）
cargo test -p tritdb --test sql_cli certainty   # 2 passed
cargo test --workspace                     # 480 passed
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## 6. 还没做

- `certainty` 作为 `WHERE` 谓词（`WHERE certainty(v) = 'CERTAIN'` 可用字符串比较，但没有专用语法/索引）；
- 整列/分组的确定性汇总（例如 `GROUP BY key` 后整组是否必然）；
- 与 `PROB_SUM` 等概率聚合的组合（`certainty(p) > 0.9` 的阈值聚合）。
