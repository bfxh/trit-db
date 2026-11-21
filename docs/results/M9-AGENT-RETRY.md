# M9：hint 端到端闭环（错误 → 修正 → 成功）

状态：6 个典型错误场景全部「首轮失败、按 hint 一次修正成功」。

日期：2026-11-21。

## 1. 为什么做这个

前面的工作把错误做成了「可判定」的结构（SQLSTATE + hint + did-you-mean 候选）。
但「可判定」不等于「可操作」——真正要证明的是：**调用方拿到错误后，能否只按错误里的信息
就修好查询**。这个闭环是 VISION 里 TypeQL 曲线（75% → 92%）在 trit-db 上的对应物。

## 2. 闭环设计

`crates/tritdb/tests/agent_retry.rs` 里每个场景是一个结构体：

```rust
struct Scenario {
    name, seed, broken, code, hint_contains,
    repair: fn(&str, &[String]) -> String,   // 只用 hint 里的信息
}
```

测试对每个场景：

1. 执行 `seed` 建数据；
2. 执行 `broken`，断言**首轮失败**且 `error.code()` 等于期望 SQLSTATE；
3. 断言 `hint` 含关键信息（候选名 / 模板 / 列名）；
4. 从 hint 的反引号里取出名字，用 `repair` 只做「hint 指出的那处修改」；
5. 重试一次，断言成功。

## 3. 六个场景

| 场景 | 错误 | SQLSTATE | hint 给出的信息 | 一次修正 |
|---|---|---|---|---|
| 列名拼错 | `SELECT stat FROM customer` | `42703` | `did you mean \`status\`?` | 替换为 `status` |
| 表名拼错 | `SELECT * FROM custmer` | `42P01` | `did you mean \`customer\`?` | 替换为 `customer` |
| 缺 GROUP BY | `SELECT k, v FROM g GROUP BY k` | `42803` | `add \`v\` to GROUP BY` | 追加 `, v` |
| 列名歧义 | `SELECT id FROM a, b` | `42702` | `qualify \`id\` as \`table.id\`` | 限定为 `a.id` |
| NOT NULL | `INSERT INTO t VALUES (NULL)` | `23502` | `supply a value for \`t.id\`` | 换成 `1` |
| 除零 | `SELECT n / d FROM nums`（`d=0`） | `22012` | `CASE WHEN d = 0 THEN NULL ...` | 套上 CASE |

结果：

```text
首轮失败：6 / 6
按 hint 一次修正成功：6 / 6（100%）
```

第二个测试 `structured_errors_expose_the_same_fields_in_json_and_text` 验证文本形式
（`[SQLSTATE 42703]` + `hint:`）与 JSON/pgwire 使用的是同一组字段（code / hint / 候选）。

## 4. 这条曲线说明什么、不说明什么

说明：

- hint 携带的信息**足够**定位修改点（候选名、模板、列名），不需要额外的 schema 查询或猜测；
- 同一套结构同时服务 CLI 文本、CLI JSON、pgwire，调用方不需要为不同入口写不同解析；
- 「一次修正」在最坏情况下也只是 1 次重试，而不是 TypeQL 场景里的多次试错。

不说明：

- 脚本替换是**上界**：真实 LLM 可能一次改不对（比如同时改多处），这里的 100% 是「信息充分性」的
  证明，不是模型准确率；
- 没有覆盖语义性错误（结果错但没报错）——那正是 `certainty()` 与 `truth()` 负责的层面。

## 5. 验收

```bash
cargo test -p tritdb --test agent_retry    # 2 passed
cargo test --workspace                     # 490 passed
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```
