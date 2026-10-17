//! Explicit launcher policies and the shared control admission boundary.
use super::*;
use sha2::{Digest, Sha256};
use std::collections::HashMap;
use tokio::time::Instant;

const MAX_RESTART_BUDGETS: usize = 64;
const MAX_RESTART_BUDGET: u16 = 100;
const MAX_DEDUP_ENTRIES: usize = 128;
const MAX_DEDUP_RESULT_BYTES: usize = 256 * 1024;
const MAX_DEDUP_RESULT_BYTES_TOTAL: usize = 8 * 1024 * 1024;
const DEDUP_RETENTION: Duration = Duration::from_secs(10 * 60);

#[derive(Debug, Default, Clone)]
pub(in crate::cli) struct Policy {
    pub(super) restart_budgets: BTreeMap<String, u16>,
    pub(super) deduplicate: bool,
}

#[derive(Default)]
pub(in crate::cli) struct AdmissionState {
    remaining: BTreeMap<String, u16>,
    operations: HashMap<String, DedupEntry>,
    result_bytes: usize,
}

struct DedupEntry {
    digest: String,
    state: DedupState,
    expires: Instant,
    result_bytes: usize,
}

enum DedupState {
    Accepted,
    Executing,
    Completed(Value),
    Unknown,
}

impl AdmissionState {
    pub(super) fn new(policy: &Policy) -> Self {
        Self {
            remaining: policy.restart_budgets.clone(),
            ..Self::default()
        }
    }

    fn expire(&mut self) {
        let now = Instant::now();
        self.operations.retain(|_, entry| {
            matches!(entry.state, DedupState::Accepted | DedupState::Executing)
                || entry.expires > now
        });
        self.result_bytes = self
            .operations
            .values()
            .map(|entry| entry.result_bytes)
            .sum();
    }

    fn complete(&mut self, key: &str, result: &Result<Value, Failure>) {
        let mut result_bytes = 0;
        let state = match result {
            Ok(value) => {
                let size = serde_json::to_vec(value).map_or(usize::MAX, |bytes| bytes.len());
                if size <= MAX_DEDUP_RESULT_BYTES
                    && self.result_bytes + size <= MAX_DEDUP_RESULT_BYTES_TOTAL
                {
                    self.result_bytes += size;
                    result_bytes = size;
                    DedupState::Completed(value.clone())
                } else {
                    DedupState::Unknown
                }
            }
            Err(_) => DedupState::Unknown,
        };
        let entry = self.operations.get_mut(key).unwrap();
        entry.state = state;
        entry.result_bytes = result_bytes;
        entry.expires = Instant::now() + DEDUP_RETENTION;
    }
}

pub(in crate::cli) fn parse_policy(budgets: &[String], deduplicate: bool) -> Result<Policy> {
    if budgets.len() > MAX_RESTART_BUDGETS {
        bail!("at most {MAX_RESTART_BUDGETS} --restart-budget entries are allowed");
    }
    let mut restart_budgets = BTreeMap::new();
    for item in budgets {
        let (service, count) = item
            .split_once('=')
            .context("--restart-budget must use SERVICE=COUNT")?;
        if service.is_empty()
            || service.len() > 256
            || service.chars().any(char::is_control)
            || service.trim() != service
        {
            bail!("--restart-budget service name is invalid");
        }
        let count: u16 = count
            .parse()
            .context("--restart-budget count must be an integer from 1 to 100")?;
        if !(1..=MAX_RESTART_BUDGET).contains(&count) {
            bail!("--restart-budget count must be from 1 to {MAX_RESTART_BUDGET}");
        }
        if restart_budgets.insert(service.to_owned(), count).is_some() {
            bail!("duplicate --restart-budget for service {service:?}");
        }
    }
    Ok(Policy {
        restart_budgets,
        deduplicate,
    })
}

#[cfg(test)]
mod tests;

