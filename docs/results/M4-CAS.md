# M4 第一刀：内容寻址 chunk 与 manifest 哈希链

状态：本地 CAS + manifest + 链验证已实现；只读副本、部分拉取、GC、签名未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-cas/src/lib.rs     SHA-256 / Hash / ChunkStore / Manifest / verify_chain
crates/tritdb/src/main.rs        cas put|get|has；manifest commit|verify|show
crates/tritdb/tests/cas_cli.rs   2 个 CLI 端到端测试
docs/ADR/039-content-addressed-manifest.md
```

## 2. 用法

```bash
tritdb cas put --store ./cas --file blob
tritdb cas get --store ./cas --hash <hex> --out copy
tritdb manifest commit --store ./cas --entry blob=<hex>
tritdb manifest verify --store ./cas --head <manifest-hex>
tritdb manifest show --store ./cas --hash <manifest-hex>
```

## 3. 语义

- chunk 地址是 SHA-256，`get` 时重新哈希校验；
- manifest 编码 `TRMF`，按名字排序，`parent` 是全零时表示根；
- `verify_chain` 沿 parent 走到根，返回 `head/root/length`；
- 篡改任意 manifest 字节 → `CorruptHash`。

## 4. 验收

```bash
cargo test -p tritdb-cas          # 4 passed
cargo test -p tritdb --test cas_cli  # 2 passed
```

## 5. 还没做

- chunk GC / 引用计数；
- manifest 签名与外部链头锚点；
- 只读副本、部分拉取、增量同步；
- 把 SQL 表快照写进 manifest（当前只存任意字节）。
