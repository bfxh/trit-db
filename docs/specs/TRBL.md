# TRBL 块格式（草案 v0）

状态：草案，**未冻结**；1.0 前允许 breaking。
实现：`crates/tritdb-format` 已落 v0（20 字节头 + CRC + canonical 校验），测试覆盖坏 magic/版本/radix/flags/长度/CRC/padding。
日期：2026-11-20

## 1. 目的

TRBL 是 `trit-db` 的原生块格式，只负责“一段三值序列 + 编码元数据 + 校验”。它不负责 manifest、事务、索引、SQL。

## 2. 块头（20 字节）

| 偏移 | 长度 | 字段 | 说明 |
|---|---|---|---|
| 0 | 4 | magic | `TRBL` |
| 4 | 1 | version | v0 = 0 |
| 5 | 1 | radix | 3 / 4 / 16 |
| 6 | 2 | flags | 保留，v0 = 0 |
| 8 | 4 | trit_count | 小端 u32 |
| 12 | 4 | payload_len | 小端 u32；不含块头 |
| 16 | 4 | crc32 | 小端 u32；覆盖块头 `0..16` + payload |
| 20 | … | payload | 打包后的字节 |

> 当前 `tritdb-page` 的页头是 `TRPG`，是**页容器**；TRBL 是**块格式**。两者不是同一个东西；M0 已分别落在 `tritdb-page` 和 `tritdb-format`。

## 3. radix 注册表

| radix | trits/byte | bit/trit | 规则 |
|---|---|---|---|
| 3 | 5 | 1.6 | base-3；字节值 0..=242；243..=255 非法 |
| 4 | 4 | 2.0 | 每 trit 2 bit；值 3 非法 |
| 16 | 2 | 4.0 | 每 trit 4 bit；值 >2 非法 |

## 4. 规范形式（canonical）

- 最后一个字节的未使用槽位必须为 0。
- 解码必须拒绝非规范 padding，避免同一序列有多种编码。
- 空序列：`trit_count=0`，`payload_len=0`。
- `payload_len` 必须等于 `radix.packed_len(trit_count)`。

## 5. 校验

- 块级 CRC32（IEEE）覆盖块头 `0..16` + payload。
- CRC 只做 corruption detection，不做安全；完整性用 BLAKE3（后续 manifest/chunk 层）。
- 坏 magic/版本/radix/长度/CRC 必须结构化报错，不 panic。

## 6. 未做

- manifest / chunk / snapshot / 列统计 / zone map。
- 压缩（zstd/lz4）与编码级联。
- 外部格式（Parquet/Arrow/Iceberg）映射。

## 7. 判红

- 任意块 encode→decode 必须逐位一致。
- 非法输入只返回 `Ok/Err`，不 panic。
- 非规范 padding 必须拒绝。
- 没有二进制基线的性能声明判红。
