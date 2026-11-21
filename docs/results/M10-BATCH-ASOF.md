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
  组合与混合谓词回退。

### 5.1 还没做

- **ASOF JOIN 语法**：还没有 `ASOF JOIN` 关键字与 binder（`asof_join` 目前是库 API）；
- **三值列存块**：`TritVec` 还没作为表的物理列格式（目前是内存向量）；
- **UPDATE/DELETE 的 WHERE**：目前只接了 SELECT（DELETE 的过滤可复用同一函数）。

## 6. 验收

```bash
cargo test -p tritdb-exec batch    # 6 passed
cargo test --workspace             # 496 passed
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```
