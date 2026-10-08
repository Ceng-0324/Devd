# Devd

[English](README.md) · **简体中文**

***这个玩意儿***能把本地开发的一组进程管起来：按依赖启动，看得见状态，找得到日志，退出时一起收好。

说实话，这个东西的灵感，因为一个 **fucking event** 而产生。

我误清了 `tmp` 目录里的文件，Codex 的 `app-server` 随后遇到了 dangling symlink——软链接还在，指向的文件没了。然后，所有会话直接崩掉。

清个临时目录，把整个工作现场清下线了。行。

这次事故让我开始在意开发环境里那些平时没人在意的关系：哪个进程依赖谁，启动了是不是就算能用，出了问题去哪里看，重启之后又该按什么顺序恢复。于是有了 devd：把启动顺序、健康状态和进程生命周期，写进一份能执行的配置里。

修复断掉的文件依赖仍然需要人来处理。devd 负责的是配置中交给它管理的服务：让日常的启动、观察、重启和收尾有个着落。

## 它能做什么

devd 是用 Rust 编写的本地开发服务管理器。**v0.6.0-alpha.1 已发布，支持 Linux、macOS 和 Windows。**

v0.6 开始能提前发现当初那场事故里的问题：启动前声明必需的文件、目录和软链接，运行中可以显式开启监测；改了配置，先看影响范围，再明确应用审阅过的计划。文件监测只报告变化，不会修文件，也不等于授权重启。重载失败会停止全栈，不自动回滚。发布要求同一源码提交通过 Linux、macOS 和 Windows 原生 CI 与归档核验。

