# Devd

[English](README.md) · **简体中文**

***这个玩意儿***能把本地开发的一组进程管起来：按依赖启动，看得见状态，找得到日志，退出时一起收好。

说实话，这个东西的灵感，因为一个 **fucking event** 而产生。

我误清了 `tmp` 目录里的文件，Codex 的 `app-server` 随后遇到了 dangling symlink——软链接还在，指向的文件没了。然后，所有会话直接崩掉。

清个临时目录，把整个工作现场清下线了。行。

这次事故让我开始在意开发环境里那些平时没人在意的关系：哪个进程依赖谁，启动了是不是就算能用，出了问题去哪里看，重启之后又该按什么顺序恢复。于是有了 devd：把启动顺序、健康状态和进程生命周期，写进一份能执行的配置里。

修复断掉的文件依赖仍然需要人来处理。devd 负责的是配置中交给它管理的服务：让日常的启动、观察、重启和收尾有个着落。

## 它能做什么

v1.2.0 补充 registry 与 `cargo-binstall` 安装元数据，源码迁移到 Rust edition 2024，MSRV 保持 1.95。
服务工作流提供有界 Node.js 项目扫描、可审阅的 task → consumer init、可配置依赖就绪期限，
以及原生任务完成门控、诊断盲区、离线报告对比和分别显式开启的 Agent 操作限制。
公共接口按 SemVer 管理，升级边界见[兼容与升级](docs/COMPATIBILITY.zh-CN.md)。
[验收说明](VALIDATION.md) 记录可复现检查与尚缺的用户价值证据；参与开发与安全报告分别见
[贡献指南](CONTRIBUTING.md) 和 [安全政策](SECURITY.md)。

