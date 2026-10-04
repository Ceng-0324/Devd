# devd - 架构设计文档

## 系统架构图

```
┌─────────────────────────────────────────────────────────────────────┐
│                          devd CLI (User Entry)                       │
│  ┌──────────┬──────────┬──────────┬──────────┬──────────┬─────────┐ │
│  │  init    │  start   │  stop    │  logs    │  status  │  graph  │ │
│  └──────────┴──────────┴──────────┴──────────┴──────────┴─────────┘ │
└────────────────────────────────┬────────────────────────────────────┘
                                 │ clap::Parser
                                 ▼
┌─────────────────────────────────────────────────────────────────────┐
│                        Core Service Manager                          │
│                                                                       │
│  ┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐  │
│  │ Config Loader    │  │ Dependency Graph │  │ State Manager    │  │
│  │ • Parse YAML     │  │ • Build DAG      │  │ • services.json  │  │
│  │ • Env substitution│ │ • Topological    │  │ • PID tracking   │  │
│  │ • Validation     │  │   sort           │  │ • Status cache   │  │
│  └──────────────────┘  └──────────────────┘  └──────────────────┘  │
│                                                                       │
│  ┌────────────────────────────────────────────────────────────────┐ │
│  │                     Service Orchestrator                        │ │
│  │  • Start/Stop/Restart services in dependency order             │ │
│  │  • Signal handling (SIGTERM → graceful shutdown)               │ │
│  │  • Parallel startup for independent services                   │ │
│  └────────────────────────────────────────────────────────────────┘ │
└────────────────────────────────┬────────────────────────────────────┘
                                 │
        ┌────────────────────────┼────────────────────────┐
        ▼                        ▼                        ▼
┌──────────────────┐  ┌──────────────────┐  ┌──────────────────┐
│  Service Task 1  │  │  Service Task 2  │  │  Service Task N  │
│  (postgres)      │  │  (backend)       │  │  (frontend)      │
├──────────────────┤  ├──────────────────┤  ├──────────────────┤
│ • Process Handle │  │ • Process Handle │  │ • Process Handle │
│ • Health Check   │  │ • Health Check   │  │ • Health Check   │
│ • Log Collector  │  │ • Log Collector  │  │ • Log Collector  │
│ • Restart Policy │  │ • Restart Policy │  │ • Restart Policy │
└──────┬───────────┘  └──────┬───────────┘  └──────┬───────────┘
       │                     │                      │
       │ tokio::process      │                      │
       ▼                     ▼                      ▼
┌──────────────┐      ┌──────────────┐      ┌──────────────┐
│  postgres    │      │  npm run dev │      │  vite dev    │
│  (Child PID) │      │  (Child PID) │      │  (Child PID) │
└──────────────┘      └──────────────┘      └──────────────┘
```

---

## 核心模块架构

