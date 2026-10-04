use std::{
    future::Future,
    io,
    pin::Pin,
    task::{Context, Poll},
    time::Duration,
};

use chrono::{TimeZone, Utc};
use devd::logging::{
    write_logs, ColorMode, LogCollector, LogEntry, LogFormatter, LogLevel, LogOptions,
};
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWriteExt, ReadBuf};

async fn bounded<F: Future>(future: F) -> F::Output {
    tokio::time::timeout(Duration::from_secs(10), future)
        .await
        .expect("logging test timed out")
}

fn collector() -> LogCollector {
    LogCollector::new(LogOptions::default()).unwrap()
}

fn plain() -> LogFormatter {
    LogFormatter {
        color: ColorMode::Never,
    }
}

fn entry(level: LogLevel) -> LogEntry {
    LogEntry {
        timestamp: Utc.with_ymd_and_hms(2026, 10, 4, 15, 30, 1).unwrap(),
        service: "backend".into(),
        generation: 1,
        level,
        message: "ready".into(),
        truncated: false,
    }
}

#[tokio::test]
async fn test_logging_stream_closure_flushes_tail_and_classifies_stderr() {
    let collector = collector();
    let start = Utc::now();
    collector
        .collect(
            b"one\r\n\nno-newline".as_slice(),
            "api".into(),
            0,
            LogLevel::Info,
        )
        .await
        .unwrap();
    collector
        .collect(
            b"failure\nlast error".as_slice(),
            "api".into(),
            1,
            LogLevel::Error,
        )
        .await
        .unwrap();
    let entries = collector.history().recent(None, 10);
    assert_eq!(
        entries
            .iter()
            .map(|e| (e.level, e.message.as_str()))
            .collect::<Vec<_>>(),
        [
            (LogLevel::Info, "one"),
            (LogLevel::Info, ""),
            (LogLevel::Info, "no-newline"),
            (LogLevel::Error, "failure"),
            (LogLevel::Error, "last error")
        ]
    );
    assert!(entries
        .iter()
        .all(|e| e.service == "api" && e.timestamp >= start && e.timestamp <= Utc::now()));
    assert_eq!(entries.last().unwrap().generation, 1);
}

#[tokio::test]
async fn test_logging_cancellation_flushes_partial_line_once() {
    let collector = collector();
    let (mut writer, reader) = tokio::io::duplex(64);
    let mut receiver = collector.subscribe();
    let capture = {
        let collector = collector.clone();
        tokio::spawn(async move {
            collector
                .collect(reader, "api".into(), 0, LogLevel::Info)
                .await
        })
    };
    writer.write_all(b"complete\nunfinished").await.unwrap();
    assert_eq!(bounded(receiver.recv()).await.unwrap().message, "complete");
    capture.abort();
    assert!(bounded(capture).await.unwrap_err().is_cancelled());
    assert_eq!(
        bounded(receiver.recv()).await.unwrap().message,
        "unfinished"
    );
    assert!(receiver.try_recv().is_err());
}

struct BrokenReader(bool);
impl AsyncRead for BrokenReader {
    fn poll_read(
        mut self: Pin<&mut Self>,
        _: &mut Context<'_>,
        buffer: &mut ReadBuf<'_>,
    ) -> Poll<io::Result<()>> {
        if self.0 {
            Poll::Ready(Err(io::Error::other("test read failure")))
        } else {
            self.0 = true;
            buffer.put_slice(b"tail");
            Poll::Ready(Ok(()))
        }
    }
}

#[tokio::test]
async fn test_logging_io_failure_flushes_tail_and_reports_diagnostic() {
    let collector = collector();
    assert!(collector
        .collect(BrokenReader(false), "api".into(), 2, LogLevel::Error)
        .await
        .is_err());
    let entries = collector.history().recent(None, 10);
    assert_eq!(entries.len(), 2);
    assert_eq!(entries[0].message, "tail");
    assert_eq!(entries[0].level, LogLevel::Error);
    assert_eq!(entries[1].level, LogLevel::Warn);
    assert!(entries[1].message.contains("test read failure"));
}

#[test]
fn test_logging_formatter_plain_levels_timestamp_and_truncation() {
    for (level, name) in [
        (LogLevel::Info, "INFO"),
        (LogLevel::Warn, "WARN"),
        (LogLevel::Error, "ERROR"),
    ] {
        assert_eq!(
            plain().format(&entry(level)),
            format!("[15:30:01.000Z] [backend] [{name}] ready\n")
        );
    }
    let mut entry = entry(LogLevel::Info);
    entry.truncated = true;
    assert!(plain().format(&entry).ends_with("ready [truncated]\n"));
}

