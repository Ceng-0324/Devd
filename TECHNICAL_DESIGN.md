# devd - 技术方案设计文档

## 项目概述

**devd** 是面向本地开发场景的轻量级服务守护工具，解决微服务/多进程项目的启动、监控、日志聚合和健康管理问题。

**核心定位**：pm2 功能太弱，docker-compose 太重，devd 填补中间空白。

---

## 技术路线

### 1. 核心语言选型：Rust

**选择理由**：
- **跨平台**：macOS/Linux/Windows 原生支持，无运行时依赖
- **系统调用友好**：进程管理、信号处理、文件监控（inotify/kqueue）
- **性能**：低内存占用，适合长期运行的守护进程
- **并发模型**：Tokio 异步运行时，天然支持多服务并发管理
- **生态成熟**：Serde、Clap、Axum 等库完善

**对比其他选项**：
| 语言 | 优势 | 劣势 | 判断 |
|------|------|------|------|
| Go | 编译快、并发简单 | 系统调用不如 Rust 灵活 | 可行但不如 Rust |
| Node.js | 开发快、生态大 | 运行时依赖、内存占用高 | 不适合守护进程 |
| Python | 开发快 | 性能差、打包困难 | 不适合 |
| Rust | 性能、跨平台、系统级 | 学习曲线陡 | ✅ 最佳 |

---

### 2. 架构风格：单进程 + 异步多任务

**设计原则**：
- devd 本身是单个守护进程
- 用 Tokio 异步管理多个子服务进程
- 每个服务对应一个 `ServiceTask`，包含：
  - 进程句柄（`tokio::process::Child`）
  - 健康检查任务（定时 HTTP/TCP probe）
  - 日志收集任务（stdout/stderr 流式读取）
  - 重启策略状态机

**优势**：
- 资源占用低（单进程多任务，不是多线程池）
- 事件驱动（服务 crash、健康检查失败触发自动重启）
- 易于测试（核心逻辑不依赖多进程）

---

### 3. 配置管理

#### 3.1 配置文件格式：YAML

**文件名**：`devd.yml`（主配置） + `devd.<profile>.yml`（环境覆盖）

**示例**：
```yaml
version: "1"

services:
  postgres:
    command: postgres -D /usr/local/var/postgres
    healthcheck:
      type: socket
      path: /tmp/.s.PGSQL.5432
      interval: 5s
    restart:
      policy: always
      max-attempts: 3

  backend:
    command: npm run dev
    cwd: ./backend
    env-file: .env.local
    depends-on:
      - postgres
    healthcheck:
      type: http
      url: http://localhost:3000/health
      interval: 10s
      timeout: 2s
      retries: 3
    restart:
      policy: on-failure
      backoff: exponential  # 1s → 2s → 4s → 8s
      max-attempts: 5
    limits:
      cpu: 50%
      memory: 1GB
```

#### 3.2 配置解析库：`serde_yaml`

**验证层**：
- 用 `serde` derive 自动反序列化
- 手动校验依赖图（检测循环依赖）
- `devd check` 命令提前发现配置错误

**当前配置入口**：`ConfigLoader::load` 读取 YAML 后自动执行语义校验；
`ConfigLoader::from_str` 只解析 YAML，调用方启动服务前必须调用 `DevdConfig::validate`。
校验不访问文件系统或网络，不启动子进程。

**校验规则**：
- 配置版本必须为 `"1"`，且至少定义一个服务。
- 服务名以 ASCII 字母、数字或下划线开头，其余字符允许 ASCII 字母、数字、`_`、`-`、`.`。
- 命令不能为空或包含 NUL；依赖必须存在且不能重复，循环依赖报告完整闭环路径。
- HTTP 检查要求带主机的 HTTP/HTTPS URL；TCP 主机不能为空或包含空白、控制字符，端口范围为 1–65535；Socket 路径不能为空。
- 健康检查 interval、timeout、retries 必须大于零。
- `http-ready`、`tcp-ready`、`socket-ready` 要求被依赖服务配置对应类型的健康检查；`started` 不要求健康检查。
- 按服务名和依赖名排序校验，保证错误输出确定。

#### 3.3 环境变量替换

**支持语法**：
```yaml
services:
  backend:
    env:
      DATABASE_URL: ${DATABASE_URL}           # 必填，未设置报错
      REDIS_URL: ${REDIS_URL:-redis://localhost}  # 可选，默认值
```

**实现**：`envsubst` 库或正则替换

---

### 4. 进程管理

#### 4.1 进程启动：`tokio::process::Command`