```
┌──────────────────────────────────────────────────────────────────────┐
│                             devd Binary                               │
│                                                                        │
│  ┌─────────────────────────────────────────────────────────────────┐ │
│  │                         CLI Layer (clap)                         │ │
│  │  • Command parsing                                               │ │
│  │  • Argument validation                                           │ │
│  │  • Output formatting (colored terminal)                          │ │
│  └────────────────────────────┬─────────────────────────────────────┘ │
│                               │                                        │
│  ┌────────────────────────────▼─────────────────────────────────────┐ │
│  │                      Application Layer                            │ │
│  │                                                                    │ │
│  │  ┌───────────────┐  ┌───────────────┐  ┌───────────────┐        │ │
│  │  │ StartCommand  │  │ StopCommand   │  │ LogsCommand   │        │ │
│  │  │ • Load config │  │ • Send signal │  │ • Query logs  │  ...   │ │
│  │  │ • Start mgr   │  │ • Wait exit   │  │ • Stream out  │        │ │
│  │  └───────────────┘  └───────────────┘  └───────────────┘        │ │
│  └────────────────────────────┬─────────────────────────────────────┘ │
│                               │                                        │
│  ┌────────────────────────────▼─────────────────────────────────────┐ │
│  │                         Domain Layer                              │ │
│  │                                                                    │ │
│  │  ┌──────────────────────────────────────────────────────────┐   │ │
│  │  │                   ServiceManager                          │   │ │
│  │  │  • Service lifecycle orchestration                        │   │ │
│  │  │  • Event loop (Tokio runtime)                             │   │ │
│  │  │  • Task spawning and coordination                         │   │ │
│  │  └──────────────┬───────────────────────────────────────────┘   │ │
│  │                 │                                                 │ │
│  │  ┌──────────────▼───────────────┐  ┌────────────────────────┐  │ │
│  │  │     DependencyResolver        │  │    HealthChecker       │  │ │
│  │  │  • Build DAG from config      │  │  • HTTP probe          │  │ │
│  │  │  • Topological sort           │  │  • TCP probe           │  │ │
│  │  │  • Cycle detection            │  │  • Socket probe        │  │ │
│  │  └──────────────────────────────┘  │  • Script probe        │  │ │
│  │                                     └────────────────────────┘  │ │
│  │  ┌──────────────────────────────┐  ┌────────────────────────┐  │ │
│  │  │      LogCollector             │  │    RestartPolicy       │  │ │
│  │  │  • Capture stdout/stderr      │  │  • Exponential backoff │  │ │
│  │  │  • Ring buffer storage        │  │  • Max attempts        │  │ │
│  │  │  • Timestamp + level parsing  │  │  • Cooldown timer      │  │ │
│  │  └──────────────────────────────┘  └────────────────────────┘  │ │
│  │                                                                   │ │
│  │  ┌──────────────────────────────┐  ┌────────────────────────┐  │ │
│  │  │      ProcessManager           │  │    StateStore          │  │
│  │  │  • tokio::process::Command    │  │  • services.json       │  │
│  │  │  • Signal forwarding          │  │  • PID registry        │  │
│  │  │  • Graceful shutdown          │  │  • Restart counters    │  │
│  │  └──────────────────────────────┘  └────────────────────────┘  │ │
│  └──────────────────────────────────────────────────────────────────┘ │
│                                                                        │
│  ┌─────────────────────────────────────────────────────────────────┐ │
│  │                      Infrastructure Layer                        │ │
│  │                                                                   │ │
│  │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐          │ │
│  │  │ ConfigLoader │  │ LogStorage   │  │ FileWatcher  │          │ │
│  │  │ (serde_yaml) │  │ (ring buffer)│  │ (notify)     │          │ │
│  │  └──────────────┘  └──────────────┘  └──────────────┘          │ │
│  │                                                                   │ │
│  │  ┌──────────────┐  ┌──────────────┐  ┌──────────────┐          │ │
│  │  │ HttpClient   │  │ SystemInfo   │  │ SignalHandler│          │ │
│  │  │ (reqwest)    │  │ (sysinfo)    │  │ (tokio)      │          │ │
│  │  └──────────────┘  └──────────────┘  └──────────────┘          │ │
│  └──────────────────────────────────────────────────────────────────┘ │
└──────────────────────────────────────────────────────────────────────┘
```

---

## 数据流图

### 1. 服务启动流程

```
┌──────────┐
│ devd     │
│ start    │
└────┬─────┘
     │
     ▼
┌──────────────────┐
│ Load devd.yml    │──┐
└────┬─────────────┘  │
     │                │ Validation Error
     ▼                ▼
┌──────────────────┐  ┌──────────┐
│ Validate Config  │─>│ Exit(1)  │
└────┬─────────────┘  └──────────┘
     │ OK
     ▼
┌──────────────────┐
│ Build Dependency │
│ Graph (DAG)      │
└────┬─────────────┘
     │
     ▼
┌──────────────────┐
│ Topological Sort │──┐
└────┬─────────────┘  │
     │                │ Cycle Detected
     ▼                ▼
┌──────────────────┐  ┌──────────┐
│ Cycle Detection  │─>│ Exit(1)  │
└────┬─────────────┘  └──────────┘
     │ No Cycle
     ▼
┌──────────────────────────────────────┐
│ Start Services in Topological Order  │
└────┬─────────────────────────────────┘
     │
     ├──> Service 1 (postgres)
     │    ├─> Spawn Process
     │    ├─> Wait for SocketReady
     │    └─> Start Health Check Loop
     │
     ├──> Service 2 (redis)
     │    ├─> Spawn Process
     │    ├─> Wait for TcpReady
     │    └─> Start Health Check Loop
     │
     └──> Service 3 (backend)
          ├─> Wait for dependencies (postgres, redis)
          ├─> Spawn Process
          ├─> Wait for HttpReady
          └─> Start Health Check Loop
```

