# M1 SQL 3VL：最小纵向切片读数

状态：parser / binder / executor / CLI / 持久化 / SQLite 对拍 / 单表 GROUP BY / 多表 INNER JOIN / CROSS JOIN / 逗号 FROM / LEFT JOIN / RIGHT JOIN / FULL JOIN / NATURAL JOIN / USING / 等值 JOIN 的 hash join / `ANALYZE` 表/列统计 / 基于统计的 `Auto` join 代价选择 / 内连接/交叉连接的 `WHERE` 合取项拆分与单源下推 / 源表列裁剪（含无歧义未限定列与零列源表投影）/ 常量折叠与恒真条件简化 / 投影常量折叠 / 结构化计划树与只读 `EXPLAIN SEXPR` / `EXPLAIN ANALYZE SEXPR` 执行汇总 / 结构化 join 键与残余标志 / 结构化过滤谓词 / 结构化 join 谓词 / 结构化 join 残余谓词 / 结构化 join 非键合取项 / 结构化 HAVING 谓词 / 结构化 Aggregate 分组键与聚合项 / 结构化 Sort 键与方向 / 结构化 Project 项与表达式 / 结构化 Scan 表名与别名 / 结构化 Scan 下推谓词 / parser 表达式嵌套深度上限 / 加法 schema 迁移 `ALTER TABLE ADD COLUMN` / `EXPLAIN SELECT` 统计估算 / `EXPLAIN ANALYZE` / JOIN 策略开关 / UPDATE / IN·BETWEEN·LIKE 谓词 / CASE 表达式已实现。
日期：2026-11-20

## 1. 交付

```text
crates/tritdb-exec/   Value、Schema、表达式、三值逻辑
crates/tritdb-sql/    lexer / parser / binder / engine / TRSQ 表存储
crates/tritdb/        sql 子命令
tools/crosscheck/sql_oracle.py
docs/specs/SQL.md
```

支持：

