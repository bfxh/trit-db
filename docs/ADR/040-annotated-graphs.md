# ADR 040：注解图（CSR 邻接表 + 三值可达 + 三值 k-NN）

状态：**Proposed**

日期：2026-11-21

## 背景

M3 的 `tritdb-exec::ann` 已经有 K3/Belnap 注解、精确模型计数和最小修复 CQA，但只有布尔表达式与事实集合，没有图/向量结构。M5 需要：

1. 一张能在内存里按行遍历的有向图（CSR 邻接表）；
2. 边带三值标注时，“从 `s` 能否到 `t`”要给出三值答案，而不是布尔答案；
3. 向量检索也要能携带三值标注，区分“确定最近”“可能最近”“确定排除”。

## 决策

新增 `tritdb-exec::graph` 模块（零依赖）：

1. **`Csr`**
   - `Csr::from_edges(nodes, edges)`：行内排序去重，`neighbors(v)` 返回有序切片，`has_edge` 二分。
   - 上限 `MAX_NODES = 4096`、`MAX_EDGES = 16384`；越界边/节点返回结构化 `GraphError`。
2. **`AnnotatedGraph`**
   - 边标签为 [`TruthValue`]（`TRUE`/`UNKNOWN`/`FALSE`），与 CSR 的 `targets` 平行存放。
   - `reach(from, to, max_hops)`：先只走 `TRUE` 边找最短路径 → `Certain`；否则走 `TRUE`/`UNKNOWN` → `Contingent`；都不存在 → `Impossible`。`FALSE` 边永不可走。
   - 返回值复用 `ann::RepairAnswer`，并带 `hops` 与最短见证路径（BFS，按跳数最少）。
   - `reachable_within(start, max_hops)`：BFS 返回 `(node, hops)`，每个节点第一次出现即最短跳数。
3. **`annotated_knn(points, query, k)`**
   - 整数坐标，平方欧氏距离用 `i128` 精确计算（不会出现浮点比较误差），溢出返回 `DistanceOverflow`。
   - 按距离升序、`id` 升序打破平局；结果按 `T/U/F` 分成 `certain` / `uncertain` / `excluded` 三组，每组最多 `k` 个。
4. **CLI**
   ```bash
   tritdb graph reach --nodes 4 --edges 0-1,1-2 [--label 1-2=U] --from 0 --to 2 [--max-hops 4]
   tritdb graph neighbors --nodes 4 --edges 0-1,1-2 --node 0 [--hops 1]
   tritdb graph knn --points "1,2;3,4" [--labels T,U] --query "1,2" [--k 3]
   ```

## 理由

- **复用 `RepairAnswer`**：可达性与 CQA 的“确定/可能/不可能”是同一个三值结构，不引入第二套枚举。
- **精确整数距离**：输出可逐字节断言，测试不依赖浮点格式化；真实向量场景再换 `f32` 也不影响接口。
- **CSR 而不是邻接矩阵**：稀疏图省内存，行有序让 `has_edge` 可以二分，也让标签数组与 `targets` 一一对应。
- **不发明显式索引**：`k` 组内暴力打分在 `MAX_NODES` 规模下足够，先把语义跑通。

## 后果 / 限制

- **纯内存、无持久化**：CSR 数组还没有写进页文件、WAL 或 manifest；每次 CLI 调用都从边列表重建。
- **没有近似索引**：`annotated_knn` 是精确扫描，不是 HNSW/IVF；维度上限 64，坐标是 `i64`。
- **边是单标签、无权重**：多重图、边权、节点标签都未支持。
- **没有递归 SQL 语法**：`graph` 目前只是 CLI/库 API，`WITH RECURSIVE` 之类尚未进 parser。
- 节点/边上限是硬编码常量，超出直接报错而不是降级。

## 验收

```bash
cargo test -p tritdb-exec graph
cargo test -p tritdb --test ann_cli
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

- CSR：重复边去重、行有序、越界边/节点报错；
- 三值可达：全 `TRUE` → `certain`、含 `UNKNOWN` → `contingent`、`FALSE` 断边 → `impossible`、`max_hops` 截断、零跳自反；
- BFS：一跳/两跳层与顺序；
- k-NN：距离排序、平局按 `id`、T/U/F 分组、维度不一致与 `i128` 溢出。
