# Devd

[English](README.md) · **简体中文**

***这个玩意儿***能把本地开发的一组进程管起来：按依赖启动，看得见状态，找得到日志，退出时一起收好。

说实话，这个东西的灵感，因为一个 **fucking event** 而产生。

我误清了 `tmp` 目录里的文件，Codex 的 `app-server` 随后遇到了 dangling symlink——软链接还在，指向的文件没了。然后，所有会话直接崩掉。

清个临时目录，把整个工作现场清下线了。行。

这次事故让我开始在意开发环境里那些平时没人在意的关系：哪个进程依赖谁，启动了是不是就算能用，出了问题去哪里看，重启之后又该按什么顺序恢复。于是有了 devd：把启动顺序、健康状态和进程生命周期，写进一份能执行的配置里。

修复断掉的文件依赖仍然需要人来处理。devd 负责的是配置中交给它管理的服务：让日常的启动、观察、重启和收尾有个着落。

## 它能做什么

devd 是用 Rust 编写的本地开发服务管理器。**v0.2.0-alpha.1 是基于 v0.1 MVP 的预发布版，支持 Linux 和 macOS**。

当前开发分支还增加了 `status` 的 CPU／内存采样、可选的依赖恢复联动重启，以及 v0.3 的首个模块多环境配置；这些能力尚未包含在 alpha.1 发布的二进制中。

一份 `devd.yml` 描述服务和依赖，`devd start` 在前台管理它们。你可以继续在另一个终端查状态、翻日志或重启某个服务。

- **按依赖启动**：独立服务并发启动；依赖可以等待进程启动，也可以等待 TCP / HTTP / Unix Socket 健康检查通过。
- **观察健康状态**：TCP、Unix Socket 连接与 HTTP 2xx 探测，记录连续失败和错误原因。Socket 路径相对服务工作目录解析，遗留文件不算健康，必须能连上。
- **处理异常退出**：支持 `always`、`on-failure`、`never`，可选固定延时或指数退避，并限制自动重试次数。
- **把日志放到一起**：收集 stdout / stderr，添加时间、服务名和颜色，按服务查询最近的输出。
- **看看服务吃了多少资源**：`status` 显示每个服务主进程的 CPU 和常驻内存。
- **有序收尾**：Ctrl+C、SIGTERM 或 `devd stop` 触发反向依赖关闭，并清理受管进程组里的后代。

适合 API、前端、worker 等需要一起运行的本地项目。服务继续使用自己的启动命令，devd 负责把它们组织起来。

## 先跑起来

需要 Rust 1.95 或更新版本。在仓库根目录安装：

```bash
cargo install --path . --locked
```

找一个项目目录，创建 `devd.yml`。先用两个只打印消息、然后等待的演示进程，看看完整流程：

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

`cwd` 相对配置文件所在目录，`env-file` 相对服务的 `cwd`。显式配置的 `env` 会覆盖环境文件里的同名值。命令支持带引号的参数；需要管道、重定向或 shell 展开时，显式使用 `sh -c '...'`。

## 常用命令

### 多环境配置

环境之间的差异可以留在同一份 YAML 里：

```yaml
version: "1"
services:
  app:
    command: npm run dev
    env: {MODE: dev, LOG_LEVEL: info}
profiles:
  staging:
    services:
      app:
        command: npm run staging
        env: {MODE: staging}
```

`devd check --profile staging` 检查合并后的配置，`devd start --profile staging` 启动它。这里的 `LOG_LEVEL` 会从基础配置继承。查看状态、读日志、重启、停止时，使用同一个 `--profile`；不指定时选择基础配置及其独立实例。这是 v0.3 开发中的第一个模块，alpha.1 发布二进制尚未包含。

服务按名称合并。`env` 和 `restart` 按字段覆盖，其余字段整体替换，包括依赖列表和健康检查。`cwd`、`env-file`、`healthcheck` 等可选字段可以用 `null` 清除；空映射表示继承，`depends-on: []` 则清空依赖。新增服务必须有命令；暂不支持删除服务或 profile 之间的继承。未选中的 profile 也会检查未知字段，依赖及就绪条件按所选结果校验。不带 profile 的 `check` 检查基础配置。

路径仍沿用配置目录和服务 `cwd` 的相对路径规则。profile 名称以 ASCII 字母、数字或下划线开头，只允许 ASCII 字母、数字、`_`、`-`、`.`，区分大小写。默认运行目录为 `.devd/<配置文件名>/profiles/<名称>/`，大写字母转义为 `~hh`，避免大小写不敏感文件系统上的实例碰撞；显式 `--state-dir` 同样追加 `profiles/<名称>/`。隔离的是控制端点和状态文件，服务端口、应用文件仍需自行配置不同值。`init` 不接受 `--profile`。

可以直接试 [dev / staging / prod 示例](examples/profiles/README.md)。

### 命令速查

