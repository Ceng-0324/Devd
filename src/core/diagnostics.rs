//! Deterministic explanations built from runtime snapshots and lifecycle facts.
//!
//! This module is deliberately read-only. It never probes services, starts a
//! process, or turns a suggested next step into a control action.
use std::collections::{BTreeMap, BTreeSet};

use serde::{Deserialize, Serialize};

use super::{
    events::{
        query::{EventBatch, EventGap, EventKind, EventSource, PersistenceState},
        EventContext, EventData, LifecycleEvent, ProbeEvidence, ProcessEvidence, ResourceValue,
        RestartCause, RestartOutcome,
    },
    service_manager::{RuntimeSnapshot, ServiceState},
};

pub const DIAGNOSTIC_SCHEMA_VERSION: u16 = 1;

#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
#[serde(rename_all = "kebab-case")]
pub enum ExplainConclusion {
    DependencyBlocked,
    StartupFailed,
    HealthFailure,
    ResourceLimit,
    Restarting,
    RestartBudgetExhausted,
    ManuallyStopped,
    Stopped,
    Healthy,
    Running,
    Unknown,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExplainEvidence {
    pub sequence: u64,
    pub event_type: EventKind,
    pub generation: Option<u64>,
    pub cause: Option<u64>,
    pub timestamp: chrono::DateTime<chrono::Utc>,
    pub detail: String,
}

#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct ExplainReport {
    pub schema_version: u16,
    pub source: EventSource,
    pub persistence: Option<PersistenceState>,
    pub context: Option<EventContext>,
    pub service: String,
    pub status: Option<ServiceState>,
    pub generation: Option<u64>,
    pub conclusion: ExplainConclusion,
    pub summary: String,
    pub details: Vec<String>,
    pub evidence: Vec<ExplainEvidence>,
    pub complete: bool,
    pub gaps: Vec<EventGap>,
    pub omitted_gaps: u64,
    pub next_steps: Vec<String>,
}