---

### 2. 健康检查流程

```
┌─────────────────────┐
│ Health Check Task   │ (Tokio task per service)
│ (Interval: 10s)     │
└──────────┬──────────┘
           │
           ▼
      ┌────────┐
      │ Tick   │<────────────┐
      └────┬───┘             │
           │                 │
           ▼                 │
    ┌───────────────┐        │
    │ Perform Check │        │
    │ (HTTP/TCP/...)│        │
    └───────┬───────┘        │
            │                │
    ┌───────┴───────┐        │
    ▼               ▼        │
┌────────┐      ┌────────┐  │
│ Success│      │ Failure│  │
└───┬────┘      └───┬────┘  │
    │               │        │
    ▼               ▼        │
┌────────────┐  ┌──────────────────┐
│ Reset      │  │ Increment        │
│ fail_count │  │ fail_count       │
└─────┬──────┘  └────┬─────────────┘
      │              │
      │              ▼
      │         ┌────────────────┐    No
      │         │ fail_count >=  │────┘
      │         │ max_retries?   │
      │         └────┬───────────┘
      │              │ Yes
      │              ▼
      │         ┌──────────────┐
      │         │ Trigger      │
      │         │ Restart      │
      │         └──────────────┘
      │
      └──────────────┘
```

---

### 3. 服务重启流程

```
┌────────────────┐
│ Restart        │
│ Triggered      │
└───────┬────────┘
        │
        ▼
┌─────────────────┐
│ Send SIGTERM    │
│ to Process      │
└───────┬─────────┘
        │
        ▼
┌─────────────────┐
│ Wait 5s for     │
│ Graceful Exit   │
└───────┬─────────┘
        │
    ┌───┴────┐
    ▼        ▼
┌────────┐  ┌────────┐
│ Exited │  │Timeout │
└───┬────┘  └───┬────┘
    │           │
    │           ▼
    │      ┌──────────┐
    │      │ SIGKILL  │
    │      └────┬─────┘
    │           │
    └───────┬───┘
            │
            ▼
   ┌─────────────────┐
   │ Check Restart   │
   │ Policy          │
   └────────┬────────┘
            │
    ┌───────┴────────┐
    ▼                ▼
┌──────────┐   ┌─────────────┐
│ attempts │   │ attempts >  │──> Give Up
│ <= max   │   │ max         │
└────┬─────┘   └─────────────┘
     │
     ▼
┌─────────────────┐
│ Calculate       │
│ Backoff Delay   │
│ (Exponential)   │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Sleep(delay)    │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Spawn Process   │
│ Again           │
└────────┬────────┘
         │
         ▼
┌─────────────────┐
│ Wait for        │
│ Health Check    │
└─────────────────┘
```

---

### 4. 日志收集流程

```
┌──────────────────┐
│ Service Process  │
└────┬────────┬────┘
     │        │
     │stdout  │stderr
     │        │
     ▼        ▼
┌─────────────────────┐
│ tokio::io::BufReader│
└──────────┬──────────┘
           │ Line-by-line
           ▼
    ┌──────────────┐
    │ LogCollector │
    └──────┬───────┘
           │
           ▼
    ┌────────────────┐
    │ Parse Timestamp│
    │ + Level        │
    └──────┬─────────┘
           │
           ▼
    ┌────────────────┐
    │ Ring Buffer    │ (Last 1000 entries)
    │ (VecDeque)     │
    └──────┬─────────┘
           │
           ├──> devd logs (Query)
           │
           └──> Disk Writer (Optional)
                ~/.devd/logs/service/2024-01-03.log
```