- `CREATE TABLE [IF NOT EXISTS]`
- `ALTER TABLE ... ADD [COLUMN] [IF NOT EXISTS]`：只追加可空列，旧行回填 `NULL`，整表单事务重写并使统计失效
- `INSERT INTO ... VALUES`
- `SELECT`：`WHERE`、`GROUP BY`、`HAVING`、`ORDER BY`、`LIMIT`、别名、`*`
- `GROUP BY` 直接列引用、`NULL` 键同组、分组聚合、分组结果排序
- 多表 `INNER JOIN`：表别名、`alias.column` 限定列、非限定唯一列、自连接、JOIN 后分组/排序
- `CROSS JOIN` / 逗号分隔 `FROM`：左右行笛卡尔积、空表、与显式 JOIN 混用、`WHERE/GROUP BY`
- `LEFT [OUTER] JOIN`：未匹配左行保留、右侧列补 `NULL`、空右表、LEFT JOIN 后 `WHERE/GROUP BY`
- `RIGHT [OUTER] JOIN` / `FULL [OUTER] JOIN`：未匹配右行/双侧行保留、另一侧补 `NULL`、空表、`WHERE/GROUP BY`
- `NATURAL [INNER|LEFT|RIGHT|FULL] JOIN`：公共列自动匹配、合并列只输出一次、双限定名访问、连续链式合并、无公共列退化为 `CROSS JOIN`
- `JOIN ... USING (col, ...)`：指定列匹配、合并列排序、重复/缺失/类型不一致校验、`LEFT/RIGHT/FULL` 的非 `NULL` 侧合并
- 等值 JOIN 执行：`USING`/`NATURAL` 与 `ON` 顶层 `AND` 的列等值支持 hash join；有 `ANALYZE` 统计时 `Auto` 按 `nested = L*R` 与 `hash = R+L+O` 的局部成本选择，缺统计时回退 hash；支持复合键、残余 `ON` 谓词、`NULL` 键不匹配；`OR`、非等值比较、表达式键只能 nested loop
- `WHERE` 合取项拆分与单源下推：所有 join 都是 `INNER`/`CROSS` 时，顶层（含括号嵌套）的 `AND` 被拆成独立合取项；每个只引用一个源表、列全部显式限定的合取项移到该源表扫描，同一源表的多个条件用 `AND` 组合；不可下推项按原顺序重组为残余 `WHERE`。外连接、`OR` 分支、`NOT` 内部条件、多源单条件和未限定列保持在 join 后。下推后的扫描行数参与 `Auto` 成本和 `EXPLAIN` 估算，文本计划显示 `Scan ... filtered`，`EXPLAIN SEXPR` 另以 `:filter` 输出绑定后的下推谓词
- 源表列裁剪：`SELECT` 不含 `*`、每个列引用都能静态归属到唯一源表、join 链不含 `USING`/`NATURAL` 时，收集 `SELECT`/`WHERE`/`GROUP BY`/`HAVING`/`ORDER BY`/`ON` 所需列，在源表限定阶段只保留严格子集；显式限定列直接归属，未限定列只在全源唯一时归属，匹配零个或多个源表都退回完整 schema。某源表不需要任何列时投影为零列并保留行数；`CROSS JOIN` / `ON TRUE` 仍按行数乘积构造，外连接走既有 NULL 行路径。源表仍完整加载，只缩小 join 前副本和中间行宽。`EXPLAIN`/`EXPLAIN ANALYZE` 的 `Scan` 节点显示 `cols=<col,...>`，零列显示 `cols=()`。含 `*`、`USING` 或 `NATURAL` 时整条查询不裁剪；歧义/未知未限定列交给 binder 按原错误路径处理
- 常量折叠与恒真条件简化：计划入口折叠 `SELECT` 项、`WHERE`/`HAVING`/`GROUP BY`/`ORDER BY`/`JOIN ... ON` 中不含列或聚合的常量表达式；成功求值替换为字面量，求值失败保留原表达式。有 alias 的投影保留 alias，裸列保留列名，无 alias 的非列/聚合投影仍命名为 `colN`，折叠不改变输出标签。只做 `TRUE AND x`、`x AND TRUE`、`FALSE OR x`、`x OR FALSE` 四个安全恒等；`WHERE`/`HAVING` 折叠为 `TRUE` 时移除对应节点，`FALSE`/`NULL` 保留。`EXPLAIN` 中不再出现恒真 `Filter`/`Having`；字面 `FALSE`/`NULL` 的过滤估算为 0。`FALSE AND x`、`TRUE OR x` 不简化，避免改变错误/短路时机
- `EXPLAIN SELECT`：绑定但不执行，输出 `Scan` / `Join` / `Filter` / `Aggregate` / `Having` / `Project` / `Sort` / `Limit` 文本计划；Join 节点标出 `HashJoin` 或 `NestedLoop`、键和残余谓词；发生谓词下推的源表显示 `Scan ... filtered`，发生列裁剪的源表显示 `cols=...`；恒真 `WHERE`/`HAVING` 不生成对应节点，字面 `FALSE`/`NULL` 仍保留并估算为 0；有 `ANALYZE` 统计时节点追加 `est rows` / `est groups`，并按同一成本规则决定显示的 join 路径
- `EXPLAIN SEXPR SELECT`：同一 `PlanNode` 树的只读括号化输出，返回一列 `plan`、一行文本；节点携带表名/别名/过滤标记/绑定后的下推谓词/估算行数/裁剪列，join 携带算法、连接类型、结构化 `:keys`、`:residual`、`:on-expr` 和完整的 `:pred`（列名 + 运算符 + 字面量/嵌套谓词），过滤节点携带结构化 `:pred`（列名 + 运算符 + 字面量/嵌套谓词），having 节点携带含聚合调用的结构化 `:pred`，投影携带输出名与表达式，排序携带键表达式与升降序，limit 携带上限；零列显示 `:cols ()`。静态模式不输出 actual/time 字段；不改变执行路径、结果行集或错误语义
- `EXPLAIN ANALYZE SEXPR SELECT`：执行一次查询并返回一列 `plan`、一行文本；顶层是 `(execution :rows N :time-ms X ...)` 执行汇总，节点只携带 `:actual-rows` / `:actual-groups` 和实际耗时，不混入 `:est-rows` / `:est-groups`；join 的 `:keys` / `:residual` / `:on-expr` / `:pred` 与静态模式同形，`having` 的 `:pred`、`scan` 的 `:filter` 也保留；扫描不单独计时，limit 只有实际行数；不返回查询结果行
- `ANALYZE [table]`：扫描表并持久化精确行数、每列 `NULL` 数和 NDV；DML 在同一事务删除旧统计，重开后可继续供 `EXPLAIN` 和 `Auto` join 选择使用
- `EXPLAIN ANALYZE SELECT`：执行一次查询，在计划节点上追加 `actual rows`（聚合节点为 `actual groups`）和墙钟耗时；谓词下推的扫描显示过滤后的 `actual rows`，列裁剪的扫描显示 `cols=...`；末行给出 `Execution: rows=N time=Xms`，不返回查询结果行；join 节点显示本次实际选择的算法
- 解析安全边界：表达式递归嵌套上限 64 层，括号、连续 `NOT`、连续一元负号都计入；超限返回 `expression nesting too deep` 的 parse error，不触发进程栈溢出。CLI 子进程也覆盖 512 层嵌套输入。见 [`ADR-031`](../ADR/031-parser-expression-depth-limit.md)
- JOIN 策略开关：`JoinStrategy::{Auto, Hash, NestedLoop}`，`Auto` 有统计时比较成本，`Hash` / `NestedLoop` 强制指定执行路径，供基准和差分测试使用
- `UPDATE ... SET column = expr [, ...] [WHERE ...]`：旧行求值、多列赋值、类型/NOT NULL 校验
- `IN` / `NOT IN`、`BETWEEN` / `NOT BETWEEN`、`LIKE` / `NOT LIKE`：三值逻辑、NULL 传播、ASCII 大小写不敏感
- `CASE WHEN` 搜索式与 `CASE operand WHEN` 简单式、`ELSE`/缺省 `NULL`、分支短路、聚合/分组内使用
- `DELETE`、`DROP TABLE`
- `COUNT(*)`、`COUNT(expr)`、`SUM`、`MIN`、`MAX`
- `INT` / `TEXT` / `BOOL`，`NOT NULL`
- `NULL` / `UNKNOWN`、三值 `AND` / `OR` / `NOT`、`IS [NOT] NULL/TRUE/FALSE/UNKNOWN`

