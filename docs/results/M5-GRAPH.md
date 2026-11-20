# M5 第一刀：注解图（CSR + 三值可达 + 三值 k-NN）

状态：内存内 CSR、三值可达、BFS 层、三值 k-NN 与 CLI 已实现；持久化、近似索引、递归 SQL 未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-exec/src/graph.rs   Csr / AnnotatedGraph / Reach / annotated_knn / GraphError
crates/tritdb/src/main.rs         graph reach|neighbors|knn
crates/tritdb/tests/ann_cli.rs    2 个 CLI 端到端测试
docs/ADR/040-annotated-graphs.md
```

## 2. 用法

```bash
tritdb graph reach --nodes 4 --edges 0-1,1-2 [--label 1-2=U] --from 0 --to 2 [--max-hops 4]
tritdb graph neighbors --nodes 4 --edges 0-1,1-2 --node 0 [--hops 1]
tritdb graph knn --points "1,2;3,4" [--labels T,U] --query "1,2" [--k 3]
```

- `--label from-to=T|U|F`（可重复），缺省 `T`；
- `--edges` 省略时可用 `--nodes` 单独建空图；
- `graph knn` 的点 id 按 `--points` 顺序从 0 分配，`--labels` 与之一一对应。

## 3. 语义

- CSR 每行排序去重，`has_edge` 二分；重复边以最后一次 `--label` 为准。
- 可达性三值：
  - 存在全 `TRUE` 路径 → `answer=certain`；
  - 否则存在只含 `TRUE`/`UNKNOWN` 的路径 → `answer=contingent`；
  - 否则 → `answer=impossible`（`hops=-`、`path=-`）。
  - 见证路径是跳数最少的那条；`--max-hops` 是硬上界。
- `neighbors` 走 `TRUE`/`UNKNOWN` 边，返回 BFS 顺序的 `(node, hops)`，每个节点只出现一次。
- k-NN 用整数平方欧氏距离（`i128`），按距离、`id` 升序排序，再按标注分到 `certain` / `uncertain` / `excluded` 三组，每组最多 `k` 个。

## 4. 验收

```bash
cargo test -p tritdb-exec graph        # 8 passed
cargo test -p tritdb --test ann_cli    # 4 passed
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

覆盖：CSR 去重/排序/越界、`certain`/`contingent`/`impossible` 与见证路径、`max_hops` 与零跳自反、BFS 一跳/两跳、k-NN 分组/平局/维度不一致/`i128` 溢出。

## 5. 还没做

- **持久化**：CSR 数组、边标签、向量还没有写进页文件/WAL/manifest，每次 CLI 调用都从边列表重建；
- **近似索引**：`annotated_knn` 是精确扫描，没有 HNSW/IVF；
- **边权与多重图**：每条有向边只有一个 `TruthValue`，没有权重、没有平行边；
- **递归 SQL**：`WITH RECURSIVE`、图模式匹配尚未进 parser；
- 节点/边上限（4096 / 16384）是硬编码常量。
