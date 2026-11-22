# trit-db 基准基线

由 `tools/bench/promote_baseline.py` 从 **5 次**同 commit 运行聚合；CI 用 `tritdb-bench check` 与它比较。

- 平台：`windows-x86_64`
- 套件：`ci`（小规模、秒级；不代表性能上限，只用于捕获数量级回归）
- 基线 commit：`3e90ba0`
- 测量口径：`1865f65` bench harness；全部 case 10 轮，`sql_insert` 每轮 reset，RSS 在 case 前后采集；生产代码仍为基线 commit
- 口径：每项 `median_ms` 取 5 次运行 median 的中位数；`min_ms` 取最小值，`max_ms` 取最大值
- 样本：`samples` 合并 5 次运行的全部逐轮值；`p50` / `p90` 由样本计算
- 内存：`rss_before_kb` / `rss_kb` 取 5 次运行的进程 RSS 中位数，`rss_delta_kb` 为对应增量

| case | 参数 | 样本数 | min (ms) | median (ms) | max (ms) | p50 (ms) | p90 (ms) | RSS 前→后 (MiB) | ΔRSS |
|---|---|---:|---:|---:|---:|---:|---:|---:|---:|
| `sql_scan` | `sql-scan --rows 512 --rounds 10` | 50 | 0.0880 | 0.1080 | 0.1290 | 0.1057 | 0.1180 | 9.8 → 10.8 | +1.1 MiB |
| `sql_point` | `sql-point --rows 512 --lookups 64` | 320 | 0.0570 | 0.0970 | 0.1500 | 0.0988 | 0.1125 | 9.8 → 10.9 | +1.1 MiB |
| `sql_group` | `sql-group --rows 512 --groups 16 --rounds 10` | 50 | 0.1450 | 0.1660 | 0.2370 | 0.1656 | 0.1818 | 9.8 → 10.9 | +1.1 MiB |
| `sql_sort` | `sql-sort --rows 512 --limit 16 --rounds 10` | 50 | 0.1810 | 0.2260 | 0.3580 | 0.2202 | 0.2583 | 9.8 → 10.9 | +1.0 MiB |
| `sql_insert` | `sql-insert --rows 128 --rounds 10 --reset-each-round` | 50 | 0.9250 | 2.4810 | 14.2040 | 2.4285 | 5.0887 | 9.9 → 11.1 | +1.3 MiB |
| `sql_update` | `sql-update --rows 512 --cutoff 64 --rounds 10` | 50 | 1.0180 | 2.8580 | 9.9650 | 2.9531 | 4.5598 | 9.8 → 11.2 | +1.3 MiB |
| `sql_delete` | `sql-delete --rows 512 --cutoff 64 --rounds 10` | 50 | 0.9540 | 2.8310 | 7.0990 | 2.5898 | 4.3868 | 9.9 → 11.2 | +1.4 MiB |
| `sql_subquery` | `sql-subquery --rows 256 --rounds 10 --query in` | 50 | 0.2680 | 0.2870 | 0.4940 | 0.2963 | 0.4141 | 9.9 → 11.2 | +1.4 MiB |
| `sql_window` | `sql-window --rows 512 --rounds 10 --query rank` | 50 | 0.2460 | 0.3170 | 0.5050 | 0.3135 | 0.3417 | 9.9 → 11.2 | +1.3 MiB |
| `sql_join` | `sql-join --rows 512 --rounds 10 --strategy auto` | 50 | 0.3330 | 0.3700 | 0.5290 | 0.3690 | 0.4264 | 9.9 → 11.2 | +1.3 MiB |
| `pstore_scan` | `pstore-scan --pages 64 --payload 1024 --rounds 10` | 50 | 0.9470 | 1.0430 | 1.9980 | 1.0570 | 1.1342 | 9.8 → 10.8 | +0.9 MiB |
| `txn_put` | `txn-put --txns 64 --keys-per-txn 4 --rounds 10` | 50 | 167.3100 | 218.9660 | 447.9010 | 222.3227 | 354.4347 | 9.8 → 10.5 | +0.6 MiB |
| `pgwire_roundtrip` | `pgwire --rows 128 --queries 32 --rounds 10` | 50 | 0.1045 | 0.1100 | 0.1453 | 0.1103 | 0.1205 | 9.8 → 11.5 | +1.7 MiB |
| `ann_knn` | `ann-knn --points 1024 --dims 8 --k 10 --rounds 10` | 50 | 0.0400 | 0.0450 | 0.0620 | 0.0451 | 0.0527 | 9.8 → 10.1 | +0.3 MiB |
| `graph_reach` | `graph-reach --nodes 256 --fanout 4 --hops 6 --rounds 10` | 50 | 0.0000 | 0.0000 | 0.0010 | 0.0004 | 0.0007 | 9.9 → 10.2 | +0.3 MiB |

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
