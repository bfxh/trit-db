# ADR 047：三值列存块（TRSQ v5）

状态：**Proposed**（实现随本 ADR 落地）

日期：2026-11-22

## 背景

M10 已有可选的读路径：`Table::k3_column(index)` 把 `LOGIC K3` / `LOGIC BELNAP` 列物化成
`TritVec`（`r3`，5 trit/字节）。TRSQ v4 已在行数据之后追加「K3 打包段」，但第一版段是
**冗余**的（行值仍在），不省空间。v5 让段成为注解列的唯一来源。

## 决策

### 1. 版本与格式

`TRSQ` 表格式升到 **v5**；v5 = 行数据（跳过注解列的值）+ 段。

段格式（`PACKED_VERSION = 2`）：

```
"K3PK" (4) | segment_version u8 (=2) | segment_count u32
每段: column_index u32 | kind u8 (0=K3, 1=Belnap) | row_count u32
      | packed_len u32 | null_len u32 | packed bytes | nulls (仅 Belnap)
```

- `LOGIC K3` 列：`r3` 打包（1.6 bit/trit），`null_len = 0`；
- `LOGIC BELNAP` 列：**2 trit/值**（`BOTH`/`NEITHER` 需要四种状态，一个 trit 不够）+
  **NULL 位图**（1 bit/行）；
- 其他类型（`INT`/`TEXT`/`BOOL`/`Ł`/`ℕ`）不打包，行内照旧。

### 2. 写路径

- v5 行循环**跳过** `LOGIC K3` / `LOGIC BELNAP` 列（不写行值）；
- 段成为这两类列的唯一来源：K3 用 `K3Column::from_rows`，Belnap 用 `B4Column::from_rows`；
- 没有注解列时写 `segment_count = 0`。

### 3. 读路径与回退

- **v5**：行循环跳过注解列（占位 `NULL`），段解析后回填：
  - `kind = 0` → `K3Column::from_parts` → `to_values`（`Value::Bool` / `Value::Null`）；
  - `kind = 1` → `B4Column::from_parts` → `to_values`（`Value::Logic` / `Value::Null`）；
  - 校验：列索引存在、`kind` 与列类型一致、`row_count` 与行数一致、`packed_len`/`null_len`
    与列长度一致、`NOT NULL` 列没有 `NULL`；
  - 段版本 1 仍可读（v4 数据：行值完整，段用于逐字节校验）。
- **v4**：行值完整，段版本 1，校验「段内容与行值一致」；
- **v3**：没有段，直接读行值（回退）；
- 结构性损坏（magic / 段版本 / 列索引 / 行数 / 长度）→ `CorruptTable`，不静默回退。

### 4. 完整性的边界

v5 的段没有行值可以对照，**段内容本身不做逐字节校验**——内容完整性由存储层承担：
表是 `TxnStore` 的整表 value，WAL 记录与页文件都有 CRC32。段内再加 CRC 是重复保护；
如果未来段被外置/分片，再在段头加校验和。

### 5. 理由

- **空间**：K3 列从每行 2 字节（tag + 1）降到约 0.2 字节；Belnap 从 2 字节降到
  0.4 字节 + 0.125 字节位图；
- **Belnap 必须 2 trit**：`BOTH`/`NEITHER` 不能进 K3，v4 不打包 Belnap 正是因为这一点；
- **回退简单**：v3/v4 数据格式不变，读取器按版本分支。

## 后果

正面：

- 注解列真正变成列存块，`K3Column` / `B4Column` 的物化成本从「每次扫描」变成「解码一次」；
- v3/v4 数据仍可读；
- 测试覆盖 v5 往返、空间对比、v4 兼容、v3 回退、结构篡改。

负面 / 未做：

- Belnap 的 2-trit 编码只用 4/9 种组合（可压缩，「4 trit/5 值」之类留待后续）；
- 没有段级压缩/分块；
- 段内容完整性依赖存储层 CRC（见 §4）。

## 验收

```bash
cargo test -p tritdb-sql storage   # 12 passed
cargo test --workspace
```

测试覆盖：

1. v5 含 `LOGIC K3` 列表往返；
2. v5 含 `LOGIC BELNAP` 列（`BOTH`/`NEITHER`/`NULL`）往返；
3. v5 编码比 v4 小（100 行，K3 列）；
4. v4 数据（行值 + 段版本 1）仍能解码；
5. v3 数据（无段）仍能解码；
6. 篡改段 `row_count` → `CorruptTable`；
7. 无注解列时 0 段。