## 2. 验收命令

```bash
cargo test -p tritdb-exec
cargo test -p tritdb-sql
cargo test -p tritdb --test sql_cli
cargo build -p tritdb
python tools/crosscheck/sql_oracle.py --tritdb target/debug/tritdb
```

本机结果：

```text
sql oracle: 138/138 passed (132 SELECT, 6 UPDATE row counts)
```

### sql-join 规模基准

`tritdb-bench sql-join` 构造两张 `rows` 行的表，键和 payload 都是 `0..rows-1`，重复执行 `SELECT COUNT(*) FROM a JOIN b ON a.id = b.id;`。查询在计时循环前解析一次，循环内只调用 `execute_statement`，因此读数不包含 SQL 解析。每档 3 轮，取 min/median/max。

```bash
cargo run -p tritdb-bench --release -- sql-join --rows 1024 --rounds 3 --strategy auto
cargo run -p tritdb-bench --release -- sql-join --rows 1024 --rounds 3 --strategy hash
cargo run -p tritdb-bench --release -- sql-join --rows 1024 --rounds 3 --strategy nested
cargo run -p tritdb-bench --release -- sql-join --rows 4096 --rounds 3 --strategy auto
cargo run -p tritdb-bench --release -- sql-join --rows 4096 --rounds 3 --strategy hash
cargo run -p tritdb-bench --release -- sql-join --rows 4096 --rounds 3 --strategy nested
```

本机 Windows x86_64、release 构建：

| rows | strategy | join_ms(min/med/max) | matches |
|---:|---|---:|---:|
| 1024 | auto | 0.464 / 0.472 / 0.591 | 1024 |
| 1024 | hash | 0.461 / 0.495 / 0.653 | 1024 |
| 1024 | nested | 66.049 / 66.075 / 66.646 | 1024 |
| 4096 | auto | 1.859 / 1.941 / 2.327 | 4096 |
| 4096 | hash | 1.948 / 2.018 / 2.452 | 4096 |
| 4096 | nested | 1059.313 / 1061.002 / 1063.252 | 4096 |