**关键设置**：
```rust
use std::process::Stdio;
use tokio::process::Command;

let arguments = shell_words::split(&service.command)?;
let (program, args) = arguments
    .split_first()
    .ok_or_else(|| anyhow::anyhow!("empty service command"))?;
let mut command = Command::new(program);
command.args(args);
if let Some(cwd) = &service.cwd {
    command.current_dir(cwd);
}
let child = command
    .envs(&service.env)
    .stdin(Stdio::null())
    .stdout(Stdio::piped())  // 捕获日志
    .stderr(Stdio::piped())
    .process_group(0)       // 独立 Unix 进程组
    .kill_on_drop(true)      // devd 退出时自动杀子进程
    .spawn()?;
```

**当前进程边界**：`src/core/process_manager.rs` 的 `ManagedProcess` 面向 macOS/Linux。
- 命令使用 `shell-words` 拆分带引号的参数后调用 `Command::new(program).args(arguments)`；不会隐式调用 shell。需要管道、重定向或 shell 展开时，显式使用 `sh -c '...'`。
- `cwd` 相对于 devd 的工作目录；相对 `env-file` 路径相对于服务的 `cwd`。使用异步文件读取和 `dotenvy` 解析，继承父环境，文件值覆盖父环境，显式 `env` 最后覆盖；不修改 devd 自身的环境。
- stdin 关闭，stdout/stderr 为独立管道，可由日志收集器各取一次；调用方应并发持续读取，避免管道写满导致服务阻塞。
- 每个服务创建独立 Unix 进程组，同时启用 `kill_on_drop(true)`。退出状态由 Tokio 的 Child 缓存，不重复维护一份状态。
- `stop(grace_period)` 发送组 SIGTERM，等待主进程退出；超时后发送组 SIGKILL 并回收主进程。主进程退出时清理仍留在组内的后代；重复停止返回同一退出状态。
- `restart` 停止旧进程后按原配置快照启动新进程，新进程需要重新取得日志管道。启动失败保留旧进程的退出状态，可修复外部文件后重试。
- 取消 wait/stop 不丢失进程所有权；Drop 对组发送 SIGKILL，由 Tokio 尽力回收主进程。过大的停止时长返回结构化错误。
- macOS 进程组只剩退出中成员时可能短暂返回 EPERM；异步组清理以 10ms 间隔重试，额外等待最多 100ms，持续权限错误仍返回给调用方。同步 try_wait 不等待或掩盖此错误。
- 进程组清理覆盖留在本组的后代；主动脱离进程组的服务不在此保证内，devd 自身遭 SIGKILL 时也无法执行 Drop。

#### 4.2 进程生命周期管理

**当前实现**：`ServiceManager` 是前台生命周期所有者，`service_task.rs` 为每个服务提供独立异步任务。构造阶段验证全部配置、健康探测器和计时参数，失败时不创建子进程或状态文件。

**状态流**：
```text
Pending -> 等待全部直接依赖 -> Starting -> Running
Running -> 探测成功 -> Healthy
Running / Healthy -> 连续失败达到阈值 -> Unhealthy
进程退出 / 健康失败 -> 按策略停止旧代 -> Restarting -> 等待依赖 -> Starting
成功退出且无需重启 -> Stopped
不可恢复失败 / 重启次数耗尽 / 依赖超时 -> Failed
关闭请求 -> 全部任务禁止启动和重启 -> 反向依赖分层 -> Stopping -> Stopped
```

- 所有服务任务同时创建，各自等待依赖状态；一个未就绪的分支不会阻挡其他分支。`started` 要求依赖拥有正在运行的进程；`tcp-ready`/`http-ready` 还要求最新状态为 Healthy。每次自动重启重新检查依赖。
- `ManagerOptions` 提供 30 秒依赖等待总时限和 5 秒停止宽限期，可由调用方配置；当前 YAML 依赖 schema 不增加 timeout 字段。
- 通过取消安全的 `ManagedProcess::wait` 观察退出，与健康探测和关闭通知并发等待。退出后立即清除快照 PID，避免已退出的旧代继续满足就绪条件。
- 没有健康检查时为 Running；探测失败未达阈值时也为 Running，保留失败计数与原因；成功时为 Healthy。`never` 的健康失败仅报告 Unhealthy，继续探测并允许恢复。
- 不可恢复失败会关闭整个服务栈并返回包含服务名和失败原因的错误；状态文件故障和任务异常同样触发清理。
- 关闭时先向全部任务发送 Quiescing，取消依赖等待、启动读取和重启延时，然后按反向拓扑层停止，层内并发。丢弃 run future 会中止服务任务并通过进程所有权清理进程组；正常关闭应使用 `run_until` 的关闭 future。
- stdout/stderr 持续并发排空，在管道读取侧拼成完整日志条目后写入有界历史和 broadcast；缓慢订阅方可能收到 Lagged，不能阻塞子进程。日志模块负责分行、格式化和历史查询。