impl Session {
    pub(super) async fn execute_control(&self, operation: Operation) -> Result<Value, Failure> {
        if let Some(result) = self.duplicate_result(&operation).await? {
            return Ok(result);
        }
        // Both adapters share this gate. A duplicate can inspect the existing
        // operation while unrelated controls retain the previous busy boundary.
        let permit = match self.controls.clone().try_acquire_owned() {
            Ok(permit) => permit,
            Err(_) => {
                if let Some(result) = self.duplicate_result(&operation).await? {
                    return Ok(result);
                }
                return Err(Failure::new(
                    "busy",
                    "another control is in progress; inspect status before retrying",
                ));
            }
        };
        // A prior operation may have finished between lookup and acquisition.
        if let Some(result) = self.duplicate_result(&operation).await? {
            return Ok(result);
        }
        let service = match &operation {
            Operation::Restart { service, .. } => Some(service.clone()),
            _ => None,
        };
        self.validate_restart_service(service.as_deref()).await?;
        let key = operation.operation_key().map(str::to_owned);
        let digest = operation_digest(&operation)?;
        {
            let mut admission = self.admission.lock().unwrap();
            admission.expire();
            if key.is_some() && admission.operations.len() >= MAX_DEDUP_ENTRIES {
                return Err(Failure::new(
                    "deduplication-capacity",
                    "operation capacity is full; wait for key expiry",
                ));
            }
            self.consume_restart_budget(&mut admission, service.as_deref())?;
            if let Some(key) = &key {
                admission.operations.insert(
                    key.clone(),
                    DedupEntry {
                        digest,
                        state: DedupState::Accepted,
                        expires: Instant::now() + DEDUP_RETENTION,
                        result_bytes: 0,
                    },
                );
            }
        }
        // Own the permit and receipt in the worker, not the response future.
        // Cancellation/lost replies cannot release the gate or refund a control
        // that may continue in the supervisor or the blocking cleanup worker.
        let receipt = AcceptedGuard {
            admission: self.admission.clone(),
            key: key.clone(),
            armed: key.is_some(),
        };
        let session = self.clone();
        tokio::spawn(async move {
            let _permit = permit;
            let mut receipt = receipt;
            tokio::task::yield_now().await;
            if let Some(key) = &key {
                if let Some(entry) = session.admission.lock().unwrap().operations.get_mut(key) {
                    entry.state = DedupState::Executing;
                }
            }
            let result = session.execute_inner(operation).await;
            let mut admission = session.admission.lock().unwrap();
            if result.as_ref().is_err_and(|error| error.rejected) {
                if let Some(service) =
                    service.filter(|service| session.policy.restart_budgets.contains_key(service))
                {
                    *admission.remaining.get_mut(&service).unwrap() += 1;
                }
                if let Some(key) = &key {
                    admission.operations.remove(key);
                }
                receipt.armed = false;
                return result;
            }
            let Some(key) = key else {
                return result;
            };
            admission.complete(&key, &result);
            receipt.armed = false;
            match result {
                Ok(value) => {
                    Ok(json!({"operation_key":key,"operation_status":"completed","result":value}))
                }
                Err(error) => Err(Failure::new(
                    "outcome-unknown",
                    format!(
                        "{}; this key will not be replayed during retention",
                        error.message
                    ),
                )),
            }
        })
        .await
        .map_err(|_| {
            Failure::new(
                "outcome-unknown",
                "control worker stopped; inspect evidence before retrying",
            )
        })?
    }

    async fn duplicate_result(&self, operation: &Operation) -> Result<Option<Value>, Failure> {
        let Some(key) = operation.operation_key() else {
            return Ok(None);
        };
        let digest = operation_digest(operation)?;
        let result = {
            let mut admission = self.admission.lock().unwrap();
            admission.expire();
            match admission.operations.get(key) {
                Some(entry) if entry.digest != digest => {
                    return Err(Failure::new(
                        "operation-key-conflict",
                        "operation_key was already used with a different request",
                    ))
                }
                Some(entry) => Some(match &entry.state {
                    DedupState::Accepted => {
                        json!({"operation_key":key,"operation_status":"accepted"})
                    }
                    DedupState::Executing => {
                        json!({"operation_key":key,"operation_status":"executing"})
                    }
                    DedupState::Completed(result) => {
                        json!({"operation_key":key,"operation_status":"completed","result":result})
                    }
                    DedupState::Unknown => {
                        json!({"operation_key":key,"operation_status":"unknown"})
                    }
                }),
                None => None,
            }
        };
        if result.is_some() {
            // Authorization has already run in execute. Never execute the
            // cached operation again merely to check its current scope.
            self.revalidate_cached_control().await?;
        }
        Ok(result)
    }