---

## 目录结构

```
devd/
├── Cargo.toml                 # Rust project manifest
├── Cargo.lock
├── README.md
├── TECHNICAL_DESIGN.md        # This file
├── ARCHITECTURE.md            # Architecture diagrams
├── AGENTS.md                  # AI agent collaboration rules
│
├── src/
│   ├── main.rs                # Entry point
│   │
│   ├── cli/                   # CLI layer
│   │   ├── mod.rs
│   │   ├── commands/
│   │   │   ├── init.rs        # devd init
│   │   │   ├── start.rs       # devd start
│   │   │   ├── stop.rs        # devd stop
│   │   │   ├── restart.rs     # devd restart
│   │   │   ├── logs.rs        # devd logs
│   │   │   ├── status.rs      # devd status
│   │   │   ├── graph.rs       # devd graph
│   │   │   └── check.rs       # devd check
│   │   └── output.rs          # Colored output formatting
│   │
│   ├── core/                  # Domain layer
│   │   ├── mod.rs
│   │   ├── service_manager.rs # Main orchestrator
│   │   ├── service_task.rs    # Per-service task
│   │   ├── dependency.rs      # Dependency graph + topological sort
│   │   ├── health_check.rs    # Health check implementations
│   │   ├── restart_policy.rs  # Restart strategy
│   │   ├── log_collector.rs   # Log streaming + buffering
│   │   └── process_manager.rs # Process spawn/kill/signal
│   │
│   ├── config/                # Configuration
│   │   ├── mod.rs
│   │   ├── loader.rs          # YAML parsing + validation
│   │   ├── schema.rs          # Config structs (serde models)
│   │   └── env_subst.rs       # Environment variable substitution
│   │
│   ├── storage/               # State persistence
│   │   ├── mod.rs
│   │   ├── state_store.rs     # services.json read/write
│   │   └── log_storage.rs     # Log file rotation
│   │
│   └── utils/                 # Infrastructure utilities
│       ├── mod.rs
│       ├── http_client.rs     # HTTP health check client
│       ├── system_info.rs     # CPU/memory stats (sysinfo)
│       └── signal_handler.rs  # SIGTERM/SIGINT handling
│
├── tests/
│   ├── integration/           # Integration tests
│   │   ├── basic_start_stop.rs
│   │   ├── dependency_order.rs
│   │   ├── auto_restart.rs
│   │   └── health_check.rs
│   └── fixtures/              # Test configs
│       ├── simple.yml
│       ├── with-deps.yml
│       └── mock-server.sh
│
└── examples/
    ├── devd.yml               # Example config
    └── mock-service/          # Demo HTTP server for testing
        └── server.js
```

---

## 关键数据结构

### Config Schema (Rust)