**调用入口**：
```rust
use devd::core::service_manager::{ManagerOptions, ServiceManager};

let manager = ServiceManager::new(config, ManagerOptions::new(state_path))?;
let states = manager.subscribe();
let logs = manager.subscribe_logs();
// 调用方可在独立任务中消费 states/logs。
let final_snapshot = manager.run().await?;
```

#### 4.3 信号处理

**捕获信号**：`ServiceManager::run` 在创建子进程前安装 Unix SIGINT（Ctrl+C）和 SIGTERM 处理器，并委托 `run_until` 执行有序关闭。测试使用隔离的子进程发送真实信号，避免修改测试运行器的全局信号行为。

**转发信号**：
- `devd restart <service>` → 发 SIGTERM 给目标服务
- 不支持热重载的服务 → SIGKILL

---

### 5. 依赖管理

#### 5.1 依赖图构建

**数据结构**：
```rust
struct DependencyGraph {
    nodes: HashMap<ServiceId, Node>,
    edges: Vec<(ServiceId, ServiceId)>,  // (from, to)
}

struct Node {
    service_id: ServiceId,
    depends_on: Vec<Dependency>,
}

struct Dependency {
    service_id: ServiceId,
    condition: DependencyCondition,
}

enum DependencyCondition {
    Started,       // 进程启动即可
    SocketReady,   // Unix socket 可连接
    TcpReady,      // TCP 端口监听
    HttpReady,     // HTTP 健康检查通过
}
```

#### 5.2 拓扑排序

**算法**：Kahn's 算法（BFS）
1. 找出入度为 0 的节点（无依赖服务）
2. 启动这些服务，等待 `condition` 满足
3. 将依赖它们的服务入度 -1，重复步骤 1

**循环依赖检测**：
- 拓扑排序失败 → 报错并列出循环路径
- 示例：`backend → frontend → admin → backend`

**当前实现**：`src/core/dependency.rs` 的 `DependencyGraph` 拥有不可变的依赖快照。
- `from_config` 按服务名和依赖名排序建图，拒绝未知或重复依赖；配置字段和 probe 匹配仍由配置校验负责。
- `dependencies` 保留直接依赖的 readiness condition；`dependents` 返回直接反向依赖；未知服务返回 `None`，无边服务返回空切片。
- `edges` 的方向明确为 `(dependent, dependency, readiness_condition)`。
- `startup_layers` 使用 Kahn 算法返回并行候选分层；层内按服务名排序。实际启动还必须等待每条依赖的 readiness condition。
- `topological_order` 按层展开，保证依赖先于被依赖方，且结果不受 YAML 顺序和 HashMap 迭代顺序影响。
- 排序失败同时返回实际闭环路径和全部未解锁服务；未解锁列表可能包含环外受阻服务。
- 配置校验通过 `validate_acyclic` 复用图遍历，仅检测循环，不分配排序结果；排序和分层使用同一套 Kahn 遍历，复用层级缓冲区。

#### 5.3 依赖条件检查

**当前实现**：编排任务订阅 watch 状态快照，复用被依赖服务自己的 TCP/HTTP 监控结果，不重复发送网络探测。读取快照和等待变更使用 `borrow_and_update`/`changed`，避免丢失唤醒；首次启动与自动重启采用同一检查路径。Socket 探测在构造阶段明确拒绝。

---

### 6. 健康检查

**当前实现**：`src/core/health_check.rs` 提供以下独立接口。

- `HealthChecker::new` 复用 `HealthCheck::validate` 校验配置并创建不可变探测快照；超大计时参数返回结构化错误，HTTP 客户端和 URL 在初始化后复用。
- `probe` 只做一次探测。TCP 建连成功为健康；HTTP 使用 GET，仅 2xx 为健康，不跟随重定向、不使用系统代理、不读取响应体。
- 每次探测的超时涵盖等待 DNS、建连及 HTTP/TLS 响应头的全过程；失败通过 `ProbeResult::Unhealthy` 报告连接错误、HTTP 状态或超时，不 panic。
- `wait_ready(overall_timeout)` 首次立即探测，失败后按 interval 重试到第一次成功；总就绪时限独立于单次 timeout 和 retries，可中断正在执行的探测，超时错误保留最后一次完成的失败。
- `HealthMonitor::next_check` 串行定期探测，首次立即执行，跳过错过的时间点；完成失败后递增计数，达到 retries 才报告 Unhealthy，成功重置计数。计数饱和而不溢出。
- 取消未完成探测不增加失败次数；监控状态只描述健康，不直接重启进程，调用方负责停止监控、就绪条件和重启策略。`HealthMonitor` 的创建和轮询在 Tokio 运行时内进行。
- MVP 只执行 TCP/HTTP。现有 Socket schema 保留，但 `HealthChecker::new` 明确返回 UnsupportedProbe，实际 Socket 和 Script 探测仍留到后续版本。

