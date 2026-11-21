# M9：结构化错误（SQLSTATE + 修复建议）

状态：`SqlError` 的错误码与 hint、CLI 输出、pgwire `ErrorResponse` 的 `C`/`H` 字段均已完成。

日期：2026-11-21。

## 1. 交付

```text
crates/tritdb-sql/src/error.rs     SqlError::code() / hint() / format_structured()
crates/tritdb/src/main.rs          sql 子命令错误输出带 [SQLSTATE ...] 与 hint 行
crates/tritdb-pgwire/src/lib.rs    write_error_fields / write_sql_error / copy_failed_sql
crates/tritdb/tests/sql_cli.rs     sql_errors_carry_sqlstate_and_hint
crates/tritdb/tests/pgwire_protocol.rs  sql_errors_report_sqlstate_and_hint
```

## 2. 为什么不新造错误码

LLM 的预训练语料里有海量 PostgreSQL 错误，**SQLSTATE（五位字符）是它们已经见过的共同语言**。
因此 `SqlError::code()` 直接返回 PostgreSQL SQLSTATE，而不是自造一套 `E1234`：

- 客户端/智能体可以按码分支，不用解析英文散文；
- 重试循环能区分「可修」（`42703` 列名不存在）与「致命」（`XX001` 数据损坏）；
- 迁移到 PostgreSQL 客户端生态时不需要翻译层。

## 3. SQLSTATE 映射

| 错误 | SQLSTATE | 含义 |
|---|---|---|
| `Parse` | `42601` | syntax_error |
| `UnknownTable` | `42P01` | undefined_table |
| `TableExists` | `42P07` | duplicate_table |
| `UnknownColumn` | `42703` | undefined_column |
| `AmbiguousColumn` | `42702` | ambiguous_column |
| `DuplicateTableAlias` | `42712` | duplicate_alias |
| `DuplicateColumn` | `42701` | duplicate_column |
| `ColumnCount` | `42601` | INSERT 值数量不匹配 |
| `NotNull` | `23502` | not_null_violation |
| `CheckViolation` | `23514` | check_violation |
| `DuplicateKey` | `23505` | unique_violation |
| `NestedAggregate` / `ColumnOutsideGroup` / `ColumnOutsideAggregate` | `42803` | grouping_error |
| `Unsupported` | `0A000` | feature_not_supported |
| `Exec(DivisionByZero)` | `22012` | division_by_zero |
| `Exec(IntegerOverflow)` | `22003` | numeric_value_out_of_range |
| `Exec(TypeMismatch)` / `Exec(NonBooleanPredicate)` | `42804` | datatype_mismatch |
| `CorruptTable` | `XX001` | data_corrupted |
| `CorruptStats` | `XX000` | internal_error |
| `Txn` | `40000` | transaction_rollback |
| `Io` | `58030` | io_error |

## 4. 修复建议（hint）

每条 hint 都回答「下一步改什么」，而不是重复错误消息：

- `42P01`：`check the table name \`nope\`; create it first if it is missing`
- `42703`：`check the column name \`stat\`; qualify it as \`table.stat\` when several tables share it`
- `42803`：`add \`v\` to GROUP BY or wrap it in an aggregate`
- `23502`：`supply a value for \`t.id\` or make the column nullable`
- `23505`：`\`t.id\` is UNIQUE; \`1\` already exists, pick another value`
- `22012`：`guard the divisor, e.g. \`CASE WHEN d = 0 THEN NULL ELSE n / d END\``
- `42804`：`cast explicitly: \`CAST(expr AS INT)\``
- `Parse` / `Io`：不给 hint（消息本身已含位置/原因）

## 5. 两种消费方式

CLI（人/脚本）：

```text
error: unknown table: nope [SQLSTATE 42P01]
hint: check the table name `nope`; create it first if it is missing
```

pgwire（PostgreSQL 客户端/智能体）：`ErrorResponse` 现在带

- `S` = `ERROR`、`V` = `ERROR`
- `C` = SQLSTATE（如 `42P01`）
- `M` = 人类可读消息
- `H` = 修复建议（可选）

## 6. 验收

```bash
cargo test -p tritdb-sql                     # 35 passed（含 3 个错误码/建议单测）
cargo test -p tritdb --test sql_cli sql_errors   # 1 passed
cargo test -p tritdb --test pgwire_protocol sql_errors_report  # 1 passed
cargo test --workspace                       # 485 passed
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

## 7. did-you-mean（已落地）

`SqlError::UnknownName { kind, name, candidates }` 在列/表名解析失败时，用大小写不敏感的
Levenshtein 距离找最近的已知名字（预算随名字长度增长、最多 3 个），Display 与 hint 都带
`did you mean `status`?`：

- 列：`resolve_column` 失败时用 schema 的列名（未限定列去掉 `table.` 前缀）；
- 表：引擎的表清单（`SqlEngine::unknown_table`），10 处构造点统一走它；
- SQLSTATE 按 `kind` 保持 `42703` / `42P01`；
- 别名回退逻辑（`ORDER BY <alias>`）同时识别 `UnknownColumn` 与 `UnknownName`。

## 8. 错误码清单与机器可读输出（已落地）

- 稳定清单：`docs/ERRORS.md`（SQLSTATE、触发场景、hint 策略、兼容规则）；
- CLI `--error-format text|json`：json 模式在 stderr 输出
  `{"sqlstate":"42P01","message":"...","hint":"..."|null}`；未知格式在运行前直接报错；
- CLI 与 pgwire 现在共享同一套码与 hint（`docs/ERRORS.md` 的三种消费方式）。
- 端到端闭环验证见 `docs/results/M9-AGENT-RETRY.md`：6 个典型错误首轮失败、按 hint 一次修正成功。
