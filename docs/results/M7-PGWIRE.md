# M7 第一刀：最小 PostgreSQL wire protocol

状态：startup / simple Query / 扩展查询（Parse/Bind/Describe/Execute/Close）/ Terminate / Sync / 事务状态映射 / `TCP_NODELAY` / TLS（rustls）/ 密码认证已实现；客户端证书、SCRAM/MD5 未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-pgwire/Cargo.toml
crates/tritdb-pgwire/src/lib.rs   serve / 协议编解码 / 类型文本化
                                  Parse/Bind/Describe/Execute/Close / $n 参数替换
crates/tritdb/src/main.rs         serve --db ... [--listen ...]
crates/tritdb/tests/pgwire.rs            5 个端到端测试
crates/tritdb/tests/pgwire_protocol.rs  19 个协议/认证/TLS 测试
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
- 认证：默认 `AuthenticationOk`（trust）；`--auth password` 时发 `AuthenticationCleartextPassword`（code 3）并校验 `PasswordMessage`。
- `Query`（`Q`）→ 执行 `execute_script`，每个结果返回 `RowDescription` / `DataRow` / `CommandComplete`，最后 `ReadyForQuery`。
- 扩展查询：`Parse`（SQL + 参数 OID）、`Bind`（`$n` → 字面量）、`Describe`（`ParameterDescription` + 真实 `RowDescription`；SELECT 用 `LIMIT 1` 取样、参数用 `NULL` 占位；非 SELECT → `NoData`）、`Execute`（复用 simple query 编码）、`Close`；`Sync` 回 `ReadyForQuery`。
- `Terminate`（`X`）断开。
- 空查询 → `EmptyQueryResponse`；SQL 错误 → `ErrorResponse` + `ReadyForQuery`。
- 事务状态：`BEGIN`/`START TRANSACTION` → `ReadyForQuery('T')`；语句失败 → `'E'`；`E` 状态下除 `ROLLBACK`/`COMMIT` 外直接拒绝；`COMMIT`/`ROLLBACK` → `'I'`。

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

端到端测试：simple query 的 startup → `CREATE TABLE` / `INSERT` → `SELECT` → 错误查询 → `Terminate`；扩展查询的 `Parse`/`Bind`/`Describe`/`Execute`/`Sync` → 参数 `$1` 替换 → `SELECT 2`；事务状态 `BEGIN`('T') → 错误('E') → `ROLLBACK`('I')；`Describe('S')` 返回 `ParameterDescription` + `RowDescription(id, name)`，非 SELECT 返回 `NoData`；4 个并发连接同时查询；密码认证接受/拒绝、trust 不发密码请求、`SSLRequest` 在无 TLS 时回 `N`；自签证书下 TLS 握手 + 查询、`--require-tls` 拒绝明文。

## 6. 还没做

- **客户端证书 / SCRAM**：只有服务端证书与 cleartext password；
- **明文密码**：`--auth password` 不带 TLS 时密码是明文，建议配合 `--require-tls`；