#### 6.1 健康检查类型

**探测类型与实现范围**：
1. **Process Probe**：由进程管理模块观察子进程退出，不属于健康探测器
2. **TCP Probe**：MVP 已实现，TCP 连接检查
3. **HTTP Probe**：MVP 已实现，HTTP GET 请求 + 2xx 状态码校验
4. **Socket Probe**：后续版本实现 Unix socket 连接检查
5. **Script Probe**：后续版本实现自定义脚本检查，退出码 0 为健康

#### 6.2 健康检查调度

**调度接口**：调用方使用 `tokio::select!` 协调取消或服务退出，不在监控器内启动隐藏任务。
```rust
use devd::core::health_check::{HealthChecker, HealthMonitor, HealthState};

let checker = HealthChecker::new(&healthcheck_config)?;
let mut monitor = HealthMonitor::new(checker);
loop {
    let observation = monitor.next_check().await;
    if observation.state == HealthState::Unhealthy {
        // 交给服务编排层处理，不在探测器中直接重启。
        report_unhealthy(observation);
    }
}
```

**状态计数**：
```text
成功 -> Healthy，连续失败计数归零
失败次数 < retries -> Retrying
失败次数 >= retries -> Unhealthy
取消未完成探测 -> 不改变计数
```

#### 6.3 HTTP 健康检查实现

**库**：`reqwest`（异步 HTTP 客户端），TCP 使用 `tokio::net::TcpStream`。

```rust
use devd::core::health_check::{HealthChecker, ProbeResult};

let checker = HealthChecker::new(&healthcheck_config)?;
// 保留 checker 供后续轮询复用。
match checker.probe().await {
    ProbeResult::Healthy => report_healthy(),
    ProbeResult::Unhealthy(failure) => report_failure(failure),
}
```

---

### 7. 日志管理

#### 7.1 日志收集

**当前数据流**：`src/logging/collector.rs` 的 `LogCollector` 直接消费每一代子进程的 stdout/stderr，两条管道并发读取。

```text
stdout / stderr -> 独立有界分行器 -> LogEntry -> 全栈环形缓冲 + 有界 broadcast
                                                         -> 单个 write_logs writer
```

- stdout 分类为 Info，stderr 分类为 Error；读管道错误产生 Warn 诊断，返回 I/O 错误，服务生命周期继续由编排层控制。
- LF 分行，CRLF 去掉末尾 CR，空行保留；没有换行的尾部在 EOF、读取错误或取消时记录一次。每代进程的管道分别分行，重启前等待读取任务结束，尾部不会与新代拼接。
- UTF-8 在完整行分帧之后解码，跨读取边界的字符保留；非法字节使用替换字符。每次读取后让出运行时，持续输出不会独占服务管理任务。
- `max_line_bytes` 默认 16 KiB，可配置 1 字节至 1 MiB；超长行只保留前缀并标记 truncated，丢弃其余字节直到换行或 EOF。截断时移除边界上未完整的 UTF-8 字符，不把超长行拆成伪造的新行。
- `timestamp` 为完整条目记录时的 UTC 时间，`generation` 是该服务的累计重启次数。跨管道/服务仅保证采集顺序，不推断服务真实写入的全局顺序。

#### 7.2 日志存储

**MVP 当前实现**：

- 全栈共享 `VecDeque<Arc<LogEntry>>`，默认保留最近 1000 条；`capacity` 可配置 1 至 65536。行长和条目数量分别受限，避免单行绕过缓冲上限。
- `ManagerOptions.logging` 在启动前校验；`LogHistory::recent(service, limit)` 按可选服务过滤，返回最新 limit 条，结果由旧到新。
- 历史插入和 live 分发使用同一个短锁建立一致顺序；锁内不等待网络、管道或终端 I/O。history 句柄不持有分发发送方，manager 退出后仍可查历史且不会阻止 writer 收尾。
- live broadcast 保留最多 256 条；慢订阅方丢失完整条目并收到 Lagged，历史仍独立采集。查询返回共享条目，调用方自行控制历史快照的持有量。
- 磁盘日志持久化与轮转留到 v0.4，本模块不写日志文件。

