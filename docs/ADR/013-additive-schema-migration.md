# ADR-013：加法 schema 迁移（ALTER TABLE ADD COLUMN）

状态：已实现（Accepted）
日期：2026-11-20

## 背景

`tritdb-sql` 目前只有 `CREATE TABLE` / `DROP TABLE`。表以 `TRSQ` 整表 value 存进 `TxnStore`，schema 和所有行一起编码在同一个 value 里；统计以独立的 `TRSS` value 存在 `sql/stats/` 下。这个布局没有 schema 版本号，也没有迁移脚本或迁移历史。

用户提出“迁移安全”后，本切片先处理最小、风险最低的一类：给已有表追加一个可空列。没有 `ALTER TABLE` 时，使用者只能导出数据、重建表、再导入；手工重写会同时暴露三个风险：

- 写一半失败留下新旧 schema 混杂的表；
- 读取旧行时按新列数解码，导致行/schema 不一致；
- 迁移后旧统计仍被 `EXPLAIN` 使用，估算与实际 schema 脱节。

本 ADR 定义一条保守的加法迁移路径，不引入通用迁移框架、schema 版本表或破坏性 DDL。

## 决策

1. 语法只支持单个加法动作：

   ```sql
   ALTER TABLE name ADD [COLUMN] [IF NOT EXISTS] column type;
   ```

   `COLUMN` 是可选的普通词，不设为保留字；`IF NOT EXISTS` 位于 `ADD [COLUMN]` 之后、列名之前。一次语句只能追加一列，不支持逗号分隔的多个动作。
2. 只允许追加**可空**列。新列一律以 `not_null = false` 写入 schema；`NOT NULL` 直接返回 `unsupported SQL`，即使表为空也拒绝。原因是没有 `DEFAULT` 机制，无法为旧行生成合法非空值；统一拒绝可以避免“空表能加、有数据不能加”的两套语义。
3. 执行前先完成全部校验，再触碰表：
   - 表不存在：`unknown table`；
   - 列已存在且未带 `IF NOT EXISTS`：`duplicate column`；
   - 列已存在且带 `IF NOT EXISTS`：返回 `column c already exists in table t`，不写表、不改统计；
   - 新列声明 `NOT NULL`：返回 `unsupported SQL`，不写表。
4. 迁移在内存中的表副本上完成：按原列顺序追加新 `Column`，再对每一行 `push(Value::Null)`。空表只改 schema，不产生行。新列因此总是位于 `SELECT *` 的最后一列。
5. 持久化复用现有 `put_table`：先 `encode_table` 把完整新表编码成功，再 `TxnStore::begin`，在同一事务里写表 key、删除 `sql/stats/<table>`、`commit`。只有 commit 成功返回后才替换内存中的 `self.tables` 和清空 `self.stats`。编码失败或事务在 commit 前失败不会改变旧表 value 或内存表；commit 返回错误时内存也不切换，但持久层可能已经完整提交，此时必须以重新 `open` 后的表 value 为准。无论哪条路径，都不会留下半张表或行/schema 不一致的表。
6. 迁移成功后统计必然失效；下一次 `EXPLAIN SELECT` 不再显示 `est rows`，直到重新 `ANALYZE`。这与 INSERT/UPDATE/DELETE 使统计失效的路径一致。
7. 不升级 `TRSQ` 容器版本。追加列只是逻辑 schema 变化，表编码本身已经按列数序列化；旧 value 解码后仍是旧 schema，迁移时整表重新编码为新 schema。这里做的是**数据重写**，不是文件格式版本迁移。
8. 明确不支持：`DROP COLUMN`、`RENAME COLUMN`、`ALTER COLUMN TYPE`、`ADD ... DEFAULT`、多列一次迁移、迁移历史表、自动回滚/降级。已提交的迁移不能靠本功能撤销；需要回退时只能使用迁移前备份，或未来另做显式的破坏性 DDL。
9. 迁移是单写者、整表重写：迁移期间持有当前 engine 的写路径，旧表和新表副本会同时占用内存；大表的耗时和 WAL 体积随行数线性增长。当前不提供在线迁移、分批回填或进度上报。

## 后果

- 正结果：旧行在新列上统一读为 `NULL`，`SELECT *` 的列顺序稳定；迁移后可以立即用包含新列的 `INSERT` 写入。
- 正结果：迁移后的 schema 和行数据会随表 value 一起持久化，关闭并重新 `open` 后仍可见。
- 正结果：失败路径可观测：重复列、`NOT NULL`、未知表都在写事务前被拒绝，旧表仍可查询；编码/事务错误也不会产生半张新表。
- 正结果：迁移和统计删除在同一事务内完成，不会出现“表已变、旧统计还在”的窗口。
- 负结果：整表重写是 O(行数) 的时间和 O(旧表+新表) 的内存；大表迁移会阻塞当前 engine 的其他写操作，且 WAL 需要容纳整张新表。
- 负结果：不能给旧行回填非空值，也没有 `DEFAULT`；所有旧行只能是 `NULL`。
- 负结果：迁移后统计为空，`EXPLAIN` 暂时失去估算，需要手动 `ANALYZE`。
- 负结果：没有迁移版本号或迁移记录，无法判断某张表经历过哪些迁移；也没有 downgrade/undo。
- 负结果：脚本仍按语句自动提交，不是一个大事务；迁移前的语句已经提交，迁移本身失败不会回滚它们，迁移成功后的后续语句失败也不会撤销迁移。需要整体原子性时必须在单条迁移语句内完成。
- 负结果：只支持追加列；列删除、改名、类型变更和约束变更仍需重建表或后续独立切片。

## 被否掉的替代

- **在旧表上原地修改行 value**：`TRSQ` 是整表单 value，无法安全地只改 schema 而不重写所有行；原地拼接还会绕过 `put_table` 的事务和统计失效合同。
- **先写临时表再替换原 key**：需要两个 key 和额外的 rename/清理步骤，故障窗口更大；当前单 key + 单事务提交已经足够保证“旧表或新表，二选一”。
- **给 `TRSQ` 加 schema 版本号并做格式迁移**：追加可空列不改变容器布局，版本号只会增加所有读写路径的兼容分支；等未来出现不兼容编码时再做格式迁移。
- **支持 `ADD COLUMN ... NOT NULL DEFAULT ...`**：需要补 `DEFAULT` 的 parser、binder、INSERT 省略列语义和旧行回填，范围远大于本切片；当前先拒绝。
- **支持 `DROP` / `RENAME` / `ALTER TYPE`**：这些操作可能丢失数据或要求全表重编码，且没有备份/回滚机制；当前不做，避免把“加法迁移”扩大成通用 DDL。
- **引入独立 migration runner 和版本表**：当前只有一个 schema 载体（表 value），没有多版本脚本需求；先闭合单条 SQL 迁移的原子性和可观测性。