1024 行时 hash 比 nested 快约两个数量级；4096 行时差距扩大到约 500 倍。行数翻四倍，nested 从约 66ms 增至约 1.06s，hash 从约 0.5ms 增至约 2ms。读数只用于本机回归对比，不作为跨机器绝对性能承诺。

### sql-plan 计划生成基准

`tritdb-bench sql-plan` 在同一份两张 `rows` 行的表上分别对三种查询生成文本计划与结构化计划：

- `join`：`SELECT a.id FROM a JOIN b ON a.id = b.id`
- `filter`：join 加两侧 `WHERE payload < 512` 下推
- `aggregate`：join + 下推过滤 + `GROUP BY` + `ORDER BY` + `LIMIT`

每个查询先预热一次，再交替测量 `EXPLAIN` 与 `EXPLAIN SEXPR`，共 5 轮，取 min/median/max。`text_chars` / `sexpr_chars` 是最后一条计划输出的字符数，不代表解析或执行结果。

```bash
cargo run -p tritdb-bench --release -- sql-plan --rows 256 --rounds 5
cargo run -p tritdb-bench --release -- sql-plan --rows 1024 --rounds 5
cargo run -p tritdb-bench --release -- sql-plan --rows 4096 --rounds 5
```

| rows | query | text_ms(min/med/max) | sexpr_ms(min/med/max) | text_chars | sexpr_chars |
|---:|---|---:|---:|---:|---:|
| 256 | join | 0.006 / 0.007 / 0.008 | 0.005 / 0.006 / 0.007 | 85 | 167 |
| 256 | filter | 0.008 / 0.009 / 0.009 | 0.007 / 0.008 / 0.008 | 87 | 235 |
| 256 | aggregate | 0.009 / 0.009 / 0.010 | 0.008 / 0.008 / 0.009 | 147 | 387 |
| 1024 | join | 0.007 / 0.007 / 0.009 | 0.006 / 0.006 / 0.008 | 85 | 167 |
| 1024 | filter | 0.009 / 0.009 / 0.009 | 0.008 / 0.008 / 0.010 | 87 | 235 |
| 1024 | aggregate | 0.009 / 0.009 / 0.010 | 0.008 / 0.008 / 0.011 | 147 | 387 |
| 4096 | join | 0.006 / 0.007 / 0.009 | 0.006 / 0.006 / 0.007 | 85 | 167 |
| 4096 | filter | 0.008 / 0.008 / 0.009 | 0.008 / 0.008 / 0.010 | 87 | 235 |
| 4096 | aggregate | 0.009 / 0.009 / 0.010 | 0.008 / 0.008 / 0.009 | 147 | 387 |

`EXPLAIN` 只绑定、不扫描表，所以计划生成耗时基本不随 `rows` 增长；它衡量的是计划构造和序列化，不是查询执行。结构化输出更长（167 / 235 / 387 字符，对文本 85 / 87 / 147），但同档耗时与文本计划接近，说明当前计划规模下 S-expression 序列化没有形成数量级开销。

对拍口径：

- 两边执行同一份 `CREATE TABLE` + `INSERT`；
- 每条 SELECT 单独跑，比较行值，不比较列名；
- `TRUE`/`FALSE` 归一化为 `1`/`0`，`NULL` 归一化为 `NULL`；
- SQLite 不支持 `IS UNKNOWN`，该语义由 Rust 测试单独覆盖。

## 3. 测试增量

| 层 | 新增 |
|---|---:|
| `tritdb-exec` 单元测试 | 12 |
| `tritdb-sql` 单元测试 | 30 |
| `tritdb-sql` 引擎集成测试 | 54 |
| `tritdb` SQL CLI 集成测试 | 3 |
| **合计** | **99** |