**数据结构**：
```rust
struct LogEntry {
    timestamp: DateTime<Utc>,
    service: String,
    generation: u32,
    level: LogLevel,
    message: String,
    truncated: bool,
}

enum LogLevel {
    Info,
    Warn,
    Error,
}
```

#### 7.3 日志输出（CLI）

**串行输出**：`write_logs` 独占一个异步 writer，完整写入每行并 flush。输出示例：`[15:30:01.000Z] [backend] [INFO] Starting server...`。

- 使用 `colored` 调色板，服务名颜色由稳定 hash 决定；Info 蓝色、Warn 黄色、Error 红色。`ColorMode::Auto` 遵循终端和环境设置，也支持 Always/Never，测试不修改全局颜色状态。
- 非颜色模式不产生 ANSI；消息中的 ESC、CR、换行等控制字符转义，避免服务日志破坏前缀和串行布局。原始文本保存在 LogEntry 中，tab 和普通 Unicode 保留。
- 截断行附加 `[truncated]`；慢输出产生 `[devd] [WARN] log output skipped N entries: subscriber lagged`，writer 返回实际输出/丢失计数。
- writer 的 I/O 错误直接返回，不阻止管道采集。调用方在 manager 外处理终端输出；manager 不隐式打印。正常退出时发送方关闭后 writer 排空剩余条目，慢或卡住的外部输出由调用方决定取消时限。

```rust
use devd::logging::{write_logs, LogFormatter};

let logs = manager.subscribe_logs();
let history = manager.log_history();
let (run, output) = tokio::join!(
    manager.run(),
    write_logs(writer, logs, LogFormatter::default()),
);
let final_snapshot = run?;
let output_summary = output?;
let recent_backend = history.recent(Some("backend"), 100);
```

**CLI 历史查询与过滤规划**（服务选择与 `--tail` 已实现，其余选项为后续规划）：
```bash
devd logs backend              # 只看某服务
devd logs backend --tail 50    # 最近 50 条，已实现
devd logs --level error        # 只看错误
devd logs --since 5m           # 最近 5 分钟
devd logs --grep "database"    # 关键词过滤
```

---

### 8. 重启策略

**MVP 当前行为**：只执行固定延时（`initial-delay`）的 `always`、`on-failure`、`never` 策略。首次启动不占重启次数；后续每次启动尝试（包括 spawn 失败）计一次，`max-attempts: 0` 禁止自动重启。次数在一次 manager 运行中累计，不因短暂健康成功而重置，防止反复崩溃绕过上限。`max-delay` 为后续指数退避保留，固定延时不使用它；配置 exponential 时编排入口明确报错，不静默改为 fixed。下面指数退避与依赖联动重启均为后续版本设计。

#### 8.1 重启策略类型

**配置项**：
```yaml
services:
  backend:
    restart:
      policy: on-failure       # always | on-failure | never
      backoff: exponential     # fixed | exponential
      initial-delay: 1s
      max-delay: 60s
      max-attempts: 5
```

#### 8.2 指数退避实现

**算法**：
```rust
struct RestartPolicy {
    policy: RestartPolicyType,
    backoff: BackoffType,
    initial_delay: Duration,
    max_delay: Duration,
    max_attempts: u32,
    current_attempts: u32,
}

impl RestartPolicy {
    fn next_delay(&mut self) -> Option<Duration> {
        if self.current_attempts >= self.max_attempts {
            return None;  // 超过最大重试次数
        }
        
        self.current_attempts += 1;
        
        let delay = match self.backoff {
            BackoffType::Fixed => self.initial_delay,
            BackoffType::Exponential => {
                let delay = self.initial_delay * 2u32.pow(self.current_attempts - 1);
                delay.min(self.max_delay)
            }
        };
        
        Some(delay)
    }
    
    fn reset(&mut self) {
        self.current_attempts = 0;
    }
}
```

#### 8.3 依赖联动重启

**场景**：postgres 重启后，backend 也需要重启（数据库连接失效）

**配置**：
```yaml
services:
  backend:
    depends-on:
      - postgres
    restart-on-dep-recovery: true
```

**实现**：
- 维护反向依赖图（谁依赖我）
- 服务恢复时，通知所有依赖方触发重启

---

### 9. 命令行界面（CLI）

#### 9.1 CLI 框架：`clap`

**MVP 已实现命令**（Linux/macOS）：
```bash
devd start                       # 前台管理全栈，实时输出日志
devd stop                        # 接受关闭请求后返回；前台完成清理后退出
devd restart <service>           # 重启指定服务，重新检查依赖
devd status [--json]             # 实时状态与 PID
devd logs [service] [--tail N]   # 内存历史快照，默认 100，范围 1–1000
devd check                       # 静态配置和 MVP 能力校验
devd graph                       # 依赖边与并行启动层
```

