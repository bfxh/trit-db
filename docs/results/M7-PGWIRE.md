# M7 第一刀：最小 PostgreSQL wire protocol

状态：startup / simple Query / Terminate / Sync 已实现；扩展查询、TLS、认证、事务状态未做

日期：2026-11-21

## 1. 交付

```text
crates/tritdb-pgwire/Cargo.toml
crates/tritdb-pgwire/src/lib.rs   serve / 协议编解码 / 类型文本化
crates/tritdb/src/main.rs         serve --db ... [--listen ...]
crates/tritdb/tests/pgwire.rs     1 个端到端测试
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
- `SSLRequest`（80877103）→ 回 `N`，客户端用明文重试。
- `Query`（`Q`）→ 执行 `execute_script`，每个结果返回 `RowDescription` / `DataRow` / `CommandComplete`，最后 `ReadyForQuery`。
- `Terminate`（`X`）断开；`Sync`（`S`）回 `ReadyForQuery`。
- 空查询 → `EmptyQueryResponse`；SQL 错误 → `ErrorResponse` + `ReadyForQuery`。
- `Parse` / `Bind` / `Describe` / `Execute` / `Close` → `ErrorResponse`（提示用 simple query）。

## 4. 类型与命令标签

- 所有列声明为 `TEXT`（oid 25）、文本格式。
- `INT` → 十进制、`TEXT` → 原文、`BOOL` → `t`/`f`、`Logic` → `TRUE/FALSE/BOTH/NEITHER`、`Ł` → `0.000`、`ℕ` → 十进制、`NULL` → -1 长度。
- 命令标签：`CREATE TABLE`、`INSERT 0 n`、`UPDATE n`、`DELETE n`、`DROP TABLE`、`ALTER TABLE`、`ANALYZE`、`OK`。

## 5. 验收

```bash
cargo test -p tritdb --test pgwire   # 1 passed
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

端到端测试：startup → `CREATE TABLE` / `INSERT`（标签正确）→ `SELECT`（列名与值）→ 错误查询得到 `ErrorResponse` → `Terminate`。

## 6. 还没做

- **扩展查询协议**：`Parse`/`Bind`/`Describe`/`Execute`；多数驱动默认走它；
- **TLS**：`SSLRequest` 直接拒绝；
- **认证**：无密码，任何能连上端口的客户端都可读写；
- **事务状态**：`BEGIN`/`COMMIT` 未映射，`ReadyForQuery` 始终 `I`；
- **并发连接**：串行 accept，每连接独立引擎。