/// Explain one service using one event batch. The same function serves live
/// and stored queries so text and JSON cannot silently disagree.
pub fn explain(
    service: &str,
    snapshot: Option<&RuntimeSnapshot>,
    batch: &EventBatch,
) -> ExplainReport {
    let state = snapshot.and_then(|snapshot| snapshot.services.get(service));
    let run = snapshot
        .and_then(|snapshot| snapshot.event_run_id.as_deref())
        .or_else(|| {
            batch
                .context
                .as_ref()
                .map(|context| context.run_id.as_str())
        });
    let mut events: Vec<&LifecycleEvent> = batch
        .entries
        .iter()
        .filter(|event| {
            event.service.as_deref() == Some(service) && run.is_none_or(|run| event.run_id == run)
        })
        .collect();
    events.sort_by_key(|event| event.sequence);
    // A live snapshot defines the generation even when it has not been
    // assigned yet (for example while a removed service is being re-added).
    // Stored queries can only describe the latest retained generation.
    let generation = match state {
        Some(state) => state.event_generation,
        None => events.iter().filter_map(|event| event.generation).max(),
    };
    events.retain(|event| event.generation == generation);
    let status = state.map(|state| state.status).or_else(|| {
        events.iter().rev().find_map(|event| match event.data {
            EventData::StateChanged { to, .. } => Some(to),
            EventData::HealthChanged { state, .. } => Some(state),
            EventData::Started { .. } => Some(ServiceState::Running),
            EventData::Starting => Some(ServiceState::Starting),
            EventData::GenerationPending => Some(ServiceState::Pending),
            _ => None,
        })
    });
    let mut evidence = Vec::new();
    let mut seen = BTreeSet::new();

    let mut conclusion = ExplainConclusion::Unknown;
    let mut summary = format!("{service}: 无法确定当前原因");
    let mut details = Vec::new();
    let mut has_diagnostic_evidence = false;

    let latest_budget = events
        .iter()
        .rev()
        .find(|event| {
            matches!(
                event.data,
                EventData::RestartDecision {
                    outcome: RestartOutcome::BudgetExhausted,
                    ..
                }
            )
        })
        .filter(|_| {
            !matches!(
                status,
                Some(
                    ServiceState::Running
                        | ServiceState::Healthy
                        | ServiceState::Stopped
                        | ServiceState::Stopping
                )
            )
        });
    let latest_trigger = events
        .iter()
        .rev()
        .find(|event| matches!(event.data, EventData::RestartTriggered { .. }))
        .copied()
        .or_else(|| {
            // New attempts explicitly link GenerationPending to the previous
            // restart decision. Follow that link, never a chronological guess.
            let mut event = *events.first()?;
            while let Some(cause) = event.cause {
                event = batch.entries.iter().find(|parent| {
                    parent.run_id == event.run_id
                        && parent.sequence == cause
                        && parent.sequence < event.sequence
                })?;
                if matches!(
                    event.data,
                    EventData::RestartTriggered { .. } | EventData::ManualRestartRequested
                ) {
                    return Some(event);
                }
            }
            None
        });
    let latest_manual_restart = events.iter().rev().find(|event| {
        matches!(
            event.data,
            EventData::ManualRestartRequested
                | EventData::ServiceStopRequested {
                    manual_restart: true
                }
        )
    });
    let latest_started = events
        .iter()
        .rev()
        .find(|event| matches!(event.data, EventData::Started { .. }));
    let latest_wait = events
        .iter()
        .rev()
        .find(|event| matches!(event.data, EventData::DependencyWaiting { .. }));
    let latest_ready = events
        .iter()
        .rev()
        .find(|event| matches!(event.data, EventData::DependencyReady { .. }));
    let latest_spawn = events
        .iter()
        .rev()
        .find(|event| matches!(event.data, EventData::SpawnFailed { .. }));
    let latest_dependency_failure = events.iter().rev().find(|event| {
        matches!(
            event.data,
            EventData::DependencyFailed { .. } | EventData::DependencyTimedOut { .. }
        )
    });
    let latest_start_failure = latest_spawn
        .into_iter()
        .chain(latest_dependency_failure)
        .max_by_key(|event| event.sequence)
        .filter(|failure| {
            !matches!(
                status,
                Some(
                    ServiceState::Running
                        | ServiceState::Healthy
                        | ServiceState::Stopped
                        | ServiceState::Stopping
                )
            ) && latest_started.is_none_or(|started| started.sequence < failure.sequence)
        });
    let latest_health_failure = events
        .iter()
        .rev()
        .find(|event| matches!(event.data, EventData::HealthChanged { .. }))
        .filter(|event| {
            matches!(
                event.data,
                EventData::HealthChanged {
                    state: ServiceState::Unhealthy,
                    ..
                }
            )
        });
    let mut metrics = BTreeSet::new();
    let latest_resource = events
        .iter()
        .rev()
        .find(|event| match &event.data {
            EventData::ResourceChanged { exceeded, evidence } => {
                let metric = matches!(evidence.value, ResourceValue::Memory { .. });
                metrics.insert(metric) && *exceeded
            }
            EventData::ResourceRestartRequested { evidence } => {
                !metrics.contains(&matches!(evidence.value, ResourceValue::Memory { .. }))
                    && matches!(
                        status,
                        Some(ServiceState::Restarting | ServiceState::Failed)
                    )
            }
            _ => false,
        })
        .filter(|_| !matches!(status, Some(ServiceState::Stopped | ServiceState::Stopping)));
    let latest_manual_stop = events.iter().rev().find(|event| {
        matches!(
            event.data,
            EventData::ServiceStopRequested {
                manual_restart: false
            }
        )
    });

    if let Some(event) = latest_budget {
        has_diagnostic_evidence = true;
        conclusion = ExplainConclusion::RestartBudgetExhausted;
        summary = format!("{service} 的自动重启预算已耗尽");
        if let EventData::RestartDecision {
            restart_count,
            policy,
            ..
        } = &event.data
        {
            details.push(format!(
                "已尝试 {} 次，策略允许的最大次数为 {}，当前策略为 {:?}",
                restart_count, policy.max_attempts, policy.policy
            ));
        }
        add_evidence(&mut evidence, &mut seen, event);
        add_cause(&mut evidence, &mut seen, event, &batch.entries);
    } else if let Some(event) = latest_wait.filter(|wait| {
        latest_ready.is_none_or(|ready| ready.sequence < wait.sequence)
            && matches!(status, Some(ServiceState::Pending | ServiceState::Starting))
    }) {
        has_diagnostic_evidence = true;
        conclusion = ExplainConclusion::DependencyBlocked;
        summary = format!("{service} 尚未启动，正在等待依赖就绪");
        if let EventData::DependencyWaiting {
            service: dependency,
            condition,
            timeout,
            remaining,
            ..
        } = &event.data
        {
            details.push(format!(
                "等待 {dependency} 满足 {}；已配置期限 {:?}，剩余 {:?}",
                condition_name(condition),
                timeout,
                remaining
            ));
            details.push(format!(
                "先检查 {dependency} 的状态和对应健康事件，不会因本次诊断自动重启服务"
            ));
        }
        add_evidence(&mut evidence, &mut seen, event);
    } else if let Some(event) = latest_start_failure {
        has_diagnostic_evidence = true;
        conclusion = ExplainConclusion::StartupFailed;
        summary = format!("{service} 启动失败");
        details.push(event_detail(event));
        add_evidence(&mut evidence, &mut seen, event);
        add_cause(&mut evidence, &mut seen, event, &batch.entries);
    } else if let Some(event) = latest_resource {
        has_diagnostic_evidence = true;
        conclusion = ExplainConclusion::ResourceLimit;
        summary = if matches!(event.data, EventData::ResourceRestartRequested { .. }) {
            format!("{service} 因资源阈值超限触发了重启")
        } else {
            format!("{service} 超过了配置的资源阈值")
        };
        details.push(event_detail(event));
        add_evidence(&mut evidence, &mut seen, event);
        add_cause(&mut evidence, &mut seen, event, &batch.entries);
    } else if let Some(event) = latest_health_failure.filter(|_| {
        matches!(
            status,
            Some(ServiceState::Unhealthy | ServiceState::Restarting | ServiceState::Failed)
        )
    }) {
        has_diagnostic_evidence = true;
        conclusion = ExplainConclusion::HealthFailure;
        summary = format!("{service} 的健康检查失败");
        details.push(event_detail(event));
        add_evidence(&mut evidence, &mut seen, event);
        add_cause(&mut evidence, &mut seen, event, &batch.entries);
    } else if let Some(event) = latest_trigger.filter(|_| {
        matches!(
            status,
            Some(ServiceState::Pending | ServiceState::Restarting | ServiceState::Starting)
        )
    }) {
        has_diagnostic_evidence = true;
        conclusion = ExplainConclusion::Restarting;
        summary = format!("{service} 正在按记录的原因重启");
        details.push(event_detail(event));
        add_evidence(&mut evidence, &mut seen, event);
        add_cause(&mut evidence, &mut seen, event, &batch.entries);
    } else if let Some(event) = latest_manual_restart.filter(|request| {
        latest_started.is_none_or(|started| started.sequence < request.sequence)
            && latest_manual_stop.is_none_or(|stop| stop.sequence < request.sequence)
            && !matches!(status, Some(ServiceState::Stopped | ServiceState::Stopping))
    }) {
        has_diagnostic_evidence = true;
        conclusion = ExplainConclusion::Restarting;
        summary = format!("{service} 收到手动重启请求，等待新进程代次");
        details.push(event_detail(event));
        add_evidence(&mut evidence, &mut seen, event);
        add_cause(&mut evidence, &mut seen, event, &batch.entries);
    } else if let Some(event) = latest_manual_stop.filter(|_| {
        status.is_none() || matches!(status, Some(ServiceState::Stopped | ServiceState::Stopping))
    }) {
        has_diagnostic_evidence = true;
        conclusion = ExplainConclusion::ManuallyStopped;
        summary = format!("{service} 已按停止请求结束");
        details.push("停止请求只清理受 devd 管理的进程，不会执行诊断建议".into());
        add_evidence(&mut evidence, &mut seen, event);
    } else {
        match status {
            Some(ServiceState::Healthy) => {
                conclusion = ExplainConclusion::Healthy;
                summary = format!("{service} 当前健康");
            }
            Some(ServiceState::Running) => {
                conclusion = ExplainConclusion::Running;
                summary = format!("{service} 正在运行，尚未报告健康结论");
            }
            Some(ServiceState::Stopped) => {
                conclusion = ExplainConclusion::Stopped;
                summary = format!("{service} 当前已停止");
            }
            Some(ServiceState::Failed) => {
                conclusion = ExplainConclusion::StartupFailed;
                summary = format!("{service} 处于失败状态，但保留事件不足以确定直接原因");
            }
            Some(other) => {
                summary = format!("{service} 当前状态为 {other:?}，缺少足够事件证据");
            }
            None => {}
        }
    }

    // Path observations are evidence alongside lifecycle conclusions. They do
    // not prove application failure or authorize a restart. Restrict to the
    // latest generation so a replacement cannot inherit an old warning.
    let mut path_events = BTreeMap::new();
    let mut has_path_evidence = false;
    for event in events.iter().rev() {
        if let EventData::PathConditionChanged { evidence: path } = &event.data {
            path_events
                .entry(path.requirement_index)
                .or_insert_with(Vec::new)
                .push(event);
        }
    }
    for events in path_events.values() {
        let latest = events[0];
        has_path_evidence = true;
        details.push(event_detail(latest));
        add_evidence(&mut evidence, &mut seen, latest);
        if matches!(&latest.data, EventData::PathConditionChanged { evidence, .. } if evidence.failure.is_none())
        {
            if let Some(failure) = events.iter().copied().find(|event| {
                matches!(&event.data, EventData::PathConditionChanged { evidence, .. } if evidence.failure.is_some())
            }) {
                details.push(event_detail(failure));
                add_evidence(&mut evidence, &mut seen, failure);
            }
        }
    }
    if has_path_evidence {
        details.push("路径条件是独立的文件系统观测，不代表应用健康状态，也不能单凭发生顺序断定后续故障原因；监测不会触发重启或修改文件".into());
    }

    if evidence.is_empty() {
        if let Some(event) = events.last() {
            details.push(format!("最近的服务事件：{}", event_detail(event)));
            add_evidence(&mut evidence, &mut seen, event);
            add_cause(&mut evidence, &mut seen, event, &batch.entries);
        }
    }

    let mut next_steps = next_steps(conclusion);
    if has_path_evidence {
        next_steps.push("用 devd doctor 只读复查 requires 路径，并结合应用日志核对影响".into());
    }
    if !has_diagnostic_evidence && !has_path_evidence {
        next_steps.insert(
            0,
            "当前没有可定位故障原因的事件证据；可在下次启动时显式开启 --persist-events 后重现"
                .into(),
        );
    }
    let complete = batch.gaps.is_empty() && batch.omitted_gaps == 0;
    if !complete {
        details.push("事件历史存在缺口，以上结论只覆盖保留下来的证据".into());
    }

    ExplainReport {
        schema_version: DIAGNOSTIC_SCHEMA_VERSION,
        source: batch.source,
        persistence: batch.persistence,
        context: batch.context.clone(),
        service: service.into(),
        // Stored observations guide the conclusion but are not live status.
        status: state.map(|state| state.status),
        generation,
        conclusion,
        summary,
        details,
        evidence,
        complete,
        gaps: batch.gaps.clone(),
        omitted_gaps: batch.omitted_gaps,
        next_steps,
    }
}