统一全局选项为 `-c/--config`（默认 `devd.yml`）、`--state-dir` 和 `--color auto|always|never`，可位于子命令前后。服务 cwd 在 CLI 边界解析为配置目录相对路径；env-file 相对服务 cwd。check/graph 复用编排入口校验，包括命令引号、Socket/exponential 等不支持设置，不启动服务或创建状态文件。文件存在性和网络可用性仍由运行时检查。

`cli/mod.rs` 负责参数、路径和展示；`cli/server.rs` 协调前台 manager、控制连接与日志 writer；`cli/protocol.rs` 使用长度前缀 JSON；`cli/stdout.rs` 对终端和管道采用可取消的非阻塞写，兼容文件及 `/dev/null`。配置默认运行目录为 `<config-dir>/.devd/<config-name>/`，状态为 services.json，端点为 control.sock；socket 路径过长时报错提示使用较短的 --state-dir。

start 先获取 StateStore 的同一把锁，再清理遗留 socket 和 bind；持锁到 socket 清理完成。不会替换非 socket 文件。socket 权限 0600；同时最多 32 个客户端，请求上限 4 KiB，响应上限 128 MiB（覆盖有界历史的 JSON 转义），读取和写入超时 5 秒。重启响应最多等待 55 秒，超时明确提示操作仍可能继续，需检查 status 后再重试。客户端整体响应等待最多 60 秒。

stop/status/logs/restart 通过活实例操作，不解析可能已改坏的 YAML，不向遗留 PID 发送信号。status 离线时报错，持久化快照仅用于诊断。stop 返回“请求已接受”；最终退出由前台命令体现。错误返回非零退出码。

手动 restart 经有界 channel 进入 manager，旧服务 actor 完成停止和日志收尾后重建 actor；保留累计重启次数与日志代次，允许重启仍有其他服务在运行时已正常退出的服务。手动操作绕过自动策略限制，但不重置自动重试的累计预算。等待健康依赖时可以被全栈停止打断；同一服务的并发重启明确拒绝。成功响应表示新代已启动，而非已经健康；服务终止失败仍触发全栈清理。

start 输出断开触发有序停止，卡住的终端不会阻止采集；退出时日志排空最多等待 1 秒。日志历史仅存活于 supervisor，logs 不支持离线查询。`init`、`top`、`--profile`、`logs --follow` 和高级过滤留待后续版本。

#### 9.2 交互式 TUI（可选）

**库**：`ratatui`（终端 UI 框架）

**功能**：
- 实时服务状态面板
- 日志滚动显示
- 按键快捷操作（r=重启, s=停止, q=退出）

**是否实现**：MVP 阶段跳过，先做 CLI，后续迭代加 TUI

---

### 10. 配置生成（`devd init`）

#### 10.1 项目扫描

**扫描策略**：
1. 检测 `docker-compose.yml` → 解析服务定义
2. 检测 `package.json` → 提取 `scripts.dev`
3. 检测 `Cargo.toml` → 提取 binary 名称
4. 检测 `requirements.txt` / `pyproject.toml` → Python 项目
5. 检测 `Makefile` → 提取常见 target（`make dev`, `make start`）

#### 10.2 交互式问答

**使用 `dialoguer` 库**：
```rust
use dialoguer::{Input, Select, Confirm};

let service_name: String = Input::new()
    .with_prompt("Service name")
    .default("backend".into())
    .interact()?;

let command: String = Input::new()
    .with_prompt("Start command")
    .interact()?;

let has_healthcheck = Confirm::new()
    .with_prompt("Add health check?")
    .default(true)
    .interact()?;
```

#### 10.3 模板生成

**内置模板**：
- `postgres`：默认端口 5432，socket 健康检查
- `redis`：默认端口 6379，TCP 健康检查
- `node`：npm run dev，HTTP 健康检查
- `rust`：cargo run，TCP 健康检查

---

### 11. 资源监控

#### 11.1 指标采集

**库**：`sysinfo`（跨平台系统信息库）

```rust
use sysinfo::{ProcessExt, System, SystemExt};

fn get_process_stats(pid: Pid) -> ProcessStats {
    let mut sys = System::new_all();
    sys.refresh_process(pid);
    
    let process = sys.process(pid).unwrap();
    
    ProcessStats {
        cpu_usage: process.cpu_usage(),
        memory_bytes: process.memory(),
        disk_read: process.disk_usage().read_bytes,
        disk_write: process.disk_usage().written_bytes,
    }
}
```