其中 26 个来自初始 SQL 3VL 切片，6 个来自 GROUP BY/HAVING 切片，10 个来自 JOIN 切片（3 个 parser + 7 个 engine），5 个来自 UPDATE 切片（1 个 parser + 4 个 engine），7 个来自谓词切片（3 个 exec + 1 个 parser + 3 个 engine），5 个来自 CASE 切片（1 个 exec + 1 个 parser + 3 个 engine），4 个来自 RIGHT/FULL JOIN 切片（1 个 parser + 3 个 engine），5 个来自 CROSS JOIN/逗号 FROM 切片（2 个 parser + 3 个 engine），5 个来自 NATURAL JOIN/USING 切片（3 个 parser + 2 个 engine，替换 1 个旧 unsupported join 测试，净增 4），1 个来自 hash join 切片（复合键/NULL 键/残余谓词/非等值回退），2 个来自 EXPLAIN 切片（1 个 parser + 1 个 engine），1 个来自 EXPLAIN ANALYZE 切片（计划实际行数/分组数/耗时），3 个来自 ANALYZE 统计切片（1 个 parser + 1 个 stats 单元 + 1 个 storage 编解码），2 个来自 EXPLAIN 估算切片（统计持久化/重开与 DML 失效），1 个来自 Auto join 代价切片（小表选 nested loop、静态与实测计划一致），2 个来自谓词下推切片（单表整体下推、跨源合取项拆分与残余条件），1 个来自源表列裁剪切片（宽表 join 裁剪、`EXPLAIN`/`EXPLAIN ANALYZE` 的 `cols=`、`*`/`USING` 回退；后续无歧义未限定列扩展只改既有断言，不新增计数），1 个来自常量折叠切片（恒真 `WHERE`/`HAVING` 移除、字面 `FALSE` 估算为 0、`AND TRUE`/`OR FALSE` 后继续下推、除零错误保留），2 个来自加法 schema 迁移切片（1 个 parser + 1 个 engine：旧行 `NULL` 回填、列顺序、重开持久化、统计失效、`IF NOT EXISTS` 幂等、`NOT NULL`/重复列/未知表拒绝、空表迁移），1 个来自投影常量折叠切片（`fold_select` 内部单测；集成测试扩展既有常量折叠用例，验证 `col1`/alias 标签和投影除零错误保留，不新增 engine 测试），1 个来自零列源表投影切片（常量投影/`COUNT(*)`/`CROSS JOIN`/`FULL JOIN` 行数保持与 `cols=()`），3 个来自结构化计划切片（1 个 parser：`EXPLAIN SEXPR` 语法；2 个 engine：连接树/估算/零列的括号化输出与单值返回），1 个来自结构化 join 键切片（USING/NATURAL 列名、ON 等值对、residual/on-expr 的 `EXPLAIN SEXPR` 渲染），2 个来自 `EXPLAIN ANALYZE SEXPR` 切片（1 个 parser：`EXPLAIN ANALYZE SEXPR` 语法；1 个 engine：执行汇总、节点 actual 字段、无 est 混入、单值返回），1 个来自结构化过滤谓词切片（`Filter` 绑定表达式转 `PlanExpr`，`OR`/`<>`/`LIKE` 嵌套渲染，列索引还原为 schema 名）；结构化 join 谓词切片扩展现有 `EXPLAIN SEXPR` 断言（`ON`/`USING`/nested-loop 的完整 `:pred`），不新增测试计数；结构化 HAVING 谓词切片继续扩展同一测试（`COUNT(*)`、`SUM`、标量列谓词的 `:pred`），也不新增计数；结构化 Aggregate 分组键与聚合项切片再扩展同一测试（全局聚合空键、仅分组空聚合、`COUNT(*)` / `SUM` 去重清单），仍不新增计数；结构化 Sort 键切片继续扩展同一测试（升降序、复合键、聚合排序键与 ANALYZE 模式），也不新增计数；结构化 Project 项切片再扩展同一测试（别名聚合输出、常量投影与 ANALYZE 模式），仍不新增计数；结构化 join 残余谓词切片继续扩展 join 断言（单键/复合键拆分、`OR` 子树保留、纯等值/USING/nested-loop 不出字段），仍不新增计数；结构化 Scan 表名与别名切片再扩展同一测试（无别名表名保持原形、显式别名输出 `:alias`、文本仍为 `Scan a AS x`），仍不新增计数；结构化 Scan 下推谓词切片继续扩展同一测试（单侧与双侧 `:filter`、文本 `Scan a filtered` 保持不变），仍不新增计数；结构化 join 非键合取项切片再扩展 join 断言（同源列等值输出 `:residual-pred` 但不改历史 `:residual` 标志，文本仍无 `residual=yes`），仍不新增计数；解析深度安全切片新增 2 个测试（1 个 parser 单元：括号/`NOT`/一元负号超限与 32 层正常解析，1 个 SQL CLI：512 层嵌套子进程返回 parse error 而不栈溢出）。M1 相关测试从 97 增至 **99**，工作区从 255 增至 **257 个正常测试 + 4 个 ignored 压力测试**。