fn add_evidence(
    evidence: &mut Vec<ExplainEvidence>,
    seen: &mut BTreeSet<u64>,
    event: &LifecycleEvent,
) {
    if seen.insert(event.sequence) {
        evidence.push(ExplainEvidence {
            sequence: event.sequence,
            event_type: event.data.kind(),
            generation: event.generation,
            cause: event.cause,
            timestamp: event.timestamp,
            detail: event_detail(event),
        });
    }
}

fn add_cause(
    evidence: &mut Vec<ExplainEvidence>,
    seen: &mut BTreeSet<u64>,
    event: &LifecycleEvent,
    entries: &[LifecycleEvent],
) {
    let mut cause = event.cause;
    let mut before = event.sequence;
    while let Some(sequence) = cause {
        let Some(parent) = entries.iter().find(|item| {
            item.sequence == sequence && item.run_id == event.run_id && item.sequence < before
        }) else {
            break;
        };
        add_evidence(evidence, seen, parent);
        before = parent.sequence;
        cause = parent.cause;
    }
}

fn event_detail(event: &LifecycleEvent) -> String {
    match &event.data {
        EventData::DependencyWaiting {
            service,
            condition,
            timeout,
            remaining,
            ..
        } => format!(
            "等待依赖 {service} 满足 {}，期限 {:?}，剩余 {:?}",
            condition_name(condition),
            timeout,
            remaining
        ),
        EventData::DependencyReady {
            service, condition, ..
        } => format!("依赖 {service} 已满足 {}", condition_name(condition)),
        EventData::DependencyFailed { service, state } => {
            format!("依赖 {service} 处于 {state:?} 状态")
        }
        EventData::DependencyTimedOut { timeout } => {
            format!("依赖就绪等待超过 {:?}", timeout)
        }
        EventData::SpawnFailed { failure } => format!("创建进程失败：{}", process_detail(failure)),
        EventData::ProcessFailed { failure } => {
            format!("进程处理失败：{}", process_detail(failure))
        }
        EventData::HealthChanged {
            state,
            consecutive_failures,
            failure,
        } => format!(
            "健康状态变为 {state:?}，连续失败 {consecutive_failures} 次{}",
            failure
                .as_ref()
                .map(|failure| format!("：{}", probe_detail(failure)))
                .unwrap_or_default()
        ),
        EventData::ResourceRestartRequested { evidence } => {
            format!("资源阈值超限并获授权重启：{evidence:?}")
        }
        EventData::RestartTriggered { reason, .. } => {
            format!("触发重启：{}", restart_detail(reason))
        }
        EventData::RestartDecision {
            outcome,
            restart_count,
            delay,
            ..
        } => format!(
            "重启决策为 {outcome:?}，累计次数 {restart_count}，退避 {:?}",
            delay
        ),
        EventData::Exited { code, signal } => {
            format!("进程退出，code={code:?}, signal={signal:?}")
        }
        EventData::ServiceStopRequested { manual_restart } => {
            format!("收到服务停止请求，manual_restart={manual_restart}")
        }
        EventData::ManualRestartRequested => "收到手动重启请求".into(),
        EventData::ReloadStarted { plan_id, .. } => format!("开始应用配置计划 {plan_id}"),
        EventData::ReloadServiceSelected { plan_id } => format!("配置计划 {plan_id} 选中此服务"),
        EventData::ReloadFinished {
            outcome,
            config_committed,
            ..
        } => {
            format!("配置重载结束：{outcome:?}，config_committed={config_committed}")
        }
        EventData::StateChanged { from, to } => format!("状态从 {from:?} 变为 {to:?}"),
        EventData::Starting => "开始启动".into(),
        EventData::Started { pid } => format!("进程已启动，pid={pid:?}"),
        EventData::GenerationPending => "等待启动新进程代次".into(),
        EventData::SupervisorStarted => "supervisor 已启动".into(),
        EventData::SupervisorStopping { reason } => format!("supervisor 开始停止：{reason:?}"),
        EventData::SupervisorStopped { failed } => format!("supervisor 已停止，failed={failed}"),
        EventData::SupervisorCancelled => "supervisor 被取消".into(),
        EventData::ActorFailed { cancelled } => format!("服务 actor 失败，cancelled={cancelled}"),
        EventData::ResourceChanged { exceeded, evidence } => {
            format!("资源状态 changed，exceeded={exceeded}：{evidence:?}")
        }
        EventData::PathConditionChanged { evidence } => format!(
            "观测到路径条件 requires[{}] ({:?}) {:?}：{}",
            evidence.requirement_index,
            evidence.requirement,
            evidence.path,
            evidence
                .failure
                .map_or("已恢复", |failure| failure.summary()),
        ),
        EventData::Omitted { original_type } => format!("事件内容已省略，原类型={original_type}"),
    }
}

