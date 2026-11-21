# ADR 044：破坏性 schema 变更（`DROP COLUMN` / `RENAME COLUMN`）

状态：**Proposed**

日期：2026-11-21

## 背景

`ALTER TABLE` 之前只支持 `ADD COLUMN`；缺少删除列与重命名列，schema 迁移只能重建表。

## 决策

1. **语法**
   ```sql
   ALTER TABLE t DROP [COLUMN] [IF EXISTS] c;
   ALTER TABLE t RENAME [COLUMN] a TO b;
   ALTER TABLE t RENAME TO u;
   ALTER TABLE t ALTER [COLUMN] c TYPE TEXT;
   ```
   - `DROP` / `IF` / `EXISTS` 复用关键字；`COLUMN` / `RENAME` / `TO` 是普通词。
   - `DROP` 不允许删除最后一列。

2. **执行**
   - `DROP COLUMN`：按索引删除 `schema.columns` 与每行对应值；表级 `UNIQUE` 组里删除该索引，并把大于它的索引减 1；空的 `UNIQUE` 组丢弃。允许删除被 `UNIQUE` 引用的列。
   - `RENAME COLUMN`：改列名；目标名已存在报 `DuplicateColumn`；表级 `UNIQUE` 索引不变。
   - `RENAME TO`：删除旧表 key 与统计 key，写入新表 key（同一事务），更新内存中的表目录。
   - `ALTER COLUMN c TYPE t`：按列逐行显式转换（`INT`↔`TEXT`、`BOOL`↔`INT`/`TEXT`），再改列类型并重写整表。
   - 以上都在单事务中重写整表（`put_table` / 自定义事务），成功后使该表统计失效。

3. **限制**
   - `CHECK` 文本原样保留：如果它引用了被删列，后续 `INSERT`/`UPDATE` 求值时会报 `UnknownColumn`；需要用户自行先删约束（当前没有 `DROP CONSTRAINT`）。
   - `ALTER COLUMN TYPE` 只覆盖 `INT` / `TEXT` / `BOOL` 之间的显式转换；注解类型走严格 `coerce`，不兼容时报错。
   - `DROP`/`RENAME` 不重写已有 `TRSQ` 版本号（v3），旧客户端解码会看到新 schema。

## 理由

- **单事务重写**与 `ADD COLUMN` 一致，保证 schema 与行数据原子更新。
- **索引调整**只处理表级 `UNIQUE`；列级 `UNIQUE`/`CHECK` 随列删除。
- **拒绝删除最后一列**避免零列表（当前引擎不支持）。

## 验收

```bash
cargo test -p tritdb --test sql_cli alter_table_drop_and_rename_column
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

- `DROP COLUMN b` 后 `SELECT a, c` 正常，重开后 schema 保留；
- `RENAME COLUMN a TO id` 后可查询 `id`；
- `RENAME TO u` 后旧名不可用、新名可查询，重开后保留；
- `ALTER COLUMN a TYPE TEXT` 与反向 `TYPE INT` 保持值；
- 删除最后一列被拒绝；
- 目标列名/表名重复被拒绝。
