# M7 第一刀：最小 PostgreSQL wire protocol

状态：startup / simple Query / 扩展查询（Parse/Bind/Describe/Execute/Close）/ Terminate / Sync / 事务状态映射 / `TCP_NODELAY` / TLS（rustls，含 mTLS、热轮换、指纹吊销）/ trust、cleartext、MD5、SCRAM-SHA-256（含持久化 verifier）认证、COPY（text/csv/binary + 自定义选项）、二进制结果与参数（含 numeric/date/timestamp/uuid）、CRL/预取 OCSP/在线 OCSP（http/https + nonce）吊销已实现；OCSP 证书链与吊销日期校验未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-pgwire/Cargo.toml
crates/tritdb-pgwire/src/lib.rs   serve / 协议编解码 / 类型文本化
                                  Parse/Bind/Describe/Execute/Close / $n 参数替换
crates/tritdb/src/main.rs         serve --db ... [--listen ...]
crates/tritdb/tests/pgwire.rs            5 个端到端测试
crates/tritdb/tests/pgwire_protocol.rs  48 个协议/认证/TLS 测试
docs/ADR/045-pgwire.md
```

## 2. 用法

```bash
cargo run -p tritdb -- serve --db sql.db --listen 127.0.0.1:5432
# 另一个终端
psql "host=127.0.0.1 port=5432 dbname=sql" -c "SELECT id, name FROM t;"
```

## 3. 协议子集

- StartupMessage v3（196608）→ `AuthenticationOk` + `ParameterStatus` + `BackendKeyData` + `ReadyForQuery`。
- `SSLRequest`（80877103）→ 配置证书时回 `S` 并做 TLS 握手；否则回 `N`，客户端用明文重试。
- 认证：默认 `AuthenticationOk`（trust）；`--auth password` 发 `AuthenticationCleartextPassword`（3）；`--auth md5` 发 `AuthenticationMD5Password`（5）+ 4 字节 salt；`--auth scram-sha-256` 走 `AuthenticationSASL`（10）→ `SASLContinue`（11）→ `SASLFinal`（12）→ `AuthenticationOk`；`--scram-verifier-file` 直接加载 `SCRAM-SHA-256$<iterations>:<salt>$<stored>:<server>` verifier。
- 吊销：`--tls-client-revoked` 支持 X.509 CRL（PEM/DER，验签后按序列号）或每行一个 SHA-256 指纹；`--tls-client-ocsp` 解析预取的 DER OCSP 响应；`--tls-client-ocsp-url` 在线向 responder 发 `application/ocsp-request`（SHA-1 `CertID`、16 字节 nonce 并校验回显、60 秒缓存，支持 `http://` 与 `https://`）；两者都先验签再按 `CertStatus::Revoked` 判定，命中即 `CertificateError::Revoked`，吊销原因写入日志。
- mTLS：`--tls-client-ca` 提供 CA 时用 `WebPkiClientVerifier` 校验客户端证书。
- 证书热轮换：`--tls-cert`/`--tls-key`/`--tls-client-ca` 的内容哈希变化后，下一个 TLS 连接使用新证书；重载失败保留旧证书。
- `Query`（`Q`）→ 执行 `execute_script`，每个结果返回 `RowDescription` / `DataRow` / `CommandComplete`，最后 `ReadyForQuery`。
- 扩展查询：`Parse`（SQL + 参数 OID）、`Bind`（`$n` → 字面量）、`Describe`（`ParameterDescription` + 真实 `RowDescription`；SELECT 用 `LIMIT 1` 取样、参数用 `NULL` 占位；非 SELECT → `NoData`）、`Execute`（复用 simple query 编码）、`Close`；`Sync` 回 `ReadyForQuery`。
- `Terminate`（`X`）断开。
- 空查询 → `EmptyQueryResponse`；SQL 错误 → `ErrorResponse` + `ReadyForQuery`。
- 事务状态：`BEGIN`/`START TRANSACTION` → `ReadyForQuery('T')`；语句失败 → `'E'`；`E` 状态下除 `ROLLBACK`/`COMMIT` 外直接拒绝；`COMMIT`/`ROLLBACK` → `'I'`。
- `COPY <table> [(cols)] FROM STDIN` → `CopyInResponse`（`G`）+ `CopyData`（`d`）行 + `CopyDone`（`c`）→ `CommandComplete("COPY n")`；支持 `text`（`\N` 为 NULL）、`csv`（引号/`""` 转义）与 `binary`（`PGCOPY` 头 + 每行字段长度），并接受 `HEADER`/`DELIMITER`/`NULL`/`QUOTE`/`ESCAPE`/`FORCE_NULL (cols)`/`FORCE_NOT_NULL (cols)`/`ENCODING 'UTF8'`；按列类型构造 `INSERT`（每 1024 行一批）。
- `COPY <table> [(cols)] TO STDOUT` → `CopyOutResponse`（`H`）+ `CopyData` 行 + `CopyDone`（`c`）→ `CommandComplete("COPY n")`；同样支持三种格式。
- 二进制结果：扩展查询 `Bind` 的 result format code（0/1，或每列一个）；`RowDescription` 的每列 format code 与 `DataRow` 编码一致（`int8` 8 字节 BE、`bool` 1 字节、`text` UTF-8）。

## 4. 类型与命令标签

- 所有列声明为 `TEXT`（oid 25）、文本格式。
- `INT` → 十进制、`TEXT` → 原文、`BOOL` → `t`/`f`、`Logic` → `TRUE/FALSE/BOTH/NEITHER`、`Ł` → `0.000`、`ℕ` → 十进制、`NULL` → -1 长度。
- 命令标签：`CREATE TABLE`、`INSERT 0 n`、`UPDATE n`、`DELETE n`、`DROP TABLE`、`ALTER TABLE`、`ANALYZE`、`OK`。

## 5. 验收

```bash
cargo test -p tritdb --test pgwire   # 5 passed
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

端到端测试：simple query 的 startup → `CREATE TABLE` / `INSERT` → `SELECT` → 错误查询 → `Terminate`；扩展查询的 `Parse`/`Bind`/`Describe`/`Execute`/`Sync` → 参数 `$1` 替换 → `SELECT 2`；事务状态 `BEGIN`('T') → 错误('E') → `ROLLBACK`('I')；`Describe('S')` 返回 `ParameterDescription` + `RowDescription(id, name)`，非 SELECT 返回 `NoData`；4 个并发连接同时查询；密码认证接受/拒绝、trust 不发密码请求、`SSLRequest` 在无 TLS 时回 `N`；自签证书下 TLS 握手 + 查询、`--require-tls` 拒绝明文；MD5 与 SCRAM 各自接受正确密码、拒绝错误密码；mTLS 接受 CA 签发的客户端证书、拒绝无证书客户端；证书文件替换后新连接使用新证书；cleartext 密码在明文连接下默认拒绝（需 `--allow-plaintext-password`）；SCRAM verifier 文件往返 + 登录、错误密码拒绝；吊销列表命中时客户端证书被拒；COPY FROM STDIN 插入两行（含 `\N`）后可查询、COPY TO STDOUT 输出文本行与 `\N`；扩展查询二进制结果（int8/bool/text 的字节编码与 RowDescription format code）、简单查询仍为文本。

## 6. 还没做

- **OCSP 响应者证书链**：只校验签名，不校验证书链与有效期；
- **OCSP 吊销日期**：不根据 `revocation_time`/`next_update` 拒绝过期响应；
- **COPY FORCE_* 与列名**：`FORCE_NULL`/`FORCE_NOT_NULL` 只按列名匹配，不支持 `*`；
