# trit-db 基准基线

由 `tools/bench/promote_baseline.py` 从 **5 次**同 commit 运行聚合；CI 用 `tritdb-bench check` 与它比较。

- 平台：`windows-x86_64`
- 套件：`ci`（小规模、秒级；不代表性能上限，只用于捕获数量级回归）
- 基线 commit：`3e90ba0`
- 口径：每项 `median_ms` 取 5 次运行 median 的中位数；`min_ms` 取最小值，`max_ms` 取最大值

| case | 参数 | min (ms) | median (ms) | max (ms) |
|---|---|---:|---:|---:|
| `sql_scan` | `sql-scan --rows 512 --rounds 3` | 0.0720 | 0.0920 | 0.1140 |
| `sql_point` | `sql-point --rows 512 --lookups 64` | 0.0930 | 0.1010 | 0.2980 |
| `sql_group` | `sql-group --rows 512 --groups 16 --rounds 3` | 0.1180 | 0.1650 | 0.1820 |
| `sql_sort` | `sql-sort --rows 512 --limit 16 --rounds 3` | 0.1850 | 0.2260 | 0.2410 |
| `sql_insert` | `sql-insert --rows 128 --rounds 2` | 1.0570 | 2.6940 | 3.7120 |
| `sql_update` | `sql-update --rows 512 --cutoff 64 --rounds 2` | 0.9050 | 4.0340 | 8.7060 |
| `sql_delete` | `sql-delete --rows 512 --cutoff 64 --rounds 2` | 0.8770 | 3.2780 | 21.1720 |
| `sql_subquery` | `sql-subquery --rows 256 --rounds 2 --query in` | 0.3100 | 0.3220 | 0.3430 |
| `sql_window` | `sql-window --rows 512 --rounds 2 --query rank` | 0.2630 | 0.2910 | 0.3070 |
| `sql_join` | `sql-join --rows 512 --rounds 2 --strategy auto` | 0.3670 | 0.4310 | 0.5100 |
| `pstore_scan` | `pstore-scan --pages 64 --payload 1024 --rounds 3` | 1.0460 | 1.1340 | 1.1920 |
| `txn_put` | `txn-put --txns 64 --keys-per-txn 4 --rounds 1` | 175.0720 | 195.7740 | 243.8030 |
| `pgwire_roundtrip` | `pgwire --rows 128 --queries 32 --rounds 2` | 0.1171 | 0.1310 | 0.1444 |
| `ann_knn` | `ann-knn --points 1024 --dims 8 --k 10 --rounds 3` | 0.0440 | 0.0490 | 0.0580 |
| `graph_reach` | `graph-reach --nodes 256 --fanout 4 --hops 6 --rounds 3` | 0.0000 | 0.0010 | 0.0010 |

## 如何更新基线

```bash
# 在当前 commit 上跑 5 次，再把 5 次的中位数提升为基线
python tools/bench/run.py --repeat 5 --no-dashboard
python tools/bench/promote_baseline.py --aggregate 5
```

## CI 门限

```bash
cargo run -p tritdb-bench --release -- check \
  --baseline docs/results/bench-baseline.json \
  --tolerance 2.0 --min-ms 0.05
```

共享 runner 噪声大，CI 只拦“超过基线 3 倍”的数量级回归；本地可用更紧的 `--tolerance 0.5`。