| 命令 | 用途 |
| --- | --- |
| `devd start` | 前台启动全栈并实时输出日志 |
| `devd stop` | 请求有序关闭，前台进程完成清理后退出 |
| `devd restart <service>` | 用启动时的配置重启一个服务，重新检查依赖 |
| `devd status [--json]` | 查看实时状态、PID、CPU／RSS、重启次数和诊断信息 |
| `devd logs [service] [--tail N] [--follow]` | 查询内存日志（默认 100 条，N 为 1–1000），可持续接收新日志 |
| `devd check` | 校验配置、命令引号、依赖关系及当前支持的设置 |
| `devd graph` | 显示依赖边和并行启动层 |
| `devd init [--service NAME] [--command CMD]` | 创建通过校验的初始配置，不覆盖现有文件 |

命令共用 `-c / --config <PATH>`、`--profile <NAME>`（`init` 除外）、`--state-dir <PATH>` 和 `--color auto|always|never`，选项可以放在子命令前后：

```bash
devd start --config ./devd.local.yml
devd --config ./devd.local.yml status --json
```

## 用之前知道这几件事

**前台运行，项目内通信。** devd 使用 Tokio 管理服务任务，其他终端通过 Unix socket 访问正在运行的实例。默认运行目录是配置目录下的 `.devd/<配置文件名>/`，建议把 `.devd/` 加进项目的 `.gitignore`。socket 路径过长时，可以用较短的 `--state-dir`；同一实例的命令要使用相同参数。

**重启有明确边界。** 手动重启以指定服务为目标，使用本次启动时的配置；成功表示目标的新进程已启动，健康检查和已开启的依赖联动可能还在进行。手动操作可以绕过 `never` 和自动重试上限，但累计重启次数不会重置。启动失败或自动重试耗尽等终止性错误会触发全栈清理。

**依赖回来了，服务也可以跟着重启。** 对已经声明 `depends-on` 的服务，加上：

```yaml
restart-on-dep-recovery: true
restart:
  policy: on-failure
  initial-delay: 1s
  max-attempts: 3
```

默认关闭。开启后，直接依赖换了进程，且所有依赖重新满足各自的就绪条件，才会重启当前正在运行的服务。依赖的手动重启和自动恢复都适用；同一进程从不健康变健康不触发。依赖不可用期间，当前服务继续运行。首次启动不补一次重启，已经正常结束的服务也不会被重新拉起。沿依赖链传播时，每一层都需要自行开启。

联动复用服务的退避设置，与其他重启共用累计 `max-attempts` 预算；耗尽后清理全栈。与 `policy: never` 同时启用、或未声明依赖，会在配置检查时明确报错。下一次启动就绪检查完成前观察到的多次恢复合并为一次重启；之后依赖再次变化，仍可能再触发一次，各服务独立恢复，不是整张依赖图的原子重启。停止可打断等待，手动重启当前服务可接管待执行的联动退避。

**日志保存在内存里。** 默认保留全栈最近 1000 条，单行最多 16 KiB，停止后不能再通过 `logs` 查询。`logs --follow` 先输出指定条数的历史日志，再持续接收新日志，按 Ctrl+C 或 supervisor 停止时退出；跟随者落后过多会报错退出。最多同时连接 16 个跟随者，为控制命令保留连接槽位。前台输出过慢时会丢弃部分实时条目并告警；前台输出管道断开会触发服务清理。

**诊断反映当前实例。** `status` 离线时返回错误，遗留状态文件仅用于诊断。配置内容被改坏后，仍可通过活实例执行 `stop`、`status`、`logs` 和 `restart`。`check` 做静态校验，可执行文件、环境文件和探测端点是否可用，要到运行时确认。命令失败返回非零退出码。

**资源指标只统计服务主进程。** supervisor 大约每秒采样一次，不累加 shell 或包管理器启动的子进程。CPU 以一个核心满载为 100%，多线程进程可以超过 100%；内存为 RSS，终端以 MiB 显示。不可用的指标显示 `-`，CPU 在启动或重启后需要两次成功采样。JSON 的可选 `resources` 包含 `cpu_percent`（预热时为 `null`）、`memory_bytes` 和 `sampled_at`。进程退出后清除指标；采样只用于观察，`limits` 仍不支持。

当前范围是本地进程管理。`init` 可生成初始配置；项目扫描和交互式模板仍在规划。热重载、磁盘日志和 TUI 也在后续规划里。

配置会拒绝未知字段和未实现的 `limits`。YAML 值按字面使用，尚未实现 `${VAR}` 替换。`backoff: exponential` 从 `initial-delay` 开始，随累计重启次数翻倍，到 `max-delay` 封顶（默认 60s，不能小于初始延时）；健康检查成功不会重置计数。fixed 不使用 `max-delay`；两种等待均可被停止操作中断。

## 开发与验证

[API + 网页 + worker 示例](examples/local-stack/README.md)提供了一套可以直接运行的服务，
附带可重复执行的故障恢复验证脚本。

```bash
cargo fmt --all -- --check
cargo test --locked --all-targets
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
```

端到端测试会启动真实子进程，验证依赖就绪、故障恢复、日志、状态和关闭后的进程清理：

```bash
cargo test --locked --test integration
```

[测试与手动验证](tests/README.md) · [发布清单](RELEASING.md) · [变更记录](CHANGELOG.md) · [MIT 许可证](LICENSE) · [架构设计](ARCHITECTURE.md)