    async fn validate_restart_service(&self, service: Option<&str>) -> Result<(), Failure> {
        let Some(service) = service else {
            return Ok(());
        };
        match self.request(Request::Status).await? {
            Response::Status(snapshot) if snapshot.services.contains_key(service) => Ok(()),
            Response::Status(_) => Err(Failure::new(
                "invalid-request",
                "restart service is not in the current configuration",
            )),
            _ => Err(Failure::new(
                "invalid-response",
                "unexpected status response",
            )),
        }
    }

    async fn revalidate_cached_control(&self) -> Result<(), Failure> {
        match self.request(Request::Identity).await {
            Ok(Response::Identity(identity)) if *identity == self.identity => Ok(()),
            Ok(Response::Identity(_)) => Err(Failure::new(
                "stale-run",
                "current supervisor no longer matches this session",
            )),
            Err(error) if error.code == "operation-failed" => {
                let matches =
                    crate::cli::instances::verify_stopped_attachment(self.identity.clone())
                        .await
                        .map_err(Failure::operation)?;
                if matches {
                    Ok(())
                } else {
                    Err(Failure::new(
                        "stale-run",
                        "stored instance/run no longer matches this session",
                    ))
                }
            }
            Err(error) => Err(error),
            _ => Err(Failure::new(
                "invalid-response",
                "unexpected identity response",
            )),
        }
    }

    fn consume_restart_budget(
        &self,
        admission: &mut AdmissionState,
        service: Option<&str>,
    ) -> Result<(), Failure> {
        let Some(service) = service else {
            return Ok(());
        };
        let Some(limit) = self.policy.restart_budgets.get(service) else {
            if !self.policy.restart_budgets.is_empty() {
                return Err(Failure::new(
                    "budget-out-of-scope",
                    "restart target is not included in the launcher restart budget",
                ));
            }
            return Ok(());
        };
        let remaining = admission
            .remaining
            .entry(service.to_owned())
            .or_insert(*limit);
        if *remaining == 0 {
            return Err(Failure::new(
                "budget-exhausted",
                "restart budget is exhausted; authorized stop remains available",
            ));
        }
        *remaining -= 1;
        Ok(())
    }

    pub(super) fn policy_report(&self) -> Value {
        let mut admission = self.admission.lock().unwrap();
        admission.expire();
        json!({
            "restart_budgets": self.policy.restart_budgets,
            "remaining_restarts": admission.remaining,
            "deduplicate": self.policy.deduplicate,
            "scope": "adapter-session-and-attached-instance-run",
            "retention_seconds": DEDUP_RETENTION.as_secs(),
            "max_operations": MAX_DEDUP_ENTRIES,
            "max_result_bytes": MAX_DEDUP_RESULT_BYTES,
            "max_total_result_bytes": MAX_DEDUP_RESULT_BYTES_TOTAL,
            "retained_operations": admission.operations.len(),
            "retained_result_bytes": admission.result_bytes,
        })
    }
}

fn operation_digest(operation: &Operation) -> Result<String, Failure> {
    let mut value = serde_json::to_value(operation).map_err(Failure::internal)?;
    if let Some(object) = value.as_object_mut() {
        object.remove("operation_key");
    }
    Ok(format!(
        "sha256:{:x}",
        Sha256::digest(value.to_string().as_bytes())
    ))
}

struct AcceptedGuard {
    admission: Arc<Mutex<AdmissionState>>,
    key: Option<String>,
    armed: bool,
}

impl Drop for AcceptedGuard {
    fn drop(&mut self) {
        if !self.armed {
            return;
        }
        if let Some(key) = &self.key {
            if let Some(entry) = self.admission.lock().unwrap().operations.get_mut(key) {
                entry.state = DedupState::Unknown;
                entry.expires = Instant::now() + DEDUP_RETENTION;
            }
        }
    }
}
