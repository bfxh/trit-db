# ADR-001：双引擎 + 单语义

状态：提议（Proposed）
日期：2026-11-20

## 背景

`trit-db` 要做“多进制列存 + 多值语义数据库”，不是纯三进制数据库。主库已经把三值的边界量过：

- 5 trit/字节 = 1.6 bit/trit，比双导轨 2.0 省 20%，不是数量级。
- 打包算术不划算（进位串行）；三值浮点误差 3×。
- tryte 查表在大行数才反超；GPU subgroup 比 SIMT 慢 4.5×。

所以必须把“三值赢的地方”和“二进制赢的地方”分开。

## 决策

1. **双引擎**：
   - 二进制行存：OLTP、热更新、高并发点写、WAL、MVCC。
   - 多进制列存：OLAP、冷数据、不确定数据、审计/时间旅行、三值向量/图。
2. **单语义**：上层一个 SQL + pgwire + MCP；语义默认 SQL 3VL，K3/Ł3/B3 作为 opt-in 注解域。
3. **不做**：纯三进制 DB、全库 trit、替代 PostgreSQL。
4. **依存方向**：`tritdb` → `tritdb-sql/proto` → `tritdb-plan` → `tritdb-exec` → `tritdb-txn` → `tritdb-index` → `tritdb-page` → `tritdb-radix`；禁止回边。

## 后果

- 正结果：三值列/位图/精确计数/审计/向量有独立赢面；二进制路径不背三值算术的包袱。
- 负结果：两套引擎要两套测试、两套读数、两套恢复路径；复杂度上升。
- 必须先做 M0（radix + page + scan），再谈 SQL。

## 被否掉的替代

- 纯三进制 DB：算术输、生态零、硬件无红利。
- 全库 trit：文本/JSON/金额/排序键都被三值拖累。
- 直接套 PostgreSQL：拿不到多值语义/审计/向量赢面，只是另一个 PG。

## 开放问题

- 二进制行存用现成的（SQLite/DuckDB/Redb）还是自研？M2 前给结论。
- 多进制列存的 manifest 用 Iceberg v2 子集还是自研 TRBL？M1 前给结论。
- K3/Ł3/B3 的注解域是否需要独立的 `tritdb-annot` crate？M3 前给结论。