#[test]
fn test_logging_formatter_color_is_explicit_and_service_color_is_stable() {
    let format = LogFormatter {
        color: ColorMode::Always,
    };
    let entry = entry(LogLevel::Error);
    let first = format.format(&entry);
    assert_eq!(first, format.format(&entry));
    assert!(first.contains("\x1b[31mERROR\x1b[0m"));
    assert!(first.contains("backend\x1b[0m"));
    assert!(!plain().format(&entry).contains('\x1b'));
    let mut other = entry.clone();
    other.service = "frontend".into();
    assert_ne!(
        first.split('[').nth(3),
        format.format(&other).split('[').nth(3)
    );
}

#[test]
fn test_logging_formatter_escapes_controls_and_preserves_unicode_and_tabs() {
    let mut entry = entry(LogLevel::Info);
    entry.message = "hello\t\u{4e2d}\nforged\r\x1b[2J\0\x08".into();
    entry.service = "api\nspoof".into();
    let formatted = plain().format(&entry);
    assert_eq!(formatted.lines().count(), 1);
    assert!(!formatted.contains('\x1b'));
    assert!(formatted.contains("hello\t\u{4e2d}\\nforged\\r"));
    assert!(formatted.contains("api\\nspoof"));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_logging_concurrent_producers_history_and_subscribers_have_same_order() {
    let collector = LogCollector::new(LogOptions {
        capacity: 200,
        ..LogOptions::default()
    })
    .unwrap();
    let mut subscriber = collector.subscribe();
    let mut tasks = tokio::task::JoinSet::new();
    for service in ["a", "b", "c", "d"] {
        let collector = collector.clone();
        tasks.spawn(async move {
            for index in 0..40 {
                collector
                    .collect(
                        format!("{service}-{index}\n").as_bytes(),
                        service.into(),
                        0,
                        LogLevel::Info,
                    )
                    .await
                    .unwrap();
                tokio::task::yield_now().await;
            }
        });
    }
    while let Some(result) = bounded(tasks.join_next()).await {
        result.unwrap();
    }
    let mut received = Vec::new();
    while let Ok(entry) = subscriber.try_recv() {
        received.push(entry);
    }
    assert_eq!(received.len(), 160);
    assert_eq!(received, collector.history().recent(None, 200));
    for service in ["a", "b", "c", "d"] {
        assert_eq!(
            received
                .iter()
                .filter(|e| e.service == service)
                .map(|e| e.message.clone())
                .collect::<Vec<_>>(),
            (0..40)
                .map(|i| format!("{service}-{i}"))
                .collect::<Vec<_>>()
        );
    }
}

#[tokio::test]
async fn test_logging_serial_writer_completes_after_producers_close_and_history_survives() {
    let collector = collector();
    let receiver = collector.subscribe();
    let history = collector.history();
    let mut output = Vec::new();
    collector
        .collect(b"one\ntwo\n".as_slice(), "api".into(), 0, LogLevel::Info)
        .await
        .unwrap();
    drop(collector);
    let summary = bounded(write_logs(&mut output, receiver, plain()))
        .await
        .unwrap();
    assert_eq!(summary.entries, 2);
    assert_eq!(summary.dropped, 0);
    let output = String::from_utf8(output).unwrap();
    let lines: Vec<_> = output.lines().collect();
    assert!(lines[0].ends_with("[api] [INFO] one"));
    assert!(lines[1].ends_with("[api] [INFO] two"));
    assert_eq!(history.recent(None, 10).len(), 2);
}

#[tokio::test]
async fn test_logging_slow_subscriber_reports_loss_of_complete_entries() {
    let collector = LogCollector::new(LogOptions {
        capacity: 600,
        ..LogOptions::default()
    })
    .unwrap();
    let receiver = collector.subscribe();
    let history = collector.history();
    for i in 0..600 {
        collector
            .collect(
                format!("line-{i}\n").as_bytes(),
                "api".into(),
                0,
                LogLevel::Info,
            )
            .await
            .unwrap();
    }
    drop(collector);
    let mut output = Vec::new();
    let summary = bounded(write_logs(&mut output, receiver, plain()))
        .await
        .unwrap();
    assert_eq!(summary.entries, 256);
    assert_eq!(summary.dropped, 344);
    let output = String::from_utf8(output).unwrap();
    assert!(output
        .lines()
        .next()
        .unwrap()
        .contains("[devd] [WARN] log output skipped 344 entries"));
    assert!(output.lines().last().unwrap().ends_with("line-599"));
    assert_eq!(output.lines().count(), 257);
    assert_eq!(history.recent(None, 1000).len(), 600);
}

#[tokio::test]
async fn test_logging_broken_output_returns_error_without_stopping_capture() {
    let collector = collector();
    let receiver = collector.subscribe();
    collector
        .collect(b"one\n".as_slice(), "api".into(), 0, LogLevel::Info)
        .await
        .unwrap();
    let (writer, reader) = tokio::io::duplex(64);
    drop(reader);
    assert!(bounded(write_logs(writer, receiver, plain()))
        .await
        .is_err());
    collector
        .collect(b"two\n".as_slice(), "api".into(), 0, LogLevel::Info)
        .await
        .unwrap();
    assert_eq!(collector.history().recent(None, 10).len(), 2);
}

#[tokio::test]
async fn test_logging_partial_async_sink_writes_whole_prefixed_lines() {
    let collector = collector();
    let receiver = collector.subscribe();
    collector
        .collect(b"one\ntwo\n".as_slice(), "api".into(), 0, LogLevel::Info)
        .await
        .unwrap();
    drop(collector);
    let (writer, mut reader) = tokio::io::duplex(3);
    let mut bytes = Vec::new();
    let (written, read) = bounded(async {
        tokio::join!(
            write_logs(writer, receiver, plain()),
            reader.read_to_end(&mut bytes)
        )
    })
    .await;
    assert_eq!(written.unwrap().entries, 2);
    read.unwrap();
    assert_eq!(String::from_utf8(bytes).unwrap().lines().count(), 2);
}

#[tokio::test]
async fn test_logging_stalled_terminal_does_not_block_capture_or_history() {
    let collector = LogCollector::new(LogOptions {
        capacity: 600,
        ..LogOptions::default()
    })
    .unwrap();
    let receiver = collector.subscribe();
    let history = collector.history();
    let (writer, mut reader) = tokio::io::duplex(1);
    let output = tokio::spawn(write_logs(writer, receiver, plain()));
    collector
        .collect(b"first\n".as_slice(), "api".into(), 0, LogLevel::Info)
        .await
        .unwrap();
    let mut first_byte = [0];
    bounded(reader.read_exact(&mut first_byte)).await.unwrap();
    let input: String = (0..600).map(|i| format!("line-{i}\n")).collect();
    bounded(collector.collect(input.as_bytes(), "api".into(), 0, LogLevel::Info))
        .await
        .unwrap();
    assert_eq!(history.recent(None, 1000).len(), 600);
    assert_eq!(history.recent(None, 1)[0].message, "line-599");
    drop(collector);
    let mut bytes = Vec::new();
    let (read, output) =
        bounded(async { tokio::join!(reader.read_to_end(&mut bytes), output) }).await;
    read.unwrap();
    let summary = output.unwrap().unwrap();
    assert_eq!(summary.entries + summary.dropped, 601);
    assert!(summary.dropped > 0);
}

#[cfg(unix)]
mod managed {
    use std::{collections::HashMap, path::Path};

    use devd::{
        config::{DevdConfig, RestartPolicyType, ServiceConfig},
        core::service_manager::{ManagerOptions, ServiceManager, ServiceManagerError},
    };
    use tempfile::{tempdir, TempDir};

    use super::*;

    fn service(script: &str, directory: &Path) -> ServiceConfig {
        let mut config: ServiceConfig =
            serde_yaml::from_str("command: test\nrestart: {policy: never}").unwrap();
        config.command = shell_words::join(["/bin/sh", "-c", script]);
        config
            .env
            .insert("DEVD_LOG_DIR".into(), directory.to_string_lossy().into());
        config
    }

    fn config(services: impl IntoIterator<Item = (&'static str, ServiceConfig)>) -> DevdConfig {
        DevdConfig {
            version: "1".into(),
            services: services
                .into_iter()
                .map(|(name, config)| (name.into(), config))
                .collect::<HashMap<_, _>>(),
        }
    }

    fn options(directory: &TempDir) -> ManagerOptions {
        let mut options = ManagerOptions::new(directory.path().join("state/services.json"));
        options.grace_period = Duration::from_millis(100);
        options
    }

    #[tokio::test]
    async fn test_logging_managed_services_capture_both_streams_and_serial_output() {
        let directory = tempdir().unwrap();
        let manager = ServiceManager::new(
            config([
                (
                    "api",
                    service(
                        "printf 'api-one\\napi-tail'; printf 'api-error\\n' >&2",
                        directory.path(),
                    ),
                ),
                (
                    "web",
                    service(
                        "printf 'web-one\\n'; printf 'web-tail' >&2",
                        directory.path(),
                    ),
                ),
            ]),
            options(&directory),
        )
        .unwrap();
        let history = manager.log_history();
        let receiver = manager.subscribe_logs();
        let mut output = Vec::new();
        let (run, written) = bounded(async {
            tokio::join!(
                manager.run_until(std::future::pending()),
                write_logs(&mut output, receiver, plain())
            )
        })
        .await;
        run.unwrap();
        assert_eq!(written.unwrap().entries, 5);
        let entries = history.recent(None, 10);
        assert_eq!(entries.len(), 5);
        assert!(entries
            .iter()
            .any(|e| e.service == "api" && e.level == LogLevel::Error && e.message == "api-error"));
        assert!(entries
            .iter()
            .any(|e| e.service == "web" && e.level == LogLevel::Error && e.message == "web-tail"));
        let expected = entries
            .iter()
            .map(|e| plain().format(e))
            .collect::<String>();
        assert_eq!(String::from_utf8(output).unwrap(), expected);
    }

    #[tokio::test]
    async fn test_logging_managed_noisy_service_does_not_block_without_subscribers() {
        let directory = tempdir().unwrap();
        let mut child = service("unused", directory.path());
        let fixture =
            Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/fixtures/process-service.sh");
        child.command = shell_words::join(["/bin/sh", fixture.to_str().unwrap(), "output"]);
        let mut options = options(&directory);
        options.logging.capacity = 7;
        let manager = ServiceManager::new(config([("noisy", child)]), options).unwrap();
        let history = manager.log_history();
        let snapshot = bounded(manager.run_until(std::future::pending()))
            .await
            .unwrap();
        assert_eq!(snapshot.services["noisy"].last_exit_code, Some(0));
        assert_eq!(history.recent(None, usize::MAX).len(), 7);
        assert!(history
            .recent(None, 7)
            .iter()
            .all(|e| e.message == "0123456789abcdefghijklmnopqrstuv"));
    }

    #[tokio::test]
    async fn test_logging_managed_restart_flushes_each_generation_tail_separately() {
        let directory = tempdir().unwrap();
        let mut child = service("if [ ! -f \"$DEVD_LOG_DIR/attempt\" ]; then touch \"$DEVD_LOG_DIR/attempt\"; printf 'old-tail'; exit 7; fi; printf 'new-tail'; exit 0", directory.path());
        child.restart.policy = RestartPolicyType::OnFailure;
        child.restart.initial_delay = Duration::ZERO;
        let manager = ServiceManager::new(config([("api", child)]), options(&directory)).unwrap();
        let history = manager.log_history();
        bounded(manager.run_until(std::future::pending()))
            .await
            .unwrap();
        let entries = history.recent(None, 10);
        assert_eq!(
            entries
                .iter()
                .map(|e| (e.generation, e.message.as_str()))
                .collect::<Vec<_>>(),
            [(0, "old-tail"), (1, "new-tail")]
        );
    }

    #[tokio::test]
    async fn test_logging_managed_shutdown_preserves_final_unterminated_stderr() {
        let directory = tempdir().unwrap();
        let child = service(
            "trap 'printf final-tail >&2; exit 0' TERM; echo ready; sleep 60 & wait",
            directory.path(),
        );
        let manager = ServiceManager::new(config([("api", child)]), options(&directory)).unwrap();
        let history = manager.log_history();
        let mut ready = manager.subscribe_logs();
        let receiver = manager.subscribe_logs();
        let mut output = Vec::new();
        let shutdown = async {
            assert_eq!(ready.recv().await.unwrap().message, "ready");
        };
        let (run, written) = bounded(async {
            tokio::join!(
                manager.run_until(shutdown),
                write_logs(&mut output, receiver, plain())
            )
        })
        .await;
        run.unwrap();
        assert_eq!(written.unwrap().entries, 2);
        let last = history.recent(None, 1).pop().unwrap();
        assert_eq!(last.message, "final-tail");
        assert_eq!(last.level, LogLevel::Error);
        assert!(String::from_utf8(output)
            .unwrap()
            .ends_with("[api] [ERROR] final-tail\n"));
    }

    #[tokio::test]
    async fn test_logging_managed_line_limit_is_applied_before_history_and_live_output() {
        let directory = tempdir().unwrap();
        let mut options = options(&directory);
        options.logging.max_line_bytes = 4;
        let child = service("printf 'abcdefghijk\\nnext\\n'", directory.path());
        let manager = ServiceManager::new(config([("api", child)]), options).unwrap();
        let history = manager.log_history();
        bounded(manager.run_until(std::future::pending()))
            .await
            .unwrap();
        let entries = history.recent(None, 10);
        assert_eq!(entries.len(), 2);
        assert_eq!(
            (entries[0].message.as_str(), entries[0].truncated),
            ("abcd", true)
        );
        assert_eq!(
            (entries[1].message.as_str(), entries[1].truncated),
            ("next", false)
        );
    }

    #[test]
    fn test_logging_managed_invalid_options_fail_before_children_or_state_files() {
        let directory = tempdir().unwrap();
        let mut options = options(&directory);
        options.logging.capacity = 0;
        let path = options.state_path.clone();
        let child = service("touch \"$DEVD_LOG_DIR/started\"", directory.path());
        assert!(matches!(
            ServiceManager::new(config([("api", child)]), options),
            Err(ServiceManagerError::LogOptions(_))
        ));
        assert!(!path.exists());
        assert!(!directory.path().join("started").exists());
    }
}