## 4. 负结果 / 边界

1. **整表重写**：每次 INSERT/DELETE/UPDATE 都把整张表重新编码成一个 KV value。数据量增大后写放大和内存峰值会先撞墙，M1 不解决。
2. **JOIN 等值有 hash/nested 两条路径、非等值只能 nested loop，仍然全内存**：不支持子查询；`CROSS JOIN`/逗号 `FROM` 直接产生行数乘积。`ON` 只从顶层 `AND` 提取列等值，`OR`/非等值/表达式键只能嵌套循环；hash 桶按右表行号构建，没有分区、批处理或 spill；有 `ANALYZE` 统计时 `Auto` 按局部成本在 hash 与 nested loop 间选择，不重排 join 顺序，也不做全局计划搜索。`WHERE` 下推只用于全内连接/交叉连接且列显式限定的单源项；顶层 `AND` 已能拆成单源合取项分别下推，但 `OR`/`NOT` 内部条件、外连接谓词和未限定列仍不拆。源表列裁剪在 `SELECT` 无 `*`、列引用可唯一归属且 join 链不含 `USING`/`NATURAL` 时生效；可唯一归属的未限定列参与裁剪，某源表无列需求时投影为零列，歧义/未知列或合并列整条查询回退完整 schema。`USING` 只接受不带限定的直接列名，`NATURAL` 只按同名公共列匹配；两者不能与 `ON` 混用，`CROSS JOIN` 不能带 `ON`/`USING`。`GROUP BY` 只支持列引用，不支持分组表达式、位置序号、GROUPING SETS/CUBE/ROLLUP。SQLite 对拍已覆盖单表表达式、分组聚合、`HAVING`、INNER/CROSS/LEFT/RIGHT/FULL/NATURAL/USING JOIN、复合键/NULL 键 hash join、非等值回退、逗号 `FROM`、空表、UPDATE、IN/BETWEEN/LIKE 谓词和 CASE。
3. **没有 pgwire**：`psql` 还连不上；CLI 是当前唯一入口。
4. **每个语句自动提交**：脚本不是一个大事务；`INSERT` 多行在一个事务内，但 `CREATE`/`INSERT`/`SELECT` 之间不共享事务。
5. **没有跨进程锁**：两个进程写同一个 SQL WAL 仍是未定义行为，和 M0 一致。
6. **统计是手动快照，索引仍没有**：`ANALYZE` 精确扫描并持久化行数、NULL 数、NDV；DML 删除旧统计但不自动重算，必须重新 `ANALYZE`。SELECT 仍全表扫描，ORDER BY 全量排序；单源合取项下推只在扫描后过滤，不减少实际扫描行数；源表列裁剪只缩小 join 前的行副本和中间结果宽度，不减少源表加载/扫描量；`EXPLAIN` 的 `est` 是启发式估算，`Auto` 用同一估算做局部 join 成本选择，但不是全局代价模型。
7. **类型严格**：不做 SQLite 式隐式转换；`INSERT INTO t (id) VALUES ('x')` 会拒绝。
8. **分组全内存**：`GROUP BY` 用 `BTreeMap<Vec<GroupKeyPart>, Vec<&Row>>`，没有 hash aggregate、分区或 spill；组数/行数受内存限制。
9. **NULL 排序按 SQLite**：ASC 最前、DESC 最后；PostgreSQL 默认相反，后续要做方言开关。
10. **JOIN 全内存**：hash 桶保存右表行号，每个 join 仍构造新的临时表并复制行，没有流式执行、分区或 spill；多表/大表会快速放大内存。
11. **谓词只做标量列表**：`IN` 不支持子查询或行构造器；`LIKE` 仅支持 `TEXT`，没有 `ESCAPE`、`GLOB` 或正则，大小写折叠只覆盖 ASCII。
12. **CASE 只做标量分支**：不支持子查询条件/结果；简单式 `CASE operand WHEN` 用本引擎的类型严格等值比较，探针与候选类型不同会报 `type mismatch`，而 SQLite 会落到 `ELSE`。未选中分支不求值，因此其中的除零/类型错误不会触发。
13. **EXPLAIN 不是全局优化器**：`EXPLAIN SELECT` 只绑定不执行，计划入口会折叠不含列/聚合的常量（含 `SELECT` 项）并移除恒真 `WHERE`/`HAVING`，有 `ANALYZE` 统计时显示启发式 `est rows` / `est groups`，并按局部成本规则选择要显示的 `HashJoin` / `NestedLoop`；内连接/交叉连接的顶层 `AND` 会拆成单源合取项并在扫描节点显示 `Scan ... filtered`，`EXPLAIN SEXPR` 另以 `:filter` 输出该下推谓词，同时改变扫描估算和 join 选择；源表列裁剪会在扫描节点显示 `cols=...`，可唯一归属的未限定列也参与裁剪；零列源表显示 `cols=()`，只保留行数；歧义/未知未限定列退回完整 schema，让 binder 报原错误。`EXPLAIN ANALYZE` 会真实执行一次，显示本次 `actual`、实际选中的算法和墙钟耗时，不返回查询结果。两者都不枚举候选计划、不重排 join 顺序、不做表达式级列下推或跨 join 列需求传播；`OR` 分支与外连接谓词不拆分；常量折叠只覆盖无列/聚合表达式和四个安全布尔恒等，不做 `FALSE AND x`、`TRUE OR x`、`NOT NOT x`、常量传播或投影下推。`JoinStrategy` 是进程内 API 开关，不是 SQL hint。
14. **迁移只做加法**：`ALTER TABLE ... ADD [COLUMN]` 只允许追加可空列；`NOT NULL`、`DEFAULT`、多列一次迁移、`DROP/RENAME/ALTER COLUMN` 均不支持。迁移在内存中复制整表、给旧行追加 `NULL`，再走 `put_table` 的单事务整表重写；大表会阻塞写者并放大 WAL/内存，失败只会停在旧表或完整新表之一。提交成功后统计立即失效，但已提交迁移没有自动回滚/降级，回退只能依赖迁移前备份或未来显式 DDL。
15. **解析嵌套上限只保护栈深度**：括号、连续 `NOT`、连续一元负号超过 64 层会返回 `expression nesting too deep`；这不限制脚本总长度、token 总数、`IN` 列表长度或结果行数。超长但平坦的 SQL 仍可能消耗大量内存，输入配额需在执行层/调用方另做。

