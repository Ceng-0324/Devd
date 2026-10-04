# Devd

- 这个东西因为一个**fucking event**而产生...

devd 是面向本地开发的轻量服务管理器：按依赖启动多个进程，执行 TCP/HTTP 健康检查，自动重启并聚合日志。当前 MVP 支持 Linux 和 macOS。

## 安装与运行

```bash
cargo install --path . --locked
```

在项目目录创建 `devd.yml`：

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
devd check
devd graph
devd start                    # 前台运行并实时输出日志，Ctrl+C 有序停止
```

在另一个终端进入相同项目目录：

```bash
devd status
devd status --json
devd logs                     # 最近 100 条缓冲日志，由旧到新
devd logs worker --tail 50
devd restart worker
devd stop
```

## 参数与行为

| 参数 | 行为 |
| --- | --- |
| `-c, --config <PATH>` | 配置文件，默认 `devd.yml`；可放在子命令前后 |
| `--state-dir <PATH>` | 覆盖运行目录；同一实例的所有命令使用相同目录 |
| `--color auto\|always\|never` | 日志颜色，默认自动判断终端 |
| `status --json` | 输出实时状态、PID、开始时间、重启次数和错误信息 |
| `logs [SERVICE] --tail <N>` | 可选服务过滤，N 为 1–1000，默认 100 |

- 服务 `cwd` 相对配置文件所在目录，省略时使用该目录；`env-file` 相对服务 `cwd`，显式 `env` 覆盖环境文件。命令支持参数引号，但不隐式调用 shell；管道等语法需显式使用 `sh -c '...'`。
- `check` 校验结构、依赖关系、命令引号和 MVP 支持的设置，不启动进程，也不保证可执行文件、环境文件或远程端点在启动时可用。`graph` 显示服务到前置依赖的边及并行启动层。
- `start` 不创建后台 daemon；另一个终端的命令通过项目 Unix socket 访问前台实例。`stop` 成功表示关闭请求已接受，前台 `start` 会在反向依赖清理完成后退出。SIGINT/SIGTERM 使用同一关闭流程。
- `restart` 使用启动时的配置，只重启指定服务，等待旧代及日志读取结束后重新检查依赖。成功表示新进程已启动，不保证此时已健康。手动重启不受 `never` 或自动重试上限阻止；累计重启次数和日志代次继续增长，自动重试仍按累计次数判断上限。其他服务不联动重启。若全栈已经退出，需要重新 `start`。
- 默认运行目录为配置文件目录下的 `.devd/<配置文件名>/`，包含控制 socket、锁和原子状态快照。路径过长时，用较短的 `--state-dir`。重复启动会失败，不会影响原实例。
- `status` 只报告可连接实例的实时状态；离线返回非零，不把遗留快照里的 PID 当作活进程。`stop`、`status`、`logs` 和 `restart` 不重新解析 YAML，因此配置内容损坏或文件删除后仍可操作原实例。
- 日志仅在内存保存，全栈最近 1000 条，单行最多 16 KiB；停止后不可查询。`start` 实时输出，`logs` 查询快照。输出过慢可能丢失实时条目并显示告警；输出断开会触发服务清理。
- 命令失败返回非零并在 stderr 输出原因。`init`、`top`、`logs --follow`、高级日志过滤、热重载和磁盘日志不在本次 MVP 命令范围内。

## 开发验证

```bash
cargo fmt --all -- --check
cargo test --locked
cargo clippy --locked --all-targets -- -D warnings
cargo build --locked
cargo doc --locked --no-deps
```

真实二进制命令测试位于 `tests/cli.rs`，可单独运行 `cargo test --locked --test cli`。设计与后续规划见 [TECHNICAL_DESIGN.md](TECHNICAL_DESIGN.md) 和 [ARCHITECTURE.md](ARCHITECTURE.md)。
