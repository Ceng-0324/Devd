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
use tokio::process::Command;

let mut child = Command::new(&service.command)
    .current_dir(&service.cwd)
    .envs(&service.env)
    .stdout(Stdio::piped())  // 捕获日志
    .stderr(Stdio::piped())
    .kill_on_drop(true)      // devd 退出时自动杀子进程
    .spawn()?;
```

#### 4.2 进程生命周期管理

**状态机**：
```
[Pending] → [Starting] → [Healthy] → [Unhealthy] → [Restarting] → [Healthy]
                ↓            ↓           ↓
           [Failed]     [Stopping]   [Failed]
```

**核心任务**：
1. **启动任务**：按依赖拓扑排序，依次启动服务
2. **监控任务**：定期检查进程是否存活（`child.try_wait()`）
3. **健康检查任务**：HTTP/TCP/Socket probe
4. **重启任务**：crash 或健康检查失败触发
5. **清理任务**：devd 退出时优雅关闭所有子进程（SIGTERM → 等 5s → SIGKILL）

#### 4.3 信号处理

**捕获信号**：
```rust
use tokio::signal;

tokio::select! {
    _ = signal::ctrl_c() => {
        // 用户按 Ctrl+C，优雅退出
        shutdown_all_services().await;
    }
    _ = sigterm_handler() => {
        // 收到 SIGTERM（systemd/launchd 发来），优雅退出
        shutdown_all_services().await;
    }
}
```

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

**实现**：
```rust
async fn check_dependency_ready(dep: &Dependency) -> Result<bool> {
    match dep.condition {
        DependencyCondition::Started => {
            // 检查进程是否存活
            service_manager.is_running(&dep.service_id)
        }
        DependencyCondition::SocketReady => {
            // 尝试连接 Unix socket
            UnixStream::connect(&socket_path).await.is_ok()
        }
        DependencyCondition::TcpReady => {
            // 尝试 TCP 连接
            TcpStream::connect(&addr).await.is_ok()
        }
        DependencyCondition::HttpReady => {
            // HTTP 健康检查
            reqwest::get(&url).await?.status().is_success()
        }
    }
}
```

---

### 6. 健康检查

#### 6.1 健康检查类型

**支持的 probe 类型**：
1. **Process Probe**：进程存活检查（`kill(pid, 0)`）
2. **TCP Probe**：TCP 连接检查
3. **HTTP Probe**：HTTP GET 请求 + 状态码校验
4. **Socket Probe**：Unix socket 连接检查
5. **Script Probe**：自定义脚本，退出码 0 为健康

#### 6.2 健康检查调度

**Tokio 定时器**：
```rust
use tokio::time::{interval, Duration};

async fn health_check_loop(service: &Service) {
    let mut ticker = interval(Duration::from_secs(service.healthcheck.interval));
    
    loop {
        ticker.tick().await;
        
        let result = perform_health_check(service).await;
        
        match result {
            Ok(true) => {
                // 健康，重置失败计数
                service.consecutive_failures = 0;
            }
            Ok(false) | Err(_) => {
                service.consecutive_failures += 1;
                
                if service.consecutive_failures >= service.healthcheck.retries {
                    // 触发重启
                    restart_service(service).await;
                }
            }
        }
    }
}
```

#### 6.3 HTTP 健康检查实现

**库**：`reqwest`（异步 HTTP 客户端）

```rust
async fn http_health_check(config: &HttpHealthCheck) -> Result<bool> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(config.timeout))
        .build()?;
    
    let resp = client.get(&config.url).send().await?;
    
    Ok(resp.status().is_success())
}
```

---

### 7. 日志管理

#### 7.1 日志收集

**流式读取**：
```rust
use tokio::io::{AsyncBufReadExt, BufReader};

async fn collect_logs(mut child: Child, service_name: String) {
    let stdout = child.stdout.take().unwrap();
    let stderr = child.stderr.take().unwrap();
    
    let stdout_reader = BufReader::new(stdout).lines();
    let stderr_reader = BufReader::new(stderr).lines();
    
    tokio::spawn(async move {
        while let Some(line) = stdout_reader.next_line().await.unwrap() {
            log_event(service_name.clone(), LogLevel::Info, line);
        }
    });
    
    tokio::spawn(async move {
        while let Some(line) = stderr_reader.next_line().await.unwrap() {
            log_event(service_name.clone(), LogLevel::Error, line);
        }
    });
}
```

#### 7.2 日志存储

**设计**：
- **内存环形缓冲区**：最近 1000 条日志（`VecDeque<LogEntry>`）
- **磁盘持久化**：可选，写入 `~/.devd/logs/<service>/<date>.log`
- **日志轮转**：按天轮转，保留最近 7 天

**数据结构**：
```rust
struct LogEntry {
    timestamp: DateTime<Utc>,
    service: String,
    level: LogLevel,
    message: String,
}

enum LogLevel {
    Info,
    Warn,
    Error,
}
```

#### 7.3 日志输出（CLI）

**彩色输出**：`colored` crate

```rust
fn print_log(entry: &LogEntry) {
    let service = entry.service.color(get_service_color(&entry.service));
    let timestamp = entry.timestamp.format("%H:%M:%S").to_string().dimmed();
    let level_icon = match entry.level {
        LogLevel::Info => "ℹ".blue(),
        LogLevel::Warn => "⚠".yellow(),
        LogLevel::Error => "✖".red(),
    };
    
    println!("{} [{}] {} {}", timestamp, service, level_icon, entry.message);
}
```

**过滤**：
```bash
devd logs backend              # 只看某服务
devd logs --level error        # 只看错误
devd logs --since 5m           # 最近 5 分钟
devd logs --grep "database"    # 关键词过滤
```

---

### 8. 重启策略

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

**子命令设计**：
```bash
devd init                      # 初始化配置
devd start [--profile <name>]  # 启动所有服务
devd stop                      # 停止所有服务
devd restart <service>         # 重启某服务
devd status                    # 查看状态
devd logs [service] [--follow] # 查看日志
devd top                       # 资源监控
devd check                     # 检查配置
devd graph                     # 显示依赖图
```

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

**`services.json`**：
```json
{
  "services": [
    {
      "name": "backend",
      "pid": 12345,
      "started_at": "2024-01-03T15:30:00Z",
      "restart_count": 2,
      "status": "healthy"
    }
  ]
}
```

**用途**：
- `devd status` 读取状态
- devd 重启后恢复服务列表（可选）

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

**场景**：
- 启动一个 mock 服务（HTTP server），验证 devd 能否正确管理
- 模拟服务 crash，验证自动重启
- 模拟健康检查失败，验证重启策略

**实现**：
```rust
#[tokio::test]
async fn test_auto_restart_on_crash() {
    let config = r#"
services:
  test-service:
    command: "bash -c 'sleep 1 && exit 1'"
    restart:
      policy: always
      max-attempts: 3
"#;
    
    let manager = ServiceManager::from_yaml(config).await.unwrap();
    manager.start_all().await.unwrap();
    
    // 等待服务 crash + 重启
    tokio::time::sleep(Duration::from_secs(5)).await;
    
    let stats = manager.get_stats("test-service").unwrap();
    assert!(stats.restart_count >= 1);
}
```

#### 13.3 端到端测试

**手动测试清单**：
1. `devd start` → 验证所有服务按顺序启动
2. 杀掉某个进程 → 验证自动重启
3. 修改 `devd.yml` → `devd reload` 验证热重载
4. `devd logs --follow` → 验证实时日志
5. `devd stop` → 验证优雅关闭

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