## 5. CI 与覆盖

- CROSS JOIN/逗号 FROM 切片 CI run：<https://github.com/bfxh/trit-db/actions/runs/44639115915>，11 个 job 全绿（lint、三平台 test、两平台 MSRV、三平台 Python 对拍、coverage、docs）。
- NATURAL JOIN/USING 切片：本地 `sql_oracle.py` 133/133（127 SELECT + 6 UPDATE 行数）；等值 hash join 切片当前为 **138/138**（132 SELECT + 6 UPDATE 行数）。三平台 CI 待推送后补。上一轮 CROSS JOIN CI 的三平台对拍为 118/118。
- 行覆盖：SQL 3VL 切片 4710/6559 = 71.81%，GROUP BY/HAVING 后 4906/6777 = 72.39%，INNER JOIN 后 5080/6957 = 73.02%，LEFT JOIN 后 5117/6991 = 73.19%，UPDATE 后 5215/7081 = 73.65%，谓词后 5531/7426 = 74.48%，CASE 后 5766/7646 = 75.41%，RIGHT/FULL JOIN 后 5814/7692 = 75.59%，CROSS JOIN/逗号 FROM 后 **5859/7737 = 75.73%**；NATURAL/USING、hash join、EXPLAIN、EXPLAIN ANALYZE、EXPLAIN ANALYZE SEXPR、结构化过滤谓词、ANALYZE 统计与估算、谓词下推、合取项拆分、源表列裁剪（含无歧义未限定列与零列源表投影）、常量折叠、投影常量折叠、结构化计划/`EXPLAIN SEXPR`、结构化 join 键、结构化 Scan 下推谓词与加法 schema 迁移切片尚未重跑覆盖率，65% 门禁仍有余量。
- 本机无法用 GNU/MSVC 工具链跑 `llvm-cov`（缺 profiler runtime / MSVC linker），覆盖数字取自 CI 的 `coverage-lcov` artifact。

