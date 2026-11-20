# ADR-028：把 Scan 的表名与别名分开

状态：Accepted

日期：2026-11-20

> 后续：ADR-029 在 `:filtered` 后新增可选 `:filter`，把下推过滤谓词结构化；本 ADR 的表名/别名字段与文本规则不变。

## 背景

`PlanKind::Scan` 之前只有一个 `label: String`。引擎在构造计划时调用 `source_label(table, qualifier)`，把表名和别名拼成：

- `a`
- `a AS x`

文本 `EXPLAIN` 需要这个标签，但结构化消费者想按表名或别名匹配扫描节点时，仍要解析 `"a AS x"`。这是计划树里最后一个把两个独立字段压进字符串的节点身份。

## 决策

1. `PlanKind::Scan` 的 `label: String` 改为：
   - `table: String`：实际表名；
   - `alias: Option<String>`：显式别名，没有别名或别名等于表名时为 `None`。
2. 文本渲染仍由 `scan_name(table, alias)` 还原：
   - `None` 渲染为 `a`；
   - `Some("x")` 渲染为 `a AS x`；
   - 因此 `EXPLAIN` / `EXPLAIN ANALYZE` 的 `Scan <label>` 逐字不变。
3. `EXPLAIN SEXPR` 的扫描节点把表名作为第一个字符串，别名作为可选字段：

```lisp
(scan "a" :alias "x" :filtered #t :est-rows N :cols ("id" "v"))
```

   - `:alias` 只在存在显式别名时输出；没有别名时形状仍是 `(scan "a" ...)`；
   - 字段顺序固定为 `:alias`、`:filtered`、估算/actual、`:cols`。
4. 本次变化只影响计划数据的表示，不改变源表绑定、别名解析、列裁剪、下推、扫描行数、join 或任何执行路径。
5. `PlanNode` 仍为 crate 内可见；文本消费者不受影响，按旧标签解析 `EXPLAIN SEXPR` 的消费者改用 `:alias`。

## 不做什么

- 不把别名合并进 `:cols` 或列名；`:cols` 仍是扫描节点保留的列名列表。
- 不为没有显式别名的表合成 `:alias`，也不把表名重复成别名。
- 不改变 `FROM a AS a` 这类“别名等于表名”的文本或结构化输出；它继续按无别名处理。
- 不引入新的计划节点、规则 pass 或执行路径。

## 影响

- 规则和脚本可以直接按 `:table` / `:alias` 匹配扫描节点，不必解析 `Scan` 标签。
- 结构化格式新增一个可选字段；只读旧 `(scan "a" ...)` 的消费者继续工作，显式别名下的第一个字符串从 `"a AS x"` 变为 `"a"`，需要读取 `:alias`。
- 文本计划、结果行集、错误语义和测试计数不变。

## 验证

- `EXPLAIN SEXPR SELECT x.v FROM a AS x` 输出 `(scan "a" :alias "x" :cols ("v"))`。
- `EXPLAIN SELECT x.v FROM a AS x WHERE x.id > 1` 的文本仍包含 `Scan a AS x`。
- 无别名扫描继续输出 `(scan "a" ...)`，既有整段 S-expression 断言保持不变。
- `EXPLAIN` / `EXPLAIN ANALYZE` 文本逐字断言保持通过；fmt、Clippy、全量测试与 SQLite 对拍通过。
