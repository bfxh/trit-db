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
```

## 理由

- **零依赖**：SHA-256 是标准算法，实现 + NIST 向量测试即可；不为一层 CAS 引入 `blake3`/`sha2`。
- **不可变审计**：manifest 按内容寻址，改写就会换哈希；parent 链天然是审计链。
- **可与现有 WAL 解耦**：CAS 层不知道 SQL/页格式，只存字节；后续 manifest 可以指向表快照、块文件或备份。

## 后果 / 限制

- **没有 GC**：删除 manifest 不会回收无引用 chunk；需要后续 `gc`。
- **没有签名**：哈希链只能证明「没被改写」，不能证明「谁写的」；链头需要外部锚点。
- **没有只读副本/部分拉取**：`ChunkStore` 只做本地读写。
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
