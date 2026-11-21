# ADR 047：三值列存块（TRSQ v6）

状态：**Proposed**（实现随本 ADR 落地）

日期：2026-11-22

## 背景

M10 已有可选的读路径（`Table::k3_column`），TRSQ v5 让 `LOGIC K3` / `LOGIC BELNAP` 列由段承担。
v6 把范围扩到全部注解列，并给段加分块；同时把 Belnap 从 2-trit 编码压到 2-bit。

## 决策

### 1. 版本与格式

`TRSQ` 表格式 = **v6**；段版本 = **3**。

段格式（`PACKED_VERSION = 3`）：

```
"K3PK" (4) | segment_version u8 (=3) | segment_count u32
每段: column_index u32 | kind u8 | row_count u32 | chunk_count u32
      | (chunk_len u32 | chunk bytes)* | null_len u32 | nulls
```

- `kind = 0`：`LOGIC K3`，`r3` 打包（1.6 bit/trit）；
- `kind = 1`：`LOGIC BELNAP`，**2 bit/值**（4 值/字节）+ NULL 位图；
- `kind = 2`：`LOGIC LUKASIEWICZ`，**10 bit/值**（千分位 0..=1000）+ NULL 位图；
- `kind = 3`：`LOGIC NAT`，**8 字节/值** + NULL 位图；
- packed 数据按 `PACKED_CHUNK = 4096` 字节切块，每块前写 `u32` 长度；
- 其他类型（`INT`/`TEXT`/`BOOL`）不打包，行内照旧。

### 2. 写路径

- v6 行循环**跳过**四类注解列；
- 段是它们的唯一来源（`K3Column` / `B4Column` / `LukasiewiczColumn` / `NatColumn`）；
- v5 编码（测试/兼容用）仍只跳过 K3/Belnap，且 Belnap 用旧的 **2-trit** 编码（`to_parts_2trit`）；
- v4 编码（测试/兼容用）行值完整 + 段版本 1；v3 编码无段。

### 3. 读路径与回退

- **v6（段版本 3）**：行内占位 `NULL`，段回填：
  - kind 0 → `K3Column::from_parts`；kind 1 → `B4Column::from_parts`（2-bit）；
  - kind 2 → `LukasiewiczColumn::from_parts`；kind 3 → `NatColumn::from_parts`；
  - 校验列类型、行数、长度、`NOT NULL`；chunk 按长度拼接后再交给 `from_parts`。
- **v5（段版本 2）**：Belnap 用 `B4Column::from_parts_2trit`（2-trit 旧格式）；
- **v4（段版本 1）**：行值完整，段用于逐字节校验；
- **v3**：无段，直接读行值；
- 结构性损坏（magic/版本/列索引/行数/长度/chunk 截断）→ `CorruptTable`，不静默回退。

### 4. 完整性的边界

段内容不做逐字节 CRC（v6 没有行值可对照），完整性依赖存储层：表是 `TxnStore` 的整表 value，
WAL 记录与页文件都有 CRC32。段外置/分片时再在段头加校验和。

### 5. 理由

- **空间**：K3 每行 2 字节 → ~0.2 字节；Belnap 2 字节 → 0.25 + 0.125 字节；
  Ł 3 字节 → 1.25 + 0.125；ℕ 9 字节 → 8 + 0.125；
- **Belnap 2-bit**：4 种状态正好 2 bit，`r3` 的 2-trit 方案浪费（9 种组合只用 4 种）；
- **分块**：长列的 packed 数据按 4 KiB 切块，读取时逐块拼接，为将来流式/分页读取留位置；
- **兼容**：v3/v4/v5 数据格式不变，读取器按版本分支。

## 后果

正面：

- 四类注解列都成为列存块，扫描不再逐行映射；
- v3/v4/v5 数据仍可读；
- 测试覆盖 v6 往返、空间对比、v5/v4/v3 兼容、分块、结构篡改。

负面 / 未做：

- ℕ 固定 8 字节/值（varint 可省小值）；
- 段没有外置/分片（仅段内分块）；
- 段内容完整性依赖存储层 CRC。

## 验收

```bash
cargo test -p tritdb-sql storage   # 15 passed
cargo test --workspace
```

测试覆盖：v6 含 K3/Belnap/Ł/ℕ 的表往返、v6 比 v5 小、v5/v4/v3 兼容、
10 000 行 ℕ 列的分块往返、篡改段 `row_count` → `CorruptTable`。