#### 11.2 资源限制

**实现方式**：
- **Linux**：cgroups v2（通过 `/sys/fs/cgroup`）
- **macOS**：无原生 cgroups，用软限制（定期检查，超限 kill 进程）
- **跨平台方案**：只做监控 + 告警，不强制限制（MVP）

---

### 12. 数据持久化

#### 12.1 状态存储

**目录结构**：
```
~/.devd/
├── config/
│   └── devd.yml          # 全局配置（可选）
├── state/
│   └── services.json     # 运行时状态（PID、启动时间、重启次数）
├── logs/
│   ├── backend/
│   │   ├── 2024-01-03.log
│   │   └── 2024-01-04.log
│   └── frontend/
└── snapshots/
    └── 2024-01-03-working.yml
```

#### 12.2 状态文件格式

**当前 `services.json`**（服务名按字典序）：
```json
{
  "supervisor_pid": 12000,
  "services": {
    "backend": {
      "pid": 12345,
      "started_at": "2024-01-03T15:30:00Z",
      "restart_count": 2,
      "status": "healthy",
      "consecutive_failures": 0,
      "last_exit_code": 7,
      "last_exit_signal": null,
      "last_error": null
    }
  }
}
```

**用途**：
- 提供持久化诊断快照，不恢复或接管旧进程。退出后保留最终状态与失败诊断；`devd status` 通过控制 socket 查询实时状态。
- 状态路径由调用方选择，同一项目必须使用同一路径。`<path>.lock` 持有非阻塞独占 flock，第二个 manager 在启动任何子进程前失败。
- 使用 `<path>.tmp` + rename 原子替换，读者不会读到半截 JSON。写入任务和服务任务保留锁所有权，防止取消时旧写入与新 manager 竞争；不要求磁盘掉电持久性。
- 强杀或取消可能留下旧快照；快照 PID 本身不证明服务仍受管理，调用方必须结合锁和 supervisor 存活判断，不能直接向缓存 PID 发信号。

---

### 13. 测试策略

#### 13.1 单元测试

**测试层级**：
- 配置解析（YAML → struct）
- 依赖图构建 + 拓扑排序
- 重启策略逻辑
- 健康检查函数

**工具**：Rust 内置 `#[test]` + `#[tokio::test]`

#### 13.2 集成测试

实际集成入口是 `tests/integration.rs`，通过编译后的公开 CLI 启动真实 shell 工作负载及后代进程。单服务与依赖链配置分别为 `tests/fixtures/simple.yml`、`dependency-chain.yml`。场景覆盖自动恢复、重试耗尽、健康失败与恢复、逐级 readiness 阻塞、反向依赖关闭、启动失败回滚以及等待依赖时的 SIGINT/SIGTERM。

测试拥有本地 HTTP mock，绑定动态端口后始终保留 listener，并通过结构化 YAML 注入地址；响应状态由测试显式控制。该 mock 模拟探测端点，真实受管进程由 workload.sh 提供。所有临时文件在独立项目目录，PID 记录原子发布。命令和条件等待有截止时间；失败输出状态与日志，作用域退出时关闭 supervisor、HTTP mock 并清理临时目录。验证包含前代进程及后代退出，不仅检查状态文件。

#### 13.3 端到端测试

**手动测试清单**：
1. `devd start` → 验证所有服务按顺序启动
2. 杀掉某个进程 → 验证自动重启
3. `devd restart <service>` → 验证 PID 更换和依赖重新检查
4. `devd logs [service] --tail 50` → 验证内存历史；前台 start 验证实时输出
5. `devd stop` → 验证优雅关闭

命令边界测试为 `cargo test --locked --test cli`；完整 MVP 场景为 `cargo test --locked --test integration`。CI 在 Linux/macOS 执行 `cargo test --locked --all-targets`，并通过公开 check 命令校验 simple.yml。完整验证矩阵、夹具边界及可复制的手动烟雾测试见 [tests/README.md](tests/README.md)。热重载不在 MVP 范围。

---

### 14. 性能考量

#### 14.1 内存占用

**目标**：devd 守护进程 < 50MB

**优化点**：
- 日志环形缓冲区（固定大小）
- 避免克隆大对象（用 `Arc<T>` 共享）
- 及时释放已停止服务的资源

#### 14.2 CPU 占用

**目标**：空闲时 < 1% CPU

**优化点**：
- 健康检查用异步 sleep，不用轮询
- 日志读取用流式处理，不用一次性加载
- 避免频繁的系统调用（缓存进程信息）

#### 14.3 启动时间

**目标**：启动 10 个服务 < 5 秒