devd 是用 Rust 编写的本地开发服务管理器。**v1.2.0 已发布，支持 Linux、macOS 和 Windows。** [下载二进制及 SHA-256 校验文件](https://github.com/Ceng-0324/Devd/releases/tag/v1.2.0)，提供 Linux x86_64、macOS arm64 和 Windows x86_64 MSVC 归档。

当初那场事故，现在有了一条能查下去的线：声明服务需要哪些文件和软链接，看清解析在哪一环断了、哪些声明服务在下游。重现问题前显式开启事故记录，supervisor 停止后仍能查看和导出证据。后来把目标补回来，也不会改写当时留下的记录。

多个开发现场一起跑时，可以找到各个 worktree 的实例，等待服务真正就绪，再按需把控制权交给 Agent。配置变化先预览再应用；正常停止后，只清理登记过的可丢弃目录。文件监测只报告变化，不授权修复或重启；重载失败会停止全栈，不自动回滚。

多环境配置、配置快照、依赖图、日志筛选、可选磁盘留存和实时终端视图贯穿这套流程：`doctor` 检查环境，`events` 查经过，`explain` 根据记录解释服务故障。

一份 `devd.yml` 描述服务和依赖，`devd start` 在前台管理它们。你可以继续在另一个终端查状态、翻日志或重启某个服务。

- **按依赖启动**：独立服务并发启动；依赖可以等待进程启动，也可以等待 TCP / HTTP / Unix Socket / 脚本健康检查通过。
- **观察健康状态**：TCP、Unix Socket 连接与 HTTP 2xx 探测，记录连续失败和错误原因。Socket 路径相对服务工作目录解析，遗留文件不算健康，必须能连上。
- **处理异常退出**：支持 `always`、`on-failure`、`never`，可选固定延时或指数退避，并限制自动重试次数。
- **把日志放到一起**：收集 stdout / stderr，添加时间、服务名和颜色，按服务查询最近的输出。
- **看看服务吃了多少资源**：`status` 显示每个服务主进程的 CPU 和常驻内存。
- **有序收尾**：Ctrl+C 或 `devd stop` 触发反向依赖关闭，并清理受管后代；Unix 还处理 SIGTERM，Windows 还处理 Ctrl+Break。

适合 API、前端、worker 等需要一起运行的本地项目。服务继续使用自己的启动命令，devd 负责把它们组织起来。

## 先跑起来

需要 Rust 1.95 或更新版本。从仓库根目录源码安装：

```bash
cargo install --path . --locked
```

发布到 crates.io 后，可直接安装已发布的 CLI：

```bash
cargo install devd --locked
```

也可以用 [cargo-binstall](https://github.com/cargo-bins/cargo-binstall) 安装预编译版本：

```bash
cargo binstall devd
```

binstall 元数据会指向对应 GitHub Release 的归档，并从归档根目录安装二进制。
需要本地 Rust 编译时使用 `cargo install`；有可用发布归档时使用 `cargo binstall`。

在项目目录运行 `devd init`，可生成一个能直接启动的配置：Unix 使用 `sh`，Windows 使用系统 PowerShell；已有文件不会被覆盖。

Node.js 项目可以先用 `devd init --scan node --json` 对有界的
`package.json` 做只读扫描，列出候选脚本，再选择构建脚本和消费者脚本生成需要审阅的
task → consumer 配置：

```bash
devd init --scan node --build-script build --consumer-script dev --json
devd init --scan node --build-script build --consumer-script dev --yes
```

扫描不会安装依赖、执行脚本或覆盖已有配置。生成任务最多运行 5 分钟，依赖就绪期限默认
为 10 分钟；用 `--dependency-timeout` 调整依赖期限。构建超过 5 分钟时，启动前还需修改生成的
`task-timeout`。
Windows 生成的 npm、pnpm 和 yarn 调用会使用对应的 `.cmd` 入口。

下面用两个 Unix 演示进程展示完整流程：

```yaml
version: "1"
services:
  worker:
    command: sh -c 'echo worker-ready; exec sleep 3600'
    restart:
      policy: on-failure
      initial-delay: 1s
      max-attempts: 3
  client:
    command: sh -c 'echo client-ready; exec sleep 3600'
    depends-on: [worker]
    restart:
      policy: never
```

```bash
devd check       # 先检查配置
devd graph       # 看依赖关系和启动层次
devd start       # 前台启动，实时输出日志
```

保持这个终端运行，在另一个终端进入同一目录：

```bash
devd status
devd logs worker --tail 50
devd restart worker
devd stop
```

也可以直接在运行 `start` 的终端按 Ctrl+C。等它退出，这组服务就收尾了。

## 换成你的服务

把 `command` 换成项目本来的启动命令即可。比如，一个提供 `/health` 接口的 Node.js API 和依赖它的前端：

```yaml
version: "1"
services:
  api:
    command: npm run dev
    cwd: ./backend
    env-file: .env.local
    listen: [127.0.0.1:3000]
    healthcheck:
      type: http
      url: http://127.0.0.1:3000/health
      interval: 2s
      timeout: 1s
      retries: 3
    restart:
      policy: on-failure
      initial-delay: 1s
      max-attempts: 3
  web:
    command: npm run dev
    cwd: ./frontend
    depends-on:
      - service: api
        condition: http-ready
    restart:
      policy: never
```

这个例子需要你已有 `backend`、`frontend`、对应的 `dev` 脚本和 `backend/.env.local`。API 的健康接口返回 2xx 后，前端才会启动；依赖等待默认最多 30 秒。

`cwd` 相对配置目录，`env-file` 相对服务的 `cwd`。命令没有隐式 shell。关键文件也能声明为启动前置条件，详见[命令指南](docs/COMMANDS.zh-CN.md#文件前置条件与路径规则)。

## 日常就这几步

启动之后，另一个终端里的事情通常很简单：

```bash
devd wait --timeout 30s       # 等到真的能用，再跑测试
devd status                  # 谁在跑，谁出了问题
devd logs api --tail 50       # 看最近的应用输出
devd explain api             # 看记录里的故障线索
devd restart api             # 显式重启这个服务
devd stop                    # 做完了，有序收尾
```

同一个现场要用相同的 `--config`、`--profile` 和 `--state-dir`。重启请求成功只表示进程启动了；需要就绪时，再用 `wait`。

## 出了问题，别只剩一句“挂了”

软链接还在，目标没了。只看到“服务挂了”没什么用，得知道当时哪里断了、哪些声明服务可能受影响，还得让证据活得比现场久一点。

先用 `requires` 声明关键文件或链接，`devd doctor` 看解析链，`devd impact api --require 0` 看声明下游。需要在运行期观察条件变化，显式配置 `monitor-requires: true`；重现之前，再开启事故记录：

```bash
devd start --record-incidents  # 在前台终端运行
```

另一终端等服务就绪，重现问题，再查看留下来的记录：

```bash
devd incidents list
devd incidents show INCIDENT_UUID
devd export --incident INCIDENT_UUID --output incident.json
# 将 JSON 复制到另一台机器或空目录，也能继续查：
devd diagnose incident.json
```

`INCIDENT_UUID` 换成列表里的完整 ID。记录窗口最多持续 5 秒，采集完成后导出；停止后查询加 `--stored`。关系图是声明影响，不能凭连线认定根因；后来修好目标，也不会改写旧证据。

若丢的是可以重建的已登记 instance 目录，审阅一份恢复配方，正常停止全栈、等 supervisor 成功退出，再执行：

```bash
devd recover --recipe recipe.yml --dry-run
devd recover --recipe recipe.yml --apply --plan PLAN_ID
```

`PLAN_ID` 使用这次预览返回的值。命令有你的系统权限，必须自己审阅；恢复不会自动启动服务。核对结果后重新 `start`，再 `wait`。共享资源和未登记目录不接管，修复失败也不装作已经恢复。

[完整事故复现](docs/COMMANDS.zh-CN.md#找到事故并把证据带走) · [恢复配方](docs/COMMANDS.zh-CN.md#执行审阅过的恢复配方)

## 按你现在要做的事找

| 要做什么 | 去哪里看 |
| --- | --- |
| 多个 worktree 一起开发 | [实例发现](docs/COMMANDS.zh-CN.md#实例身份与发现)、[端口与路径](docs/COMMANDS.zh-CN.md#显式配置实例端口与路径) |
| 环境启动、切换与配置调整 | [等待就绪](docs/COMMANDS.zh-CN.md#等待就绪)、[profiles](docs/COMMANDS.zh-CN.md#多环境配置)、[重载](docs/COMMANDS.zh-CN.md#配置变更预览与应用) |
| 构建或迁移完成后再启动下游 | [一次性任务](docs/COMMANDS.zh-CN.md#一次性任务)、[可运行示例](examples/tasks/README.md) |
| 看故障、保存和移交证据 | [覆盖与盲区](docs/COMMANDS.zh-CN.md#看清诊断能覆盖什么)、[报告导出](docs/COMMANDS.zh-CN.md#导出诊断证据)、[事故记录](docs/COMMANDS.zh-CN.md#留下有界的事故证据)、[离线诊断](docs/COMMANDS.zh-CN.md#换台机器继续诊断)、[离线对比](docs/COMMANDS.zh-CN.md#离线对比两份移交报告) |
| 对比资源变化、恢复与清理 | [基线](docs/COMMANDS.zh-CN.md#检查环境漂移)、[恢复配方](docs/COMMANDS.zh-CN.md#执行审阅过的恢复配方)、[清理](docs/COMMANDS.zh-CN.md#清理可丢弃的实例数据) |
| 让工具接入当前现场 | [MCP](docs/COMMANDS.zh-CN.md#接入-mcp-客户端)、[Agent JSON 接口](docs/COMMANDS.zh-CN.md#给-agent-的实例接口) |

全部参数与示例见[命令指南](docs/COMMANDS.zh-CN.md)。平台行为、日志留存和权限边界见[使用须知](docs/USAGE_NOTES.zh-CN.md)。

## 开发与验证

想试一个完整项目，可以从 [API + 网页 + worker 示例](examples/local-stack/README.md) 开始，里面有可重复执行的故障恢复脚本。

准备拿自己的项目试？[真实项目试用指南](https://github.com/Ceng-0324/Devd/blob/dev/docs/FIELD_TESTING.zh-CN.md)说明如何在自己的项目里试用，试完只留一份记录，没跑顺的地方也算结果。

参与开发见[贡献指南](CONTRIBUTING.md)，专项检查见[测试与手动验证](tests/README.md)。发布流程见[发布清单](RELEASING.md)，漏洞报告见[安全政策](SECURITY.md)。

[变更记录](CHANGELOG.md) · [MIT 许可证](LICENSE) · [架构设计](ARCHITECTURE.md)
