# 兼容与升级

[English](COMPATIBILITY.md)

升级一个工具，不该顺便把项目的启动方式重学一遍。这里约定 v1.0.0 候选与后续
1.x 的兼容范围；写下契约不等于 v1.0.0 已经发布。

## 哪些东西可以依赖

| 接口 | 支持范围 |
| --- | --- |
| CLI | 1.x 保持已文档化命令、参数、实例选择和退出码含义兼容，可增加可选参数与命令。终端文字、颜色和表格排版不适合拿来做解析接口。 |
| YAML | 保留 `version: "1"`、命名 profile、已文档化字段与既有 snake_case 别名。省略 `kind` 仍是 service，简写依赖仍是 started。未知或冲突配置会拒绝。 |
| Agent | JSON-lines schema 1、方法、target/grant 核验与错误码。请求拒绝未知字段；客户端应容忍回复新增字段，先看 `ok`，再看报告结果。 |
| MCP | stdio 协商 `2025-06-18`／`2025-11-25`，保留已文档化工具及参数。协议版本与 devd、证据版本是三回事；工具返回成功仍可能携带失败结果。 |
| 移交报告 | `diagnose`、`compare` 和离线 MCP 接受 export schema 1（在线）、2（历史运行）、3（事故）。内嵌事故 artifact 1/2 保留各自限制，schema 1 没有留存完整关系图。未知 artifact 版本只作未知证据，未知外层 export 版本拒绝。 |
| 其他版本化数据 | 基线、配置快照与恢复配方保留各自版本校验。快照恢复到新配置文件，不恢复进程；基线是元数据证据，不是文件正文备份。 |

新增可选能力不能悄悄开启监测、持久化、Agent 控制、自动恢复或目录清理。
修复可能拒绝过去漏检的畸形或不安全输入，兼容不代表保留 bug。确实需要破坏兼容时，
必须明确改变相应版本并提供迁移说明，不能让旧字段悄悄换个意思。

结构化客户端应忽略新增对象字段、检查 schema，对未知枚举、结果和错误码保守处理。
旧版读取器不保证认识新任务事件或后续 schema；遇到不支持版本，保留原始报告并升级读取器。
缺字段、截断和 unknown 都不能被当成成功。

Rust 库 API、本地 supervisor wire protocol、内部状态文件、计划摘要与磁盘布局不属于
稳定接入接口。外部工具使用 CLI、Agent 或 MCP；活 supervisor 与客户端使用同版二进制，
不把新旧版本混用控制当作升级方式。

## 退出码到底表示什么

| 退出码 | 含义 |
| --- | --- |
| `0` | 命令完成了自己的契约。`coverage`、`diagnose`、`compare` 只是成功产出报告，包含差异或 unknown 也可返回 0，不代表健康、相同或完整。 |
| `1` | 配置、运行或 I/O 失败，或门控命令得到否定结果：wait 失败／超时、纯任务栈失败、baseline check 有变化／未知、重载／清理／恢复未完成。JSON 报告可能已经输出。 |
| `2` | 参数解析阶段发现 CLI 语法或参数值错误。 |

`restart` 确认新尝试，真正就绪或任务完成要再用 `wait`；`stop` 请求停栈，清理／恢复前
还要等前台 `start` 成功退出。操作系统直接用信号终止进程时，状态随平台而异。
Agent/MCP 的单次请求错误放在 JSON 回复里，不对应一次进程退出；`ok` 或工具成功只说明
拿到了报告。检查 `outcome`，启用去重后则检查包裹内的 `result.outcome`。

策略错误码包括 `operation-key-required`、`operation-key-conflict`、`budget-out-of-scope`、
`budget-exhausted`、`deduplication-capacity`、`busy`、`outcome-unknown`；既有授权与目标错误
包括 `permission-denied`、`wrong-instance`、`stale-run`。错误说明文字不作机器契约。

## 从 v0.9 升级

正常停止旧 supervisor，等它退出，保存需要的证据，再替换二进制。对用到的每个 profile
运行 `check`，然后重新 `start`；adapter 重新连接，获取新的 instance/run target。
旧计划与回执不授权操作新运行，磁盘 PID 不会被接管，也不会恢复旧进程。升级本身不要求删除状态。

旧服务 YAML 不必补 task。需要替换构建 wrapper 时，显式用 `kind: task` 与
`condition: completed-successfully`；任务默认不自动重试，也不能混入只适用于长驻服务的
健康／恢复设置。旧输出文件不会放行新一代任务，详见[任务指南](COMMANDS.zh-CN.md#一次性任务)。

旧 Agent 控制默认回复形状保持不变。`--restart-budget` 与 `--deduplicate` 分别由启动者
显式开启，都不增加 grant。去重开启后，客户端须传 `operation_key` 并理解
`{operation_key, operation_status, result?}`；丢回复保持原键和原请求，unknown 先查证据。
保证只在当前会话和保留期内有效，重连会重置预算和键，详见[Agent 指南](COMMANDS.zh-CN.md#给-agent-的实例接口)。

## 怎么验证这份约定

`tests/compatibility.rs` 使用已发布 v0.9.0-alpha.1 macOS 二进制实际导出的在线、历史运行和
事故报告，原始字节保持不变；哈希与来源提交见 `tests/fixtures/compat/v0.9/manifest.json`。
旧 YAML 别名和 profile 也有不执行程序的回归检查。既有诊断测试覆盖旧版缺字段、schema-1
事故、未来版本和不完整证据，原生生命周期测试覆盖任务、重载及控制语义。

[验收说明](../VALIDATION.md) 列出完整场景和固定版本对照。这些是工程证据；独立项目的
实际接入价值仍标为未验证，不能拿测试通过冒充有人持续使用。