```rust
// src/config/schema.rs

#[derive(Debug, Deserialize)]
pub struct DevdConfig {
    pub version: String,
    pub services: HashMap<String, ServiceConfig>,
}

#[derive(Debug, Deserialize)]
pub struct ServiceConfig {
    pub command: String,
    #[serde(default)]
    pub cwd: Option<PathBuf>,
    #[serde(default)]
    pub env: HashMap<String, String>,
    #[serde(default)]
    pub env_file: Option<PathBuf>,
    #[serde(default)]
    pub depends_on: Vec<Dependency>,
    #[serde(default)]
    pub healthcheck: Option<HealthCheck>,
    #[serde(default)]
    pub restart: RestartPolicy,
    #[serde(default)]
    pub limits: Option<ResourceLimits>,
}

#[derive(Debug, Deserialize)]
pub struct Dependency {
    pub service: String,
    #[serde(default)]
    pub condition: DependencyCondition,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyCondition {
    Started,
    SocketReady,
    TcpReady,
    HttpReady,
}

#[derive(Debug, Deserialize)]
#[serde(tag = "type")]
pub enum HealthCheck {
    #[serde(rename = "http")]
    Http {
        url: String,
        interval: u64,
        timeout: u64,
        retries: u32,
    },
    #[serde(rename = "tcp")]
    Tcp {
        port: u16,
        interval: u64,
        timeout: u64,
    },
    #[serde(rename = "socket")]
    Socket {
        path: PathBuf,
        interval: u64,
    },
}

#[derive(Debug, Deserialize)]
pub struct RestartPolicy {
    pub policy: RestartPolicyType,
    pub backoff: BackoffType,
    pub initial_delay: u64,
    pub max_delay: u64,
    pub max_attempts: u32,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "kebab-case")]
pub enum RestartPolicyType {
    Always,
    OnFailure,
    Never,
}

#[derive(Debug, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum BackoffType {
    Fixed,
    Exponential,
}
```

---

### Service Task State Machine

```rust
// src/core/service_task.rs

pub struct ServiceTask {
    pub name: String,
    pub config: ServiceConfig,
    pub state: ServiceState,
    pub process: Option<Child>,
    pub restart_policy: RestartPolicy,
    pub health_checker: HealthChecker,
    pub log_collector: LogCollector,
}

#[derive(Debug, Clone, PartialEq)]
pub enum ServiceState {
    Pending,
    Starting,
    Healthy,
    Unhealthy,
    Restarting,
    Stopping,
    Stopped,
    Failed,
}

impl ServiceTask {
    pub async fn start(&mut self) -> Result<()> {
        // Spawn process, start health check, collect logs
    }
    
    pub async fn stop(&mut self) -> Result<()> {
        // Send SIGTERM, wait, SIGKILL if needed
    }
    
    pub async fn restart(&mut self) -> Result<()> {
        // Stop + delay + start
    }
    
    pub async fn health_check_loop(&mut self) {
        // Loop until task is stopped
    }
}
```

---

## 并发模型

```
┌───────────────────────────────────────────────┐
│          Tokio Runtime (Single Thread)        │
│                                               │
│  ┌─────────────────────────────────────────┐ │
│  │         Main Event Loop                 │ │
│  │  • Signal handling                      │ │
│  │  • CLI command dispatch                 │ │
│  └───────────┬─────────────────────────────┘ │
│              │                                 │
│  ┌───────────▼───────────────────────┐       │
│  │  ServiceManager::run()            │       │
│  │  • Spawn N service tasks          │       │
│  │  • Await completion               │       │
│  └───────────┬───────────────────────┘       │
│              │                                 │
│   ┌──────────┼──────────┐                    │
│   ▼          ▼          ▼                    │
│ ┌────────┐ ┌────────┐ ┌────────┐            │
│ │Service │ │Service │ │Service │            │
│ │Task 1  │ │Task 2  │ │Task N  │            │
│ └───┬────┘ └───┬────┘ └───┬────┘            │
│     │          │          │                  │
│   ┌─┴──────────┴──────────┴─┐               │
│   │  Concurrent Execution    │               │
│   │  (tokio::spawn for each) │               │
│   └──────────────────────────┘               │
└───────────────────────────────────────────────┘
```

**说明**：
- Tokio 默认单线程运行时（`#[tokio::main(flavor = "current_thread")]`）
- 每个服务是独立的异步任务（`tokio::spawn`）
- 健康检查、日志收集、重启策略都是该任务内的 sub-task
- 避免锁竞争（用 `mpsc` channel 通信）

---

## 部署架构

