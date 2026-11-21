# ADR 045：最小 PostgreSQL wire protocol（pgwire）

状态：**Proposed**

日期：2026-11-21

## 背景

数据库目前只能通过 `tritdb sql --db ... --sql ...` 调用；生态里最通用的连接方式是 PostgreSQL wire protocol。先做最小闭环，让 `psql`/驱动能用 simple query 协议连上来。

## 决策

1. **新 crate `tritdb-pgwire`**（零外部依赖，`std::net`）：
   - `serve(db: PathBuf, listener: TcpListener)`：串行 accept，每个连接打开自己的 `SqlEngine`。
   - `handle_connection`：startup → `AuthenticationOk` + 若干 `ParameterStatus` + `BackendKeyData` + `ReadyForQuery`。
2. **协议子集**
   - StartupMessage v3（196608）；`SSLRequest`（80877103）回复 `N` 并要求重新 startup。
   - `Query`（`Q`）：执行 `SqlEngine::execute_script`，逐条返回 `RowDescription` / `DataRow` / `CommandComplete`，最后 `ReadyForQuery`。
   - `Terminate`（`X`）断开；`Sync`（`S`）返回 `ReadyForQuery`。
   - 扩展查询：`Parse` 保存 SQL 与参数 OID；`Bind` 把 `$n` 替换为字面量（按 OID 区分 int/bool/text）；`Describe` 返回 `ParameterDescription` + `NoData`；`Execute` 复用 simple query 的结果编码；`Close` 删除 prepared/portal。
   - `Sync` 返回 `ReadyForQuery`，`Flush` 是 no-op。
   - 空查询返回 `EmptyQueryResponse`。
3. **类型与格式**
   - 所有列统一声明为 `TEXT`（oid 25）、文本格式；值编码：`INT` 十进制、`TEXT` 原文、`BOOL` → `t`/`f`、`Logic` → `TRUE/FALSE/BOTH/NEITHER`、`Ł` → `0.000`、`ℕ` 十进制、`NULL` → -1 长度。
   - 命令标签按引擎消息推断：`CREATE TABLE`、`INSERT 0 n`、`UPDATE n`、`DELETE n`、`DROP TABLE`、`ALTER TABLE`、`ANALYZE`、`OK`。
4. **CLI**：`tritdb serve --db sql.db [--listen 127.0.0.1:5432]`；启动后打印 `listening=<addr> db=<path>`。

## 理由

- **零依赖**：wire protocol 的 simple query 子集足够小，用 `std::net` 直接实现。
- **文本格式**：避免二进制类型映射；所有值都能用文本表示，客户端解析简单。
- **串行连接**：`TxnStore` 有跨进程写锁，多连接并发写会冲突；先串行，后续再做连接池/只读副本。

## 后果 / 限制

- **无 TLS**：`SSLRequest` 直接拒绝；只适合本地/受信网络。
- **扩展查询只覆盖最小子集**：`Describe` 不返回结果列信息（`NoData`），`Execute` 忽略 row limit，参数只按文本格式处理。
- **无认证**：任何能连上端口的客户端都可读写。
- **串行 + 每连接独立引擎**：同一时刻只有一个连接在处理；连接数多时排队。
- 所有列都是 TEXT，客户端看到的类型信息不精确。
- 事务控制（`BEGIN`/`COMMIT`）未映射到 pgwire 状态；`ReadyForQuery` 始终是 `I`。

## 验收

```bash
cargo test -p tritdb --test pgwire   # 1 passed
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

端到端测试：startup → `CREATE TABLE` / `INSERT`（标签 `CREATE TABLE`、`INSERT 0 2`）→ `SELECT`（`RowDescription` 列名 `id`/`name`、`DataRow` 值）→ 错误查询得到 `ErrorResponse` → `Terminate`。
