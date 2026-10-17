# 命令指南

兼容范围、退出码与升级方式见[兼容与升级](COMPATIBILITY.zh-CN.md)。

[English](COMMANDS.md) · **简体中文** · [返回 README](../README.zh-CN.md)

服务跑起来了，接下来呢？按手头的事情往下找。每节先给用法，再说明失败、权限和证据的边界，省得出事时还要猜。

- [一次性任务](#一次性任务)
- [项目扫描与审阅式 init](#项目扫描与审阅式-init)
- [文件前置条件与路径规则](#文件前置条件与路径规则)
- [实例身份与发现](#实例身份与发现)
- [等待就绪](#等待就绪)
- [看清诊断能覆盖什么](#看清诊断能覆盖什么)
- [导出诊断证据](#导出诊断证据)
- [留下有界的事故证据](#留下有界的事故证据)
- [找到事故，并把证据带走](#找到事故并把证据带走)
- [换台机器继续诊断](#换台机器继续诊断)
- [离线对比两份移交报告](#离线对比两份移交报告)
- [显式配置实例端口与路径](#显式配置实例端口与路径)
- [检查环境漂移](#检查环境漂移)
- [执行审阅过的恢复配方](#执行审阅过的恢复配方)
- [清理可丢弃的实例数据](#清理可丢弃的实例数据)
- [接入 MCP 客户端](#接入-mcp-客户端)
- [给 Agent 的实例接口](#给-agent-的实例接口)
- [多环境配置](#多环境配置)
- [配置快照](#配置快照)
- [依赖图](#依赖图)
- [配置变更预览与应用](#配置变更预览与应用)
- [命令速查](#命令速查)

### 一次性任务

生成代码、构建资源、执行迁移，真正要等的是“做完了”，而不是“进程起来了”：

```yaml
version: "1"
services:
  generate:
    kind: task
    command: python3 scripts/generate.py
    task-timeout: 2m
  api:
    command: python3 app.py
    depends-on:
      - service: generate
        condition: completed-successfully
```

`kind` 默认是 `service`。任务只有退出码为 0、受管进程树清理完成、日志经过有界排空后才进入 `succeeded`，此时没有 PID。所有指向任务的依赖都必须写 `completed-successfully`；长驻服务不能使用这个完成条件。任务禁止健康探测和 `always` 重启，也不支持依赖恢复联动或资源超限重启；文件前置条件、环境映射与只读监测仍然可用。

任务默认 `restart.policy: never`，即使写了 restart 块却没写 policy，也不会自动重试。需要时显式设为 `on-failure`，沿用已有退避和累计 `max-attempts`。但迁移执行一半失败，再试一次可能重复外部副作用，devd 不会替你回滚数据库。失败且没有剩余重试次数时停止全栈；停止、取消和超时都不会算成功，即使程序在被终止时返回了 0。

`task-timeout`（别名 `task_timeout`）须为正值，默认 `5m`，从进程启动后开始计时。超时会触发正常进程树关闭，清理还可能需要额外时间。依赖就绪使用顶层 `dependency-timeout`（别名 `dependency_timeout`），默认 `30s`，可写 `10m` 这样的正时长或整数秒数，零值和超出运行时计时器范围的值会拒绝；profile 可以覆盖，`null` 恢复默认值。`wait --timeout` 只改变客户端期限。调大 task-timeout 不会延长下游等待，因此应选择能覆盖构建窗口的依赖期限。长驻服务不能设置 task-timeout。profile 整体替换 kind 和 task-timeout，`null` 清除任务超时覆盖；restart 仍按字段合并，因此基础配置明确写出的 policy 会继续被继承。

重启消费者会复用本次 supervisor 运行内已经成功的任务。`restart generate` 才是明确重跑：受理时立即作废旧成功结果，命令在新进程启动或快速任务完成后返回，需要等完成再用 `wait generate`。已放行的消费者不会因此被追溯停掉；长驻消费者可以显式开启 `restart-on-dep-recovery`，等任务新代次成功后联动重启。新启动尝试必须等新结果。重新启动 supervisor 则会重新执行任务，不采信旧状态或完成标记文件。

reload 只重跑受影响任务及其下游，无关任务保留成功结果。每层要等任务成功才能推进，仍沿用依赖等待期限；重载期间暂停受影响任务的自动重试，失败停止全栈。纯任务栈全部成功后 `start` 退出 0，脚本直接使用这个退出码；之后 supervisor 已经结束，不能再指望在线 `wait` 回答。

状态与导出报告增加 `kind`，wait 证据增加 `requires_completed`；旧报告省略这些字段仍可读取。新增 `succeeded`、`completed-successfully` 和 `task-timed-out` 枚举值需要支持任务的新版本读取，旧二进制不保证向前兼容。可运行示例见 [examples/tasks](../examples/tasks/README.md)。

### 项目扫描与审阅式 init

`init --scan node` 读取配置目录中的 `package.json`，生成“先完成构建任务、再启动长驻消费者”的候选配置。扫描限制为 manifest 1 MiB、脚本 128 项、workspace 128 项；只识别 npm、pnpm、yarn、bun，不安装依赖、不读取 `.env`、不执行脚本。manifest 必须是普通且非符号链接文件；FIFO 等特殊文件会拒绝。

先审阅候选，再写入配置：

```bash
devd init --scan node --json
devd init --scan node --build-script build --consumer-script dev --json
devd init --scan node --build-script build --consumer-script dev --yes
```

第一条只输出扫描元数据和候选脚本；第二条输出带 `published: false` 的生成 YAML；第三条仅在目标配置不存在时写入。`--json --yes` 只输出 JSON 报告。需要从 stdin 选择脚本并确认时使用 `--interactive`。生成任务固定为 `task-timeout: 5m`，因此 `--dependency-timeout` 不能小于 `5m`（默认 `10m`）。

只扫描根 manifest。workspace 的数组／对象声明按稳定顺序输出并标为未确认，不展开 glob、不读取子包 manifest。显式 `packageManager` 优先于锁文件元数据，冲突给警告；没有显式字段且锁文件提示冲突、或包管理器不受支持时失败。脚本名以带引号参数传给 `<manager> run`，不增加隐式 shell；Windows 生成的 npm/pnpm/yarn 命令使用对应的 `.cmd` 入口，保证 `doctor` 与实际启动使用同一个可执行文件。空值、非字符串、以 `-` 开头或含控制字符的脚本名省略并计数。`build`、`dev` 等名称仅是角色建议，用户显式选择后才建立 task／consumer 配方，不推断端口、健康探测、环境文件或 workspace 依赖。写入前审阅调用命令、脚本正文、cwd、成功语义和两类超时。消费者名由 `--service` 指定（默认 `app`），与生成任务名冲突会拒绝。

选择 `build` 和 `dev` 后的结果示例：

```yaml
version: "1"
dependency-timeout: 600s
services:
  build-task:
    kind: task
    cwd: .
    command: npm run build
    task-timeout: 300s
  app:
    cwd: .
    command: npm run dev
    depends-on: [{service: build-task, condition: completed-successfully}]
    restart: {policy: never}
```

仅扫描的 JSON 使用 `schema_version: 1`，包含来源／包管理器、scripts（名称、调用命令、脚本正文、source、cwd、角色建议）、workspace 声明、省略计数、`unconfirmed`、`warnings` 和 `complete`。`complete` 只描述扫描覆盖，不确认生命周期语义。选择模式 JSON 以 `scan` 包装扫描报告，并带 `configuration`、`published` 和 `next_steps`；只有成功发布后 `published` 才为 true。`--interactive` 在确认前展示同一候选和 YAML，与 `--yes`、`--json` 冲突；`--command` 仅用于原有 starter 模式。无效选择、manifest／配置／I/O 失败退出 1，clap 参数冲突退出 2。

扫描限时 30 秒，发布另限 30 秒，输出上限 2 MiB／5 秒，单次交互输入上限 8 KiB。Ctrl+C 可取消；取消先于发布受理时阻止迟到文件，已受理发布可能完成。失败清理临时文件，已有普通文件、链接和特殊文件都不覆盖。单次 OS 文件调用不能强行打断。报告可能包含路径和脚本正文，移交前须审阅。

只修改 timeout 的 reload 不重启服务；后续尝试（包括保留 actor 的手动／自动重启）读取已提交策略，进行中的依赖等待保持原期限。reload JSON 的 `config_changed_fields` 和文本配置字段显示此变化。超时事件／错误保留实际时长；客户端期限与控制授权仍独立。


### 文件前置条件与路径规则

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

### 实例身份与发现

```bash
devd instances --json                  # 当前仓库和全部 worktree
devd identity --profile staging --json # 指定活实例的身份
```

`start` 会登记配置、profile、规范化状态目录、项目/worktree 根目录、supervisor PID、启动时间和事件 `run_id`。`instance_id` 由配置路径、规范化状态目录和 profile 生成，同一实例重启或切换分支时保持不变，路径移动后会变化。分支和提交是启动时的观测；detached HEAD 的分支为空，尚无提交的分支其提交字段为空。

`instances` 从当前 Git 仓库及其 worktree 的 `.devd/instances/*.json` 读取索引，包括子目录配置和启动时登记的自定义 `--state-dir`。普通目录以配置目录为项目根，请在该目录查询；未安装 Git 时只能发现当前目录。它不扫描全盘，也无法发现尚未登记的旧版 supervisor。即使状态目录在项目外，启动登记仍要求项目索引可写；索引建立失败时不会启动服务。

索引仅是线索，必须连接控制端点并核对完整身份后才标为 `live`；其他结果为 `unreachable`、`identity-mismatch` 或 `unsupported`。不可达不代表进程已死，停止后的记录会保留，再次启动同一实例时替换。发现命令不改索引、不接管旧 PID、不停止进程、不删除数据。JSON 包含 `schema_version: 1`、`entries`、`warnings` 和 `complete`；`complete` 仅指所选范围的登记记录已检查且没有索引警告，不保证服务在线或已找到本机所有实例。最多检查 64 个 worktree、256 条记录，每条上限 64 KiB；端点并发上限 8，每个等待 750 ms。繁忙实例可能暂时不可达，可稍后重试。

`instances` 以当前目录确定范围，不接受非默认配置路径、profile 或状态目录选择器；`identity` 沿用 `status` 的选择器，YAML 删除后仍可查询。它们不隔离端口、应用文件或数据库。报告包含本地路径，分享前请留意。

### 等待就绪

进程启动了，不代表下一步就能用。跑测试、执行脚本，或者把环境交给另一个工具之前，可以先等活实例报告就绪：

```bash
devd wait                              # 等全栈，默认最多 30 秒
devd wait api worker --timeout 1m --json
devd wait api --profile staging --state-dir ./runtime
```

不写服务名时选择 supervisor 当前有效配置中的全部服务；指定名称时精确匹配、自动去重。有健康检查的服务必须达到 `healthy` 且持有 PID，其他长驻服务必须为 `running` 且持有 PID；任务须成功退出并进入 `succeeded`，此时没有 PID。沿用启动时的 `--config`、`--profile` 和 `--state-dir`。YAML 改坏或删除后仍能等待：命令只读取活状态，不重读配置，也不额外执行探测。

默认期限 `30s` 包含连接建立，可用 `--timeout` 设置 `1ms` 到 `1h`。同一 supervisor 内手动重启或崩溃恢复可以继续等待新代次；手动重启请求一旦被接受，旧代次就不能满足等待。全栈停止、已接受的配置重载（包括等价配置）或控制连接断开会终止本次等待。预览和被拒绝的重载不影响等待，也不会自动重连新的 supervisor。Ctrl+C 只取消等待命令。

退出码 `0` 表示在同一次活观测中，所选服务全部就绪；它不会锁住这些进程，也不保证之后持续健康。其他等待结果退出 `1`，CLI 参数错误退出 `2`。所选服务进入 stopped/failed 会终止等待；pending、starting、unhealthy 和 restarting 可以继续等。已观测到的全栈停止或重载优先于服务就绪。每个 supervisor 最多接受 8 个并发等待，多出的请求返回 `busy`，为控制命令保留连接余量。

`--json` 在 stdout 输出一份最终报告，失败也有报告：`schema_version: 1`、`outcome`、`instance_id`、`run_id`、`observed_at`、`elapsed_ms`、`services`、`blocking` 和 `message`。每个服务包含健康/任务完成要求、观测状态、PID、代次、是否就绪、是否有手动重启待完成，以及最近错误。结果为 `ready`、`failed`、`timed-out`、`cancelled`、`stopping`、`reloaded`、`disconnected`、`unavailable`、`invalid-service` 或 `busy`。超时、取消或断连时保留最后收到的证据，可能已经过时；未收到报告时身份和观测字段为 null。`waiting` 仅用于内部流式进度，不是 CLI 最终结果。参数错误可能在生成报告前退出。

最终输出在就绪等待结束后另有 5 秒的写入和 flush 期限；管道阻塞时仍可用 Ctrl+C 取消。输出失败以非零状态退出，文本或 JSON 可能不完整，服务不会因此停止。

### 看清诊断能覆盖什么

在 `requires` 写上一个文件，只能保证启动前检查它；服务跑起来之后是否还在监测，是另一回事。事件写进磁盘，也不表示事故记录已经开启。别等证据用得上的时候，才发现根本没留：

```bash
devd coverage                         # 看所选磁盘配置/profile 的声明
devd coverage --online               # 看当前运行的有效配置与记录器观测
devd coverage --online --json         # 同一份说明，输出 schema-1 JSON
```

默认模式不需要 supervisor，最多读取 1 MiB 配置，应用 `--profile` 后列出服务、`requires` 索引、命名路径及归属、健康探测和资源阈值。`cwd_resolved` 表示已有有效工作目录，包含默认目录，不表示 YAML 一定显式写了 `cwd`。它不检查应用资源是否存在，不读资源正文，不运行服务或探针、不尝试绑定端口、不创建可清理目录，也不会开启监测或授予控制权。所以，只要 YAML 合法，程序、dotenv 或依赖文件缺失仍能给出覆盖说明；检查实际前置条件请用 `doctor`。

`--online` 必须连接所选的活实例，读取 supervisor 当前使用的配置、instance/run 身份、观测到的服务状态与记录器状态。磁盘上尚未应用的编辑不会混进来；重载成功或重新启动后才更新，删除 YAML 后也能按原路径连接尚在运行的实例。记录选项来自本次启动参数，旧目录里有文件不算开启证据。连不上就报错，不会退回静态结果或把旧状态说成当前健康。

| 证据状态 | 含义 |
| --- | --- |
| `unknown` | 静态检查没有观测本次运行的记录选项 |
| `disabled` | 本次运行未开启该记录器 |
| `recording` | 已开启且尚未观测到失败，不保证证据完整 |
| `failed` | 本次已观测到写入失败，保留的证据可能不完整 |
| 已知缺口 | findings 分别说明内存窗口淘汰、事件截断、事故触发或上下文遗漏，以及本记录器的事故制品淘汰，并标注来源和后果 |

事件、应用日志与事故记录独立判断。事件或事故写入失败时服务继续运行；磁盘日志仍沿用失败后停栈的策略，因此查询时可能已经无法连接。事故淘汰计数可能包含先前运行的制品；内存事件被淘汰，也不代表磁盘上的副本一定丢了。

容量不是时间承诺：事件和日志显示本次启动的单文件大小及归档份数，事故最多保留 20 份、每份最多 2 MiB，内存事件容量为 1024 条。这个命令不扫描磁盘证据，writer 延迟、未完成制品、计数器之外的历史缺失仍可能未知；停栈后用 `events --stored`、`incidents ... --stored` 查看实际留存。

配置开启监测，也不证明已经取得有效采样。路径监测只在受管进程代次执行期间工作，任务完成后结束；轮询可能遗漏瞬时变化，也不检测文件正文。`paths` 映射本身不开监测，同一路径可以另外声明到 `requires`。CPU/RSS 仅覆盖主 PID，样本可能不可用。未声明的隐式依赖仍是未知，报告不提供健康裁定或可靠性评分。

文本与 JSON 共用 findings。JSON 给出来源、采集时间、`atomic: false` 和省略数量；在线状态与各记录器观测并非原子快照。最多展示 256 个服务及合计 512 条 requires/paths，输出上限 2 MiB，采集限 30 秒，输出限 5 秒；Ctrl+C 只取消本次查询。退出 `0` 表示成功生成报告，即使报告含缺口；采集、连接、校验、取消或输出失败退出 `1`（输出可能不完整），参数错误退出 `2`。不输出资源路径值、命令、环境正文或应用日志，但服务名、绑定名与配置路径仍可能敏感。本模块不新增 Agent/MCP 工具。

### 导出诊断证据

需要分享故障现场时，从选中的活 supervisor 导出一份有界报告：

```bash
devd export --output diagnostic.json
devd export --profile staging --state-dir ./runtime --output staging.json --include-logs
```

新 JSON 文件包含实例与运行身份、服务状态和 PID/代次、最近最多 1000 条生命周期事件，以及当前或近期出现的服务解释（最多 256 个服务）。事件游标、`gaps` 和 `omitted_gaps` 表示历史缺口。`captured_from`、`captured_to`、`event_watermark_after` 与 `stable_during_capture` 描述采样区间；稳定只表示采集期间未观测到状态或事件水位变化，不代表跨来源原子快照，也不保证之后仍是这个状态。不稳定的报告仍可排查，但状态与事件可能对应不同时间。

`omitted_fields` 列出有意省略的自由文本字段，错误正文缺席不表示没有发生错误。默认不包含应用日志；`--include-logs` 才加入最近最多 200 条原始内存日志，分享前必须检查。状态错误正文、重载失败正文、配置命令和环境值不会进入默认报告。这是字段白名单，不是通用秘密脱敏：服务名称、配置/状态/worktree 路径、路径条件中的文件路径、Git 上下文，以及显式加入的日志仍可能泄露隐私。导出通过临时文件发布为新文件，绝不覆盖现有目标；YAML 被删除后，只要 supervisor 还在运行仍可导出。默认在线模式不查询离线历史；两种模式都不控制服务。

**supervisor 已经停了，就导出真正留下来的证据。** 要分析的运行必须事先开启事件持久化：

```bash
# 重现问题之前：
devd start --persist-events
# 停止后，即使 devd.yml 已删除也能导出：
devd export --stored --run latest --output retained.json
# 使用 identity/events 或此前报告中的运行 UUID：
devd export --stored --run 123e4567-e89b-12d3-a456-426614174000 --output selected-run.json
```

`--stored` 必须搭配 `--run UUID|latest`。latest 指保留文件顺序中的最后一次运行，不代表最近启动的 supervisor，也不按墙上时钟排序。UUID 不存在或已被完全轮转删除时返回错误。使用启动时相同的 config/profile/state 选择；命令不加载 YAML、不连接端点、不探测进程，也不拿当前文件检查结果补造历史。事件严格限定在所选 run，各服务解释使用保留下来的最新事件代次，窗口中的旧代事件仍保留代次标签。

在线 CLI 与 Agent 导出保持 schema 1，增加 `source: live`；没有 source 的旧 schema 1 报告仍按 live 读取。离线使用 schema 2、`source: stored`，复用事件、解释和安全状态模型。顶层 `identity` 为 null、`state` 为空：旧事件存储只有 run/profile/state 上下文，没有完整实例身份或 Git 快照。`stored.selection` 保存请求的 config/profile/state 及推导的 `selected_instance_id`，它是查询来源，不是历史实例身份证明。`events.context` 保留原始记录，即使文件后来被移动。采集时间指本次导出，事件时间指原观察；`event_watermark_after` 为保留游标，`stable_during_capture` 与 `stored.atomic` 均为 false。

`stored.historical_state` 区分 services.json 的 `missing`、`invalid`、`unknown-run`、`different-run` 和 `retained-matching-run`。只有匹配 run 的快照才附带白名单服务字段，始终标为 `stale: true`；`file_modified_at` 只是文件元数据，不是状态观察时间。历史 PID 不授予控制权，历史状态不参与当前健康判断。`stored.live_state` 为 `unavailable-offline`，解释中的 `status` 保持 null；即使曾强杀 supervisor，也不能据此断言进程现在是否存活。

`begin_retained`、`end_retained` 独立表示存储 writer 的起止记录。标记缺失、事件缺口、内容截断或尾部限额都会令 `event_coverage_complete` 为 false。序号缺口可能来自轮转，`unfinished-record` 表示 current 文件尾部未写完，该片段不能可靠归属某次运行。完整记录损坏、归档文件截断则返回错误。结束标记只证明 writer 关闭事件流，不证明所有进程都成功停止；未记录的证据不会被重建。

离线 `--include-logs` 将最多 200 条原始历史日志放在独立的 `stored.unassigned_logs` 附件。旧日志格式没有 run_id，内容可能跨越多次运行，明确标为 `run_attribution: unknown-no-run-id-in-log-format`，不参与所选 run 的解释。轮转/运行覆盖保持未知，`omitted_entries` 与 `unfinished_bytes` 分别报告尾部省略及未写完字节。日志目录不存在时标为 missing，默认不读取它。附件标注可能敏感，沿用在线报告的字段白名单和路径隐私边界。

离线采集以共享模式持有已有 `services.json.lock`，并复用事件/日志读取锁。supervisor 或 writer 正在写入、状态锁缺失、受管文件是链接/特殊文件/多硬链接时拒绝读取，不创建源目录或锁文件。每种历史最多扫描 64 MiB、101 个受管文件，最多识别 256 次运行，事件尾部最多 1000 条、服务名最多 256 个；历史状态上限 1 MiB，两种模式最终 JSON 上限 16 MiB。超限失败，不发布部分报告。

两种 CLI 模式采集及准备阶段限时 30 秒、支持 Ctrl+C，再通过临时文件以不覆盖方式发布。发布前受理的取消可阻止后台任务迟到落盘；发布已被受理时允许完成，取消不删除已保存的报告。取消/期限在文件系统调用之间检查，单次阻塞 OS 调用无法强行打断。保存成功提示另有 5 秒输出期限。

### 留下有界的事故证据

碰上偶发故障，不必等它消失后再拼现场。重现前显式开启采集：

```bash
devd start --record-incidents
```

它与事件/日志持久化、文件监测、自动重启和清理授权相互独立。触发范围为未观测到停止请求的非零/信号退出、启动/进程/actor 失败、服务进入 Failed、健康进入 Unhealthy、依赖失败/超时、资源超限和文件条件失效。正常退出与手动控制本身不触发。文件故障来自启动检查或显式配置的 `monitor-requires`；此参数不会偷偷开启持续监测。

同服务、同代次的触发合并到一个 5 秒后续窗口，从采集器收到首个触发时计时，重复失败不会延长期限。每个事故最多保留前序、后续各 200 条事件，两段分别限 256 KiB；采集中、排队中和写入中的事故合计最多 4 个。容量满时丢弃新触发，订阅缺口、丢弃触发、合并次数和上下文省略量均计数，采集器结束时也会在 stderr 报告覆盖缺口。停止、重载、代次替换提前结束已有窗口并记录原因，之后的新故障可另建事故。控制请求及结果只记录窗口内真正观察到的事件，不把“请求已发出”写成“操作已成功”。

制品位于所选实例/profile 的 `incidents/`，schema 2 JSONL 依次写入 `begin`、`resources` 与 `end`。初始记录保存实例/run、触发代次、前序事件，以及收到触发后观察到的状态白名单和运行配置摘要。此时配置或代次可能已经变化，所以单独标明观察时间与代次，不保证它们等同于触发瞬间。新的路径检查复用有界解析器，最多检查触发服务的 8 项 requires/paths；它与历史条件事件分开保存，不能用新观察替换旧事实，也不靠时间相邻认定根因。

schema 2 还保存当时有效配置中的声明依赖边，以及资源匹配依据推导出的直接使用者和下游服务。`incidents show` 将 DAG 投影成易读的树；`[ref]` 表示已在其他分支展开的共享服务，JSON 则保留全部记录边。它是声明影响假设，不是实际访问追踪，也不是因果证明。配置摘要限 256 服务、每服务 64 依赖、128 KiB；关系区限 512 KiB、1,024 条边。各资源投影共用最多 256 个不同路径的检查批次，每个所选资源最多考察 256 项声明、保留 64 项匹配；树输出限 128 KiB，另附覆盖说明。省略、别名未知和中断都会留下证据，服务图不完整时不会生成看似完整的资源投影。之后修复目标或删掉 YAML，查询仍只读这些历史事实。schema 1 制品保持可读，并明确显示 `unknown-not-recorded-by-schema-1`，不会用今天的文件重建过去。

每个文件最多 2 MiB，总计保留 20 个，包含未完成制品。启动时按已有文件修改时间/名称排序，新制品按受理顺序加入，仍在采集的文件免于淘汰；记录本次记录器的淘汰计数。只管理 UUID 命名的事故文件，其他文件、日志、事件、快照保留。目录有独立 writer 锁，新文件绝不覆盖已有目标，拒绝链接、重解析点、特殊文件和多硬链接。

单个有界订阅者与单个阻塞工作线程负责采集，磁盘 I/O 不进入 supervisor 控制循环或应用输出排空链。路径检查在 OS 调用之间响应截止时间和提前结束；正常关闭排空已受理写入后才释放状态锁，单次卡住的文件系统调用不能强行打断。启动存储失败时不启动服务；**运行中记录器失败则停用本次事故记录、在 stderr 报告，服务继续运行。** 缺少 `end` 或尾部 JSONL 未写完都是不完整证据，取消后无法确定的计数用 null 表示。强杀 supervisor 不承诺补齐尾部。

默认制品不含应用日志、完整 YAML、命令、环境/dotenv 正文和自由错误文本；路径、链接目标与标识仍可能敏感。证据不是文件系统快照，也不是可恢复的会话备份。`export --stored --run` 读取独立持久化的事件历史；事故查询和 `export --incident` 读取事故目录。


### 找到事故，并把证据带走

```bash
devd incidents list --json
devd incidents show INCIDENT_UUID
devd export --incident INCIDENT_UUID --output incident.json

# 停止后，即使 YAML 已经删掉，也可以读取：
devd incidents list --stored --json
devd incidents show INCIDENT_UUID --stored --json
devd export --stored --incident INCIDENT_UUID --output saved-incident.json
```

在线查询绑定所选 supervisor 的 instance 和当前 run，要求本次运行开启事故记录。离线选择相同 config/profile/state，列出该实例所有保留运行的事故；查看、导出使用完整 UUID，`--incident` 与 `--run` 互斥。列表按记录的观察时间倒序、UUID 排序。两种方式都不会重新检查今天的目标、加载当前 YAML，或拿新代次状态填补旧现场。

查询 JSON 为 schema 1；事故导出为 schema 3，复用已有事件/解释白名单，增加 `incident` 证据区。普通在线导出与 Agent 保持 schema 1，历史运行导出保持 schema 2。即使在线获取，事故报告的 `source` 仍为 `stored`，顶层实时 identity/state 留空；历史身份、观察到的状态和代次、配置摘要、路径链、控制结果及省略量各自保留来源。解释只使用触发服务所属代次的已存事件。schema 1 事故未保存完整声明影响图，因此明确标为 `unknown-not-recorded-by-schema-1`，不从当前配置重建成历史事实。

完整窗口、提前结束、缺少 end、尾部截断/损坏及不支持的版本分别显示。有效前缀可以继续查看，不支持的版本只提供有界诊断。`complete` 只说明有界记录窗口的覆盖情况，不代表完整运行历史、根因证明或完整资源影响图。淘汰计数仅覆盖写入它的那次记录器；UUID 不存在可能是留存淘汰，不能据此断定事故没发生过。

`--include-logs` 单独授权附带最多 200 条应用日志：在线日志属于所选当前 run，但不代表事故窗口；离线磁盘日志归属未知。两者都不参与事故解释。默认省略日志、命令、环境正文和自由错误文本，路径与标识仍可能敏感。报告是诊断证据，不是可恢复的会话或数据库备份。

读取保留状态/writer lease，拒绝链接、重解析点、多硬链接及特殊文件。在线最多两个阻塞读取任务，与 writer 串行访问制品；停止/断连在 OS 调用之间取消。离线须先停止 supervisor。目录扫描最多 256 项，列表最多读取 20 个文件、每个 2 MiB，输出最多 16 MiB；异常多于 20 个时按文件名选择并标注省略。检测到读取期间文件替换则报告变化，不返回混合证据。查询/准备限时 30 秒，终端输出限时 5 秒；Ctrl+C 可取消，导出沿用既有取消与不覆盖发布规则，单次阻塞 OS 调用无法强行打断。

下面在 Linux/macOS 上用 Python 3 复现一次临时目标丢失。终端 A：

```bash
incident_demo=$(mktemp -d)
cd "$incident_demo"
printf 'keep me\n' > target.txt
ln -s target.txt resource
cat > devd.yml <<'YAML'
version: '1'
services:
  worker:
    command: python3 -u -c 'import time; print("ready"); time.sleep(600)'
    restart: {policy: never}
    requires: [{type: file, path: resource}]
    monitor-requires: true
YAML
pwd  # 终端 B 使用这里打印的目录。
devd start --record-incidents
```

终端 B 进入该目录，先执行 `devd wait --timeout 30s`，再执行 `rm ./target.txt`。监测事件产生后，`devd incidents list` 给出 UUID，`devd incidents show UUID` 展示当时保存的悬空链接链。窗口在 5 秒后结束，停止/重载/换代也可能提前结束。执行 `devd stop` 后即可使用上面的 `--stored` 命令；后来补回 `target.txt`，也不会改写已有事故证据。

### 离线对比两份移交报告

两次事故看起来相似，证据却可能完全不同。把比较也做成显式的离线动作：

```bash
devd compare before.json after.json
devd compare before.json after.json --json
```

两份文件都会先经过 `diagnose` 使用的同一套有界校验器。它不需要 YAML、状态目录、原始路径或运行中的 supervisor，不跟随报告里的路径和 URL，不执行命令，不写文件，也不提供控制或 Agent/MCP 操作。

JSON 输出是 schema 1。它先分别说明两侧的采集范围，再把配置/profile/instance/platform/范围标为 `comparable`、`different` 或 `unknown`。资源按报告里保存的选择器匹配，请求路径和观察到的目标只输出哈希，同时保留观察区间和证据来源；链接目标或声明变化是留存的观察，不是文件正文变化证明。关系边、服务状态、重新计算的历史解释、事故触发事件数据哈希和证据缺口分别比较。触发事件正文不会输出，触发差异也不代表共同根因。字段缺失、截断、不支持或不可比时返回 `unknown`，不会硬凑成差异；症状相似也不会被说成同一根因、复发或恢复效果。

事件缺口按公开的 `reason` 分组，分别比较记录数量与载荷指纹，不受记录排列顺序影响；省略的缺口诊断数量也单独比较。每侧的 `complete` 要求采集稳定、没有缺口或省略的缺口诊断，且保留的事件均未截断。它只表示证据覆盖情况，不代表当前健康或操作成功。

日志附件只作为元数据。数量和归属可以不同，但日志正文永远不进入事故窗口事实。输出最多保留 2048 项变化、16 MiB；每个输入沿用 16 MiB 普通文件限制。采集和输出分别限 30 秒、5 秒，Ctrl+C 只取消本次比较。退出 `0` 表示已生成报告（即使存在未知），输入损坏、不支持、身份/顺序不一致、超限或不可读取退出 `1`，参数错误退出 `2`。

### 换台机器继续诊断

现场可以消失，证据不用跟着报废。把导出的 JSON 单独复制到装有 devd 的另一台机器或空目录：

```bash
devd diagnose incident.json
devd diagnose incident.json --json
# 普通在线报告、历史运行报告也能读：
devd diagnose diagnostic.json --json
devd diagnose retained.json
```

`diagnose` 支持导出 schema 1（在线）、2（历史运行）、3（事故），兼容没有 `source` 的旧在线报告。不需要原 YAML、状态目录、注册索引或 supervisor，`--config`、`--profile`、`--state-dir` 在这里不作实例选择。它只读传入的报告，不访问嵌入路径/URL，不连接原端点，也不执行报告里的内容或控制进程。

文本与 JSON 使用同一份历史事实：采集区间、历史身份和陈旧状态、按事件序号排列的时间线、缺口、重新计算的解释，以及事故中已记录的资源观察与声明影响。文本将保存的 DAG 投影为树，共享节点显示引用；缺失关系仍然未知。事故解释固定触发代次，其他报告解释保留事件中的最新代次，采集时状态另列。报告自带的解释不会直接采信，旧代故障不会扣到已恢复的新代上。这些结果都不代表当前健康，连线也不证明因果；结构校验不等于验证报告真伪。

机器输出为诊断 schema 1，`source: imported-report`。报告有效但证据不完整、内部事故版本不支持时，命令仍可成功返回诊断，需检查其中的缺口；未知导出版本、JSON 损坏/截断、事件身份或顺序矛盾、超限输入则报错。事故缺少 end 或可选关系区损坏时，保留合法证据并标明未知。无法从剩余记录重验的原读取器诊断标为 `imported-reader-reported`。原始日志不展示、不参与解释，只单列附件数量和运行归属。路径与标识仍可能敏感。

输入必须是至多 16 MiB 的普通单硬链接文件；拒绝软链接、重解析点和特殊文件，检测到读取时替换则失败。最多 1,000 条事件、256 个服务名、129 条缺口记录，每份日志附件最多 200 条，嵌入事故仍受原有 2 MiB 限额约束。准备限时 30 秒、输出限时 5 秒，Ctrl+C 可取消；终端字段转义。命令只向 stdout 输出，不创建运行状态或修复文件。

### 显式配置实例端口与路径

两个 worktree 不该一边抢同一个 API 端口，一边往同一个临时目录里写东西。把应用需要的地址和路径集中声明：

```yaml
version: "1"
services:
  api:
    command: python api.py
    ports:
      API_ADDR: 127.0.0.1:3100
    paths:
      API_DATA: {scope: instance, path: api-data}
      SHARED_DATA: {scope: shared, path: ./datasets}
  web:
    command: python web.py
    depends-on: [api]
profiles:
  second:
    services:
      api:
        ports: {API_ADDR: 127.0.0.1:3101}
```

`api`、它的脚本探测以及直接依赖它的 `web` 都会收到 `API_ADDR`、`API_DATA`、`SHARED_DATA` 环境变量。`API_ADDR` 是完整 TCP 地址（IPv6 为 `[地址]:端口`），不是 URL，也不是纯端口号。应用必须主动读取这些变量；devd 不替换命令、URL 或任意 YAML 字符串。映射必须使用具体单播地址；通配绑定地址仍可通过 `listen` 声明。脚本探测可读取同一环境，内置 TCP/HTTP 探测字段仍需显式配置。

`instance` 路径按 `<所选状态目录>/runtime/` 解析，因此示例中的 `api-data` 随 profile/worktree 隔离。路径分量只允许小写 ASCII 字母、数字、`_`、`-`、`.`，拒绝父目录跳转、设备保留名、空格和尾点。`shared` 路径按声明服务的 `cwd` 解析，即使传给依赖者也保持同一个地址。默认由应用准备自己的运行目录，共享数据仍然共享；显式开启 `cleanup: true` 时由 devd 创建专用可丢弃目录，见下一节。scope 是配置归属，不是文件系统沙箱，也不授权删除已有文件或在清理时跟随软链接。

映射名称采用大写 ASCII 环境变量名；与服务显式 `env` 重名（不区分大小写），或与该服务能看到的另一映射重名，都会拒绝配置。映射覆盖继承环境和 dotenv 值，只传给直接依赖者。profile 对 `ports`、`paths` 分别整体替换，`{}` 清空对应映射。reload 预览会列出变更字段及下游影响，应用后受影响的新代次收到新环境。

`check` 拒绝地址声明冲突和实例独占路径重叠；`doctor` 对 `ports` 与 `listen` 地址实际尝试绑定后立即释放。**检查空闲不等于预约成功。** 启动前仍可能被其他进程抢占，应用绑定失败继续通过日志、退出和健康事件呈现；devd 不自动分配端口，也不凭退出状态猜测失败原因。

### 检查环境漂移

链接没断，不代表它还指着你原来选的目录。就绪检查能发现坏掉的前置条件；显式基线则用来回答：配置没动，这些关键资源的解析关系和元数据变了吗？

```bash
devd baseline capture --service api --resource requires:0 --resource cwd --output baseline.json
devd baseline check baseline.json --json
# 看过变化后，再显式保存新文件，接受新的基线。
devd baseline capture --service api --resource requires:0 --resource cwd --output accepted-baseline.json
```

`--resource` 可重复选择 `requires:索引`、`paths:名称`、`cwd`、`program`、`env-file`。program 只取命令第一个 token 的显式路径：`./bin/api` 可以，`api` 不行；不搜索 PATH，也不猜 shell 中真正执行的解释器或脚本。相对路径沿用配置目录／服务 cwd 的规则，instance 映射沿用所选状态目录。capture 不检查资源可读性、不执行程序、不读资源正文、不创建映射目录，也不连接 supervisor；它只保存新基线，并在 `<所选状态目录>/baseline-origin.id` 留一个本地来源标记。

check 比较的是当前所选的**磁盘有效配置**，沿用 `--profile` 和 `--state-dir`，不冒充 supervisor 内存配置。导入的资源选择器必须经过当前配置解析，基线中记录的路径不会变成任意文件读取指令。JSON schema 1 同时保留旧观察、当前观察、各自采集区间、比较原因，以及当前声明使用者和下游影响。这是独立的基线比较报告，暂不能交给 diagnose 或 MCP 的报告适配入口。

结果区分 `no-observed-change`（未观察到变化）、`changed`（已观察到变化）、`unknown`（无法比较）；有变化或未知时，打印报告后返回非零退出码。允许记录缺失路径，所以能观察缺失后恢复；接受基线不代表资源健康。声明变更、选择器消失、来源不匹配、不支持的解析链和检查中变化都标未知。检查不修复文件、不重启服务，也不改变 ready。

本地来源结合随机标记、标记文件身份、平台、主机提示及 config/profile/state 路径；check 不会补建缺失标记。只复制基线到另一处通常会无法比较，文件身份不能作为跨机器稳定 ID；连来源标记和上下文一起克隆也不构成机器认证。旧基线保留，接受变化须重新 capture 到新文件，已有目标绝不覆盖。

这些是非原子的元数据和链接目标观察，**不证明正文相同，也不证明故障因果**。祖先目录内增删其他文件不算资源替换；Unix 记录本地文件身份，Windows 记录属性和创建时间，文件身份明确不可用（`comparison.complete: false`）。路径和目标仍可能敏感。最多选择 32 项资源，基线输入／输出各限 2 MiB，配置输入限 1 MiB，比较输出限 16 MiB；解析沿用 128 节点／40 链接限制，影响图限 256 服务／1,024 条边，并共享有界观察缓存，别名覆盖不足须显示。准备工作限时 30 秒且可取消，输出限时 5 秒，不能打断单次 OS 文件调用。发布接受前取消可防止后台迟到生成基线；已接受的发布可能完成，来源标记也可能已建立。运行期监测和恢复仍须另行设计、授权。

### 执行审阅过的恢复配方

查清楚生成缓存丢失以后，先停止**全栈**，等前台 supervisor 成功退出。监测、基线和 `cleanup: true` 都不会替你运行修复命令；恢复是另一项显式操作。首版只允许当前仍声明 `scope: instance, cleanup: true`、且确实由 devd 创建登记的目录，共享或未登记路径拒绝接管。

新建独立 `recipe.yml`，把下面的程序和脚本路径换成你审阅过的生成器：

```yaml
version: 1
service: api
resources: [API_CACHE]
steps:
  - id: generate-cache
    resource: API_CACHE
    command: ["/absolute/path/python", "/absolute/path/rebuild-cache.py"]
    timeout: 30s
    requires: []
    ensures: [{type: file, path: generated.json}]
```

```bash
devd stop
devd recover --recipe recipe.yml --dry-run --json
devd recover --recipe recipe.yml --apply --plan PLAN_ID --json
# 核对结果后，再显式启动、验证就绪：
devd start  # 在另一个终端前台运行
devd wait --timeout 30s
```

沿用相同 config/profile/state。预览不写文件、不执行命令；`sha256:...:TIMESTAMP` 计划有效期 5 分钟，绑定配置／配方字节及文件身份、登记／运行身份、目录清单和程序元数据。应用及每个新动作前重验输入、期限和资源。观察非原子，也不证明文件正文相同；恢复期间请停下其它写入者。

仅重建丢失的**已登记叶目录**并写入新归属标记，父目录须仍然完整；原有受管目录内容保留。替换目录、链接／reparse point、硬链接、特殊文件、未完成清理及共享映射重叠均拒绝。配方不提供内建删除动作。命令 cwd 固定为该步骤选择的资源目录；相对程序路径按服务有效 cwd 解析，配方文件路径按发起终端解析。程序必须是显式路径，Windows 可用 `C:/...`，不搜索 PATH、没有隐式 shell。`requires`／`ensures` 只接受资源内普通相对文件／目录条件，不允许越界、链接或 devd 标记，不读资源正文。

**命令拥有你的系统权限，这不是沙箱。** 用户仍可显式声明 shell；devd 不能约束任意程序只写选定目录。命令继承发起终端环境，不加载服务 dotenv/env 覆盖；stdin 关闭、stdout/stderr 丢弃，预览和记录不回显参数正文或应用输出，路径仍可能敏感。整个执行和进程树清理期间持有实例状态锁，新 supervisor 不能抢跑。Ctrl+C 停止当前命令树并取消后续步骤；超时或失败也停止后续步骤，不承诺回滚，不自动启动服务。

schema 1 结果和每步 accepted／completed／failed／cancelled／timed-out／unknown 证据同步保存在 `<state-dir>/recovery-receipts.json`。退出成功还须通过声明的后置条件；这不证明服务就绪或根因已修复。重复应用旧计划只返回留存记录，`already_attempted: true` 不代表重新检查了今天的文件。同一配方摘要在同 run 只尝试一次；已知失败应先核对现场，再明确修改配方或完成一次新的正常运行，重新预览。强杀或无法确认进程树清理会把实例保留为不安全，拒绝 start／clean／recover；保留证据并人工核实，不要改归属记录绕过拒绝。devd 不向历史 PID 发信号，不删除事故、不自动接受漂移基线；本模块也不向 Agent／MCP 开放恢复权限。

最多 16 个资源、32 个步骤、每命令 128 个参数（各 8 KiB）、每步 32 项前置和 1–32 项后置条件、每命令超时不超过 5 分钟。配方须普通单硬链接文件，限 256 KiB；配置限 1 MiB。最多留存 64 次记录、总文件 1 MiB，不自动淘汰，满额须显式归档／核实。每次文件准备限时 30 秒、stdout 限时 5 秒；单次 OS 调用不能被强行打断，在途 worker 持锁直到退出。

### 清理可丢弃的实例数据

项目就是从一次误删事故开始的，删除权限当然不能靠猜。单独声明 `scope: instance` 不允许删除。确实可以丢弃的缓存，给它一个专用目录，再显式开启：

```yaml
services:
  api:
    command: python api.py
    paths:
      API_CACHE: {scope: instance, path: api-cache, cleanup: true}
```

`start` 会在启动服务前创建并登记该目录。**这表示目录及其后续所有内容都可以丢弃。** 已有但未登记的目录拒绝接管，持久数据请放在别处。`cleanup` 默认 `false`，只允许用于 instance 路径；`check`、`doctor` 和预览不会创建这些目录。

停止全栈，等运行 `start` 的进程成功退出后再操作：

```bash
devd stop
devd clean --dry-run --json
devd clean --apply --plan sha256:... --json  # 使用预览返回的 plan_id
```

前后使用相同的 `--config`、`--profile`、`--state-dir`。两个清理命令都必须取得已有实例状态锁，并核对上次授权运行的成功结束记录。连不上 supervisor、被强杀或运行失败都不满足这个条件，清理和目录复用会拒绝，需人工核实资源。devd 不会拿旧状态里的 PID 发信号来强行制造清理条件。不确定的数据先保留；急需恢复开发可换一个新的专用状态目录，不要编辑归属记录绕过拒绝。

计划同时检查当前 YAML 授权、创建登记、目录身份、归属标记和有界目录清单。配置字节、运行身份、登记或观测到的目录树变化都会让旧计划失效。所有候选都通过检查后才开始删除。共享映射（含解析后的别名）、未登记路径、日志、事件、快照和实例元数据保留；撤销 `cleanup` 后，旧目录也保留。清理声明发生变化必须全栈 stop/start，reload 预览会标明不能应用。

删除不是事务，失败可能留下部分进度。排除报告中的问题后重新预览再试；已完成计划重复应用不会碰同一路径上后来出现的用户目录。原本已消失的目录只注销记录。JSON 为 schema 1，分别报告 `removed`、`already_absent`、`already_applied` 和失败原因。

清理期间也应停止外部写入者。链接／reparse point、硬链接、特殊文件、跨文件系统目录、嵌套 devd 归属或状态标记、非 UTF-8 文件名都会拒绝；最多登记 128 个根目录，每根最多 10,000 个条目、64 层嵌套目录。清单检查文件元数据，不读取应用文件内容，因此不构成内容哈希，也不抵御同用户恶意进程任意篡改文件系统。父目录和归属记录保留，供重试和后续启动核对。

### 接入 MCP 客户端

让客户端先看清现场，再按需交出控制权。`devd mcp --stdio` 使用 MCP JSON-RPC；下面的 JSON-lines `agent` 接口继续保留。先启动 supervisor，再为采用 `mcpServers` 的客户端配置只读连接（替换两个绝对路径）：

```json
{
  "mcpServers": {
    "devd": {
      "command": "/absolute/path/to/devd",
      "args": ["mcp", "--stdio", "--config", "/absolute/project/devd.yml"]
    }
  }
}
```

有 profile 或自定义状态目录时，加上与 supervisor 一致的 `--profile` / `--state-dir`。需要控制时，在 `args` 中追加 `"--allow", "restart,stop,reload,clean"`，只保留愿意授予的项。可独立增加 `--restart-budget SERVICE=COUNT` 限制本次 adapter 会话内各服务可受理的重启次数，也可增加 `--deduplicate` 启用控制请求去重；两者默认关闭，不会增加 grant。会话固定绑定一个 instance/run；从 `devd_identity` 取得两个 ID，每个控制传入 `"target": {"instance_id": "…", "run_id": "…"}`。supervisor 换代后必须重新建会话，客户端确认和工具描述不能代替服务端授权。

| 工具 | 参数与行为 |
| --- | --- |
| `devd_identity`、`devd_status` | 无参数，读取绑定的活身份和服务状态 |
| `devd_events` | 可选 `tail`（1–1000，默认 100）、`service`，保留历史缺口 |
| `devd_explain` | 必填 `service` |
| `devd_wait` | 可选 `services`（默认全栈）、`timeout_ms`（1–3600000，默认 30000） |
| `devd_export` | 可选 `include_logs`（默认 false），返回 JSON，不写文件 |
| `devd_incidents_list`、`devd_incidents_show` | 列出本次运行留存事故；show 必填事故 UUID `id` |
| `devd_restart`、`devd_stop` | 必填 `target`，restart 还需 `service`；须分别授权。启用去重后所有控制还须传 `operation_key`（1–128 字节） |
| `devd_reload_preview`、`devd_reload_apply` | 可选 `candidate`；apply 还需 `target`、新鲜 `plan_id` 和 `reload` 授权 |
| `devd_clean_preview`、`devd_clean_apply` | 沿用同一会话，正常停栈后使用；apply 还需 `target`、新鲜 `plan_id` 和 `clean` 授权 |

工具在一个文本内容块中返回 JSON。先检查 `isError`，再检查报告的 `outcome`，拿到报告不代表等待、重载或清理成功。事件和事故证据保留来源与缺口；路径、诊断消息仍可能敏感，显式日志最多附带最近 200 条。相对 candidate 按 adapter 的工作目录解析。

拿到移交报告时，把客户端参数改为 `["mcp", "--stdio", "--report", "/absolute/path/report.json"]`。此时只开放无参数的 `devd_diagnose`，复用 `devd diagnose` 的验证器，只加载启动者指定的报告，所有证据标为历史。无需原项目或 supervisor，拒绝在线选择参数和控制授权，也不能通过工具参数读取另一条路径。

支持 MCP `2025-06-18` 与 `2025-11-25`。输入每行 16 KiB、输出 8 MiB，内部 supervisor 请求仍限 4096 字节；最多八个未完成请求、一个控制处理器。输入超限、未完成请求 ID 重复或并发超限会关闭 adapter，输出超限返回对应请求的错误。初始化和普通调用限 30 秒，wait 沿用所选期限；输出阻塞限 5 秒。取消、EOF、Ctrl+C 只结束 adapter 工作，不停止服务；已受理控制可能继续，重试前查状态和事件，传输 ID 不负责动作去重。启用 `--deduplicate` 后应使用相同 `operation_key` 重试；同键不同请求被拒绝，未知结果不重放。每会话最多保留 128 条：活动条目不过期，完成／unknown 结果保留 10 分钟；容量满时拒绝新键。重启预算耗尽不妨碍已授权 stop。这是接口能力限制，不是同用户沙箱。固定版本官方 Inspector 验证见 [tests/README.md](../tests/README.md)，不冒充模型驱动的客户端对话验收。

### 给 Agent 的实例接口

让 Agent 看状态，不代表顺手把停机按钮也递过去。先启动服务，再用相同的实例参数启动逐行 JSON 接口：

```bash
devd agent --stdio                                   # 默认只读
devd agent --stdio --allow restart,stop               # 只开放指定控制
devd agent --stdio --allow restart --restart-budget worker=3
devd agent --stdio --allow restart,stop --deduplicate
```

权限由启动者一次确定，请求不能自行增加权限或改变 config/profile/state。每个会话绑定一个**活实例及其本次运行**，supervisor 换代后会拒绝，不能悄悄跟过去。停止全栈后保持这个会话打开，即可预览／执行清理；新会话需要活 supervisor，独立离线清理仍用 `devd clean`。`--restart-budget SERVICE=COUNT` 按服务限制本会话可受理的重启次数，只在已有 restart grant 生效，未列出的服务会被拒绝。`--deduplicate` 独立开启后，控制请求必须带 `operation_key`。普通 CLI 保持原有权限。这是接口能力限制，不是同用户进程沙箱；不新增远程守护进程、多用户 RBAC 或自动启动。

stdin 每行一个 UTF-8 JSON 对象，stdout 只输出 JSON 回复；启动或 I/O 失败写 stderr 并非零退出：

```json
{"schema_version":1,"id":"inspect-1","operation":{"method":"status"}}
```

每条回复包含 `schema_version`、`id`、`instance_id`、`run_id`、`ok`。成功返回 `data`，拒绝返回 `error: {code, message}`；非法请求外壳的 `id` 为 null。ID 为 1–128 字节、不含控制字符，默认只关联请求。启用去重时控制请求另带独立的 `operation_key`；同键同摘要返回原操作状态/结果，同键不同摘要拒绝。未知请求字段和方法会拒绝；客户端应接受回复新增字段，并检查 schema 1。`ok: true` 只表示拿到了报告，wait/reload/clean 仍要检查 `data.outcome`；等待超时、部分清理都不能当成功。

| method | operation 内的其他字段 | 权限 |
| --- | --- | --- |
| `describe` | 无；返回能力及会话建立时观测的身份 | 读取 |
| `identity` | 无；查询绑定的活 supervisor | 读取 |
| `status` | 无；返回 `observed_at` 与白名单 `services` 状态 | 读取 |
| `events` | `query: {filter: {}, tail: 100, cursor: null}`，沿用 service/kinds/since 过滤及 run/sequence 游标 | 读取 |
| `explain` | `service` | 读取 |
| `wait` | `timeout_ms`（1–3600000）、可选 `services`（默认全栈）；只返回最终就绪报告 | 读取 |
| `export` | 可选 `include_logs`（默认 false）；直接返回报告，不写文件 | 读取 |
| `incidents` | 可选事故 UUID `id`；省略则列出本次运行的留存事故 | 读取 |
| `reload-preview` | 可选 `candidate`，默认原配置路径 | 读取 |
| `clean-preview` | 无；要求同一运行已成功停止 | 读取 |
| `restart` | `target`、`service`；沿用重启及依赖联动语义 | `restart` |
| `stop` | `target`；回复只表示正在停止，不表示收尾完成 | `stop` |
| `reload-apply` | `target`、`plan_id`、可选 `candidate`，须与预览一致 | `reload` |
| `clean-apply` | `target`、`plan_id`，保留全部目录归属检查 | `clean` |

从 `describe` 或 `identity` 取得身份，每个控制请求都带两个标识。接口在 I/O 前检查授权，supervisor 在实际操作前再次核对作用域；离线清理在状态锁内核对运行。

下面的控制示例要求会话已用 `--allow restart --deduplicate` 启动：

```json
{"schema_version":1,"id":"restart-1","operation":{"method":"restart","service":"api","operation_key":"restart-api-1","target":{"instance_id":"<instance_id>","run_id":"<run_id>"}}}
```

开启策略后，`describe` 和在线 `identity` 的 `control_policy` 会报告限制与剩余额度；默认 identity 格式不变。预算每服务允许 1–100 次，最多声明 64 个服务。校验失败或 supervisor 明确拒绝不扣次数；结果不确定时保留预留额度，取消或丢回复不会返还。它只限制 Agent 手动重启，不会主动触发恢复。

启用去重后，四种控制都要求独立 `operation_key`（1–128 个 UTF-8 字节，无控制字符），摘要绑定方法、目标及请求内容，传输 ID 不参与；重试时 candidate 的路径写法也要相同。`data`（MCP 为文本块内 JSON）变为 `{operation_key, operation_status, result?}`。`accepted` 表示 adapter 已登记待转发，`executing` 表示正在转发／等待用例，`completed` 表示已有报告，`unknown` 表示结果无法确定。这是 adapter 对操作的认知，不冒充 supervisor 的原子受理回执。重载／清理完成仍要查 `result.outcome`，stop 的 `stopping` 也只说明已请求停栈；`ok: true` 搭配 unknown 不等于操作成功。

每个 adapter 会话最多保留 128 条操作，accepted／executing 不过期；completed／unknown 从完成或转为未知时起保留 10 分钟，查询不续期，满额拒绝新键。单条结果最多 256 KiB，合计最多 8 MiB；超限结果首次仍可返回，随后只保留 unknown 记录以阻止重放。缓存命中也重验 grant 与 instance/run，停止后的核验只读证据，不重跑清理。明确拒绝会释放键。过期或 adapter 重启后，同键会被视为新操作，新会话也会重置预算；这里没有跨会话、跨 adapter 或崩溃后 exactly-once 保证。遇到 unknown 应先核对证据，别靠换键或重连蒙混过去。

策略错误码包括 `operation-key-required`、`operation-key-conflict`、`budget-out-of-scope`、`budget-exhausted`、`deduplication-capacity`、`busy` 和 `outcome-unknown`。

相对 candidate 按 Agent 进程工作目录解析，普通诊断不重读 YAML。status 和重启回复复用导出的状态白名单，省略自由文本错误；export 沿用已有隐私边界。事件、解释、等待和错误消息保留原有诊断细节，可能包含本地路径或命令文本，不承诺通用脱敏。应用日志仅在显式 export 请求中附带。

请求串行执行：每行含换行最多 16 KiB，包装后的 supervisor 请求最多 4096 字节，回复最多 8 MiB，stdout 写入阻塞超过 5 秒会退出。超大输入回复错误后关闭，其他非法请求可继续发送。EOF 处理完已接收输入再退出；Ctrl+C 退出接口并取消正在等待的只读请求，二者都不隐式停止服务。已经受理的控制（含阻塞中的清理）可能在断连后继续，重试前查 status/events；长时间 wait 不该阻塞其他操作时，使用另一个会话。

稳定错误码为 `invalid-request`、`unsupported-version`、`permission-denied`、`wrong-instance`、`stale-run`、`operation-failed`、`invalid-response`、`request-too-large`、`response-too-large`、`input-failed`。message 用于说明，不作为解析契约；操作报告继续保留历史缺口、旧计划拒绝和部分进度语义。

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

服务按名称合并。`env` 和 `restart` 按字段覆盖，其余字段整体替换，包括依赖列表、路径前置条件、端口/路径映射和健康检查。`cwd`、`env-file`、`healthcheck` 等可选字段可以用 `null` 清除；空 `env`/`restart` 映射表示继承，`ports: {}`、`paths: {}`、`depends-on: []` 和 `requires: []` 分别清空对应字段。新增服务必须有命令；暂不支持删除服务或 profile 之间的继承。未选中的 profile 也会检查未知字段，依赖及就绪条件按所选结果校验。不带 profile 的 `check` 检查基础配置。

路径仍沿用配置目录和服务 `cwd` 的相对路径规则。profile 名称以 ASCII 字母、数字或下划线开头，只允许 ASCII 字母、数字、`_`、`-`、`.`，区分大小写。默认运行目录为 `.devd/<配置文件名>/profiles/<名称>/`，大写字母转义为 `~hh`，避免大小写不敏感文件系统上的实例碰撞；显式 `--state-dir` 同样追加 `profiles/<名称>/`。隔离的是控制端点和状态文件，服务端口、应用文件仍需自行配置不同值。`init` 不接受 `--profile`。

可以直接试 [dev / staging / prod 示例](../examples/profiles/README.md)，其中也有依赖图导出、日志筛选和快照恢复的完整步骤。

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

### 配置变更预览与应用

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

先按旧图逆序停止受影响服务，等旧 actor 和进程组清理完成，再切换配置基线、按新图正序分层启动。任务须成功完成；有健康检查的长驻服务必须通过检查，其余长驻服务必须启动；每层使用候选配置的 `dependency-timeout`（默认 30 秒）。无关服务保留原进程，等价配置不会重启任何服务。执行期间拒绝手动 restart 和第二次 reload；受影响服务暂停自动恢复，成功后恢复新配置中的策略，无关服务继续按原策略运行。全栈 stop 可以抢占任意阶段。

失败会停止全栈，包括无关服务，不自动回滚。配置一旦切换，新的内存基线保持到 supervisor 退出。执行报告列出已完成停止、观测到的启动、已就绪的层和 `config_committed`；失败或中断返回非零退出码。生命周期事件记录计划、结果及进度，可通过 `events` 查询，启用事件持久化后也可用 `--stored`。客户端断开或等待响应超过 60 秒不会取消已受理的重载，重试前先检查 status/events。

`--dry-run` 与 `--apply` 必须二选一。预览校验 YAML、命令引号、设置和依赖图，不执行服务/探测命令、读取 dotenv 正文、检查实时路径/端口或写运行状态。比较范围只有 YAML 定义；dotenv、继承环境和程序文件内容变化需要显式 restart。文件监听自动重载仍后置。

### 命令速查

| 命令 | 用途 |
| --- | --- |
| `devd start [--persist-logs] [--persist-events] [--record-incidents]` | 前台启动全栈，分别显式开启日志、事件留存与有界事故采集 |
| `devd stop` | 请求有序关闭，前台进程完成清理后退出 |
| `devd restart <service>` | 用当前有效配置重启一个服务，重新检查依赖 |
| `devd reload --dry-run / --apply --plan ID [--candidate PATH] [--json]` | 预览或显式应用受影响服务的配置变化 |
| `devd status [--json]` | 查看实时状态、PID、CPU／RSS、重启次数和诊断信息 |
| `devd identity [--json]` | 查看所选活 supervisor 的实例与运行身份 |
| `devd instances [--json]` | 发现当前仓库及其 worktree 中已登记的实例 |
| `devd incidents list/show [UUID] [--stored] [--json]` | 查询当前运行或离线保留的事故证据 |
| `devd diagnose REPORT.json [--json]` | 无需原项目，直接诊断移交报告 |
| `devd compare LEFT.json RIGHT.json [--json]` | 离线比较两份移交历史报告 |
| `devd export --incident UUID [--stored] --output FILE [--include-logs]` | 导出单事故报告 |
| `devd export --output FILE [--include-logs]` | 导出有界的活实例诊断报告到新 JSON 文件 |
| `devd export --stored --run UUID/latest --output FILE [--include-logs]` | 停止后导出指定历史运行 |
| `devd clean --dry-run [--json]` | 在成功停止后预览已登记、明确授权丢弃的目录 |
| `devd clean --apply --plan ID [--json]` | 应用当前清理计划，保留共享和未登记数据 |
| `devd mcp --stdio [--allow restart,stop,reload,clean] [--report FILE]` | 为一个活运行或隔离历史报告提供 MCP 工具 |
| `devd agent --stdio [--allow restart,stop,reload,clean]` | 提供绑定本次运行、控制须显式授权的 JSON 接口 |
| `devd top` | 在交互式终端查看运行中的服务和实时日志 |
| `devd events [service] [--type TYPE] [--since DURATION] [--tail N] [--cursor RUN_UUID:NEXT_SEQUENCE] [--json] [--follow \| --stored]` | 查询生命周期经过、游标与历史缺口 |
| `devd explain <service> [--json] [--stored]` | 基于确定性事件证据解释一个服务最近的故障或状态 |
| `devd baseline capture --service NAME --resource SELECTOR --output FILE` | 显式选资源并接受为新基线 |
| `devd baseline check FILE [--json]` | 按磁盘配置比较资源，变化／未知返回非零 |
| `devd recover --recipe FILE --dry-run [--json]` | 正常停栈后预览已登记资源的恢复配方 |
| `devd recover --recipe FILE --apply --plan ID [--json]` | 显式执行当前计划，留存结果并拒绝未知结果重放 |
| `devd coverage [--online] [--json]` | 只读说明声明覆盖与已知证据缺口，不探测或改变服务 |
| `devd doctor [--json]` | 检查服务启动前置条件，不启动服务 |
| `devd impact SERVICE --require INDEX/--path NAME [--scope project/worktrees] [--json]` | 查看声明资源的使用者和潜在下游影响 |
| `devd logs [service] [--tail N] [--level info|warn|error] [--since DURATION] [--grep TEXT] [--follow \| --stored]` | 查询内存或离线磁盘日志，跟随实时输出 |
| `devd check` | 校验配置、命令引号、依赖关系及当前支持的设置 |
| `devd graph [--format text|dot|mermaid]` | 显示依赖边与启动层，或导出依赖图 |
| `devd init [--service NAME] [--command CMD]` | 创建通过校验的初始配置，不覆盖现有文件 |
| `devd init --scan node [--build-script NAME] [--consumer-script NAME] [--json] [--yes]` | 审阅有界 Node.js 扫描结果，并按需生成 task → consumer 配置 |
| `devd snapshot save <NAME>` | 保存磁盘上整份 YAML 到项目状态目录 |
| `devd snapshot restore <NAME> --output <FILENAME>` | 恢复成原配置目录中的新文件 |

命令共用 `-c / --config <PATH>`、`--profile <NAME>`（`init`、`snapshot` 除外）、`--state-dir <PATH>` 和 `--color auto|always|never`。`instances` 改用当前目录确定范围，不接受非默认配置路径、profile 或状态目录选择器。选项可以放在子命令前后：

```bash
devd start --config ./devd.local.yml
devd --config ./devd.local.yml status --json
```

