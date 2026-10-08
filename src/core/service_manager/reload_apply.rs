use super::*;
use crate::core::reload::{self, ChangeKind, ReloadOutcome, ReloadPlan, ReloadReport};

pub(super) struct ReloadRequest {
    pub candidate: DevdConfig,
    pub plan_id: String,
    pub reply: oneshot::Sender<Result<ReloadReport, String>>,
}

enum Phase {
    Stop {
        layer: usize,
        sent: bool,
    },
    Start {
        layer: usize,
        sent: bool,
        deadline: Instant,
    },
}

/// Driven only by the manager loop; owns no detached orchestration task.
pub(super) struct ReloadExecution {
    candidate: DevdConfig,
    prepared: PreparedConfig,
    plan: ReloadPlan,
    report: ReloadReport,
    reply: oneshot::Sender<Result<ReloadReport, String>>,
    cause: u64,
    phase: Phase,
    pub failure: Option<String>,
}

type Reply = oneshot::Sender<Result<ReloadReport, String>>;

impl ReloadExecution {
    pub fn begin(
        manager: &ServiceManager,
        request: ReloadRequest,
        controls: &BTreeMap<String, watch::Sender<Control>>,
    ) -> Result<Self, (Reply, String)> {
        // The snapshot read lock covers plan verification and quiescing. An
        // actor cannot publish a replacement generation between these steps.
        let snapshot = manager.snapshots.borrow();
        let validate = || -> Result<(ReloadPlan, PreparedConfig), String> {
            let plan = reload::preview(
                &manager.config,
                &request.candidate,
                &snapshot,
                manager.options.profile.as_deref(),
            )
            .map_err(|error| format!("invalid candidate configuration: {error:#}"))?;
            if plan.plan_id != request.plan_id {
                return Err(
                    "configuration plan is stale; run 'devd reload --dry-run' again".into(),
                );
            }
            if snapshot.services.values().any(|state| {
                !matches!(
                    state.status,
                    ServiceState::Running
                        | ServiceState::Healthy
                        | ServiceState::Unhealthy
                        | ServiceState::Stopped
                )
            }) {
                return Err(
                    "services are transitioning; wait for stable states and preview again".into(),
                );
            }
            let prepared = prepare_config(&request.candidate).map_err(|error| error.to_string())?;
            Ok((plan, prepared))
        };
        let (plan, prepared) = match validate() {
            Ok(value) => value,
            Err(error) => return Err((request.reply, error)),
        };
        let cause = manager.events.record(
            None,
            None,
            None,
            EventData::ReloadStarted {
                plan_id: plan.plan_id.clone(),
                base_config_id: plan.base_config_id.clone(),
                candidate_config_id: plan.candidate_config_id.clone(),
            },
        );
        for (name, impact) in &plan.services {
            if impact.change == ChangeKind::Unchanged {
                continue;
            }
            manager.events.record(
                Some(name),
                snapshot
                    .services
                    .get(name)
                    .and_then(|state| state.event_generation),
                Some(cause),
                EventData::ReloadServiceSelected {
                    plan_id: plan.plan_id.clone(),
                },
            );
            if let Some(control) = controls.get(name) {
                control.send_replace(Control::Quiescing);
            }
        }
        let report = ReloadReport {
            schema_version: 1,
            plan_id: plan.plan_id.clone(),
            run_id: plan.run_id.clone(),
            base_config_id: plan.base_config_id.clone(),
            candidate_config_id: plan.candidate_config_id.clone(),
            outcome: ReloadOutcome::Applied,
            config_committed: false,
            stopped: Vec::new(),
            started: Vec::new(),
            ready: Vec::new(),
            failure: None,
        };
        Ok(Self {
            candidate: request.candidate,
            prepared,
            plan,
            report,
            reply: request.reply,
            cause,
            phase: Phase::Stop {
                layer: 0,
                sent: false,
            },
            failure: None,
        })
    }

    pub fn affects(&self, name: &str) -> bool {
        self.plan
            .services
            .get(name)
            .is_some_and(|impact| impact.change != ChangeKind::Unchanged)
    }

    pub fn deadline(&self) -> Option<Instant> {
        match self.phase {
            Phase::Start {
                sent: true,
                deadline,
                ..
            } => Some(deadline),
            _ => None,
        }
    }

    pub fn observe(&mut self, snapshot: &RuntimeSnapshot, names: &BTreeMap<Id, String>) {
        if !self.report.config_committed {
            self.report.stopped = self
                .plan
                .stop_layers
                .iter()
                .flatten()
                .filter(|name| !names.values().any(|active| active == *name))
                .cloned()
                .collect();
        } else {
            for name in self.plan.start_layers.iter().flatten() {
                if snapshot
                    .services
                    .get(name)
                    .is_some_and(|state| state.started_at.is_some())
                    && !self.report.started.contains(name)
                {
                    self.report.started.push(name.clone());
                }
            }
        }
    }