## 6. 下一步

- 等值 hash join、可复现 `sql-join` 基准、`EXPLAIN SELECT`、`EXPLAIN SEXPR`、`EXPLAIN ANALYZE`、`EXPLAIN ANALYZE SEXPR`、`ANALYZE` 统计、`Auto` 局部代价选择、`WHERE` 合取项下推、源表列裁剪（含无歧义未限定列与零列源表投影）、常量折叠与投影常量折叠、结构化 `PlanNode`/`JoinKeys` 已做：右表建桶、左表探测，`USING`/`NATURAL` 与 `ON` 顶层 `AND` 的列等值可命中；非等值/OR/表达式键只能 nested loop；`JoinStrategy` 可强制路径，`EXPLAIN` 直接显示 `HashJoin` / `NestedLoop`、键与启发式 `est rows`，`EXPLAIN ANALYZE` 显示实际行数/分组数、实际算法与耗时；内连接/交叉连接的顶层 `AND` 会按来源拆分并把单源合取项下推到扫描；无 `*` / `USING` / `NATURAL` 且列引用可唯一归属时，会在源表限定阶段裁剪未引用列（包括全源唯一的未限定列），零列源表只保留行数并在 `Scan` 节点显示 `cols=...` / `cols=()`；不含列/聚合的常量会折叠（含 `SELECT` 项），恒真 `WHERE`/`HAVING` 被移除；`EXPLAIN SEXPR` 把同一棵树输出为只读括号化计划；`JoinPlan` 的键、residual 和 on_expr 已从字符串片段拆成结构化字段，文本计划保持不变；`EXPLAIN SEXPR` 的 join 字段已改为 `:keys` / `:residual` / `:on-expr`；`Filter` 的绑定谓词已转为 `PlanExpr` 并通过 `:pred` 输出，列索引还原为 schema 名；join 的完整 `ON` / `USING` / `NATURAL` 谓词也已转为 `PlanExpr` 并通过 `:pred` 输出，`JoinKeys` / `residual` / `on_expr` 继续保留；`Having` 的 `GroupExpr` 也已转为含聚合调用的 `PlanExpr` 并通过 `:pred` 输出；`Scan` 的表名与别名已拆成 `table` / `alias` 并输出可选 `:alias`，下推到扫描的谓词也已转为 `PlanExpr` 并通过可选 `:filter` 输出；hash join 的 `:residual-pred` 已与历史 `:residual` 标志解耦，同源列等值等非键合取项也会输出；`EXPLAIN ANALYZE SEXPR` 把同一棵执行计划加 `(execution ...)` 汇总，节点只输出 actual 字段。下一步是在 `PlanNode` 上做规则化重写、逻辑/物理计划分层、表达式级列下推与投影下推；join 顺序搜索、全局代价模型和索引仍不做。子查询需要新的作用域和计划层，成本更高。
- `tritdb-plan`：逻辑计划、表达式级列下推、投影下推与更完整的残余谓词分析。
- `tritdb-proto`：pgwire v3 simple query + extended query，先让 `psql` 能连。
- 表存储从整表重写改成页/行格式，接 `tritdb-page`。
- SQL 3VL + GROUP BY + JOIN + DML + 谓词 + CASE 套件扩到 `trit-clean` 的完整原始语义用例（当前 132 条 SELECT + 6 条 UPDATE 行数对拍为本仓自定义对拍集）。
- 并发 SQL 压力测试：多连接 SELECT + 单写者 INSERT，测读者 P99。
