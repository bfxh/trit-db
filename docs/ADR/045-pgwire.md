# ADR 045：最小 PostgreSQL wire protocol（pgwire）

状态：**Proposed**

日期：2026-11-21

## 背景

数据库目前只能通过 `tritdb sql --db ... --sql ...` 调用；生态里最通用的连接方式是 PostgreSQL wire protocol。先做最小闭环，让 `psql`/驱动能用 simple query 协议连上来。

## 决策

1. **新 crate `tritdb-pgwire`**（零外部依赖，`std::net`）：
   - `serve(db: PathBuf, listener: TcpListener)`：打开一个共享 `SqlEngine`（`Arc<Mutex<...>>`），accept 后为每个连接 spawn 一个线程；语句在引擎锁上串行化。
   - 每个连接都设置 `TCP_NODELAY`：协议是「一条小消息一次往返」，Nagle + delayed ACK 会让 Linux 上每次往返多等约 40 ms。
   - `handle_connection`：startup → `AuthenticationOk` + 若干 `ParameterStatus` + `BackendKeyData` + `ReadyForQuery`。
2. **协议子集**
   - StartupMessage v3（196608）；`SSLRequest`（80877103）回复 `N` 并要求重新 startup。
   - `Query`（`Q`）：执行 `SqlEngine::execute_script`，逐条返回 `RowDescription` / `DataRow` / `CommandComplete`，最后 `ReadyForQuery`。
   - `Terminate`（`X`）断开；`Sync`（`S`）返回 `ReadyForQuery`。
   - 扩展查询：`Parse` 保存 SQL 与参数 OID；`Bind` 把 `$n` 替换为字面量（按 OID 区分 int/bool/text）；`Describe` 返回 `ParameterDescription` + 真实 `RowDescription`（对 SELECT 用 `LIMIT 1` 取样列名与类型，参数用 `NULL` 占位；非 SELECT 返回 `NoData`）；`Execute` 复用 simple query 的结果编码；`Close` 删除 prepared/portal。
   - `Sync` 返回 `ReadyForQuery`，`Flush` 是 no-op。
   - 事务状态：连接级 `TxStatus`（Idle / InTransaction / Failed）；`BEGIN`/`START TRANSACTION` → `T`，语句失败 → `E`，`COMMIT`/`ROLLBACK` → `I`；`E` 状态下除 `ROLLBACK`/`COMMIT` 外直接拒绝。
   - 空查询返回 `EmptyQueryResponse`。
3. **类型与格式**
   - 列类型 OID 按取样推断：第一个非 `NULL` 值为 `INT` → `int8`(20)、`BOOL` → `bool`(16)，其余（含全 `NULL` 列、空结果）→ `text`(25)；数据一律文本格式；值编码：`INT` 十进制、`TEXT` 原文、`BOOL` → `t`/`f`、`Logic` → `TRUE/FALSE/BOTH/NEITHER`、`Ł` → `0.000`、`ℕ` 十进制、`NULL` → -1 长度。
   - 命令标签按引擎消息推断：`CREATE TABLE`、`INSERT 0 n`、`UPDATE n`、`DELETE n`、`DROP TABLE`、`ALTER TABLE`、`ANALYZE`、`OK`。
4. **CLI**：`tritdb serve --db sql.db [--listen 127.0.0.1:5432]`；启动后打印 `listening=<addr> db=<path>`。

## 理由

- **零依赖**：wire protocol 的 simple query 子集足够小，用 `std::net` 直接实现。
- **文本格式**：避免二进制类型映射；所有值都能用文本表示，客户端解析简单。
- **共享引擎锁**：多连接可以同时握手/等待，但执行语句时串行；避免多连接同时写 `TxnStore` 的锁冲突。

## 后果 / 限制

- **无 TLS**：`SSLRequest` 直接拒绝；只适合本地/受信网络。
- **扩展查询只覆盖最小子集**：`Describe` 对 SELECT 返回列名与取样推断的类型，非 SELECT 返回 `NoData`；`Execute` 忽略 row limit，参数只按文本格式处理。
- **无认证**：任何能连上端口的客户端都可读写。
- **执行串行化**：连接并发，但语句在共享引擎锁上排队；不是多写者并发。
- 类型只按取样值推断：全 `NULL` 列、空结果回落成 `text`；`Logic`/`Ł`/`ℕ` 等非 `INT`/`BOOL` 值也报 `text`。
- **事务状态只是协议层**：`BEGIN`/`COMMIT`/`ROLLBACK` 不改变引擎的单语句提交模型，不会把多条语句合成一个原子事务；`E` 状态只是拒绝后续语句。

## 验收

```bash
cargo test -p tritdb --test pgwire   # 1 passed
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

端到端测试：startup → `CREATE TABLE` / `INSERT`（标签 `CREATE TABLE`、`INSERT 0 2`）→ `SELECT`（`RowDescription` 列名 `id`/`name`、`DataRow` 值）→ 错误查询得到 `ErrorResponse` → `Terminate`。