可以从 [GitHub Releases](https://github.com/Ceng-0324/Devd/releases/tag/v0.6.0-alpha.1) 下载 Linux x86_64、macOS Apple Silicon、Windows x86_64 制品及 SHA-256 校验文件。发布源码提交 `e9ca335` 已通过三平台原生 CI 和归档二进制故障恢复演练。

v0.5 补上的诊断工具也贯穿这套流程：`events` 查经过，`explain` 根据记录解释故障，`doctor` 在启动前检查环境，`listen` 声明服务自己的 TCP 监听端口。

已有的可选磁盘日志、交互式终端界面、CPU／内存采样、资源告警与显式授权的自动恢复、自定义脚本健康检查、依赖恢复联动、多环境配置、配置快照、依赖图导出和日志筛选继续保留。

一份 `devd.yml` 描述服务和依赖，`devd start` 在前台管理它们。你可以继续在另一个终端查状态、翻日志或重启某个服务。

- **按依赖启动**：独立服务并发启动；依赖可以等待进程启动，也可以等待 TCP / HTTP / Unix Socket / 脚本健康检查通过。
- **观察健康状态**：TCP、Unix Socket 连接与 HTTP 2xx 探测，记录连续失败和错误原因。Socket 路径相对服务工作目录解析，遗留文件不算健康，必须能连上。
- **处理异常退出**：支持 `always`、`on-failure`、`never`，可选固定延时或指数退避，并限制自动重试次数。
- **把日志放到一起**：收集 stdout / stderr，添加时间、服务名和颜色，按服务查询最近的输出。
- **看看服务吃了多少资源**：`status` 显示每个服务主进程的 CPU 和常驻内存。
- **有序收尾**：Ctrl+C 或 `devd stop` 触发反向依赖关闭，并清理受管后代；Unix 还处理 SIGTERM，Windows 还处理 Ctrl+Break。

适合 API、前端、worker 等需要一起运行的本地项目。服务继续使用自己的启动命令，devd 负责把它们组织起来。

## 先跑起来

需要 Rust 1.95 或更新版本。在仓库根目录安装：

```bash
cargo install --path . --locked
```

在项目目录运行 `devd init`，可生成一个能直接启动的配置：Unix 使用 `sh`，Windows 使用系统 PowerShell；已有文件不会被覆盖。下面用两个 Unix 演示进程展示完整流程：

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

文件前置条件与应用健康检查分开声明。服务依赖就绪后、进程启动前会立即检查：

```yaml
services:
  api:
    command: npm run dev
    cwd: ./backend
    requires:
      - {type: file, path: .env.local}
      - {type: directory, path: uploads}
      - {type: symlink, path: current-data}
```

`file` 要求路径是可读普通文件，`directory` 要求是可访问目录，`symlink` 要求链接指向现存文件或目录。相对路径按服务生效后的 `cwd` 解析，也可以使用绝对路径。条件不满足时该服务不会启动，并按正常规则清理整组服务。`devd doctor` 使用相同检查，不启动服务，也不读取文件内容。

需要在启动后观察这些条件时，在 `requires` 列表非空的服务上设置 `monitor-requires: true`。默认是 `false`，profile 可以再次关闭，也允许配合 `restart.policy: never` 使用。每次采样完成后间隔一秒，连续两次得到相同变化才报告。条件失效、失效原因变化和恢复会写入 `path-condition-changed` 事件及告警/恢复日志，重复结果不会刷屏。轮询可能错过短暂变化；文件或软链接换成另一个仍满足条件的对象，也不会产生事件。

用 `devd events api --type path-condition-changed --json` 查看事件，或用 `devd explain api` 查看该进程代次每条条件的最近观测；已恢复时也会引用此前的失效证据。这些观测不改变应用健康状态、不重启服务，也不修改文件，更不能单凭发生顺序断定应用故障的原因。服务停止或换代时，旧进程的监测随之结束。需要停止后保留证据，仍须在启动时显式加 `--persist-events`，之后用 `events --stored` 或 `explain api --stored` 查询。

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

`devd check --profile staging` 检查合并后的配置，`devd start --profile staging` 启动它。这里的 `LOG_LEVEL` 会从基础配置继承。查看状态、读日志、重启、停止时，使用同一个 `--profile`；不指定时选择基础配置及其独立实例。

服务按名称合并。`env` 和 `restart` 按字段覆盖，其余字段整体替换，包括依赖列表、路径前置条件和健康检查。`cwd`、`env-file`、`healthcheck` 等可选字段可以用 `null` 清除；空映射表示继承，`depends-on: []` 和 `requires: []` 分别清空对应列表。新增服务必须有命令；暂不支持删除服务或 profile 之间的继承。未选中的 profile 也会检查未知字段，依赖及就绪条件按所选结果校验。不带 profile 的 `check` 检查基础配置。

路径仍沿用配置目录和服务 `cwd` 的相对路径规则。profile 名称以 ASCII 字母、数字或下划线开头，只允许 ASCII 字母、数字、`_`、`-`、`.`，区分大小写。默认运行目录为 `.devd/<配置文件名>/profiles/<名称>/`，大写字母转义为 `~hh`，避免大小写不敏感文件系统上的实例碰撞；显式 `--state-dir` 同样追加 `profiles/<名称>/`。隔离的是控制端点和状态文件，服务端口、应用文件仍需自行配置不同值。`init` 不接受 `--profile`。

可以直接试 [dev / staging / prod 示例](examples/profiles/README.md)，其中也有依赖图导出、日志筛选和快照恢复的完整步骤。

### 配置快照

改配置前先留一份。需要回退时，恢复成新文件，再检查它：

```bash
devd snapshot save before-refactor
devd snapshot restore before-refactor --output devd.recovered.yml
devd check --config devd.recovered.yml
```

快照原样复制磁盘上的整份 YAML，包括所有 profile；默认保存在 `.devd/<配置文件名>/snapshots/<名称>.yml`，也可以用 `--state-dir` 指定存放目录。恢复时沿用相同的 `--config` 和 `--state-dir`，即使原配置已被删除也可以。名称长度为 1–64，只允许小写 ASCII 字母、数字、`_`、`-`、`.`，且首字符只能是字母、数字或 `_`。

`--output` 只能是原配置目录里的新文件名，这样相对 `cwd` 和环境文件路径不会变义。保存和恢复都不覆盖已有文件。快照保留原始字节，不做配置校验；使用恢复文件前请运行 `check`。它不保存运行状态，不热重载正在运行的 supervisor，也不启动或接管旧进程。快照包含全部 profile，因此 `snapshot` 不接受 `--profile`。

### 依赖图

`graph` 默认输出依赖列表和并行启动层。需要可视化时，可以导出 Graphviz DOT 或 Mermaid 源码：

```bash
devd graph --format dot > dependencies.dot
dot -Tsvg dependencies.dot -o dependencies.svg # 已安装 Graphviz 时
devd graph --profile staging --format mermaid > dependencies.mmd
```

图中的箭头从前置服务指向依赖它的服务，边上的标签是就绪条件；没有依赖边的服务也会显示。`graph` 会校验配置，但不启动服务或创建运行状态。DOT 和 Mermaid 输出是供相应渲染器使用的源码，不是图片文件。

### 配置变更预览与应用（v0.6）

改一个服务，可能牵动半个栈。动进程之前，先让正在运行的 supervisor 列出影响：

```bash
devd reload --dry-run
devd reload --dry-run --candidate devd.next.yml --json
devd reload --dry-run --profile staging --candidate devd.next.yml
```

使用与启动时相同的 `--config`、`--profile` 和 `--state-dir` 定位实例。比较基线是 supervisor 内存里保留的有效配置，磁盘 YAML 改了也不会偷换基线。候选默认取 `--config`；`--candidate` 可指定另一份文件，不改变连接的实例。候选里的相对 `cwd` 按候选文件自己的目录解析，并使用该实例已有的 profile；候选必须包含这个 profile。预览需要活实例和不超过 1 MiB 的可读普通 YAML 文件。

报告区分 `added`、`removed`、`modified`、`dependency-affected` 和 `unchanged`，列出变化字段，以及哪些直接变更的前置服务带来了影响。沿旧、新两张依赖图追踪全部下游，不受 `restart-on-dep-recovery` 开关限制。预计顺序是先按旧图逆序分层停止受影响服务，再按新图正序分层启动；无关服务不进入这两份列表。成功报告只列字段名，不输出配置值、命令正文或环境内容；校验错误沿用通常的配置诊断。

`--json` 返回 `schema_version: 1`、supervisor 的 `run_id`、旧/新有效配置摘要、进程代次和 `plan_id`。确认影响后，用这次预览的 ID 和同一候选文件显式应用：

```bash
devd reload --apply --plan 'sha256:<64位十六进制摘要>' --candidate devd.next.yml
# 加 --json 可取得执行报告，包括部分失败和中断。
```

应用时重新读取、校验候选，并对活实例重新计算计划。配置、进程代次或生命周期状态变化都会使 ID 过期，需要重新预览；实例中的服务也必须处于稳定状态。`apply_available: true` 表示支持应用，不保证稍后执行必然成功。实例/profile、状态目录和 supervisor 日志选项固定。候选只更新内存中的有效配置，不改写源 YAML，也不改变默认候选路径。

先按旧图逆序停止受影响服务，等旧 actor 和进程组清理完成，再切换配置基线、按新图正序分层启动。有健康检查的服务必须通过检查，其余服务必须启动；每层沿用依赖超时（30 秒）。无关服务保留原进程，等价配置不会重启任何服务。执行期间拒绝手动 restart 和第二次 reload；受影响服务暂停自动恢复，成功后恢复新配置中的策略，无关服务继续按原策略运行。全栈 stop 可以抢占任意阶段。

失败会停止全栈，包括无关服务，不自动回滚。配置一旦切换，新的内存基线保持到 supervisor 退出。执行报告列出已完成停止、观测到的启动、已就绪的层和 `config_committed`；失败或中断返回非零退出码。生命周期事件记录计划、结果及进度，可通过 `events` 查询，启用事件持久化后也可用 `--stored`。客户端断开或等待响应超过 60 秒不会取消已受理的重载，重试前先检查 status/events。

`--dry-run` 与 `--apply` 必须二选一。预览校验 YAML、命令引号、设置和依赖图，不执行服务/探测命令、读取 dotenv 正文、检查实时路径/端口或写运行状态。比较范围只有 YAML 定义；dotenv、继承环境和程序文件内容变化需要显式 restart。文件监听自动重载仍后置。

### 命令速查

| 命令 | 用途 |
| --- | --- |
| `devd start [--persist-logs] [--persist-events]` | 前台启动全栈，可选保留日志和生命周期事件，分别设置大小与保留数 |
| `devd stop` | 请求有序关闭，前台进程完成清理后退出 |
| `devd restart <service>` | 用当前有效配置重启一个服务，重新检查依赖 |
| `devd reload --dry-run / --apply --plan ID [--candidate PATH] [--json]` | 预览或显式应用受影响服务的配置变化（v0.6） |
| `devd status [--json]` | 查看实时状态、PID、CPU／RSS、重启次数和诊断信息 |
| `devd top` | 在交互式终端查看运行中的服务和实时日志 |
| `devd events [service] [--type TYPE] [--since DURATION] [--tail N] [--cursor RUN_UUID:NEXT_SEQUENCE] [--json] [--follow \| --stored]` | 查询生命周期经过、游标与历史缺口 |
| `devd explain <service> [--json] [--stored]` | 基于确定性事件证据解释一个服务最近的故障或状态 |
| `devd doctor [--json]` | 检查服务启动前置条件，不启动服务 |
| `devd logs [service] [--tail N] [--level info|warn|error] [--since DURATION] [--grep TEXT] [--follow \| --stored]` | 查询内存或离线磁盘日志，跟随实时输出 |
| `devd check` | 校验配置、命令引号、依赖关系及当前支持的设置 |
| `devd graph [--format text|dot|mermaid]` | 显示依赖边与启动层，或导出依赖图 |
| `devd init [--service NAME] [--command CMD]` | 创建通过校验的初始配置，不覆盖现有文件 |
| `devd snapshot save <NAME>` | 保存磁盘上整份 YAML 到项目状态目录 |
| `devd snapshot restore <NAME> --output <FILENAME>` | 恢复成原配置目录中的新文件 |

命令共用 `-c / --config <PATH>`、`--profile <NAME>`（`init`、`snapshot` 除外）、`--state-dir <PATH>` 和 `--color auto|always|never`，选项可以放在子命令前后：

```bash
devd start --config ./devd.local.yml
devd --config ./devd.local.yml status --json
```

## 用之前知道这几件事

**前台运行，项目内通信。** devd 使用 Tokio 管理服务任务，其他终端通过 Unix socket 或仅允许当前用户访问的 Windows 本地命名管道连接实例。默认运行目录是配置目录下的 `.devd/<配置文件名>/`，建议把 `.devd/` 加进项目的 `.gitignore`。Unix socket 路径过长时，可以用较短的 `--state-dir`；同一实例的命令要使用相同参数。

**Windows 有自己的进程收尾方式。** 支持 Windows 10/11、Windows Server 2016+；每代服务在开始执行前就归入独立 Job Object，后代一起受管。停止时先向服务自己的控制台进程组发送 Ctrl+Break，宽限期结束后终止整个 Job；无控制台服务直接终止。应用需要处理 Ctrl+Break 才能优雅退出。devd 被关闭或强杀时，Job 也会关闭并清理后代。TCP、HTTP、脚本探测、`top`、profile、快照、资源控制和磁盘日志使用相同命令；Windows 下 `check`、`start` 会拒绝 Unix socket 探测，请换成 TCP 或脚本。TUI 需要交互式控制台。

Windows 命令也使用 shell 风格引号。YAML 路径优先写 `/`，有空格的程序路径需要加引号，例如 `command: "'C:/Program Files/Python/python.exe' app.py"`。需要脚本解释器时显式调用 `powershell.exe -NoProfile -File script.ps1` 或 `cmd.exe /C ...`。Windows 快速体验只需 `devd init`，然后一个终端运行 `devd start`，另一个终端执行 `devd status`、`devd top` 或 `devd stop`。

**健康检查可以用自己的命令扩展。** 端口能连、HTTP 返回正常，还不足以证明业务就绪时，用 `type: script` 执行自定义检查。任意语言的脚本或可执行程序都可以，退出码 `0` 表示健康。假设应用已提供 `scripts/check_ready.py`：

```yaml
services:
  api:
    command: python3 app.py
    cwd: backend
    env-file: .env
    healthcheck:
      type: script
      command: python3 scripts/check_ready.py
      interval: 5s
      timeout: 2s
      retries: 3
  web:
    command: npm run dev
    cwd: frontend
    depends-on:
      - service: api
        condition: script-ready
```

声明脚本探测即允许它以 devd 当前用户身份执行。它继承服务的工作目录与环境，优先级为显式 `env` 高于 `env-file`，再高于继承环境；每次探测都会重新读取环境文件。命令使用与服务相同的参数引用规则，不隐式经过 shell；需要管道或 shell 展开时显式写 `sh -c '...'`。标准输入关闭，标准输出和错误输出丢弃；退出状态决定健康，失败原因显示在 `status` 中。

首次立即探测，后续串行执行。默认 `interval: 10s`、`timeout: 2s`、`retries: 3`。非零退出、信号退出、执行失败或超时均计为一次失败，成功清零；达到失败阈值后沿用服务的重启策略。`script-ready` 等待第一次成功，前置服务必须配置脚本探测。超时涵盖环境加载、执行及正常结束时的进程树清理。探测超时、取消、服务重启或关闭时，会终止探测的 Unix 进程组或 Windows Job；Unix 探测子进程应保留在该组内。正常结束也会清理后台后代。`check` 和 `graph` 只校验，不执行探测；profile 仍整体替换健康检查。

**重启有明确边界。** 手动重启以指定服务为目标，使用当前有效配置，包括已经提交的重载；成功表示目标的新进程已启动，健康检查和已开启的依赖联动可能还在进行。手动操作可以绕过 `never` 和自动重试上限，但累计重启次数不会重置。启动失败或自动重试耗尽等终止性错误会触发全栈清理。

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

**出事之后，至少要知道发生了什么。** `devd events` 记录启动、退出、依赖、健康、资源和重启决策，带运行 ID、进程代次与因果引用。

```bash
devd events api --type restart-decision --since 10m --tail 20
devd events --json --follow
devd start --persist-events --event-max-size 10 --event-keep 3
# 停止后查询：
devd events --stored --json
```

全栈内存保留 1024 条事件，单条序列化上限 16 KiB。`--tail` 默认 100，范围 1–1000，先筛选再取尾部。`--type` 可重复指定，命中任意一个类型即保留；服务名精确匹配，`--since` 在命令开始时固定包含边界的 UTC 阈值。顺序以序号为准，不受系统时间回拨影响。在线查询不读取 YAML；日志和事件共用最多 16 个跟随连接，Ctrl+C 只退出查询。

`--json` 返回带 schema 版本的批次，包含 `source`、`context`、`entries`、`gaps`、`first_available`、`cursor` 和在线磁盘状态 `persistence`（`disabled`／`recording`／`failed`）。跟随模式每行一个 JSON 批次，只更新水位或磁盘状态时事件列表可以为空。`--cursor RUN_UUID:NEXT_SEQUENCE` 从指定序号开始，包含该序号；返回游标指向下一个待检查位置，即使筛选没有命中也会前进。首批历史与订阅原子衔接，后续跟随批次的 `first_available` 为空。历史淘汰、运行变更、尾部截取都显式报告缺口；超前游标报错。慢订阅者报告缺口后以错误退出，可用最后游标重连，恢复仍在内存中的记录。拿旧运行游标查询新运行时，会返回新运行并标出运行变更。

事件写盘需要单独开启 `--persist-events`，放在实例/profile 的 `events/` 目录，与 `logs/` 独立，共用文件锁、安全访问和轮转规则。默认每文件 10 MiB、3 份归档，共 40 MiB；`--event-max-size` 支持 1–1024 MiB，`--event-keep` 支持 1–100，均要求显式开启。启动时无法打开存储则不启动服务。**运行中事件写盘失败，会停用本次磁盘事件记录，在 stderr 和在线事件查询中报告，服务继续运行。** 重新以持久化参数启动 supervisor 才会重试磁盘记录。

`events --stored` 要求 writer 已停止，沿用相同实例/profile 参数，YAML 删除后仍可查询。按保留文件顺序跨运行筛选、取尾部；批次的 context、最早可用序号和游标描述最后一个保留运行。保留范围内的游标可继续读到后续运行；游标所属运行已丢失时返回缺口及空事件列表，去掉游标即可查看剩余历史。离线未知服务无匹配结果。输出明确标为 `stored`，旧 PID 仅为历史证据。

正常关闭会排空并同步磁盘；慢磁盘订阅、未完整结束的运行和未写完的尾记录都会显示缺口。下次持久化启动只修复未完成尾记录，并留下恢复标记；完整记录损坏或 schema 不支持时明确报错。单次查询最多保留 128 个缺口诊断，`omitted_gaps` 标出被丢弃的旧诊断数。轮转、强杀与掉电仍可能丢失历史，这是一份有边界的诊断记录。事件不保存命令、URL 或环境内容，但上下文仍包含实例状态路径，路径条件事件还会包含解析后的声明路径（不保存文件内容或解析后的软链接目标）。

**不靠猜，解释一次故障。** `devd explain <service>` 是只读、确定性的诊断报告，依据当前 supervisor 快照和结构化生命周期事件，区分依赖阻塞、启动失败、健康失败、资源超限、重启预算耗尽、手动停止以及健康／运行状态，并引用对应的事件序号、进程代次、因果事件和时间。它不会探测服务、启动或重启进程，也不会执行报告里的建议。

```bash
devd explain api
devd explain api --json
# 持久化运行停止后：
devd explain api --stored --json
```

在线模式连接正在运行的 supervisor，不重新读取 YAML。`--stored` 在 writer 停止后读取保留的事件文件，即使 YAML 已删除也可用；报告会标记 `source: stored`，历史 PID 只作为证据。报告中的 `complete: false` 表示存在明确缺口或被省略的缺口，结论只对保留下来的历史负责。没有匹配事件时会明确说无法确定原因，并给出下一步只读检查。任何模式都不会自动修复。

**启动前先查环境。** `devd doctor` 检查服务工作目录、声明的 `requires` 路径、dotenv 文件、服务命令和脚本探测程序是否可找到，以及显式声明的 TCP 监听地址。它不会启动服务、执行命令或探测脚本，也不会改动文件和进程。`requires` 使用与启动相同的 evaluator 检查可读文件、可访问目录及目标存在的软链接。用 `listen` 声明服务自己的端口，例如 `listen: [127.0.0.1:3000]`；健康检查目标不会被当作服务自有端口。检查会短暂绑定后释放地址，只说明检查当时是否可用。没有声明时会标记为 `not-checked`。可用 `--profile` 检查合并后的配置，用 `--json` 获取带版本的机器可读报告。发现失败时返回非零退出码；通过检查不代表之后启动必然成功。

```bash
devd doctor
devd doctor --profile staging --json
```

**日志默认保存在内存里。** 保留全栈最近 1000 条，单行最多 16 KiB；没有开启持久化时，停止后无法再查询。`logs --follow` 先输出指定条数的历史日志，再持续接收新日志，按 Ctrl+C 或 supervisor 停止时退出；跟随者落后过多会报错退出。最多同时连接 16 个跟随者，为控制命令保留连接槽位。前台输出过慢时会丢弃部分实时条目并告警；前台输出管道断开会触发服务清理。

**需要留案底，就显式写盘。** 用 `devd start --persist-logs` 启动，退出后用 `devd logs --stored` 查询：

```bash
devd start --persist-logs --log-max-size 10 --log-keep 3
# 前台 supervisor 停止后：
devd logs api --stored --level error --since 1h --tail 50
```

实例状态目录下保存 `logs/current.jsonl` 和 `logs/archive-1.jsonl`（最新归档），归档数量由配置控制。每条记录保留 UTC 时间、服务名、进程代次、级别、消息和截断标记。默认单文件 10 MiB，保留 3 份归档，日志数据最多 40 MiB；完整记录将超过上限时先轮转。`--log-max-size` 支持 1–1024 MiB，`--log-keep` 支持 1–100 份归档，均要求 `--persist-logs`。减小保留数后，下次持久化启动会删除多余的受管归档。减小文件大小上限不会重写已有归档，它们会随正常轮转淘汰。Unix 新目录和文件权限为 0700／0600，Windows 继承目录 ACL，应将项目和状态目录保存在当前用户目录内。日志可能含应用密钥；多次启动会追加到当前文件，共用这套保留上限。

查询时使用与启动相同的 `--config`、`--profile` 和 `--state-dir`。不同实例/profile 的日志隔离；YAML 被删除后仍可用 `--stored` 查询，但必须先停止持久化 writer，且不能与 `--follow` 同用。日志目录不存在时报错，服务名没有历史记录时返回空结果。普通 `logs` 仍只读本次运行的内存。磁盘查询在全部保留文件上使用相同过滤条件，再取最新的 tail 条。

磁盘写入使用独立的有界订阅，不阻塞服务采集。磁盘过慢可能丢失完整条目，文件中会留下包含丢失数量的 `devd` WARN。写盘失败会有序停止全栈并返回错误。正常关闭会排空已接收条目并同步磁盘；强杀或断电可能丢失尚未同步的数据。离线查询忽略异常中断留下的最后半条 JSONL，下次持久化启动会清掉它并记录警告；损坏的完整记录会让查询报错。这是有上限的开发日志，不是审计日志。

`--level`、`--since`、`--grep` 可单独使用或组合，用于筛选历史和实时日志。例如 `devd logs api --level error --since 5m --grep database --tail 50 --follow` 先显示最多 50 条匹配的内存历史，再持续接收匹配的新日志。级别精确匹配；`--grep` 对原始消息做区分大小写的字面匹配。`--since` 支持 `ms`、`s`、`m`、`h`（如 `500ms`、`2h`），在命令发起时固定截止时间。筛选无法找回已从内存淘汰的日志。

**诊断各管一层。** `status` 离线时返回错误，遗留状态文件仅用于诊断。配置内容被改坏后，仍可通过活实例执行 `stop`、`status`、`logs`、`restart` 和 `top`。`check` 校验配置结构和关系，`doctor` 不执行项目命令，只检查启动前置条件；健康探测只由 supervisor 在运行时执行。命令失败返回非零退出码。

`devd top` 连接与 `status`、`logs` 相同的运行实例，展示服务状态、PID、重启次数、CPU／RSS 和有界的实时日志。上下方向键（或 j/k）选服务，`r` 重启选中服务，Page Up/Down 翻日志，End 回到最新记录。`s` 会先要求确认停止整个服务栈；Enter 或再次按 `s` 确认，Esc 或 `n` 取消。`q` 和 Ctrl+C 只退出界面。需要交互式终端，也不会自行启动 supervisor。

**资源指标只统计服务主进程。** supervisor 大约每秒采样一次，不累加 shell 或包管理器启动的子进程。CPU 以一个核心满载为 100%，多线程进程可以超过 100%；内存为 RSS，终端以 MiB 显示。不可用的指标显示 `-`，CPU 在启动或重启后需要两次成功采样。JSON 的可选 `resources` 包含 `cpu_percent`（预热时为 `null`）、`memory_bytes` 和 `sampled_at`。进程退出后清除指标。

可选的 `limits` 在采样值超出阈值时记录告警，恢复到阈值内时再记录一次：

```yaml
services:
  api:
    command: ./run-api
    limits: {cpu: '150%', memory: 512MiB}
```

CPU 使用正整数百分比；内存使用正整数及 `B`、`KB`、`MB`、`GB`、`KiB`、`MiB` 或 `GiB` 单位，十进制与二进制单位不同。每次跨越阈值只记录一次，进程重启后重新计数。缺失采样及 CPU 预热不会清除既有告警。不包含子进程用量，也不提供 CPU 或内存硬限额。

**超限重启需要按服务显式授权。** 省略 `limits.on-exceed` 默认为 `warn`，服务继续运行。需要自动恢复时才开启：

```yaml
services:
  api:
    command: ./run-api
    limits:
      memory: 512MiB
      on-exceed: restart
    restart:
      policy: on-failure
      backoff: exponential
      initial-delay: 1s
      max-attempts: 3
```

同一指标连续 3 次有效采样超限才触发，大约每秒采样一次。恢复到阈值内或缺样会重置该指标的计数；CPU 预热只重置 CPU 计数。触发决定保留到当前进程代次结束。devd 停止进程组、排空日志，沿用现有退避并重新检查依赖后再启动。超限、崩溃、健康检查和依赖恢复重启共用累计预算；耗尽后服务失败并清理全栈。停止可以打断退避，手动重启可以接管等待，动作原因会写入日志和失败诊断。`on-exceed: restart` 与 `restart.policy: never` 冲突，启动前直接报错。profile 整体替换 `limits`，替换时省略 `on-exceed` 会回到 `warn`，`limits: null` 清除阈值。配置修改在下次启动 supervisor 或显式选择性重载时生效，不需要 root 权限或运行中弹窗确认。

当前范围是本地进程管理。`init` 可生成初始配置；项目扫描、交互式模板和文件监听自动重载仍在后续规划里；v0.6 提供手动选择性重载。

配置会拒绝未知字段和无效的 `limits`。YAML 值按字面使用，尚未实现 `${VAR}` 替换。`backoff: exponential` 从 `initial-delay` 开始，随累计重启次数翻倍，到 `max-delay` 封顶（默认 60s，不能小于初始延时）；健康检查成功不会重置计数。fixed 不使用 `max-delay`；两种等待均可被停止操作中断。

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
