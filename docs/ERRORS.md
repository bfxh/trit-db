# 错误码清单（SQLSTATE 稳定引用）

状态：稳定。新增错误码只追加，不修改已有码的语义。日期：2026-11-21。

本文件是 `crates/tritdb-sql/src/error.rs` 中 `SqlError::code()` 的稳定清单。
模型与客户端应当按 SQLSTATE 分支，而不是解析英文消息；`hint` 是可选的修复建议。

## 1. 输出格式

三种消费方式共享同一套码：

| 场景 | 形式 |
|---|---|
| CLI 默认 | `error: <message> [SQLSTATE 42P01]` + 下一行 `hint: <hint>` |
| CLI `--error-format json` | stderr 一行 JSON：`{"sqlstate":"42P01","message":"...","hint":"..."}`（无 hint 时 `null`） |
| pgwire | `ErrorResponse` 的 `C`（SQLSTATE）与 `H`（hint）字段 |

示例：

```bash
$ tritdb sql --db x.db --sql "SELECT stat FROM t" --error-format json
{"sqlstate":"42703","message":"unknown column: stat","hint":"check the column name `stat`; qualify it as `table.stat` when several tables share it"}
```

## 2. 清单

### 解析与名字

| SQLSTATE | 名称 | 触发 | hint 策略 |
|---|---|---|---|
| `42601` | syntax_error | lexer/parser 失败、INSERT 值数量不匹配 | 消息含字符位置；`ColumnCount` 给出期望值数 |
| `42P01` | undefined_table | 表不存在（SELECT/INSERT/UPDATE/DELETE/JOIN/COPY） | 最近表名候选（Levenshtein） |
| `42P07` | duplicate_table | CREATE TABLE 重名 | 先 DROP 或换名 |
| `42703` | undefined_column | 列不存在 | 最近列名候选（Levenshtein） |
| `42702` | ambiguous_column | 未限定列在多表里重名 | 限定为 `table.column` |
| `42712` | duplicate_alias | FROM/JOIN 别名重复 | 每个来源给不同别名 |
| `42701` | duplicate_column | CREATE TABLE 列重名 | 改名 |

### 约束

| SQLSTATE | 名称 | 触发 | hint 策略 |
|---|---|---|---|
| `23502` | not_null_violation | NOT NULL 列收到 NULL | 提供值或把列改为可空 |
| `23514` | check_violation | CHECK 求值为 FALSE | 指出 `CHECK (<expr>)` 文本 |
| `23505` | unique_violation | UNIQUE 列重复 | 指出冲突值并建议换值 |

### 聚合与分组

| SQLSTATE | 名称 | 触发 | hint 策略 |
|---|---|---|---|
| `42803` | grouping_error | 嵌套聚合、列不在 GROUP BY、聚合查询里的裸列 | 加入 GROUP BY 或包进聚合；嵌套聚合用子查询 |

### 执行

| SQLSTATE | 名称 | 触发 | hint 策略 |
|---|---|---|---|
| `22012` | division_by_zero | 除数为零 | 用 `CASE WHEN d = 0 THEN NULL ELSE n / d END` 保护 |
| `22003` | numeric_value_out_of_range | i64 溢出 | 拆分表达式或缩窄范围 |
| `42804` | datatype_mismatch | 混合类型比较/谓词非布尔 | 显式 `CAST`，注解列用 `truth()`/`certainty()` |

### 存储、事务与 IO

| SQLSTATE | 名称 | 触发 | hint 策略 |
|---|---|---|---|
| `XX001` | data_corrupted | 持久化表值损坏 | 从快照/checkpoint 恢复 |
| `XX000` | internal_error | 统计损坏、行形状不匹配 | 重试一次并上报查询 |
| `40000` | transaction_rollback | 事务/WAL 错误 | 重试或 ROLLBACK |
| `58030` | io_error | 文件/网络 IO 失败 | 无自动建议 |
| `0A000` | feature_not_supported | 本构建未实现 | 指向 `docs/specs/SQL.md` |

## 3. did-you-mean

`42P01` 与 `42703` 在有接近候选时，消息与 hint 会带
`did you mean \`status\`?`（大小写不敏感 Levenshtein，预算随名字长度增长、最多 3 个）。
候选来自：

- 列：`resolve_column` 调用点的 schema 列名；
- 表：`SqlEngine` 的已加载表清单。

## 4. 兼容规则

1. 已有 SQLSTATE 的语义不改变；新错误优先复用 PostgreSQL 标准码。
2. 新增码必须同时更新本文件与 `error.rs` 的 `code()`。
3. `hint` 允许增删（它是建议，不是契约）；`code` 是契约。
4. 客户端不应依赖消息文本，只依赖 `code` 与字段存在性。