    #[allow(clippy::too_many_arguments)]
    pub fn advance(
        &mut self,
        manager: &mut ServiceManager,
        controls: &mut BTreeMap<String, watch::Sender<Control>>,
        tasks: &mut JoinSet<()>,
        names: &mut BTreeMap<Id, String>,
        store: &Arc<StateStore>,
        limits: &watch::Sender<BTreeMap<String, ResourceThresholds>>,
    ) -> Result<bool, String> {
        self.observe(&manager.snapshots.borrow(), names);
        if manager
            .snapshots
            .borrow()
            .services
            .values()
            .any(|state| state.status == ServiceState::Failed)
        {
            return Err("service failed during reload; stopping the whole stack".into());
        }
        loop {
            match &mut self.phase {
                Phase::Stop { layer, sent } => {
                    if let Some(group) = self.plan.stop_layers.get(*layer) {
                        if !*sent {
                            for name in group {
                                controls[name].send_replace(Control::Stopping(Some(self.cause)));
                            }
                            *sent = true;
                        }
                        if names.values().any(|name| group.contains(name)) {
                            return Ok(false);
                        }
                        *layer += 1;
                        *sent = false;
                    } else {
                        // All old affected actors have joined before removing
                        // their snapshots or exposing the new dependency graph.
                        manager.config = self.candidate.clone();
                        manager.layers = self.prepared.layers.clone();
                        manager.checkers = self.prepared.checkers.clone();
                        manager.resource_limits = manager
                            .config
                            .services
                            .iter()
                            .filter_map(|(name, service)| {
                                service.limits.as_ref().map(|value| {
                                    (name.clone(), value.thresholds().expect("validated limits"))
                                })
                            })
                            .collect();
                        controls.retain(|name, _| manager.config.services.contains_key(name));
                        for name in manager
                            .config
                            .services
                            .keys()
                            .filter(|name| self.affects(name))
                        {
                            let (sender, _) = watch::channel(Control::Loading);
                            controls.insert(name.clone(), sender);
                        }
                        manager.snapshots.send_modify(|snapshot| {
                            snapshot
                                .services
                                .retain(|name, _| manager.config.services.contains_key(name));
                            for name in manager
                                .config
                                .services
                                .keys()
                                .filter(|name| self.affects(name))
                            {
                                let restart_count = snapshot
                                    .services
                                    .get(name)
                                    .map_or(0, |old| old.restart_count.saturating_add(1));
                                snapshot.services.insert(
                                    name.clone(),
                                    ServiceSnapshot {
                                        restart_count,
                                        ..Default::default()
                                    },
                                );
                            }
                            manager
                                .configurations
                                .send_replace(Arc::new(manager.config.clone()));
                            limits.send_replace(
                                manager
                                    .resource_limits
                                    .iter()
                                    .filter(|(name, _)| !self.affects(name))
                                    .map(|(name, value)| (name.clone(), *value))
                                    .collect(),
                            );
                        });
                        self.report.stopped =
                            self.plan.stop_layers.iter().flatten().cloned().collect();
                        self.report.config_committed = true;
                        self.phase = Phase::Start {
                            layer: 0,
                            sent: false,
                            deadline: Instant::now(),
                        };
                    }
                }
                Phase::Start {
                    layer,
                    sent,
                    deadline,
                } => {
                    if let Some(group) = self.plan.start_layers.get(*layer) {
                        if !*sent {
                            for name in group {
                                let (sender, receiver) = watch::channel(Control::Loading);
                                controls.insert(name.clone(), sender);
                                let id = manager.spawn_actor(
                                    name,
                                    receiver,
                                    store,
                                    tasks,
                                    Some(self.cause),
                                );
                                names.insert(id, name.clone());
                            }
                            *sent = true;
                            *deadline = Instant::now() + manager.options.dependency_timeout;
                        }
                        let snapshot = manager.snapshots.borrow();
                        for name in self.plan.start_layers.iter().take(*layer + 1).flatten() {
                            if matches!(
                                snapshot.services[name].status,
                                ServiceState::Stopped | ServiceState::Failed
                            ) {
                                return Err(format!("service '{name}' failed or exited during reload; stopping the whole stack"));
                            }
                        }
                        let ready = group.iter().all(|name| {
                            let state = &snapshot.services[name];
                            state.pid.is_some()
                                && if manager.checkers[name].is_some() {
                                    state.status == ServiceState::Healthy
                                } else {
                                    state.status == ServiceState::Running
                                }
                        });
                        if !ready {
                            if Instant::now() >= *deadline {
                                return Err(
                                    "reload readiness timed out; stopping the whole stack".into()
                                );
                            }
                            return Ok(false);
                        }
                        self.report.ready.extend(group.iter().cloned());
                        *layer += 1;
                        *sent = false;
                    } else {
                        let snapshot = manager.snapshots.borrow();
                        if self.plan.start_layers.iter().flatten().any(|name| {
                            let state = &snapshot.services[name];
                            state.pid.is_none()
                                || if manager.checkers[name].is_some() {
                                    state.status != ServiceState::Healthy
                                } else {
                                    state.status != ServiceState::Running
                                }
                        }) {
                            return Err(
                                "service lost readiness during reload; stopping the whole stack"
                                    .into(),
                            );
                        }
                        for name in self.plan.start_layers.iter().flatten() {
                            controls[name].send_replace(Control::Running);
                        }
                        limits.send_replace(manager.resource_limits.clone());
                        self.observe(&snapshot, names);
                        return Ok(true);
                    }
                }
            }
        }
    }

