# M10 第一刀：三值批量谓词与 as-of join

状态：`tritdb-exec::batch` 已提供批量谓词求值与 asof join 两个原语，与标量/暴力实现对拍通过。
SQL 行式求值路径未改动（见「切片边界」）。

日期：2026-11-21。

## 1. 切片边界（为什么只做两块原语）

M10 的完整目标是「向量化扫描 + asof join + 三值列存块」。整条链路要动 SQL 的
binder/planner/executor，风险大且很难在一步内判红。这一刀只做**执行层最底层的两块纯原语**：

1. **三值批量谓词**：把 `WHERE` 的布尔树（`NOT`/`AND`/`OR`）在 `TritVec`（`r3` 打包，5 trit/字节）
   上逐元素求值——这是「向量化谓词」的语义核；
2. **as-of join**：q 的 `aj`，时序分析的核心原语。

两者都不改现有行式 `eval`：批量路径是 opt-in 的，正确性由「对标量真值表 / 对暴力实现」
两条独立 oracle 保证。

## 2. API

```rust
// crates/tritdb-exec/src/batch.rs
pub enum TriPredicate { Atom(usize), Not(Box<TriPredicate>), And(Vec<TriPredicate>), Or(Vec<TriPredicate>) }

pub fn eval_predicate(predicate: &TriPredicate, atoms: &[TritVec]) -> Result<TritVec, BatchError>;
pub fn filter_indices(truth: &TritVec) -> Vec<usize>;
pub fn asof_join(left: &[i64], right: &[i64]) -> Result<Vec<Option<usize>>, BatchError>;
```

## 3. 语义

- **谓词**：`Atom(i)` 取第 `i` 个原子列；`Not/And/Or` 用 K3 真值表逐元素求值
  （`TritVec::neg/meet/join`）。`And`/`Or` 的空项列表报 `EmptyTerms`，Atom 越界报
  `AtomOutOfRange`，长度不一致报 `LengthMismatch`；
- **过滤**：`filter_indices` 只保留 `TRUE` 的行——`FALSE` 与 `UNKNOWN` 都丢弃，与 SQL
  `WHERE` 一致（`UNKNOWN` 不是「可能通过」）；
- **asof**：右表必须非降序（乱序报 `UnorderedRight`），每个左行取「最后一个
  `right <= left`」的下标；相等时间戳取最后一条（q 的 `aj` 约定）；无匹配为 `None`。

## 4. 对拍与规模

| 测试 | oracle | 规模 |
|---|---|---|
| `batch_predicates_match_the_scalar_truth_tables` | 逐行 K3 标量求值（同一真值表） | 300 组随机谓词（深度 ≤ 3、1–3 个原子、1–60 行） |
| `asof_matches_bruteforce` | 暴力扫描 | 200 组随机左右表 |
| `asof_handles_edges` | 手工期望 | 空表、无匹配、相等时间、乱序拒绝 |
| `asof_scales_to_100k_rows` | 前缀暴力对拍 | 左右各 10 万行 |

性能说明：批量路径复用 `r3` 打包（≈1.6 bit/trit）；这一刀不做严格的计时断言（避免 CI flaky），
规模测试保证算法是 O(n log m) 而不是 O(n·m)。

## 5. 接进 SQL（已完成 SELECT WHERE）

`execute_select_inner` 现在先尝试 `batch_filter_indices`：

- 只接受 `AND`/`OR`/`NOT` 与 `LOGIC K3` / `LOGIC BELNAP` 列；比较、算术、聚合、子查询回退行式；
- Belnap 的 `BOTH`/`NEITHER` 与 `NULL` 一律映射为 `UNKNOWN`（不通过 `WHERE`，与 `truth_of` 一致）；
- debug 构建每次命中都会再用行式 `passes_filter` 算一遍并 `debug_assert_eq!` 对拍；
- 集成测试 `batch_where_matches_row_path_on_annotation_columns` 覆盖 K3/Belnap 的 AND/OR/NOT
  组合与混合谓词回退；
- `DELETE` 与 `UPDATE` 的 `WHERE` 复用同一 `batch_filter_indices`（同一 debug 对拍），
  集成测试 `batch_where_drives_update_and_delete` 覆盖更新/删除与混合谓词回退；
- `ASOF JOIN` 语法已接入（`ON <left>.k = <right>.k [AND ...] AND <left>.ts >= <right>.ts`）：
  等值键分组 + 单 `INT` 时间键、右表按组稳定排序、`NULL` 键/时间不匹配、左外语义；
  语义与 SQLSTATE 见 `docs/ADR/046-asof-join.md`。

### 5.1 K3 列扫描向量化（可选读路径）

`tritdb-exec::batch::K3Column` 与 `Table::k3_column(index)`：

- 把 `LOGIC K3` / `LOGIC BELNAP` 列从行值物化成 `TritVec`（`r3` 打包）；
- **只读路径**：存储仍是行式 `Value`，写路径不变；非注解列返回 `None`；
- `batch_filter_indices` 现在用 `K3Column::from_rows` 构建原子列（一次物化 + 打包），
  `NULL` / `BOTH` / `NEITHER` 映射 `UNKNOWN`（与 `WHERE` 语义一致）；
- 测试：`k3_column_round_trips_row_values`（往返 / 过滤 / 越界）、
  `table_k3_column_only_for_annotation_columns`（类型校验）；批量 WHERE 的 debug 对拍继续覆盖。

### 5.2 三值列存块（TRSQ v6）

表格式 v6：行内**跳过** `LOGIC K3` / `LOGIC BELNAP` / `LOGIC LUKASIEWICZ` / `LOGIC NAT`，
值由段承担。

- K3：`r3` 打包（1.6 bit/trit）；Belnap：**2 bit/值**（4 值/字节）+ NULL 位图；
  Ł：**10 bit/值**（千分位）+ 位图；ℕ：8 字节/值 + 位图；
- 段格式（版本 3）：`K3PK` + 段版本 + 段数 + 每段
  `column_index / kind / row_count / chunk_count / (chunk_len / chunk)* / null_len / nulls`；
  packed 数据按 4096 字节分块；
- 读路径：v6 从段回填并校验（列类型、行数、长度、NOT NULL）；v5（Belnap 2-trit）、
  v4（行值 + 段校验）、v3（无段）兼容；结构性损坏报 `CorruptTable`；
- 段内容不做逐字节 CRC，完整性依赖存储层（WAL/页 CRC）；
- 语义见 `docs/ADR/047-k3-column-blocks.md`。

### 5.3 还没做

- **ℕ 的变长编码**：当前固定 8 字节/值，varint 可省小值；
- **段外置/分片**：当前每列一段、段内分块。

## 6. 验收

```bash
cargo test -p tritdb-exec batch    # 6 passed
cargo test --workspace             # 496 passed
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```
