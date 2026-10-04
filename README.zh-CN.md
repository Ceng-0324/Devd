# Devd

[English](README.md) · **简体中文**

***这个玩意儿***能把本地开发的一组进程管起来：按依赖启动，看得见状态，找得到日志，退出时一起收好。

说实话，这个东西的灵感，因为一个 **fucking event** 而产生。

我误清了 `tmp` 目录里的文件，Codex 的 `app-server` 随后遇到了 dangling symlink——软链接还在，指向的文件没了。然后，所有会话直接崩掉。

清个临时目录，把整个工作现场清下线了。行。

这次事故让我开始在意开发环境里那些平时没人在意的关系：哪个进程依赖谁，启动了是不是就算能用，出了问题去哪里看，重启之后又该按什么顺序恢复。于是有了 devd：把启动顺序、健康状态和进程生命周期，写进一份能执行的配置里。

修复断掉的文件依赖仍然需要人来处理。devd 负责的是配置中交给它管理的服务：让日常的启动、观察、重启和收尾有个着落。

## 它能做什么

devd 是用 Rust 编写的本地开发服务管理器，当前为 **v0.1 MVP，支持 Linux 和 macOS**。

一份 `devd.yml` 描述服务和依赖，`devd start` 在前台管理它们。你可以继续在另一个终端查状态、翻日志或重启某个服务。

- **按依赖启动**：独立服务并发启动；依赖可以等待进程启动，也可以等待 TCP / HTTP 健康检查通过。
- **观察健康状态**：TCP 连接与 HTTP 2xx 探测，记录连续失败和错误原因。
- **处理异常退出**：支持 `always`、`on-failure`、`never`，使用固定延时和有限的自动重试次数。
- **把日志放到一起**：收集 stdout / stderr，添加时间、服务名和颜色，按服务查询最近的输出。
- **有序收尾**：Ctrl+C、SIGTERM 或 `devd stop` 触发反向依赖关闭，并清理受管进程组里的后代。

适合 API、前端、worker 等需要一起运行的本地项目。服务继续使用自己的启动命令，devd 负责把它们组织起来。

## 先跑起来

需要 Rust 工具链。在仓库根目录安装：

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

| 命令 | 用途 |
| --- | --- |
| `devd start` | 前台启动全栈并实时输出日志 |
| `devd stop` | 请求有序关闭，前台进程完成清理后退出 |
| `devd restart <service>` | 用启动时的配置重启一个服务，重新检查依赖 |
| `devd status [--json]` | 查看实时状态、PID、重启次数和诊断信息 |
| `devd logs [service] [--tail N]` | 查询内存日志，默认 100 条，N 为 1–1000 |
| `devd check` | 校验配置、命令引号、依赖关系及当前支持的设置 |
| `devd graph` | 显示依赖边和并行启动层 |

所有命令共用 `-c / --config <PATH>`、`--state-dir <PATH>` 和 `--color auto|always|never`，选项可以放在子命令前后：

```bash
devd start --config ./devd.local.yml
devd --config ./devd.local.yml status --json
```

## 用之前知道这几件事

**前台运行，项目内通信。** devd 使用 Tokio 管理服务任务，其他终端通过 Unix socket 访问正在运行的实例。默认运行目录是配置目录下的 `.devd/<配置文件名>/`，建议把 `.devd/` 加进项目的 `.gitignore`。socket 路径过长时，可以用较短的 `--state-dir`；同一实例的命令要使用相同参数。

**重启有明确边界。** 手动重启只作用于指定服务，使用本次启动时的配置；成功表示新进程已启动，健康检查可能还在进行。手动操作可以绕过 `never` 和自动重试上限，但累计重启次数不会重置。启动失败或自动重试耗尽等终止性错误会触发全栈清理。

**日志保存在内存里。** 默认保留全栈最近 1000 条，单行最多 16 KiB，停止后不能再通过 `logs` 查询。前台输出过慢时会丢弃部分实时条目并告警；输出管道断开会触发服务清理。

**诊断反映当前实例。** `status` 离线时返回错误，遗留状态文件仅用于诊断。配置内容被改坏后，仍可通过活实例执行 `stop`、`status`、`logs` 和 `restart`。`check` 做静态校验，可执行文件、环境文件和探测端点是否可用，要到运行时确认。命令失败返回非零退出码。

当前范围是本地进程管理。配置生成、资源监控、热重载、指数退避、`logs --follow`、磁盘日志和 TUI 都还在后续规划里。

配置会拒绝未知字段和未实现的 `limits`。YAML 值按字面使用，尚未实现 `${VAR}` 替换。fixed 策略下的 `max-delay` 为预留字段，不改变重试延时。

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

[测试与手动验证](tests/README.md) · [技术方案](TECHNICAL_DESIGN.md) · [架构设计](ARCHITECTURE.md)
