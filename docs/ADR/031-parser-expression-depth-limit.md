# ADR-031：限制 SQL 表达式递归嵌套深度

状态：Accepted

日期：2026-11-20

## 背景

SQL parser 是递归下降实现。括号表达式、连续 `NOT` 和连续一元负号都会沿语法层递归调用；输入完全由调用方控制时，深嵌套可以把线程栈打穿，表现为进程级 `STATUS_STACK_OVERFLOW`，而不是可恢复的 SQL 错误。

本切片在 Windows debug 测试中实测：约 256 层括号已经触发栈溢出。只靠“输入很短”或“本地 CLI”不能作为安全边界，parser 需要一个显式的资源上限。

## 决策

1. 在 `Parser` 上增加表达式递归深度计数，上限 `MAX_EXPR_DEPTH = 64`。
2. 守卫覆盖三条实际递归路径：
   - `parse_expr`：括号、`IN` 列表元素、`CASE` 操作数/分支、聚合参数等重新进入表达式解析的入口；
   - `parse_not`：连续 `NOT` 的递归；
   - `parse_unary`：连续一元负号的递归。
3. 超过上限返回 `SqlError::Parse`，消息为 `expression nesting too deep`，位置取当前 token 偏移；不 panic，不 abort，不改变已解析语句。
4. 上限按“递归入口层数”计数，不按 token 数或输入字节数计数；普通查询、现有测试和 SQLite 对拍不受影响。
5. 该守卫只保护 parser 栈深度，不承诺限制所有 SQL 资源；超长 `IN` 列表、超长脚本、超大结果集仍由调用方/执行层承担。

## 不做什么

- 不引入 SQL 输入总字节数、token 总数或结果行数配额；这些是独立的资源治理问题。
- 不重写 parser 为非递归实现，也不改变现有文法、优先级或错误消息格式。
- 不把上限暴露为 SQL 语法或会话参数；当前是进程内固定安全边界。
- 不对执行器表达式求值增加第二套深度限制；本切片只处理解析阶段。

## 影响

- 超过 64 层递归嵌套的 SQL 现在会稳定返回 parse error；此前这类输入在部分平台上可能栈溢出。
- 64 层对正常手写/生成 SQL 足够宽裕；实测 32 层括号仍可解析。
- 深度计数在成功/失败返回时都会递减；守卫只增加常数级计数开销。
- 错误位置指向触发上限的 token，调用方可以按普通 parse error 处理。

## 验证

- `SELECT ((((...1...)))) FROM t;` 超过上限时返回 `expression nesting too deep`。
- 连续 `NOT` 链和连续一元负号链同样被拒绝。
- 32 层括号仍能正常解析。
- CLI 子进程收到 512 层嵌套 SQL 时返回非零退出码和该错误，不触发栈溢出。
- `cargo fmt --all --check`、`cargo clippy --workspace --all-targets -- -D warnings`、`cargo test --workspace`、SQLite 对拍通过。