**优化点**：
- 并行启动无依赖服务
- 依赖检查用超时机制，避免无限等待

---

### 15. 跨平台兼容性

#### 15.1 平台差异处理

| 功能 | Linux | macOS | Windows |
|------|-------|-------|---------|
| 进程管理 | ✅ fork/exec | ✅ fork/exec | ✅ CreateProcess |
| 信号处理 | ✅ SIGTERM/SIGKILL | ✅ SIGTERM/SIGKILL | ⚠️ 模拟信号 |
| Unix Socket | ✅ | ✅ | ❌ 不支持 |
| 文件监控 | ✅ inotify | ✅ kqueue | ✅ ReadDirectoryChangesW |
| Cgroups | ✅ v2 | ❌ 无 | ❌ 无 |

#### 15.2 MVP 阶段策略

**优先支持**：macOS + Linux
**Windows 支持**：延后到 v0.2

---

### 16. 发布和分发

#### 16.1 编译产物

**单二进制文件**：
- Linux: `devd-x86_64-unknown-linux-gnu`
- macOS: `devd-aarch64-apple-darwin` + `devd-x86_64-apple-darwin`
- Windows: `devd-x86_64-pc-windows-msvc.exe`（可选）

#### 16.2 安装方式

**Homebrew**（推荐）：
```bash
brew tap yourusername/devd
brew install devd
```

**Cargo**：
```bash
cargo install devd
```

**预编译二进制**：
```bash
curl -L https://github.com/yourusername/devd/releases/latest/download/devd-$(uname -s)-$(uname -m) -o /usr/local/bin/devd
chmod +x /usr/local/bin/devd
```

---

## 技术风险和缓解

### 风险 1：进程管理复杂度

**风险**：子进程僵尸、孤儿进程、信号竞争
**缓解**：
- 用 `tokio::process` 自动管理进程生命周期
- 设置 `kill_on_drop(true)` 防止孤儿进程
- 信号处理用 `tokio::signal` 规避竞争

### 风险 2：健康检查误判

**风险**：网络抖动导致健康检查失败 → 误重启
**缓解**：
- 设置重试次数（连续 3 次失败才重启）
- 超时时间可配置
- 提供"宽松模式"（只记录错误，不重启）

### 风险 3：依赖图复杂场景

**风险**：循环依赖、条件依赖判断失败
**缓解**：
- 启动前校验依赖图（`devd check`）
- 提供依赖图可视化（`devd graph`）
- 文档明确说明依赖配置最佳实践

### 风险 4：跨平台兼容性

**风险**：macOS 和 Linux 系统调用差异
**缓解**：
- 用成熟的跨平台库（`tokio`, `sysinfo`）
- 条件编译处理平台差异（`#[cfg(target_os = "linux")]`）
- CI 覆盖多平台测试

---

## 开发路线图

### MVP（v0.1）- 2 周
- [x] 配置解析（YAML → struct）
- [x] 基础进程管理（启动、停止、重启）
- [x] 依赖图构建 + 拓扑排序
- [x] 简单健康检查（TCP、HTTP）
- [x] 日志收集 + 彩色输出
- [x] CLI 基础命令（start, stop, logs, status）

### v0.2 - 1 周
- [ ] 指数退避重启策略
- [ ] 依赖联动重启
- [ ] Unix Socket 健康检查
- [ ] 资源监控（CPU、内存）
- [ ] 配置生成（`devd init`）

### v0.3 - 1 周
- [ ] 多环境配置（dev/staging/prod）
- [ ] 快照和恢复
- [ ] 依赖图可视化
- [ ] 日志过滤和查询
- [ ] 完善文档和示例

### v0.4 - 2 周
- [ ] 交互式 TUI
- [ ] 日志持久化 + 轮转
- [ ] 资源限制（软限制 + 告警）
- [ ] 插件系统（可扩展健康检查）
- [ ] Windows 支持

---

## 参考资料

**类似项目**：
- [pm2](https://github.com/Unitech/pm2) - Node.js 进程管理器
- [foreman](https://github.com/ddollar/foreman) - Procfile 启动工具
- [hivemind](https://github.com/DarthSim/hivemind) - Go 实现的 foreman 替代品
- [overmind](https://github.com/DarthSim/overmind) - tmux-based 进程管理器

**Rust 生态**：
- [Tokio](https://tokio.rs/) - 异步运行时
- [Clap](https://docs.rs/clap/) - CLI 参数解析
- [Serde](https://serde.rs/) - 序列化/反序列化
- [Reqwest](https://docs.rs/reqwest/) - HTTP 客户端
- [Sysinfo](https://docs.rs/sysinfo/) - 系统信息采集