fn probe_detail(failure: &ProbeEvidence) -> String {
    match failure {
        ProbeEvidence::Timeout { timeout } => format!("探测超时 {:?}", timeout),
        ProbeEvidence::Tcp { os_code } => format!("TCP 连接失败，os_code={os_code:?}"),
        ProbeEvidence::Socket { os_code } => format!("Unix socket 连接失败，os_code={os_code:?}"),
        ProbeEvidence::Http { connection_error } => {
            format!("HTTP 请求失败，connection_error={connection_error}")
        }
        ProbeEvidence::HttpStatus { status } => format!("HTTP 返回状态 {status}"),
        ProbeEvidence::Script => "脚本探测失败".into(),
    }
}

fn process_detail(failure: &ProcessEvidence) -> String {
    format!("操作 {:?}", failure)
}

fn restart_detail(reason: &RestartCause) -> String {
    match reason {
        RestartCause::ProcessExit => "进程异常退出".into(),
        RestartCause::SpawnFailure => "进程创建失败".into(),
        RestartCause::HealthFailure => "健康检查失败".into(),
        RestartCause::DependencyRecovery { services } => format!(
            "依赖恢复：{}",
            services
                .iter()
                .map(|item| item.service.as_str())
                .collect::<Vec<_>>()
                .join(", ")
        ),
        RestartCause::ResourceLimit { .. } => "资源阈值超限".into(),
    }
}

