# ADR 039：内容寻址 chunk + manifest 哈希链

状态：**Proposed**

日期：2026-11-21

## 背景

M2 的存储是页文件 + WAL；M4 需要「manifest + 内容寻址 chunk + 哈希链审计」这一层，才能谈只读副本、部分拉取和可验证备份。

## 决策

新增 `tritdb-cas` crate，零依赖（SHA-256 在 crate 内实现）：

1. **内容寻址 chunk**
   - `ChunkStore` 以 SHA-256 为地址，路径 `<root>/chunks/<前两位>/<其余>`。
   - `put` 先写临时文件再 rename；已存在直接去重。
   - `get` 读出后重新哈希，和请求地址不一致返回 `CorruptHash`。
2. **Manifest**
   - `Manifest { version, parent, created_ms, entries: BTreeMap<String, Hash> }`。
   - 编码 `TRMF`：版本、parent（32 字节，全零表示根）、时间戳、按名字排序的条目。
   - manifest 本身也是 chunk，`store` 返回它的哈希。
3. **哈希链审计**
   - `verify_chain(store, head)` 沿 `parent` 往回走，逐个校验 manifest 的内容哈希；缺失 parent 返回 `NotFound`，环返回 `Cycle`。
   - 篡改任何一个 manifest 字节，`ChunkStore::get` 的哈希校验就会失败。

CLI：

```bash
tritdb cas put --store dir --file blob
tritdb cas get --store dir --hash <hex> --out blob
tritdb manifest commit --store dir [--parent <hex>] --entry name=<hex>
tritdb manifest verify --store dir --head <hex>
tritdb manifest show --store dir --hash <hex>
tritdb cas gc --store dir --root <hex>
tritdb manifest replicate --from src --to dst --head <hex> [--entry name]
tritdb manifest snapshot --store dir --db sql.db [--parent <hex>]
tritdb manifest restore --store dir --head <hex> --db restored.db
tritdb manifest snapshot-pages --store dir --data file.pages [--parent <hex>]
tritdb manifest restore-pages --store dir --head <hex> --out file.pages
tritdb anchor write --out anchor.bin --head <hex> [--key-file secret]
tritdb anchor verify --file anchor.bin [--store dir] [--head <hex>] [--key-file secret]
```

## 理由

- **零依赖**：SHA-256 是标准算法，实现 + NIST 向量测试即可；不为一层 CAS 引入 `blake3`/`sha2`。
- **不可变审计**：manifest 按内容寻址，改写就会换哈希；parent 链天然是审计链。
- **可与现有 WAL 解耦**：CAS 层不知道 SQL/页格式，只存字节；后续 manifest 可以指向表快照、块文件或备份。

## 后果 / 限制

- **GC 是显式操作**：`gc(roots)` 只保留从给定 manifest 头可达的 chunk；调用方必须先确认 roots 完整。
- **锚点是共享密钥**：`Anchor` 记录链头与创建时间，用 HMAC-SHA256 签名；它证明「持有密钥的人签过这个链头」，不是公开密钥的作者身份。锚点文件必须存在 CAS 目录之外，否则攻击者可以连同数据一起替换。
- **只读副本是最小闭环**：`replicate` 复制 manifest 链与 entry chunk，复制后重新 `verify_chain`；目标目录由调用方按只读方式使用。`replicate_subset` / `--entry` 可以只拉指定 entry（部分拉取），增量同步未做。
- **两级快照**：`manifest snapshot --db` 备份 `TxnStore` 的 key/value；`manifest snapshot-pages --data` 按 8 KiB 页把 `pstore` 页文件切成 chunk（entry `page/<index>`，外加记录页大小与页数的 `layout`），`restore-pages` 重建页文件。页快照不做 WAL 重放，调用方应先 `pstore checkpoint`。
- SHA-256 而非 BLAKE3；替换只影响 `Hash` 实现。
- `created_ms` 只是本地时间，不是可信时间戳。

## 验收

```bash
cargo test -p tritdb-cas
cargo test -p tritdb --test cas_cli
```

- SHA-256 对 NIST 空串 / `abc` / 56 字节向量；
- chunk 去重、篡改检测；
- manifest 编解码往返、parent 链验证、缺失 parent；
- CLI 端到端 put → get → commit → verify；篡改 manifest 后 verify 非零退出。
