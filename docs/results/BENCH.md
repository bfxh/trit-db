# trit-db 基准基线

由 `tritdb-bench record` 生成；CI 用 `tritdb-bench check` 与它比较。

- 平台：`windows-x86_64`
- 套件：`ci`（小规模、秒级；不代表性能上限，只用于捕获数量级回归）
- 指标：各基准 `*_ms` 的 min / median / max

| case | 参数 | min (ms) | median (ms) | max (ms) |
|---|---|---:|---:|---:|
| `sql_scan` | `sql-scan --rows 512 --rounds 3` | 0.0920 | 0.1030 | 0.1380 |
| `sql_point` | `sql-point --rows 512 --lookups 64` | 0.1210 | 0.1480 | 0.1830 |
| `sql_group` | `sql-group --rows 512 --groups 16 --rounds 3` | 0.1780 | 0.1800 | 0.1810 |
| `sql_sort` | `sql-sort --rows 512 --limit 16 --rounds 3` | 0.3050 | 0.3070 | 0.3280 |
| `sql_insert` | `sql-insert --rows 128 --rounds 2` | 2.5330 | 2.5790 | 2.6260 |
| `sql_update` | `sql-update --rows 512 --cutoff 64 --rounds 2` | 3.5900 | 3.7490 | 3.9080 |
| `sql_delete` | `sql-delete --rows 512 --cutoff 64 --rounds 2` | 2.6260 | 2.6340 | 2.6420 |
| `sql_subquery` | `sql-subquery --rows 256 --rounds 2 --query in` | 0.4590 | 0.4640 | 0.4680 |
| `sql_window` | `sql-window --rows 512 --rounds 2 --query rank` | 0.3790 | 0.4110 | 0.4430 |
| `sql_join` | `sql-join --rows 512 --rounds 2 --strategy auto` | 0.4610 | 0.5550 | 0.6490 |
| `pstore_scan` | `pstore-scan --pages 64 --payload 1024 --rounds 3` | 1.2510 | 1.3330 | 1.3790 |
| `txn_put` | `txn-put --txns 64 --keys-per-txn 4 --rounds 1` | 207.5360 | 207.5360 | 207.5360 |
| `pgwire_roundtrip` | `pgwire --rows 128 --queries 32 --rounds 2` | 0.1528 | 0.1603 | 0.1678 |
| `ann_knn` | `ann-knn --points 1024 --dims 8 --k 10 --rounds 3` | 0.0650 | 0.0680 | 0.0740 |
| `graph_reach` | `graph-reach --nodes 256 --fanout 4 --hops 6 --rounds 3` | 0.0010 | 0.0010 | 0.0020 |

## 如何更新基线

```bash
cargo run -p tritdb-bench --release -- record \
  --out docs/results/bench-baseline.json \
  --markdown docs/results/BENCH.md
```

## CI 门限

```bash
cargo run -p tritdb-bench --release -- check \
  --baseline docs/results/bench-baseline.json \
  --tolerance 2.0 --min-ms 0.05
```

共享 runner 噪声大，CI 只拦“超过基线 3 倍”的数量级回归；本地可用更紧的 `--tolerance 0.5`。