    pub fn finish(
        mut self,
        events: &EventRecorder,
        outcome: ReloadOutcome,
        failure: Option<String>,
    ) {
        self.report.outcome = outcome;
        self.report.failure = failure;
        events.record(
            None,
            None,
            Some(self.cause),
            EventData::ReloadFinished {
                plan_id: self.plan.plan_id,
                outcome,
                config_committed: self.report.config_committed,
                stopped: self.report.stopped.clone(),
                started: self.report.started.clone(),
                ready: self.report.ready.clone(),
                failure: self.report.failure.clone(),
            },
        );
        let _ = self.reply.send(Ok(self.report));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (
        ServiceManager,
        DevdConfig,
        BTreeMap<String, watch::Sender<Control>>,
    ) {
        let config: DevdConfig =
            serde_yaml::from_str("version: '1'\nservices:\n  worker: {command: worker}\n").unwrap();
        let mut candidate = config.clone();
        candidate.services.get_mut("worker").unwrap().command = "replacement".into();
        let manager =
            ServiceManager::new(config, ManagerOptions::new("unused-state.json")).unwrap();
        let (sender, _) = watch::channel(Control::Running);
        (manager, candidate, [("worker".into(), sender)].into())
    }

    #[test]
    fn test_reload_rejects_unstable_or_stale_plan_without_events_or_controls() {
        let (manager, candidate, controls) = fixture();
        for plan_id in [
            "stale".into(),
            reload::preview(
                &manager.config,
                &candidate,
                &manager.snapshots.borrow(),
                None,
            )
            .unwrap()
            .plan_id,
        ] {
            let (reply, _) = oneshot::channel();
            let before = manager.events.history().snapshot().entries;
            let Err((_, error)) = ReloadExecution::begin(
                &manager,
                ReloadRequest {
                    candidate: candidate.clone(),
                    plan_id,
                    reply,
                },
                &controls,
            ) else {
                panic!("must reject")
            };
            assert!(error.contains("stale") || error.contains("transitioning"));
            assert_eq!(*controls["worker"].borrow(), Control::Running);
            assert_eq!(manager.events.history().snapshot().entries, before);
        }
    }

    #[tokio::test]
    async fn test_reload_failed_old_actor_prevents_configuration_commit() {
        let (mut manager, candidate, mut controls) = fixture();
        manager.snapshots.send_modify(|snapshot| {
            snapshot.services.get_mut("worker").unwrap().status = ServiceState::Running
        });
        let plan = reload::preview(
            &manager.config,
            &candidate,
            &manager.snapshots.borrow(),
            None,
        )
        .unwrap();
        let (reply, _) = oneshot::channel();
        let mut execution = ReloadExecution::begin(
            &manager,
            ReloadRequest {
                candidate,
                plan_id: plan.plan_id,
                reply,
            },
            &controls,
        )
        .unwrap_or_else(|(_, error)| panic!("{error}"));
        manager.snapshots.send_modify(|snapshot| {
            snapshot.services.get_mut("worker").unwrap().status = ServiceState::Failed
        });
        let root = tempfile::tempdir().unwrap();
        let store = Arc::new(
            StateStore::open(&root.path().join("state.json"))
                .await
                .unwrap(),
        );
        let (limits, _) = watch::channel(BTreeMap::new());
        let mut tasks = JoinSet::new();
        assert!(execution
            .advance(
                &mut manager,
                &mut controls,
                &mut tasks,
                &mut BTreeMap::new(),
                &store,
                &limits
            )
            .unwrap_err()
            .contains("service failed"));
        assert!(!execution.report.config_committed);
        assert_eq!(manager.config.services["worker"].command, "worker");
        assert_eq!(
            manager.configurations.borrow().services["worker"].command,
            "worker"
        );
        assert!(tasks.is_empty());
    }
}