```
┌──────────────────────────────────────┐
│  Developer Machine                   │
│                                      │
│  ┌────────────────────────────────┐ │
│  │  Terminal                      │ │
│  │  $ devd start                  │ │
│  └──────────┬─────────────────────┘ │
│             │                        │
│  ┌──────────▼─────────────────────┐ │
│  │  devd Process                  │ │
│  │  PID: 12345                    │ │
│  │  Memory: ~30MB                 │ │
│  └──────────┬─────────────────────┘ │
│             │                        │
│     ┌───────┼───────┐               │
│     ▼       ▼       ▼               │
│  ┌──────┐┌──────┐┌──────┐          │
│  │postgres││backend││frontend│      │
│  │PID 123││PID 456││PID 789│       │
│  └──────┘└──────┘└──────┘          │
│                                      │
│  ┌────────────────────────────────┐ │
│  │  ~/.devd/                      │ │
│  │  ├── state/services.json       │ │
│  │  └── logs/                     │ │
│  └────────────────────────────────┘ │
└──────────────────────────────────────┘
```

**说明**：
- devd 是前台进程（不是 daemon），用户 Ctrl+C 即退出
- 所有子服务是 devd 的子进程（`kill_on_drop` 保证清理）
- 状态文件持久化，下次启动可恢复（可选）

---

## 扩展点设计

### 1. 健康检查插件

```rust
// 用户可自定义健康检查
pub trait HealthChecker: Send + Sync {
    async fn check(&self) -> Result<bool>;
}

// 内置实现
pub struct HttpHealthChecker { ... }
pub struct TcpHealthChecker { ... }
pub struct ScriptHealthChecker { ... }

// 用户可通过配置加载自定义脚本
```

### 2. 日志处理插件

```rust
// 用户可自定义日志处理（例如发送到 Loki、ElasticSearch）
pub trait LogSink: Send + Sync {
    async fn write(&self, entry: &LogEntry) -> Result<()>;
}

// 内置实现
pub struct StdoutSink { ... }
pub struct FileSink { ... }

// 未来扩展
// pub struct LokiSink { ... }
```

---

## 性能指标目标

| 指标 | 目标值 | 测量方法 |
|------|--------|----------|
| devd 内存占用 | < 50 MB | `ps aux | grep devd` |
| devd CPU 占用（空闲） | < 1% | `top -pid <devd_pid>` |
| 启动 10 个服务耗时 | < 5s | `time devd start` |
| 日志吞吐量 | > 10k lines/s | 压测工具 + 计时 |
| 健康检查延迟 | < 100ms (HTTP) | 日志时间戳对比 |

---

## 安全考量

### 1. 环境变量注入
- 配置文件中的 `${VAR}` 只替换已设置的环境变量
- 未设置的变量报错（防止意外使用空值）
- 不支持命令执行（`$(cmd)`），只替换变量

### 2. 进程隔离
- 子进程继承 devd 的用户权限（不提权）
- 不支持以不同用户运行服务（避免权限问题）
- 资源限制通过软限制（监控 + 告警），不用 cgroups（避免 root 权限）

### 3. 日志脱敏（未来）
- 配置敏感字段白名单（`DATABASE_URL`、`API_KEY`）
- 日志输出时自动 mask

---

## 参考架构

**类似项目架构对比**：

| 项目 | 语言 | 架构风格 | 并发模型 | 判断 |
|------|------|----------|----------|------|
| pm2 | Node.js | 单进程 + 事件循环 | 单线程异步 | 简洁但功能弱 |
| foreman | Ruby | 单进程 + 多线程 | 线程池 | 简单但无健康检查 |
| hivemind | Go | 单进程 + goroutines | CSP 并发 | 轻量但依赖图弱 |
| systemd | C | 多进程 + D-Bus | 多进程 IPC | 强大但复杂 |
| **devd** | Rust | 单进程 + async | Tokio | ✅ 平衡性能和功能 |

---

## 总结

**架构核心思想**：
1. **简单**：单进程守护，避免复杂的 IPC
2. **异步**：Tokio 并发模型，高效管理多服务
3. **模块化**：清晰的分层架构，易于测试和扩展
4. **跨平台**：Rust + 成熟库，一次编写处处运行

**技术亮点**：
- 依赖图驱动的启动顺序
- 多层健康检查 + 自动重启
- 统一日志流 + 彩色输出
- 轻量、快速、易用
