# 提交前安全审查清单

每次改动涉及以下任一区域时逐条核对；编号对应
`docs/security/AUDIT-2026-11-22.md`。

## 解码器 / 序列化
- [ ] 每个长度/计数字段在分配前校验（`count <= bytes.len() / 最小条目大小`），参考 #84、#41
- [ ] magic / version / CRC / trailing bytes 全部校验
- [ ] 新增解码器进入 `crates/tritdb/tests/fuzz_decode.rs`，随机字节 + 强制 magic 两条路径都覆盖
- [ ] 单条消息/记录有大小上限（16 MiB / 64 MiB 级）

## SQL / 执行器
- [ ] 用户输入不进 `format!` 拼接（参数化或转义），参考 #1、#2
- [ ] 行/组级 CPU 与内存预算：LIKE 步数、JOIN 行数、IN 集合、相关子查询行数，参考 #9、#10、#16、#17
- [ ] 写路径的行数上限与读路径一致，参考 #11
- [ ] 查询可被 deadline 取消（`ServerOptions::query_timeout` + `SqlEngine::set_deadline`）

## pgwire
- [ ] 认证比较 constant-time，参考 #5
- [ ] 连接数、prepared、portal、COPY 批、EXPLAIN 回显有上限，参考 #6、#13、#14、#34、#36
- [ ] COPY 解码拒绝不完整/非法多字节序列，参考 #38
- [ ] `Execute` 的 `max_rows` 分页（PortalSuspended）
- [ ] `Describe` 只绑定不执行，参考 #31
- [ ] 参数替换识别注释与字符串，参考 #9 的收尾项

## 存储 / 恢复
- [ ] 路径由 LSN / hash 派生，不拼接用户字符串，参考 #32
- [ ] 崩溃点有测试（reset、checkpoint、跨段恢复）
- [ ] PITR 跨段与 prune 有回归

## CI / 依赖
- [ ] `cargo test --workspace`、`cargo clippy -D warnings`、`cargo fmt --check` 全绿
- [ ] fuzz release 轮次（`TRITDB_FUZZ_ROUNDS=20000`）通过
- [ ] `cargo audit` 无未处理公告
- [ ] workflows 无 `pull_request_target`，`run:` 不拼接不可信输入
