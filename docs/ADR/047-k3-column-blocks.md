# ADR 047：三值列存块（TRSQ v4 的 K3 打包段）

状态：**Proposed**（实现随本 ADR 落地）

日期：2026-11-22

## 背景

M10 已经提供了可选的读路径：`Table::k3_column(index)` 把 `LOGIC K3` / `LOGIC BELNAP` 列
物化成 `TritVec`（`r3` 打包，5 trit/字节），供批量谓词使用。但存储仍是行式 `Value`，
每次扫描都要逐行映射。要把 `TritVec` 变成列存块，需要先定存储格式与回退规则。

## 决策

### 1. 版本

`TRSQ` 表格式从 v3 升到 **v4**。v4 = v3 的行结构 + 可选的「K3 打包段」。

### 2. 段格式

在行数据之后追加：

```
"K3PK" (4) | segment_version u8 (=1) | segment_count u32
每个段: column_index u32 | row_count u32 | packed_len u32 | packed bytes (r3)
```

- 只对 `LOGIC K3` 列（`DataType::Logic`）打包；
- `LOGIC BELNAP` 不打包：`BOTH`/`NEITHER` 无法无损映射到 K3，避免信息丢失；
- `LOGIC LUKASIEWICZ` / `LOGIC NAT` 不打包：不是三值。

### 3. 写路径

- 行数据保持 v3 编码（逐行逐列 `encode_value`），不跳过 K3 列的值；
- 段对每个 `LOGIC K3` 列写 `K3Column::from_rows` 的打包字节；
- 没有 K3 列时写 `segment_count = 0`（段头仍写，便于将来扩展）。

**第一版段是附加冗余**（行值仍在），目的是把格式位置、严格校验与回退路径钉死；
「行内跳过 K3 列、由段替代」的省空间版本留到 v5。

### 4. 读路径与回退

- v4：解析段并**严格校验**——magic、段版本、列索引存在且为 `LOGIC K3`、
  `row_count` 与行数一致、`packed_len` 与实际一致、段内容与 `K3Column::from_rows` 的结果
  逐字节相等、段后无多余字节；
- v3：没有段，直接从行值读取（回退）；
- v4 段损坏 → `CorruptTable`（**不**静默回退到行值：静默回退会掩盖损坏）。

### 5. 理由

- **先格式、后省空间**：段内容由行值可重算，所以第一版可以零风险地验证编码/解码/校验路径；
- **只打包 K3**：Belnap 的 `BOTH`/`NEITHER` 无法无损进 K3，混入会破坏语义；
- **严格校验 + 不静默回退**：与项目「错误必须可判定」的原则一致。

## 后果

正面：

- TRSQ v4 有了列存块的位置，`K3Column` 从「扫描时物化」升级为「格式内建」；
- v3 数据可读（无段回退）；
- 段损坏、列类型不符、行数不符都有明确错误。

负面 / 未做：

- 段是冗余的，**不省空间**（省空间需要 v5：行内跳过 K3 列、段替代）；
- Belnap/Ł/ℕ 列不打包；
- 没有压缩（`r3` 已是 1.6 bit/trit，进一步压缩留待后续）。

## 验收

```bash
cargo test -p tritdb-sql storage   # v4 往返 / 0 段 / 段损坏 / v3 回退
cargo test --workspace
```

测试覆盖：

1. 含 `LOGIC K3` 列的表 v4 往返，行值与打包段都在；
2. 无 K3 列的表写 0 段；
3. 篡改段字节 → `CorruptTable`；
4. 把 v4 字节的版本改回 3 并截掉段 → 解码成功（回退）。
