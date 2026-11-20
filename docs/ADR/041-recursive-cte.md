# ADR 041：递归 CTE（`WITH [RECURSIVE]`）

状态：**Proposed**

日期：2026-11-21

## 背景

`tritdb-sql` 的 parser 只支持单层 `SELECT`；M5 的图能力（`tritdb-exec::graph`）只能在 CLI 里手工跑，无法用 SQL 表达「沿边递归展开」。需要最小可用的 `WITH RECURSIVE`：锚点 + 递归项，迭代到不动点。

## 决策

1. **语法**
   ```sql
   WITH [RECURSIVE] name [(col, ...)] AS (seed [UNION [ALL] step]) [, ...]
   SELECT | INSERT | UPDATE | DELETE
   ```
   - `seed` / `step` 是 `SELECT` 或嵌套 `WITH`；`UNION` 可省略（普通 CTE），省略时只执行 `seed`。
   - `FROM` 可省略（`SELECT 1` 作为锚点），此时扫描一张零列单行虚拟表。
   - 递归项通过 CTE 名引用自身；外层查询可以像普通表一样 `FROM name`。
   - body 可以是 `SELECT`、嵌套 `WITH`、`INSERT`、`UPDATE`、`DELETE`；DDL 被拒绝。

2. **执行语义（working table）**
   - 递归项只看到**上一轮新产生的行**，不是全部累积结果——这是 SQL 标准语义，也是 `UNION ALL` 能终止的前提。
   - `UNION`：新行与累积结果去重；`UNION ALL`：保留重复。
   - 下一轮无新行即到不动点。
   - 上限：`MAX_RECURSIVE_ITERATIONS = 1000` 轮、`MAX_RECURSIVE_ROWS = 100_000` 行；任一超限返回 `Unsupported` 而不是 OOM。

3. **物化**
   - CTE 被物化成内存 `Table`：列名取显式列表，否则取 seed 输出的列名；列类型从 seed 结果每列第一个非 `NULL` 值推断（全 `NULL` 时按 `TEXT`）。
   - 执行期间临时注入 `SqlEngine.tables`，外层查询结束后恢复同名真实表（`with_query` 用 `saved` 栈保存/恢复）。同名 CTE 会遮蔽真实表，仅在该语句内生效。
   - 嵌套 `WITH` 通过 `Query { Select, With }` 递归求值，seed/step/body 都可以是 `WITH` 查询。

4. **`INSERT ... SELECT`**
   - `INSERT INTO t [(cols)] SELECT ...` / `WITH ...`：先执行查询得到行，再按目标列做类型 `coerce` 与 `NOT NULL` 检查，最后走与 `VALUES` 相同的约束、唯一性与落盘路径。

## 理由

- **working table** 是标准语义，直接决定了树形 `UNION ALL` 能否终止；实现成累积结果会让常见的父子展开无限重复。
- **临时注入 catalog** 复用现有的 `select`/`build_from`/`bind_select` 全链路，不需要给执行器加「外部表」参数。
- **显式上限**：递归 CTE 是用户可控的死循环来源，必须硬失败。

## 后果 / 限制

- `UPDATE` / `DELETE` 的 `WHERE` 里还没有子查询，所以 CTE 实际上只能在 `SELECT` 与 `INSERT ... SELECT` 中被消费；`WITH ... UPDATE/DELETE` 语法可用，但 CTE 内容不影响结果。
- `WITH` 不能出现在子查询里（当前没有子查询语法）。
- 无 `FROM` 的 `SELECT` 等价于零列单行表，只适合常量种子。
- 类型推断是启发式的：列类型取第一个非 `NULL` 值，混类型列会在后续比较时报错。
- `UNION` 去重是 `Vec::contains` 线性扫描；`MAX_RECURSIVE_ROWS` 内可用，不是为大规模图设计的。
- CTE 结果不落盘，也不产生统计信息。

## 验收

```bash
cargo test -p tritdb-sql --lib parses_
cargo test -p tritdb --test sql_cli with_recursive
cargo test --workspace
cargo clippy --workspace --all-targets -- -D warnings
cargo fmt --all -- --check
```

- parser：`WITH RECURSIVE` 单 CTE、`UNION ALL`、多 CTE、嵌套 `WITH`；
- 执行：环形图 `UNION` 去重到 `{0,1,2,3}`、树形 `UNION ALL` 不重复、列数不匹配报错、无不动点触发迭代上限；
- 语法缺口：`SELECT 1` 无 `FROM` 锚点、嵌套 `WITH`、`WITH ... INSERT/UPDATE/DELETE`、`INSERT ... SELECT`、DDL body 被拒绝。