fn condition_name(condition: &crate::config::DependencyCondition) -> &'static str {
    match condition {
        crate::config::DependencyCondition::Started => "started",
        crate::config::DependencyCondition::SocketReady => "socket-ready",
        crate::config::DependencyCondition::TcpReady => "tcp-ready",
        crate::config::DependencyCondition::HttpReady => "http-ready",
        crate::config::DependencyCondition::ScriptReady => "script-ready",
    }
}

fn next_steps(conclusion: ExplainConclusion) -> Vec<String> {
    match conclusion {
        ExplainConclusion::DependencyBlocked => vec!["检查依赖服务的 status 与 events".into()],
        ExplainConclusion::StartupFailed => vec!["检查启动失败事件和服务配置，再手动重试".into()],
        ExplainConclusion::HealthFailure => vec!["检查健康探测类型、端点状态和最近事件".into()],
        ExplainConclusion::ResourceLimit => {
            vec!["检查资源阈值、采样证据和是否明确授权了自动重启".into()]
        }
        ExplainConclusion::Restarting => vec!["等待新代次就绪；需要停止时使用 devd stop".into()],
        ExplainConclusion::RestartBudgetExhausted => {
            vec!["修复直接原因后再调整 max-attempts 或重新启动 supervisor".into()]
        }
        ExplainConclusion::ManuallyStopped | ExplainConclusion::Stopped => {
            vec!["使用 devd restart <service> 或重新启动 supervisor".into()]
        }
        ExplainConclusion::Healthy | ExplainConclusion::Running => {
            vec!["继续用 devd events 观察后续生命周期".into()]
        }
        ExplainConclusion::Unknown => vec!["先查询 devd status 与 devd events 获取更多事实".into()],
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::events::{query::EventQuery, EventRecorder};
    use crate::core::service_manager::ServiceSnapshot;

    fn batch(recorder: &EventRecorder) -> EventBatch {
        EventQuery {
            filter: Default::default(),
            tail: 1000,
            cursor: None,
        }
        .select(recorder.history().snapshot())
        .unwrap()
    }

    fn path_change(
        failure: Option<super::super::path_requirements::PathRequirementFailureKind>,
    ) -> EventData {
        EventData::PathConditionChanged {
            evidence: super::super::events::PathConditionEvidence {
                requirement_index: 0,
                requirement: crate::config::PathRequirementType::File,
                path: "input\n\u{1b}[31m".into(),
                failure,
            },
        }
    }

    #[test]
    fn test_explain_path_observations_include_recovery_without_inventing_causality() {
        use super::super::path_requirements::PathRequirementFailureKind::Missing;
        let recorder = EventRecorder::new("state".into(), None);
        recorder.record(Some("api"), Some(1), None, path_change(Some(Missing)));
        recorder.record(Some("api"), Some(1), None, path_change(None));
        let mut batch = batch(&recorder);
        batch.gaps.push(EventGap::IncompleteRun {
            run_id: recorder.run_id().into(),
        });
        let report = explain("api", None, &batch);
        assert_eq!(report.conclusion, ExplainConclusion::Unknown);
        assert_eq!(report.evidence.len(), 2);
        assert_eq!(report.evidence[0].sequence, 1);
        assert_eq!(report.evidence[1].sequence, 0);
        assert!(report.evidence.iter().all(|item| item.cause.is_none()));
        assert!(report
            .details
            .iter()
            .any(|detail| detail.contains("已恢复")));
        assert!(report
            .details
            .iter()
            .any(|detail| detail.contains("不代表应用健康状态")));
        assert!(report
            .details
            .iter()
            .all(|detail| !detail.contains('\n') && !detail.contains('\u{1b}')));
        assert!(!report.complete);
    }

    #[test]
    fn test_explain_path_observations_do_not_cross_runs_or_generations() {
        use super::super::path_requirements::PathRequirementFailureKind::Missing;
        let recorder = EventRecorder::new("state".into(), None);
        recorder.record(Some("api"), Some(1), None, path_change(Some(Missing)));
        // The newest generation has no path failures. Old evidence is not its state.
        recorder.record(
            Some("api"),
            Some(2),
            None,
            EventData::Started { pid: Some(123) },
        );
        let report = explain("api", None, &batch(&recorder));
        assert!(!report
            .evidence
            .iter()
            .any(|e| e.event_type == EventKind::PathConditionChanged));

        recorder.record(Some("api"), Some(2), None, path_change(Some(Missing)));
        let snapshot = RuntimeSnapshot {
            supervisor_pid: 1,
            event_run_id: Some("another-run".into()),
            services: [(
                "api".into(),
                ServiceSnapshot {
                    status: ServiceState::Running,
                    event_generation: Some(2),
                    ..Default::default()
                },
            )]
            .into(),
        };
        let report = explain("api", Some(&snapshot), &batch(&recorder));
        assert!(!report
            .evidence
            .iter()
            .any(|e| e.event_type == EventKind::PathConditionChanged));
    }

    #[test]
    fn test_explain_dependency_wait_is_deterministic_and_cites_event() {
        let recorder = EventRecorder::new("state".into(), None);
        recorder.record(
            Some("web"),
            Some(1),
            None,
            EventData::DependencyWaiting {
                service: "api".into(),
                condition: crate::config::DependencyCondition::HttpReady,
                observed_generation: None,
                timeout: std::time::Duration::from_secs(30),
                remaining: std::time::Duration::from_secs(12),
            },
        );
        let snapshot = RuntimeSnapshot {
            supervisor_pid: 1,
            event_run_id: Some(recorder.run_id().into()),
            services: [(
                "web".into(),
                ServiceSnapshot {
                    event_generation: Some(1),
                    ..Default::default()
                },
            )]
            .into(),
        };
        let report = explain("web", Some(&snapshot), &batch(&recorder));
        assert_eq!(report.conclusion, ExplainConclusion::DependencyBlocked);
        assert_eq!(report.evidence[0].sequence, 0);
        assert!(report.complete);
    }

    #[test]
    fn test_explain_budget_exhaustion_includes_causal_trigger() {
        let recorder = EventRecorder::new("state".into(), None);
        let trigger = recorder.record(
            Some("api"),
            Some(1),
            None,
            EventData::RestartTriggered {
                reason: RestartCause::HealthFailure,
                policy: (&crate::config::RestartPolicy::default()).into(),
            },
        );
        recorder.record(
            Some("api"),
            Some(1),
            Some(trigger),
            EventData::RestartDecision {
                outcome: RestartOutcome::BudgetExhausted,
                policy: (&crate::config::RestartPolicy::default()).into(),
                restart_count: 3,
                delay: None,
            },
        );
        let report = explain("api", None, &batch(&recorder));
        assert_eq!(report.conclusion, ExplainConclusion::RestartBudgetExhausted);
        assert_eq!(report.evidence.len(), 2);
        assert!(report.complete);
    }

    #[test]
    fn test_explain_manual_restart_waits_for_new_generation() {
        let recorder = EventRecorder::new("state".into(), None);
        recorder.record(
            Some("api"),
            Some(1),
            None,
            EventData::ManualRestartRequested,
        );
        recorder.record(
            Some("api"),
            Some(1),
            None,
            EventData::ServiceStopRequested {
                manual_restart: true,
            },
        );
        let report = explain("api", None, &batch(&recorder));
        assert_eq!(report.conclusion, ExplainConclusion::Restarting);
        assert_eq!(
            report.evidence[0].event_type,
            EventKind::ServiceStopRequested
        );
        recorder.record(
            Some("api"),
            Some(1),
            None,
            EventData::ServiceStopRequested {
                manual_restart: false,
            },
        );
        assert_eq!(
            explain("api", None, &batch(&recorder)).conclusion,
            ExplainConclusion::ManuallyStopped
        );
    }

    #[test]
    fn test_explain_event_gaps_mark_report_incomplete() {
        let recorder = EventRecorder::new("state".into(), None);
        recorder.record(Some("api"), Some(1), None, EventData::Starting);
        let mut batch = batch(&recorder);
        batch.gaps.push(EventGap::IncompleteRun {
            run_id: recorder.run_id().into(),
        });
        let report = explain("api", None, &batch);
        assert!(!report.complete);
        assert_eq!(report.gaps.len(), 1);
        assert!(report
            .details
            .iter()
            .any(|detail| detail.contains("历史存在缺口")));
    }

    #[test]
    fn test_explain_resource_warning_does_not_claim_restart() {
        let recorder = EventRecorder::new("state".into(), None);
        recorder.record(
            Some("api"),
            Some(1),
            None,
            EventData::ResourceChanged {
                exceeded: true,
                evidence: super::super::events::ResourceEvidence {
                    value: super::super::events::ResourceValue::Memory {
                        bytes: 20,
                        limit_bytes: 10,
                    },
                    sampled_at: chrono::Utc::now(),
                    consecutive_samples: 1,
                    restart_authorized: false,
                },
            },
        );
        let report = explain("api", None, &batch(&recorder));
        assert_eq!(report.conclusion, ExplainConclusion::ResourceLimit);
        assert!(report.summary.contains("超过了配置的资源阈值"));
        assert!(!report.summary.contains("触发了重启"));
    }

    fn runtime(
        recorder: &EventRecorder,
        generation: Option<u64>,
        status: ServiceState,
    ) -> RuntimeSnapshot {
        RuntimeSnapshot {
            supervisor_pid: 1,
            event_run_id: Some(recorder.run_id().into()),
            services: [(
                "api".into(),
                ServiceSnapshot {
                    status,
                    event_generation: generation,
                    ..Default::default()
                },
            )]
            .into(),
        }
    }

    fn resource(memory: bool, exceeded: bool) -> EventData {
        EventData::ResourceChanged {
            exceeded,
            evidence: super::super::events::ResourceEvidence {
                value: if memory {
                    ResourceValue::Memory {
                        bytes: 20,
                        limit_bytes: 10,
                    }
                } else {
                    ResourceValue::Cpu {
                        percent: 20.0,
                        limit_percent: 10,
                    }
                },
                sampled_at: chrono::Utc::now(),
                consecutive_samples: 1,
                restart_authorized: false,
            },
        }
    }

    #[test]
    fn test_explain_new_attempt_does_not_inherit_previous_failure_or_manual_control() {
        for old in [
            EventData::SpawnFailed {
                failure: ProcessEvidence::Spawn { os_code: Some(2) },
            },
            EventData::DependencyTimedOut {
                timeout: std::time::Duration::from_secs(1),
            },
            EventData::RestartDecision {
                outcome: RestartOutcome::BudgetExhausted,
                policy: (&crate::config::RestartPolicy::default()).into(),
                restart_count: 3,
                delay: None,
            },
            EventData::ManualRestartRequested,
            EventData::ServiceStopRequested {
                manual_restart: false,
            },
            resource(true, true),
        ] {
            let recorder = EventRecorder::new("state".into(), None);
            recorder.record(Some("api"), Some(1), None, old);
            recorder.record(
                Some("api"),
                Some(2),
                None,
                EventData::Started { pid: Some(123) },
            );
            let batch = batch(&recorder);
            let snapshot = runtime(&recorder, Some(2), ServiceState::Running);
            for snapshot in [Some(&snapshot), None] {
                let report = explain("api", snapshot, &batch);
                assert_eq!(report.conclusion, ExplainConclusion::Running);
                assert_eq!(report.generation, Some(2));
                assert!(report.evidence.iter().all(|e| e.generation == Some(2)));
            }
            // A re-added service has not acquired its new generation yet.
            let snapshot = runtime(&recorder, None, ServiceState::Pending);
            let report = explain("api", Some(&snapshot), &batch);
            assert_eq!(report.conclusion, ExplainConclusion::Unknown);
            assert!(report.evidence.is_empty());
        }
    }

    #[test]
    fn test_explain_resource_recovery_is_per_metric_and_respects_stopped_state() {
        let recorder = EventRecorder::new("state".into(), None);
        recorder.record(
            Some("api"),
            Some(1),
            None,
            EventData::Started { pid: Some(123) },
        );
        for event in [
            resource(true, true),
            resource(false, true),
            resource(false, false),
        ] {
            recorder.record(Some("api"), Some(1), None, event);
        }
        let snapshot = runtime(&recorder, Some(1), ServiceState::Running);
        let report = explain("api", Some(&snapshot), &batch(&recorder));
        assert_eq!(report.conclusion, ExplainConclusion::ResourceLimit);
        assert_eq!(report.evidence[0].sequence, 1); // Memory is still exceeded.
        let stopped = runtime(&recorder, Some(1), ServiceState::Stopped);
        assert_eq!(
            explain("api", Some(&stopped), &batch(&recorder)).conclusion,
            ExplainConclusion::Stopped
        );
        recorder.record(Some("api"), Some(1), None, resource(true, false));
        let mut batch = batch(&recorder);
        batch.gaps.push(EventGap::IncompleteRun {
            run_id: recorder.run_id().into(),
        });
        for snapshot in [Some(&snapshot), None] {
            let report = explain("api", snapshot, &batch);
            assert_eq!(report.conclusion, ExplainConclusion::Running);
            assert!(!report.complete);
        }
    }

    #[test]
    fn test_explain_restart_keeps_explicit_previous_generation_cause_until_running() {
        let recorder = EventRecorder::new("state".into(), None);
        let trigger = recorder.record(
            Some("api"),
            Some(0),
            None,
            EventData::RestartTriggered {
                reason: RestartCause::SpawnFailure,
                policy: (&crate::config::RestartPolicy::default()).into(),
            },
        );
        let generation = recorder.record(
            Some("api"),
            Some(1),
            Some(trigger),
            EventData::GenerationPending,
        );
        recorder.record(
            Some("api"),
            Some(generation),
            Some(generation),
            EventData::Starting,
        );
        let snapshot = runtime(&recorder, Some(generation), ServiceState::Starting);
        let report = explain("api", Some(&snapshot), &batch(&recorder));
        assert_eq!(report.conclusion, ExplainConclusion::Restarting);
        assert_eq!(report.evidence[0].sequence, trigger);
        recorder.record(
            Some("api"),
            Some(generation),
            Some(generation),
            EventData::Started { pid: Some(123) },
        );
        let snapshot = runtime(&recorder, Some(generation), ServiceState::Running);
        assert_eq!(
            explain("api", Some(&snapshot), &batch(&recorder)).conclusion,
            ExplainConclusion::Running
        );
    }
}
