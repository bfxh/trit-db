# M6 第七刀：破坏性 schema（`DROP` / `RENAME COLUMN`）

状态：`ALTER TABLE ... DROP COLUMN` / `RENAME COLUMN` 已实现；`ALTER COLUMN TYPE`、`RENAME TABLE`、`DROP CONSTRAINT` 未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-sql/src/ast.rs      Statement::AlterTableDropColumn / AlterTableRenameColumn
crates/tritdb-sql/src/parser.rs   ALTER TABLE ... DROP [COLUMN] / RENAME [COLUMN]
crates/tritdb-sql/src/engine.rs   alter_table_drop_column / alter_table_rename_column
crates/tritdb/tests/sql_cli.rs    1 个端到端测试
docs/ADR/044-destructive-schema.md
```

## 2. 用法

```sql
ALTER TABLE t DROP COLUMN b;
ALTER TABLE t RENAME COLUMN a TO id;
```

## 3. 语义

- `DROP COLUMN`：删除列与每行对应值；表级 `UNIQUE` 组删除该索引并把后续索引减 1；空组丢弃；不允许删除最后一列。
- `RENAME COLUMN`：改列名；目标名重复报 `DuplicateColumn`；表级 `UNIQUE` 索引不变。
- 两者都在单事务中重写整表，成功后使统计失效。
- `CHECK` 文本原样保留；若引用被删列，后续 DML 会报 `UnknownColumn`。

## 4. 验收

```bash
cargo test -p tritdb --test sql_cli alter_table_drop_and_rename_column
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

覆盖：`DROP COLUMN b` 后查询 `a, c`、重开后 schema 保留；`RENAME COLUMN a TO id` 后可查询；删除最后一列被拒绝。

## 5. 还没做

- **`ALTER COLUMN TYPE`**：类型变更；
- **`RENAME TABLE`** / 列顺序调整；
- **`DROP CONSTRAINT`**：`CHECK` / `UNIQUE` 约束删除；
- 删除列后旧 `CHECK` 文本可能引用不存在的列。
