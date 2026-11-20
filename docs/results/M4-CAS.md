# M4：内容寻址 chunk、manifest 哈希链与备份闭环

状态：本地 CAS、manifest 链、GC、只读副本 + 部分拉取、HMAC 锚点、SQL 快照/恢复、页文件快照/恢复均已实现；增量同步未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-cas/src/lib.rs     SHA-256 / HMAC / Hash / ChunkStore / Manifest / verify_chain
                                  gc / replicate / replicate_subset / Anchor
crates/tritdb/src/main.rs        cas put|get|has|gc；manifest commit|verify|show|replicate
                                  snapshot|restore|snapshot-pages|restore-pages；anchor write|verify
crates/tritdb/tests/cas_cli.rs   6 个 CLI 端到端测试
docs/ADR/039-content-addressed-manifest.md
```

## 2. 用法

```bash
tritdb cas put --store ./cas --file blob
tritdb cas get --store ./cas --hash <hex> --out copy
tritdb manifest commit --store ./cas --entry blob=<hex>
tritdb manifest verify --store ./cas --head <manifest-hex>
tritdb manifest show --store ./cas --hash <manifest-hex>
tritdb cas gc --store ./cas --root <manifest-hex>
tritdb manifest replicate --from ./cas --to ./replica --head <manifest-hex> --entry blob
tritdb manifest snapshot --store ./cas --db sql.db --parent <manifest-hex>
tritdb manifest restore --store ./cas --head <manifest-hex> --db restored.db
tritdb manifest snapshot-pages --store ./cas --data pstore.pages --parent <manifest-hex>
tritdb manifest restore-pages --store ./cas --head <manifest-hex> --out restored.pages
tritdb anchor write --out ./head.anchor --head <manifest-hex> --key-file ./secret
tritdb anchor verify --file ./head.anchor --store ./cas --head <manifest-hex> --key-file ./secret
```

## 3. 语义

- chunk 地址是 SHA-256，`get` 时重新哈希校验；
- manifest 编码 `TRMF`，按名字排序，`parent` 是全零时表示根；
- `verify_chain` 沿 parent 走到根，返回 `head/root/length`；
- 篡改任意 manifest 字节 → `CorruptHash`；
- `replicate_subset`（CLI `--entry`）只复制指定 entry，manifest 链完整复制，目标 `verify_chain` 仍通过；
- `Anchor` 记录链头与创建时间，`--key-file` 给出 HMAC-SHA256 签名；锚点文件放在 CAS 目录之外；
- `snapshot-pages` 把页文件按 8 KiB 页切成 chunk（`page/<index>`），`layout` entry 记录 `TRPF` 版本、页大小与页数；`restore-pages` 先写 `.tmp` 再 rename，缺 chunk 时不留半成品文件。

## 4. 验收

```bash
cargo test -p tritdb-cas            # 9 passed
cargo test -p tritdb --test cas_cli # 6 passed
```

## 5. 还没做

- 增量同步：部分拉取按 entry 过滤，但仍遍历整条 manifest 链；没有「只传差异」的协商；
- 公开密钥签名：锚点是 HMAC 共享密钥，不是可公开验证的作者签名；
- 可信时间戳与链头透明日志：`created_ms` 与锚点位置由调用方保管。
