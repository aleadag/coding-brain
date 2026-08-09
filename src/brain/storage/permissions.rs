use std::os::unix::ffi::{OsStrExt, OsStringExt};
use std::os::unix::fs::MetadataExt;
use std::sync::atomic::{AtomicU64, Ordering};
use std::time::{SystemTime, UNIX_EPOCH};

use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityKind, ActivityState, ProjectEvidence,
    SessionTarget, SessionTargetProvenance,
};
use coding_brain_core::lifecycle::{
    ApplyOutcome, LifecycleEvent, LifecycleIdentity, PermissionAction, PermissionAuthority,
};
use coding_brain_core::project::ProjectId;
use coding_brain_core::provider::AgentProvider;
use rusqlite::{OptionalExtension, Transaction, TransactionBehavior, params};
use sha2::{Digest, Sha256};

use crate::brain::decisions::{DecisionRecord, DecisionType, HookDecisionRecord};
use crate::brain::permission_request_lock::{PermissionRequestGuard, PermissionRequestLockStore};

use super::{ActivityCursor, BrainDb, StorageDeadline, StorageError};

const REQUEST_IDENTITY_DOMAIN: &[u8] = b"coding-brain.sqlite-permission-request.v1";
static ATTEMPT_COUNTER: AtomicU64 = AtomicU64::new(0);

#[derive(Clone, Debug, Eq, Hash, PartialEq)]
pub struct AttemptId(String);

impl AttemptId {
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct PermissionAdmission {
    pub lifecycle: LifecycleIdentity,
    pub request_key: String,
    pub project_id: ProjectId,
    pub tool_name: String,
    pub tool_use_id: Option<String>,
    pub activity_id: String,
    pub observed_at_ms: u64,
    pub evaluating_at_ms: u64,
}

impl PermissionAdmission {
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        lifecycle: LifecycleIdentity,
        request_key: impl Into<String>,
        project_id: ProjectId,
        tool_name: impl Into<String>,
        tool_use_id: Option<String>,
        activity_id: impl Into<String>,
        observed_at_ms: u64,
        evaluating_at_ms: u64,
    ) -> Self {
        Self {
            lifecycle,
            request_key: request_key.into(),
            project_id,
            tool_name: tool_name.into(),
            tool_use_id,
            activity_id: activity_id.into(),
            observed_at_ms,
            evaluating_at_ms,
        }
    }
}

pub struct PermissionAttemptGuard {
    attempt_id: AttemptId,
    admission: PermissionAdmission,
    request_identity_key: String,
    deadline: StorageDeadline,
    database_binding: DatabaseBinding,
    _request_guard: PermissionRequestGuard,
}

impl std::fmt::Debug for PermissionAttemptGuard {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("PermissionAttemptGuard")
            .field("attempt_id", &self.attempt_id)
            .finish_non_exhaustive()
    }
}

impl PermissionAttemptGuard {
    pub fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum DeliveryEvidence {
    Delivered,
    Failed,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub enum PermissionState {
    Absent,
    CommittedDeliveryUnknown(PermissionAuthority),
    Delivered(PermissionAuthority),
    DeliveryFailed(PermissionAuthority),
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum PermissionEvidenceKind {
    ProviderAuthority,
    DeterministicSafety,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoricalPermissionProvenance {
    ProposalTerminal,
    JournalCorrelated,
    LifecycleCorrelated,
}

impl HistoricalPermissionProvenance {
    fn parse(value: &str) -> Result<Self, StorageError> {
        match value {
            "proposal_terminal" => Ok(Self::ProposalTerminal),
            "journal_correlated" => Ok(Self::JournalCorrelated),
            "lifecycle_correlated" => Ok(Self::LifecycleCorrelated),
            _ => Err(StorageError::InvalidStorage(
                "historical permission provenance is invalid",
            )),
        }
    }
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HistoricalDeliveryState {
    Unknown,
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct HistoricalPermissionAuthority {
    pub decision_id: String,
    pub terminal_cursor: ActivityCursor,
    pub action: PermissionAction,
    pub provenance: HistoricalPermissionProvenance,
    pub transaction_id: Option<String>,
    pub request_key: Option<String>,
    pub response_eligible: bool,
    pub delivery_state: HistoricalDeliveryState,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HistoricalPermissionAuthorityPage {
    pub authorities: Vec<HistoricalPermissionAuthority>,
    pub next_cursor: Option<ActivityCursor>,
    pub serialized_bytes: usize,
}

impl PermissionEvidenceKind {
    fn label(self) -> &'static str {
        match self {
            Self::ProviderAuthority => "provider_authority",
            Self::DeterministicSafety => "deterministic_safety",
        }
    }
}

pub struct PreparedPermissionCommit {
    guard: PermissionAttemptGuard,
    proposal: HookDecisionRecord,
    terminal: ActivityEvent,
    authority: PermissionAuthority,
    evidence_kind: PermissionEvidenceKind,
    response_eligible: bool,
}

impl PreparedPermissionCommit {
    pub(crate) fn new(
        guard: PermissionAttemptGuard,
        proposal: HookDecisionRecord,
        terminal: ActivityEvent,
        authority: PermissionAuthority,
        evidence_kind: PermissionEvidenceKind,
        response_eligible: bool,
    ) -> Result<Self, StorageError> {
        let prepared = Self {
            guard,
            proposal,
            terminal,
            authority,
            evidence_kind,
            response_eligible,
        };
        prepared.validate()?;
        Ok(prepared)
    }

    fn validate(&self) -> Result<(), StorageError> {
        let admission = &self.guard.admission;
        let session = self
            .terminal
            .session
            .as_ref()
            .ok_or(StorageError::PermissionAttemptMismatch)?;
        let expected_state = match self.authority.action {
            PermissionAction::Allow => ActivityState::Allowed,
            PermissionAction::Deny => ActivityState::Denied,
        };
        let proposal_action = match self.authority.action {
            PermissionAction::Allow => "approve",
            PermissionAction::Deny => "deny",
        };
        if self.terminal.schema_version != ACTIVITY_SCHEMA_VERSION
            || self.terminal.kind != ActivityKind::Decision
            || self.terminal.state != expected_state
            || self.terminal.activity_id != admission.activity_id
            || self.terminal.decision_id.as_deref() != Some(&self.proposal.decision_id)
            || self.proposal.provider != admission.lifecycle.provider()
            || self.proposal.session_id != admission.lifecycle.session_id()
            || self.proposal.turn_id != admission.lifecycle.turn_id().unwrap_or_default()
            || self.proposal.brain_action != proposal_action
            || session.provider != admission.lifecycle.provider()
            || session.session_id != admission.lifecycle.session_id()
            || session.provider_session_id.as_deref() != admission.lifecycle.provider_session_id()
            || session.turn_id.as_deref() != admission.lifecycle.turn_id()
            || session.tool_use_id != admission.tool_use_id
            || session.project_id != admission.project_id
            || session.cwd != admission.lifecycle.cwd()
            || self.terminal.project.project_id != admission.project_id
            || self.terminal.project.cwd != admission.lifecycle.cwd()
            || self.terminal.tool.as_deref() != Some(&admission.tool_name)
            || self.terminal.recorded_at_ms < admission.evaluating_at_ms
            || self.terminal.recorded_at_ms > i64::MAX as u64
            || self.authority.transaction_id.is_empty()
            || self.authority.transaction_id.len() > 512
            || (self.evidence_kind == PermissionEvidenceKind::DeterministicSafety
                && (self.authority.action != PermissionAction::Deny || self.response_eligible))
        {
            return Err(StorageError::PermissionAttemptMismatch);
        }
        Ok(())
    }
}

#[derive(Debug)]
pub struct CommittedPermission {
    attempt_id: AttemptId,
    terminal_cursor: ActivityCursor,
    authority: PermissionAuthority,
    response_eligible: bool,
    deadline: StorageDeadline,
    database_binding: DatabaseBinding,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
struct DatabaseBinding {
    dev: u64,
    ino: u64,
}

impl CommittedPermission {
    pub fn attempt_id(&self) -> &AttemptId {
        &self.attempt_id
    }

    pub fn terminal_cursor(&self) -> ActivityCursor {
        self.terminal_cursor
    }

    pub fn authority(&self) -> &PermissionAuthority {
        &self.authority
    }

    pub fn response_eligible(&self) -> bool {
        self.response_eligible
    }
}

impl BrainDb {
    pub fn admit_permission(
        &mut self,
        admission: PermissionAdmission,
    ) -> Result<Option<PermissionAttemptGuard>, StorageError> {
        self.admit_permission_inner(admission).map_err(|error| {
            super::maintenance::map_storage_error(super::StorageOperation::Admission, false, error)
        })
    }

    fn admit_permission_inner(
        &mut self,
        admission: PermissionAdmission,
    ) -> Result<Option<PermissionAttemptGuard>, StorageError> {
        validate_admission(&admission)?;
        let deadline = self.deadline.ok_or(StorageError::InvalidStorage(
            "permission admission requires a hook deadline",
        ))?;
        deadline.ensure_remaining()?;
        let state_root = self
            .database_path
            .parent()
            .and_then(std::path::Path::parent)
            .ok_or(StorageError::InvalidStorage("database has no state root"))?;
        let request_guard = PermissionRequestLockStore::at(state_root)
            .try_acquire(&admission.lifecycle, &admission.request_key)
            .map_err(|error| StorageError::Io(std::io::Error::other(error)))?;
        let Some(request_guard) = request_guard else {
            return Ok(None);
        };
        deadline.ensure_remaining()?;
        let attempt_id = next_attempt_id();
        let database_binding = database_binding(&self.database_path)?;
        let request_identity_key = request_identity_key(&admission)?;
        let observed = admission_event(
            &admission,
            ActivityState::Observed,
            admission.observed_at_ms,
        );
        let evaluating = admission_event(
            &admission,
            ActivityState::Evaluating,
            admission.evaluating_at_ms,
        );
        let prepared_events = [observed, evaluating]
            .into_iter()
            .map(super::activity::prepare_activity)
            .collect::<Result<Vec<_>, _>>()?;
        super::activity::apply_deadline(&self.connection, Some(deadline))?;
        #[cfg(feature = "fault-injection")]
        if super::hit_fault(
            super::FaultPoint::AdmissionWrite,
            super::FaultPosition::Before,
        )? {
            return Err(StorageError::from(rusqlite::Error::SqliteFailure(
                rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_FULL),
                None,
            )));
        }
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::maintenance::sqlite_fault("permission-admission-body")?;
        transaction.execute(
            "UPDATE permission_attempts
             SET attempt_state = 'abandoned', updated_at_ms = max(updated_at_ms, ?1)
             WHERE request_identity_key = ?2 AND attempt_state = 'evaluating'",
            params![
                i64::try_from(admission.observed_at_ms).map_err(|_| {
                    StorageError::InvalidStorage("permission timestamp is invalid")
                })?,
                request_identity_key.as_str()
            ],
        )?;
        let duplicate = transaction
            .query_row(
                "SELECT 1 FROM permission_attempts INDEXED BY permission_attempts_request_active
                 WHERE request_identity_key = ?1
                   AND attempt_state IN ('evaluating', 'needs_input', 'decided')
                 ORDER BY updated_at_ms DESC LIMIT 1",
                [&request_identity_key],
                |_| Ok(()),
            )
            .optional()?
            .is_some();
        if duplicate {
            return Ok(None);
        }
        let now = i64::try_from(admission.observed_at_ms)
            .map_err(|_| StorageError::InvalidStorage("permission timestamp is invalid"))?;
        let updated = i64::try_from(admission.evaluating_at_ms)
            .map_err(|_| StorageError::InvalidStorage("permission timestamp is invalid"))?;
        let project = serde_json::to_vec(&admission.project_id)
            .map_err(|_| StorageError::InvalidStorage("project identity is invalid"))?;
        let inserted = transaction.execute(
            "INSERT INTO permission_attempts (
                attempt_id, request_identity_key, provider, session_id, provider_session_id,
                turn_id, tool_use_id, request_key, cwd, project_id, tool_name, activity_id,
                authority_action, attempt_state, created_at_ms, updated_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9, ?10, ?11, ?12,
                       NULL, 'evaluating', ?13, ?14)",
            params![
                attempt_id.as_str(),
                request_identity_key,
                admission.lifecycle.provider().as_str(),
                admission.lifecycle.session_id(),
                admission.lifecycle.provider_session_id(),
                admission.lifecycle.turn_id(),
                admission.tool_use_id,
                admission.request_key,
                admission.lifecycle.cwd().as_os_str().as_bytes(),
                project,
                admission.tool_name,
                admission.activity_id,
                now,
                updated,
            ],
        )?;
        if inserted != 1 {
            return Err(StorageError::PermissionAttemptMismatch);
        }
        let current = super::activity::validated_high_water(&transaction)?;
        let next = current.checked_add(2).ok_or(StorageError::InvalidStorage(
            "activity cursor space is exhausted",
        ))?;
        transaction.execute(
            "UPDATE schema_meta SET activity_high_water = ?1 WHERE singleton = 1",
            [next],
        )?;
        for (offset, event) in prepared_events.iter().enumerate() {
            super::activity::insert_activity(
                &transaction,
                current + i64::try_from(offset).unwrap() + 1,
                event,
                Some(attempt_id.as_str()),
            )?;
        }
        super::activity::commit_before_deadline(
            Some(deadline),
            super::StorageOperation::Admission,
            || transaction.commit(),
        )
        .map_err(|error| {
            super::maintenance::map_storage_error(super::StorageOperation::Admission, true, error)
        })?;
        Ok(Some(PermissionAttemptGuard {
            attempt_id,
            admission,
            request_identity_key,
            deadline,
            database_binding,
            _request_guard: request_guard,
        }))
    }

    pub(crate) fn commit_permission(
        &mut self,
        prepared: PreparedPermissionCommit,
    ) -> Result<CommittedPermission, StorageError> {
        self.commit_permission_inner(prepared).map_err(|error| {
            super::maintenance::map_storage_error(super::StorageOperation::Commit, false, error)
        })
    }

    pub(crate) fn finish_permission_without_authority(
        &mut self,
        guard: PermissionAttemptGuard,
        terminal: ActivityEvent,
    ) -> Result<ActivityCursor, StorageError> {
        if guard.database_binding != database_binding(&self.database_path)?
            || !matches!(
                terminal.state,
                ActivityState::Abstained | ActivityState::Error
            )
            || terminal.activity_id != guard.admission.activity_id
            || terminal.decision_id.is_some()
        {
            return Err(StorageError::PermissionAttemptMismatch);
        }
        guard.deadline.ensure_remaining()?;
        let recorded_at_ms = terminal.recorded_at_ms;
        let prepared = super::activity::prepare_activity(terminal)?;
        super::activity::apply_deadline(&self.connection, Some(guard.deadline))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        require_exact_attempt(&transaction, &guard)?;
        let updated = transaction.execute(
            "UPDATE permission_attempts SET attempt_state = 'needs_input', updated_at_ms = ?1
             WHERE attempt_id = ?2 AND attempt_state = 'evaluating' AND authority_action IS NULL",
            params![
                i64::try_from(recorded_at_ms).map_err(|_| {
                    StorageError::InvalidStorage("permission timestamp is invalid")
                })?,
                guard.attempt_id.as_str()
            ],
        )?;
        if updated != 1 {
            return Err(StorageError::PermissionAttemptMismatch);
        }
        let current = super::activity::validated_high_water(&transaction)?;
        let cursor = current.checked_add(1).ok_or(StorageError::InvalidStorage(
            "activity cursor space is exhausted",
        ))?;
        transaction.execute(
            "UPDATE schema_meta SET activity_high_water = ?1 WHERE singleton = 1",
            [cursor],
        )?;
        super::activity::insert_activity(
            &transaction,
            cursor,
            &prepared,
            Some(guard.attempt_id.as_str()),
        )?;
        super::activity::commit_before_deadline(
            Some(guard.deadline),
            super::StorageOperation::Commit,
            || transaction.commit(),
        )?;
        ActivityCursor::try_from(cursor)
    }

    fn commit_permission_inner(
        &mut self,
        prepared: PreparedPermissionCommit,
    ) -> Result<CommittedPermission, StorageError> {
        prepared.validate()?;
        if prepared.guard.database_binding != database_binding(&self.database_path)? {
            return Err(StorageError::PermissionAttemptMismatch);
        }
        prepared.guard.deadline.ensure_remaining()?;
        let action = action_label(prepared.authority.action);
        let terminal = super::activity::prepare_activity(prepared.terminal.clone())?;
        let proposal = hook_record_as_decision(&prepared.proposal);
        let proposal_bytes = super::decisions::serialize_record(&proposal)?;
        if proposal_bytes.len() > super::decisions::MAX_DECISION_RECORD_BYTES {
            return Err(StorageError::InvalidStorage(
                "permission proposal is too large",
            ));
        }
        super::activity::apply_deadline(&self.connection, Some(prepared.guard.deadline))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::maintenance::sqlite_fault("permission-commit-body")?;
        require_exact_attempt(&transaction, &prepared.guard)?;
        let mut response_eligible = prepared.response_eligible;
        if prepared.evidence_kind != PermissionEvidenceKind::DeterministicSafety
            && prepared.authority.action == PermissionAction::Allow
        {
            let mut lifecycle = super::lifecycle::load_lifecycle_snapshot(
                &transaction,
                Some(prepared.guard.deadline),
            )?;
            let event = LifecycleEvent::permission_with_authority(
                prepared.guard.admission.lifecycle.clone(),
                prepared.guard.admission.request_key.clone(),
                prepared.authority.clone(),
            )
            .map_err(|_| StorageError::PermissionAttemptMismatch)?;
            let lifecycle_applied = matches!(
                lifecycle
                    .record_at(event, prepared.terminal.recorded_at_ms)
                    .outcome,
                ApplyOutcome::Applied
            );
            lifecycle.remove_permission_state();
            response_eligible &=
                lifecycle_applied && super::lifecycle::validate_snapshot(&lifecycle).is_ok();
        }
        let decided_at = i64::try_from(prepared.terminal.recorded_at_ms)
            .map_err(|_| StorageError::InvalidStorage("permission timestamp is invalid"))?;
        let updated = transaction.execute(
            "UPDATE permission_attempts SET authority_action = ?1, attempt_state = 'decided',
                    updated_at_ms = ?2
             WHERE attempt_id = ?3 AND attempt_state = 'evaluating' AND authority_action IS NULL",
            params![action, decided_at, prepared.guard.attempt_id.as_str()],
        )?;
        if updated != 1 {
            return Err(StorageError::PermissionAlreadyCommitted);
        }
        let current = super::activity::validated_high_water(&transaction)?;
        let cursor = current.checked_add(1).ok_or(StorageError::InvalidStorage(
            "activity cursor space is exhausted",
        ))?;
        transaction.execute(
            "UPDATE schema_meta SET activity_high_water = ?1 WHERE singleton = 1",
            [cursor],
        )?;
        super::activity::insert_activity(
            &transaction,
            cursor,
            &terminal,
            Some(prepared.guard.attempt_id.as_str()),
        )?;
        let session = prepared.terminal.session.as_ref().unwrap();
        transaction.execute(
            "INSERT INTO decision_identities (
                decision_id, identity_kind, permission_attempt_id, provider, session_id,
                turn_id, tool_use_id, authority_action, decision_source, decided_at_ms
             ) VALUES (?1, 'permission', ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                prepared.proposal.decision_id,
                prepared.guard.attempt_id.as_str(),
                prepared.proposal.provider.as_str(),
                prepared.proposal.session_id,
                prepared.proposal.turn_id,
                session.tool_use_id,
                action,
                if prepared.evidence_kind == PermissionEvidenceKind::DeterministicSafety {
                    "deterministic_safety"
                } else {
                    "model"
                },
                decided_at,
            ],
        )?;
        transaction.execute(
            "INSERT INTO decision_payloads (
                decision_id, payload_kind, source_cursor, normalized_command,
                reasoning, note, decision_record
             ) VALUES (?1, 'permission', ?2, ?3, ?4, NULL, ?5)",
            params![
                prepared.proposal.decision_id,
                cursor,
                bounded(&prepared.proposal.command),
                bounded(&prepared.proposal.brain_reasoning),
                proposal_bytes,
            ],
        )?;
        let delivery_state = if response_eligible {
            "pending"
        } else {
            "not_required"
        };
        transaction.execute(
            "INSERT INTO permission_commits (
                attempt_id, transaction_id, decision_id, terminal_activity_id, authority_action,
                evidence_kind, delivery_state, response_eligible, committed_at_ms
             ) VALUES (?1, ?2, ?3, ?4, ?5, ?6, ?7, ?8, ?9)",
            params![
                prepared.guard.attempt_id.as_str(),
                prepared.authority.transaction_id,
                prepared.proposal.decision_id,
                prepared.terminal.activity_id,
                action,
                prepared.evidence_kind.label(),
                delivery_state,
                i64::from(response_eligible),
                decided_at,
            ],
        )?;
        permission_fault("before-commit")?;
        super::activity::commit_before_deadline(
            Some(prepared.guard.deadline),
            super::StorageOperation::Commit,
            || {
                super::maintenance::sqlite_fault("permission-commit-commit")?;
                #[cfg(feature = "fault-injection")]
                match super::hit_fault(
                    super::FaultPoint::CommitBeforeCall,
                    super::FaultPosition::Before,
                ) {
                    Ok(true) => std::process::abort(),
                    Ok(false) => {}
                    Err(error) => {
                        return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(error)));
                    }
                }
                transaction.commit()
            },
        )
        .map_err(|error| {
            super::maintenance::map_storage_error(super::StorageOperation::Commit, true, error)
        })?;
        #[cfg(feature = "fault-injection")]
        if super::hit_fault(
            super::FaultPoint::CommitAfterReturn,
            super::FaultPosition::After,
        )? {
            std::process::abort();
        }
        permission_fault("after-commit")?;
        Ok(CommittedPermission {
            attempt_id: prepared.guard.attempt_id,
            terminal_cursor: ActivityCursor::try_from(cursor)?,
            authority: prepared.authority,
            response_eligible,
            deadline: prepared.guard.deadline,
            database_binding: prepared.guard.database_binding,
        })
    }

    pub fn record_delivery(
        &mut self,
        committed: &CommittedPermission,
        evidence: DeliveryEvidence,
    ) -> Result<ActivityCursor, StorageError> {
        self.record_delivery_inner(committed, evidence)
            .map_err(|error| {
                super::maintenance::map_storage_error(
                    super::StorageOperation::Delivery,
                    false,
                    error,
                )
            })
    }

    fn record_delivery_inner(
        &mut self,
        committed: &CommittedPermission,
        evidence: DeliveryEvidence,
    ) -> Result<ActivityCursor, StorageError> {
        let deadline = committed.deadline;
        deadline.ensure_remaining()?;
        if committed.database_binding != database_binding(&self.database_path)? {
            return Err(StorageError::PermissionAttemptMismatch);
        }
        if !committed.response_eligible {
            return Err(StorageError::PermissionAttemptMismatch);
        }
        super::activity::apply_deadline(&self.connection, Some(deadline))?;
        let (authority, _) = validated_permission_commit(&self.connection, &committed.attempt_id)?
            .ok_or(StorageError::PermissionAttemptMismatch)?;
        if authority != committed.authority {
            return Err(StorageError::PermissionAttemptMismatch);
        }
        permission_fault("before-delivery-transaction")?;
        super::activity::apply_deadline(&self.connection, Some(deadline))?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::maintenance::sqlite_fault("permission-delivery-body")?;
        let (activity_id, payload): (String, Vec<u8>) = transaction
            .query_row(
                "SELECT c.terminal_activity_id, a.event_payload
                 FROM permission_commits c
                 JOIN activity_events a
                   ON a.activity_id = c.terminal_activity_id
                  AND a.permission_attempt_id = c.attempt_id
                  AND a.terminal_action = c.authority_action
                 WHERE c.attempt_id = ?1 AND c.response_eligible = 1
                   AND c.delivery_state IN ('pending', 'unknown')",
                [committed.attempt_id.as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .optional()?
            .ok_or(StorageError::PermissionAttemptMismatch)?;
        let mut event: ActivityEvent = serde_json::from_slice(&payload)
            .map_err(|_| StorageError::InvalidStorage("terminal activity is corrupt"))?;
        event.state = match evidence {
            DeliveryEvidence::Delivered => ActivityState::Delivered,
            DeliveryEvidence::Failed => ActivityState::DeliveryFailed,
        };
        event.recorded_at_ms = epoch_ms()?;
        event.activity_id = activity_id;
        let activity = super::activity::prepare_activity(event)?;
        let current = super::activity::validated_high_water(&transaction)?;
        let cursor = current.checked_add(1).ok_or(StorageError::InvalidStorage(
            "activity cursor space is exhausted",
        ))?;
        transaction.execute(
            "UPDATE schema_meta SET activity_high_water = ?1 WHERE singleton = 1",
            [cursor],
        )?;
        super::activity::insert_activity(
            &transaction,
            cursor,
            &activity,
            Some(committed.attempt_id.as_str()),
        )?;
        let state = match evidence {
            DeliveryEvidence::Delivered => "delivered",
            DeliveryEvidence::Failed => "failed",
        };
        if transaction.execute(
            "UPDATE permission_commits SET delivery_state = ?1
             WHERE attempt_id = ?2 AND delivery_state IN ('pending', 'unknown')",
            params![state, committed.attempt_id.as_str()],
        )? != 1
        {
            return Err(StorageError::PermissionAttemptMismatch);
        }
        permission_fault("before-delivery-commit")?;
        super::activity::commit_before_deadline(
            Some(deadline),
            super::StorageOperation::Delivery,
            || {
                super::maintenance::sqlite_fault("permission-delivery-commit")?;
                #[cfg(feature = "fault-injection")]
                match super::hit_fault(
                    super::FaultPoint::DeliveryWrite,
                    super::FaultPosition::Before,
                ) {
                    Ok(true) => {
                        return Err(rusqlite::Error::SqliteFailure(
                            rusqlite::ffi::Error::new(rusqlite::ffi::SQLITE_IOERR_FSYNC),
                            None,
                        ));
                    }
                    Ok(false) => {}
                    Err(error) => {
                        return Err(rusqlite::Error::ToSqlConversionFailure(Box::new(error)));
                    }
                }
                transaction.commit()
            },
        )
        .map_err(|error| {
            super::maintenance::map_storage_error(super::StorageOperation::Delivery, true, error)
        })?;
        ActivityCursor::try_from(cursor)
    }

    pub fn permission_state(
        &self,
        attempt_id: &AttemptId,
    ) -> Result<PermissionState, StorageError> {
        super::activity::apply_deadline(&self.connection, self.deadline)?;
        let Some((authority, delivery)) =
            validated_permission_commit(&self.connection, attempt_id)?
        else {
            return Ok(PermissionState::Absent);
        };
        match delivery.as_str() {
            "pending" | "unknown" | "not_required" => {
                Ok(PermissionState::CommittedDeliveryUnknown(authority))
            }
            "delivered" => Ok(PermissionState::Delivered(authority)),
            "failed" => Ok(PermissionState::DeliveryFailed(authority)),
            _ => Err(StorageError::InvalidStorage(
                "permission delivery state is invalid",
            )),
        }
    }

    pub fn permission_decision(
        &self,
        attempt_id: &AttemptId,
    ) -> Result<Option<PermissionAuthority>, StorageError> {
        super::activity::apply_deadline(&self.connection, self.deadline)?;
        Ok(validated_permission_commit(&self.connection, attempt_id)?
            .map(|(authority, _)| authority))
    }

    pub fn explain_permission_lookup(&self) -> Result<String, StorageError> {
        super::activity::explain_query(
            &self.connection,
            "EXPLAIN QUERY PLAN SELECT attempt_id FROM permission_attempts
             INDEXED BY permission_attempts_request_active
             WHERE request_identity_key = 'probe'
               AND attempt_state IN ('evaluating', 'needs_input', 'decided')
             ORDER BY updated_at_ms DESC LIMIT 1",
            self.deadline,
        )
    }

    pub fn historical_permission_authority_after(
        &self,
        after: Option<ActivityCursor>,
        max_rows: usize,
        max_bytes: usize,
    ) -> Result<HistoricalPermissionAuthorityPage, StorageError> {
        if max_rows == 0 || max_bytes == 0 {
            return Err(StorageError::InvalidStorage(
                "historical permission read bounds are invalid",
            ));
        }
        let row_limit = max_rows
            .checked_add(1)
            .and_then(|value| i64::try_from(value).ok())
            .ok_or(StorageError::InvalidStorage(
                "historical permission row bound is invalid",
            ))?;
        super::activity::apply_deadline(&self.connection, self.deadline)?;
        let mut statement = self.connection.prepare(
            "SELECT decision_id, terminal_source_cursor, decision_kind, authority_action,
                    terminal_event_kind, terminal_event_state, terminal_action,
                    provenance_kind, transaction_id, request_key,
                    response_eligible, delivery_state
             FROM historical_permission_authority
                  INDEXED BY historical_permission_authority_cursor
             WHERE terminal_source_cursor > ?1
             ORDER BY terminal_source_cursor ASC, decision_id ASC LIMIT ?2",
        )?;
        let mut rows = statement.query(params![
            after.map_or(0, |cursor| cursor.get() as i64),
            row_limit
        ])?;
        let mut authorities = Vec::new();
        let mut serialized_bytes = 0usize;
        let mut has_more = false;
        while let Some(row) = rows.next()? {
            if authorities.len() == max_rows {
                has_more = true;
                break;
            }
            let raw = HistoricalAuthorityRow {
                decision_id: row.get(0)?,
                terminal_cursor: row.get(1)?,
                decision_kind: row.get(2)?,
                authority_action: row.get(3)?,
                terminal_event_kind: row.get(4)?,
                terminal_event_state: row.get(5)?,
                terminal_action: row.get(6)?,
                provenance_kind: row.get(7)?,
                transaction_id: row.get(8)?,
                request_key: row.get(9)?,
                response_eligible: row.get(10)?,
                delivery_state: row.get(11)?,
            };
            let row_bytes = raw.serialized_bytes()?;
            let next_bytes =
                serialized_bytes
                    .checked_add(row_bytes)
                    .ok_or(StorageError::InvalidStorage(
                        "historical permission byte bound is exceeded",
                    ))?;
            if next_bytes > max_bytes {
                if authorities.is_empty() {
                    return Err(StorageError::InvalidStorage(
                        "historical permission byte bound cannot hold the next row",
                    ));
                }
                has_more = true;
                break;
            }
            authorities.push(validated_historical_authority(&self.connection, raw)?);
            serialized_bytes = next_bytes;
            super::activity::ensure_deadline(self.deadline)?;
        }
        let next_cursor = has_more
            .then(|| {
                authorities
                    .last()
                    .map(|authority| authority.terminal_cursor)
            })
            .flatten();
        Ok(HistoricalPermissionAuthorityPage {
            authorities,
            next_cursor,
            serialized_bytes,
        })
    }

    pub fn explain_historical_permission_lookup(&self) -> Result<String, StorageError> {
        super::activity::explain_query(
            &self.connection,
            "EXPLAIN QUERY PLAN
             SELECT decision_id FROM historical_permission_authority
                  INDEXED BY historical_permission_authority_cursor
             WHERE terminal_source_cursor > 0
             ORDER BY terminal_source_cursor ASC, decision_id ASC LIMIT 1",
            self.deadline,
        )
    }
}

#[derive(Debug)]
pub(super) struct HistoricalAuthorityRow {
    decision_id: String,
    terminal_cursor: i64,
    decision_kind: String,
    authority_action: String,
    terminal_event_kind: String,
    terminal_event_state: String,
    terminal_action: String,
    provenance_kind: String,
    transaction_id: Option<String>,
    request_key: Option<String>,
    response_eligible: i64,
    delivery_state: String,
}

impl HistoricalAuthorityRow {
    fn serialized_bytes(&self) -> Result<usize, StorageError> {
        [
            self.decision_id.len(),
            self.decision_kind.len(),
            self.authority_action.len(),
            self.terminal_event_kind.len(),
            self.terminal_event_state.len(),
            self.terminal_action.len(),
            self.provenance_kind.len(),
            self.transaction_id.as_deref().map_or(0, str::len),
            self.request_key.as_deref().map_or(0, str::len),
            self.delivery_state.len(),
            std::mem::size_of::<i64>() * 2,
        ]
        .into_iter()
        .try_fold(0usize, |total, size| total.checked_add(size))
        .ok_or(StorageError::InvalidStorage(
            "historical permission byte bound is exceeded",
        ))
    }
}

pub(super) fn validated_historical_authority(
    connection: &rusqlite::Connection,
    raw: HistoricalAuthorityRow,
) -> Result<HistoricalPermissionAuthority, StorageError> {
    let action = parse_action(&raw.authority_action)?;
    let terminal_cursor = ActivityCursor::try_from(raw.terminal_cursor)?;
    let provenance = HistoricalPermissionProvenance::parse(&raw.provenance_kind)?;
    let provenance_matches_identifiers = match provenance {
        HistoricalPermissionProvenance::ProposalTerminal => {
            raw.transaction_id.is_none() && raw.request_key.is_none()
        }
        HistoricalPermissionProvenance::JournalCorrelated
        | HistoricalPermissionProvenance::LifecycleCorrelated => {
            raw.transaction_id.is_some() && raw.request_key.is_some()
        }
    };
    let transaction_valid = raw
        .transaction_id
        .as_deref()
        .is_none_or(|value| !value.is_empty() && value.len() <= 512);
    let request_valid = raw.request_key.as_deref().is_none_or(|value| {
        value.len() == 64
            && value
                .bytes()
                .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
    });
    let expected_state = match action {
        PermissionAction::Allow => "allowed",
        PermissionAction::Deny => "denied",
    };
    if raw.decision_kind != "permission"
        || raw.terminal_event_kind != "decision"
        || raw.terminal_event_state != expected_state
        || raw.terminal_action != raw.authority_action
        || raw.response_eligible != 0
        || raw.delivery_state != "unknown"
        || !provenance_matches_identifiers
        || !transaction_valid
        || !request_valid
    {
        return Err(StorageError::InvalidStorage(
            "historical permission authority tuple is invalid",
        ));
    }
    let decision = connection
        .query_row(
            "SELECT identity_kind, permission_attempt_id, provider, session_id, turn_id,
                    tool_use_id, authority_action, decision_source, decided_at_ms
             FROM decision_identities WHERE decision_id = ?1",
            [&raw.decision_id],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, Option<String>>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, Option<String>>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, Option<String>>(6)?,
                    row.get::<_, Option<String>>(7)?,
                    row.get::<_, i64>(8)?,
                ))
            },
        )
        .optional()?
        .ok_or(StorageError::InvalidStorage(
            "historical permission decision anchor is absent",
        ))?;
    let provider = match decision.2.as_str() {
        "codex" => AgentProvider::Codex,
        "claude" => AgentProvider::Claude,
        "antigravity" => AgentProvider::Antigravity,
        _ => {
            return Err(StorageError::InvalidStorage(
                "historical permission provider is invalid",
            ));
        }
    };
    let decided_at_ms = u64::try_from(decision.8).map_err(|_| {
        StorageError::InvalidStorage("historical permission decision timestamp is invalid")
    })?;
    if decision.0 != "permission"
        || decision.1.is_some()
        || decision.6.as_deref() != Some(raw.authority_action.as_str())
        || decision.7.as_deref() != Some("model")
    {
        return Err(StorageError::InvalidStorage(
            "historical permission decision anchor is invalid",
        ));
    }
    let identity = super::decisions::DecisionIdentity::permission(
        raw.decision_id.clone(),
        provider,
        decision.3.ok_or(StorageError::InvalidStorage(
            "historical permission decision identity is incomplete",
        ))?,
        decision.4.ok_or(StorageError::InvalidStorage(
            "historical permission decision identity is incomplete",
        ))?,
        decision.5,
        action,
        "model",
        decided_at_ms,
    );
    let high_water = super::activity::validated_high_water(connection)?;
    if raw.terminal_cursor > high_water {
        return Err(StorageError::InvalidStorage(
            "historical permission terminal cursor exceeds the activity high-water",
        ));
    }
    let activity = super::activity::validated_activity_at(connection, terminal_cursor)?;
    super::decisions::validate_source_activity_event(&identity, &activity.event)?;
    Ok(HistoricalPermissionAuthority {
        decision_id: raw.decision_id,
        terminal_cursor,
        action,
        provenance,
        transaction_id: raw.transaction_id,
        request_key: raw.request_key,
        response_eligible: false,
        delivery_state: HistoricalDeliveryState::Unknown,
    })
}

pub(super) fn validated_historical_authority_by_decision(
    connection: &rusqlite::Connection,
    decision_id: &str,
) -> Result<Option<HistoricalPermissionAuthority>, StorageError> {
    let raw = connection
        .query_row(
            "SELECT decision_id, terminal_source_cursor, decision_kind, authority_action,
                    terminal_event_kind, terminal_event_state, terminal_action,
                    provenance_kind, transaction_id, request_key,
                    response_eligible, delivery_state
             FROM historical_permission_authority WHERE decision_id = ?1",
            [decision_id],
            |row| {
                Ok(HistoricalAuthorityRow {
                    decision_id: row.get(0)?,
                    terminal_cursor: row.get(1)?,
                    decision_kind: row.get(2)?,
                    authority_action: row.get(3)?,
                    terminal_event_kind: row.get(4)?,
                    terminal_event_state: row.get(5)?,
                    terminal_action: row.get(6)?,
                    provenance_kind: row.get(7)?,
                    transaction_id: row.get(8)?,
                    request_key: row.get(9)?,
                    response_eligible: row.get(10)?,
                    delivery_state: row.get(11)?,
                })
            },
        )
        .optional()?;
    raw.map(|raw| validated_historical_authority(connection, raw))
        .transpose()
}

fn validated_permission_commit(
    connection: &rusqlite::Connection,
    attempt_id: &AttemptId,
) -> Result<Option<(PermissionAuthority, String)>, StorageError> {
    let present = connection.query_row(
        "SELECT count(*) FROM permission_commits WHERE attempt_id = ?1",
        [attempt_id.as_str()],
        |row| row.get::<_, i64>(0),
    )?;
    if present == 0 {
        return Ok(None);
    }
    if present != 1 {
        return Err(StorageError::InvalidStorage(
            "permission commit cardinality is invalid",
        ));
    }
    let row = connection
        .query_row(
            "SELECT c.transaction_id, c.authority_action, c.delivery_state,
                    c.evidence_kind, c.response_eligible,
                    a.attempt_state, a.authority_action, a.provider, a.session_id,
                    a.provider_session_id, a.turn_id, a.tool_use_id, a.cwd, a.project_id,
                    a.tool_name, a.activity_id,
                    d.decision_id, d.identity_kind, d.provider, d.session_id, d.turn_id,
                    d.tool_use_id, d.authority_action, d.decision_source,
                    e.source_cursor,
                    a.request_identity_key, a.request_key, a.created_at_ms, a.updated_at_ms,
                    d.decided_at_ms, e.recorded_at_ms, c.committed_at_ms
             FROM permission_commits c
             JOIN permission_attempts a
               ON a.attempt_id = c.attempt_id AND a.authority_action = c.authority_action
             JOIN decision_identities d
               ON d.decision_id = c.decision_id
              AND d.permission_attempt_id = c.attempt_id
              AND d.authority_action = c.authority_action
             JOIN activity_events e
               ON e.activity_id = c.terminal_activity_id
              AND e.permission_attempt_id = c.attempt_id
              AND e.terminal_action = c.authority_action
             WHERE c.attempt_id = ?1 LIMIT 1",
            [attempt_id.as_str()],
            |row| {
                Ok(CommitEvidenceRow {
                    transaction_id: row.get(0)?,
                    action: row.get(1)?,
                    delivery: row.get(2)?,
                    evidence: row.get(3)?,
                    response_eligible: row.get(4)?,
                    attempt_state: row.get(5)?,
                    attempt_action: row.get(6)?,
                    attempt_provider: row.get(7)?,
                    attempt_session: row.get(8)?,
                    attempt_provider_session: row.get(9)?,
                    attempt_turn: row.get(10)?,
                    attempt_tool_use: row.get(11)?,
                    attempt_cwd: row.get(12)?,
                    attempt_project: row.get(13)?,
                    attempt_tool: row.get(14)?,
                    attempt_activity: row.get(15)?,
                    decision_id: row.get(16)?,
                    decision_kind: row.get(17)?,
                    decision_provider: row.get(18)?,
                    decision_session: row.get(19)?,
                    decision_turn: row.get(20)?,
                    decision_tool_use: row.get(21)?,
                    decision_action: row.get(22)?,
                    decision_source: row.get(23)?,
                    event_source_cursor: row.get(24)?,
                    request_identity_key: row.get(25)?,
                    request_key: row.get(26)?,
                    attempt_created_at_ms: row.get(27)?,
                    attempt_updated_at_ms: row.get(28)?,
                    decision_decided_at_ms: row.get(29)?,
                    event_recorded_at_ms: row.get(30)?,
                    commit_committed_at_ms: row.get(31)?,
                })
            },
        )
        .optional()?
        .ok_or(StorageError::InvalidStorage(
            "permission commit relations are incoherent",
        ))?;
    let action = parse_action(&row.action)?;
    let delivery_valid = match row.response_eligible {
        0 => row.delivery == "not_required",
        1 => matches!(
            row.delivery.as_str(),
            "pending" | "delivered" | "failed" | "unknown"
        ),
        _ => false,
    };
    let high_water = super::activity::validated_high_water(connection)?;
    let event_cursor = ActivityCursor::try_from(row.event_source_cursor)?;
    if row.event_source_cursor > high_water {
        return Err(StorageError::InvalidStorage(
            "permission terminal cursor exceeds the activity high-water",
        ));
    }
    let event = super::activity::validated_activity_at(connection, event_cursor)?.event;
    let event_session = event.session.as_ref().ok_or(StorageError::InvalidStorage(
        "permission terminal payload has no session",
    ))?;
    let stored_admission = stored_permission_admission(&row)?;
    let event_project = &stored_admission.project_id;
    let created_at_ms = u64::try_from(row.attempt_created_at_ms)
        .map_err(|_| StorageError::InvalidStorage("permission attempt timestamp is invalid"))?;
    let updated_at_ms = u64::try_from(row.attempt_updated_at_ms)
        .map_err(|_| StorageError::InvalidStorage("permission attempt timestamp is invalid"))?;
    let decision_at_ms = u64::try_from(row.decision_decided_at_ms)
        .map_err(|_| StorageError::InvalidStorage("permission decision timestamp is invalid"))?;
    let event_at_ms = u64::try_from(row.event_recorded_at_ms)
        .map_err(|_| StorageError::InvalidStorage("permission event timestamp is invalid"))?;
    let committed_at_ms = u64::try_from(row.commit_committed_at_ms)
        .map_err(|_| StorageError::InvalidStorage("permission commit timestamp is invalid"))?;
    let source_matches = match row.evidence.as_str() {
        "provider_authority" => row.decision_source.as_deref() == Some("model"),
        "deterministic_safety" => row.decision_source.as_deref() == Some("deterministic_safety"),
        _ => false,
    };
    if row.transaction_id.is_empty()
        || row.transaction_id.len() > 512
        || request_identity_key(&stored_admission)? != row.request_identity_key
        || created_at_ms > updated_at_ms
        || updated_at_ms != decision_at_ms
        || decision_at_ms != event_at_ms
        || event_at_ms != committed_at_ms
        || event.recorded_at_ms != event_at_ms
        || !matches!(
            row.evidence.as_str(),
            "provider_authority" | "deterministic_safety"
        )
        || (row.evidence == "deterministic_safety"
            && (action != PermissionAction::Deny
                || row.response_eligible != 0
                || row.delivery != "not_required"))
        || !source_matches
        || !delivery_valid
        || row.attempt_state != "decided"
        || row.attempt_action.as_deref() != Some(row.action.as_str())
        || row.decision_kind != "permission"
        || row.decision_action.as_deref() != Some(row.action.as_str())
        || row.decision_provider != row.attempt_provider
        || row.decision_session.as_deref() != Some(row.attempt_session.as_str())
        || row.decision_turn.as_deref() != Some(row.attempt_turn.as_str())
        || row.decision_tool_use != row.attempt_tool_use
        || event.activity_id != row.attempt_activity
        || event.schema_version != ACTIVITY_SCHEMA_VERSION
        || event.kind != ActivityKind::Decision
        || !event.has_consistent_payload()
        || event.clone().normalized() != event
        || event.decision_id.as_deref() != Some(row.decision_id.as_str())
        || event.state
            != match action {
                PermissionAction::Allow => ActivityState::Allowed,
                PermissionAction::Deny => ActivityState::Denied,
            }
        || event_session.provider.as_str() != row.attempt_provider
        || event_session.session_id != row.attempt_session
        || event_session.provider_session_id != row.attempt_provider_session
        || event_session.turn_id.as_deref() != Some(row.attempt_turn.as_str())
        || event_session.tool_use_id != row.attempt_tool_use
        || event_session.cwd.as_os_str().as_bytes() != row.attempt_cwd
        || event_session.project_id != *event_project
        || event.project.cwd.as_os_str().as_bytes() != row.attempt_cwd
        || event.project.project_id != *event_project
        || event.tool.as_deref() != Some(row.attempt_tool.as_str())
    {
        return Err(StorageError::InvalidStorage(
            "permission commit typed evidence is invalid",
        ));
    }
    Ok(Some((
        PermissionAuthority {
            transaction_id: row.transaction_id,
            action,
        },
        row.delivery,
    )))
}

struct CommitEvidenceRow {
    transaction_id: String,
    action: String,
    delivery: String,
    evidence: String,
    response_eligible: i64,
    attempt_state: String,
    attempt_action: Option<String>,
    attempt_provider: String,
    attempt_session: String,
    attempt_provider_session: Option<String>,
    attempt_turn: String,
    attempt_tool_use: Option<String>,
    attempt_cwd: Vec<u8>,
    attempt_project: Vec<u8>,
    attempt_tool: String,
    attempt_activity: String,
    decision_id: String,
    decision_kind: String,
    decision_provider: String,
    decision_session: Option<String>,
    decision_turn: Option<String>,
    decision_tool_use: Option<String>,
    decision_action: Option<String>,
    decision_source: Option<String>,
    event_source_cursor: i64,
    request_identity_key: String,
    request_key: String,
    attempt_created_at_ms: i64,
    attempt_updated_at_ms: i64,
    decision_decided_at_ms: i64,
    event_recorded_at_ms: i64,
    commit_committed_at_ms: i64,
}

fn stored_permission_admission(
    row: &CommitEvidenceRow,
) -> Result<PermissionAdmission, StorageError> {
    let provider = match row.attempt_provider.as_str() {
        "codex" => AgentProvider::Codex,
        "claude" => AgentProvider::Claude,
        "antigravity" => AgentProvider::Antigravity,
        _ => {
            return Err(StorageError::InvalidStorage(
                "permission attempt provider is invalid",
            ));
        }
    };
    let lifecycle = LifecycleIdentity::try_new_with_provider_session(
        provider,
        row.attempt_session.clone(),
        row.attempt_provider_session.clone(),
        Some(row.attempt_turn.clone()),
        None,
        std::path::PathBuf::from(std::ffi::OsString::from_vec(row.attempt_cwd.clone())),
    )
    .map_err(|_| StorageError::InvalidStorage("permission attempt identity is invalid"))?;
    let project_id = serde_json::from_slice::<ProjectId>(&row.attempt_project)
        .map_err(|_| StorageError::InvalidStorage("permission project identity is corrupt"))?;
    let created_at_ms = u64::try_from(row.attempt_created_at_ms)
        .map_err(|_| StorageError::InvalidStorage("permission attempt timestamp is invalid"))?;
    let admission = PermissionAdmission::new(
        lifecycle,
        row.request_key.clone(),
        project_id,
        row.attempt_tool.clone(),
        row.attempt_tool_use.clone(),
        row.attempt_activity.clone(),
        created_at_ms,
        created_at_ms,
    );
    validate_admission(&admission)?;
    Ok(admission)
}

fn require_exact_attempt(
    transaction: &Transaction<'_>,
    guard: &PermissionAttemptGuard,
) -> Result<(), StorageError> {
    let row = transaction
        .query_row(
            "SELECT request_identity_key, provider, session_id, provider_session_id, turn_id,
                    tool_use_id, request_key, cwd, project_id, tool_name, activity_id,
                    attempt_state, authority_action, created_at_ms, updated_at_ms
             FROM permission_attempts WHERE attempt_id = ?1",
            [guard.attempt_id.as_str()],
            |row| {
                Ok((
                    row.get::<_, String>(0)?,
                    row.get::<_, String>(1)?,
                    row.get::<_, String>(2)?,
                    row.get::<_, Option<String>>(3)?,
                    row.get::<_, String>(4)?,
                    row.get::<_, Option<String>>(5)?,
                    row.get::<_, String>(6)?,
                    row.get::<_, Vec<u8>>(7)?,
                    row.get::<_, Vec<u8>>(8)?,
                    row.get::<_, String>(9)?,
                    row.get::<_, String>(10)?,
                    row.get::<_, String>(11)?,
                    row.get::<_, Option<String>>(12)?,
                    row.get::<_, i64>(13)?,
                    row.get::<_, i64>(14)?,
                ))
            },
        )
        .optional()?;
    let Some(row) = row else {
        return Err(StorageError::PermissionAttemptMismatch);
    };
    let admission = &guard.admission;
    let project = serde_json::from_slice::<ProjectId>(&row.8)
        .map_err(|_| StorageError::PermissionAttemptMismatch)?;
    if row.0 != guard.request_identity_key
        || row.1 != admission.lifecycle.provider().as_str()
        || row.2 != admission.lifecycle.session_id()
        || row.3.as_deref() != admission.lifecycle.provider_session_id()
        || row.4 != admission.lifecycle.turn_id().unwrap_or_default()
        || row.5 != admission.tool_use_id
        || row.6 != admission.request_key
        || row.7 != admission.lifecycle.cwd().as_os_str().as_bytes()
        || project != admission.project_id
        || row.9 != admission.tool_name
        || row.10 != admission.activity_id
        || row.11 != "evaluating"
        || row.12.is_some()
        || row.13 != i64::try_from(admission.observed_at_ms).unwrap_or(-1)
        || row.14 != i64::try_from(admission.evaluating_at_ms).unwrap_or(-1)
    {
        return Err(StorageError::PermissionAttemptMismatch);
    }
    if !guard
        ._request_guard
        .matches(&guard.admission.lifecycle, &guard.admission.request_key)
    {
        return Err(StorageError::PermissionAttemptMismatch);
    }
    Ok(())
}

fn validate_admission(admission: &PermissionAdmission) -> Result<(), StorageError> {
    let bounded = |value: &str| !value.is_empty() && value.len() <= 512;
    if admission.request_key.len() != 64
        || !admission
            .request_key
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || !bounded(&admission.tool_name)
        || !bounded(&admission.activity_id)
        || admission
            .tool_use_id
            .as_deref()
            .is_some_and(|value| !bounded(value))
        || admission.lifecycle.turn_id().is_none()
        || admission.evaluating_at_ms < admission.observed_at_ms
        || admission.evaluating_at_ms > i64::MAX as u64
    {
        return Err(StorageError::InvalidStorage(
            "permission admission is invalid",
        ));
    }
    let project = serde_json::to_vec(&admission.project_id)
        .map_err(|_| StorageError::InvalidStorage("project identity is invalid"))?;
    if project.is_empty() || project.len() > 4096 {
        return Err(StorageError::InvalidStorage("project identity is invalid"));
    }
    Ok(())
}

fn admission_event(
    admission: &PermissionAdmission,
    state: ActivityState,
    at: u64,
) -> ActivityEvent {
    ActivityEvent {
        schema_version: ACTIVITY_SCHEMA_VERSION,
        kind: ActivityKind::Decision,
        activity_id: admission.activity_id.clone(),
        recorded_at_ms: at,
        project: ProjectEvidence {
            project_id: admission.project_id.clone(),
            cwd: admission.lifecycle.cwd().to_path_buf(),
            label: None,
        },
        session: Some(SessionTarget {
            provider: admission.lifecycle.provider(),
            session_id: admission.lifecycle.session_id().to_owned(),
            provider_session_id: admission.lifecycle.provider_session_id().map(str::to_owned),
            turn_id: admission.lifecycle.turn_id().map(str::to_owned),
            tool_use_id: admission.tool_use_id.clone(),
            project_id: admission.project_id.clone(),
            cwd: admission.lifecycle.cwd().to_path_buf(),
            provider_hints: Vec::new(),
            provenance: SessionTargetProvenance::Structured,
        }),
        state,
        tool: Some(admission.tool_name.clone()),
        normalized_command: None,
        fingerprint: None,
        rule_id: None,
        confidence: None,
        threshold: None,
        reasoning: None,
        decision_id: None,
        outcome: None,
        correction: None,
        note: None,
        supersedes: None,
    }
}

fn request_identity_key(admission: &PermissionAdmission) -> Result<String, StorageError> {
    let mut hash = Sha256::new();
    hash_field(&mut hash, REQUEST_IDENTITY_DOMAIN);
    hash_field(
        &mut hash,
        admission.lifecycle.provider().as_str().as_bytes(),
    );
    hash_field(&mut hash, admission.lifecycle.session_id().as_bytes());
    hash_optional(
        &mut hash,
        admission.lifecycle.provider_session_id().map(str::as_bytes),
    );
    hash_field(
        &mut hash,
        admission.lifecycle.turn_id().unwrap_or_default().as_bytes(),
    );
    hash_field(&mut hash, admission.lifecycle.cwd().as_os_str().as_bytes());
    hash_field(&mut hash, admission.request_key.as_bytes());
    hash_optional(
        &mut hash,
        admission.tool_use_id.as_deref().map(str::as_bytes),
    );
    hash_field(&mut hash, admission.tool_name.as_bytes());
    hash_field(
        &mut hash,
        &serde_json::to_vec(&admission.project_id)
            .map_err(|_| StorageError::InvalidStorage("project identity is invalid"))?,
    );
    Ok(format!("{:x}", hash.finalize()))
}

fn hash_optional(hash: &mut Sha256, value: Option<&[u8]>) {
    if let Some(value) = value {
        hash.update([1]);
        hash_field(hash, value);
    } else {
        hash.update([0]);
    }
}

fn hash_field(hash: &mut Sha256, value: &[u8]) {
    hash.update((value.len() as u64).to_be_bytes());
    hash.update(value);
}

fn next_attempt_id() -> AttemptId {
    let nanos = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_nanos();
    let sequence = ATTEMPT_COUNTER.fetch_add(1, Ordering::Relaxed);
    AttemptId(format!("attempt-{nanos}-{}-{sequence}", std::process::id()))
}

fn database_binding(path: &std::path::Path) -> Result<DatabaseBinding, StorageError> {
    let metadata = std::fs::symlink_metadata(path)?;
    if !metadata.file_type().is_file() || metadata.nlink() != 1 {
        return Err(StorageError::InvalidStorage(
            "permission database identity is invalid",
        ));
    }
    Ok(DatabaseBinding {
        dev: metadata.dev(),
        ino: metadata.ino(),
    })
}

fn epoch_ms() -> Result<u64, StorageError> {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap_or_default()
        .as_millis()
        .try_into()
        .map_err(|_| StorageError::InvalidStorage("permission timestamp is invalid"))
}

fn action_label(action: PermissionAction) -> &'static str {
    match action {
        PermissionAction::Allow => "allow",
        PermissionAction::Deny => "deny",
    }
}

fn parse_action(action: &str) -> Result<PermissionAction, StorageError> {
    match action {
        "allow" => Ok(PermissionAction::Allow),
        "deny" => Ok(PermissionAction::Deny),
        _ => Err(StorageError::InvalidStorage("permission action is invalid")),
    }
}

fn bounded(value: &str) -> Option<String> {
    (!value.is_empty()).then(|| value.chars().take(4096).collect())
}

fn hook_record_as_decision(record: &HookDecisionRecord) -> DecisionRecord {
    DecisionRecord {
        provider: record.provider,
        timestamp: record.ts.clone(),
        pid: record.pid,
        project: record.project.clone(),
        tool: Some(record.tool.clone()),
        command: Some(record.command.clone()),
        brain_action: record.brain_action.clone(),
        brain_confidence: record.brain_confidence,
        brain_reasoning: record.brain_reasoning.clone(),
        user_action: record.user_action.clone(),
        context: None,
        outcome: None,
        decision_type: DecisionType::Session,
        suggested_at: Some(record.suggested_at),
        resolved_at: Some(record.resolved_at),
        override_reason: None,
        decision_id: Some(record.decision_id.clone()),
        brain_decision_ms: None,
        cache_hit: None,
        canonical: None,
    }
}

#[cfg(test)]
fn permission_fault(stage: &str) -> Result<(), StorageError> {
    let fault = std::env::var_os("CODING_BRAIN_SQLITE_PERMISSION_FAULT");
    if stage == "before-delivery-transaction"
        && fault.as_deref() == Some(std::ffi::OsStr::new("sleep-before-delivery-transaction"))
    {
        std::thread::sleep(std::time::Duration::from_millis(500));
        return Ok(());
    }
    if stage == "after-commit"
        && fault.as_deref() == Some(std::ffi::OsStr::new("abort-after-commit"))
    {
        std::process::abort();
    }
    if fault.as_deref() == Some(std::ffi::OsStr::new(stage)) {
        return Err(StorageError::Io(std::io::Error::other(format!(
            "injected permission fault at {stage}"
        ))));
    }
    Ok(())
}

#[cfg(not(test))]
fn permission_fault(_stage: &str) -> Result<(), StorageError> {
    Ok(())
}

#[cfg(test)]
mod tests {
    use std::fs;
    #[cfg(feature = "fault-injection")]
    use std::fs::File;
    #[cfg(feature = "fault-injection")]
    use std::io::Read;
    #[cfg(feature = "fault-injection")]
    use std::os::fd::{AsRawFd, FromRawFd};
    #[cfg(feature = "fault-injection")]
    use std::os::unix::fs::MetadataExt;
    use std::os::unix::fs::PermissionsExt;
    use std::process::{Command, Stdio};
    use std::thread;
    use std::time::Duration;

    use coding_brain_core::brain_activity::{
        ActivityEvent, ActivityKind, ActivityState, ProjectEvidence, SessionTarget,
        SessionTargetProvenance,
    };
    use coding_brain_core::lifecycle::{LifecycleIdentity, PermissionAction, PermissionAuthority};
    use coding_brain_core::project::ProjectId;
    use coding_brain_core::provider::AgentProvider;
    use rusqlite::{ffi, params};
    use sha2::{Digest, Sha256};

    use crate::brain::decisions::HookDecisionRecord;
    use crate::brain::storage::{DecisionIdentity, DecisionKind, DecisionPayload};

    use super::{
        AttemptId, BrainDb, CommittedPermission, DeliveryEvidence, PermissionAdmission,
        PermissionEvidenceKind, PermissionState, PreparedPermissionCommit, StorageDeadline,
        database_binding, hook_record_as_decision,
    };
    use crate::brain::storage::{
        OpenRole, ReviewDb, StorageError, StorageFaultCategory, StorageOperation, StoragePaths,
    };

    const MODELED_PROVIDER_RESPONSE: &[u8] = b"{\"permission\":\"allow\"}\n";

    #[cfg(feature = "fault-injection")]
    struct LiveFaultFixture {
        capability: std::path::PathBuf,
        nonce: String,
        point: crate::brain::storage::FaultPoint,
        read: File,
        write: Option<File>,
    }

    #[cfg(feature = "fault-injection")]
    impl LiveFaultFixture {
        fn new(root: &std::path::Path, point: crate::brain::storage::FaultPoint) -> Self {
            let capability_dir = root.join("fault-capability");
            fs::create_dir(&capability_dir).unwrap();
            fs::set_permissions(&capability_dir, fs::Permissions::from_mode(0o700)).unwrap();
            let mut descriptors = [0; 2];
            assert_eq!(unsafe { libc::pipe(descriptors.as_mut_ptr()) }, 0);
            let read = unsafe { File::from_raw_fd(descriptors[0]) };
            let write = unsafe { File::from_raw_fd(descriptors[1]) };
            let metadata = write.metadata().unwrap();
            let capability = capability_dir.join("fault.json");
            let nonce = "permission-live-fault".to_owned();
            let record = serde_json::json!({
                "version": 1,
                "state_root": root,
                "nonce": nonce,
                "selection": { "kind": "matrix", "selection": point },
                "control_device": metadata.dev(),
                "control_inode": metadata.ino(),
            });
            fs::write(&capability, serde_json::to_vec(&record).unwrap()).unwrap();
            fs::set_permissions(&capability, fs::Permissions::from_mode(0o600)).unwrap();
            Self {
                capability,
                nonce,
                point,
                read,
                write: Some(write),
            }
        }

        fn spawn(&mut self, root: &std::path::Path, mode: &str) -> std::process::Child {
            let write = self.write.take().unwrap();
            let child = Command::new(std::env::current_exe().unwrap())
                .args([
                    "--ignored",
                    "--exact",
                    "brain::storage::permissions::tests::live_fault_process_helper",
                    "--nocapture",
                ])
                .env("CODING_BRAIN_PERMISSION_LIVE_FAULT_ROOT", root)
                .env("CODING_BRAIN_PERMISSION_LIVE_FAULT_MODE", mode)
                .env(
                    "CODING_BRAIN_PERMISSION_LIVE_FAULT_CAPABILITY",
                    &self.capability,
                )
                .env("CODING_BRAIN_PERMISSION_LIVE_FAULT_NONCE", &self.nonce)
                .env(
                    "CODING_BRAIN_PERMISSION_LIVE_FAULT_POINT",
                    serde_json::to_string(&self.point).unwrap(),
                )
                .env(
                    "CODING_BRAIN_PERMISSION_LIVE_FAULT_FD",
                    write.as_raw_fd().to_string(),
                )
                .stdin(Stdio::null())
                .stdout(Stdio::null())
                .stderr(Stdio::null())
                .spawn()
                .unwrap();
            drop(write);
            child
        }

        fn marker(mut self) -> Vec<u8> {
            let mut marker = Vec::new();
            self.read.read_to_end(&mut marker).unwrap();
            marker
        }
    }

    fn root() -> tempfile::TempDir {
        let root = tempfile::tempdir().unwrap();
        fs::set_permissions(root.path(), fs::Permissions::from_mode(0o700)).unwrap();
        root
    }

    fn admission(activity_id: &str) -> PermissionAdmission {
        let request_key = format!("{:x}", Sha256::digest(activity_id.as_bytes()));
        PermissionAdmission::new(
            LifecycleIdentity::try_new(
                AgentProvider::Codex,
                "session-1".into(),
                Some("turn-1".into()),
                None,
                "/work/project".into(),
            )
            .unwrap(),
            request_key,
            ProjectId::Temporary("project-1".into()),
            "Bash",
            None,
            activity_id,
            1,
            2,
        )
    }

    fn proposal(decision_id: &str) -> HookDecisionRecord {
        HookDecisionRecord {
            provider: AgentProvider::Codex,
            ts: "2026-08-05T00:00:00Z".into(),
            pid: 1,
            project: "project".into(),
            tool: "Bash".into(),
            command: "cargo test".into(),
            brain_action: "approve".into(),
            brain_confidence: 0.9,
            brain_reasoning: "safe".into(),
            brain_source: "brain".into(),
            brain_threshold: Some(0.8),
            user_action: "hook_allow".into(),
            decision_type: "session".into(),
            suggested_at: 1,
            resolved_at: 2,
            decision_id: decision_id.into(),
            session_id: "session-1".into(),
            turn_id: "turn-1".into(),
        }
    }

    fn terminal(admission: &PermissionAdmission, decision_id: &str) -> ActivityEvent {
        ActivityEvent {
            schema_version: coding_brain_core::brain_activity::ACTIVITY_SCHEMA_VERSION,
            kind: ActivityKind::Decision,
            activity_id: admission.activity_id.clone(),
            recorded_at_ms: 3,
            project: ProjectEvidence {
                project_id: admission.project_id.clone(),
                cwd: admission.lifecycle.cwd().to_path_buf(),
                label: None,
            },
            session: Some(SessionTarget {
                provider: AgentProvider::Codex,
                session_id: "session-1".into(),
                provider_session_id: None,
                turn_id: Some("turn-1".into()),
                tool_use_id: None,
                project_id: admission.project_id.clone(),
                cwd: admission.lifecycle.cwd().to_path_buf(),
                provider_hints: Vec::new(),
                provenance: SessionTargetProvenance::Structured,
            }),
            state: ActivityState::Allowed,
            tool: Some("Bash".into()),
            normalized_command: Some("cargo test".into()),
            fingerprint: None,
            rule_id: None,
            confidence: Some(0.9),
            threshold: Some(0.8),
            reasoning: Some("safe".into()),
            decision_id: Some(decision_id.into()),
            outcome: None,
            correction: None,
            note: None,
            supersedes: None,
        }
    }

    fn open(paths: &StoragePaths, duration: Duration) -> BrainDb {
        BrainDb::open_current(paths, OpenRole::Hook, StorageDeadline::after(duration)).unwrap()
    }

    #[test]
    fn historical_authority_never_enters_live_permission_or_delivery_apis() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        let mut db = BrainDb::create_current(&paths).unwrap();
        let historical_admission = admission("historical-activity");
        let historical_decision_id = "historical-decision";
        let cursor = db
            .append_activity(terminal(&historical_admission, historical_decision_id))
            .unwrap();
        let historical_proposal = proposal(historical_decision_id);
        db.insert_decision(
            &DecisionIdentity::permission(
                historical_decision_id,
                AgentProvider::Codex,
                "session-1",
                "turn-1",
                None,
                PermissionAction::Allow,
                "model",
                3,
            ),
            &DecisionPayload::new(
                DecisionKind::Permission,
                cursor,
                hook_record_as_decision(&historical_proposal),
            ),
        )
        .unwrap();
        db.connection
            .execute(
                "INSERT INTO historical_permission_authority (
                    decision_id, terminal_source_cursor, decision_kind, authority_action,
                    terminal_event_kind, terminal_event_state, terminal_action,
                    provenance_kind, transaction_id, request_key,
                    response_eligible, delivery_state
                 ) VALUES (?1, ?2, 'permission', 'allow', 'decision', 'allowed', 'allow',
                           'proposal_terminal', NULL, NULL, 0, 'unknown')",
                params![historical_decision_id, cursor.get() as i64],
            )
            .unwrap();

        drop(db);
        let mut db = open(&paths, Duration::from_secs(1));
        let live_guard = db
            .admit_permission(admission("live-activity"))
            .unwrap()
            .unwrap();
        assert_eq!(
            db.permission_state(live_guard.attempt_id()).unwrap(),
            PermissionState::Absent
        );
        assert_eq!(
            db.permission_decision(live_guard.attempt_id()).unwrap(),
            None
        );
        let fake_delivery = CommittedPermission {
            attempt_id: live_guard.attempt_id().clone(),
            terminal_cursor: cursor,
            authority: PermissionAuthority {
                transaction_id: "not-historical".into(),
                action: PermissionAction::Allow,
            },
            response_eligible: true,
            deadline: StorageDeadline::after(Duration::from_secs(1)),
            database_binding: database_binding(&paths.brain_db()).unwrap(),
        };
        assert!(matches!(
            db.record_delivery(&fake_delivery, DeliveryEvidence::Delivered),
            Err(StorageError::PermissionAttemptMismatch)
        ));
        assert_eq!(
            db.connection
                .query_row("SELECT count(*) FROM permission_commits", [], |row| {
                    row.get::<_, i64>(0)
                })
                .unwrap(),
            0
        );
    }

    fn wait_for(path: &std::path::Path) {
        let deadline = std::time::Instant::now() + Duration::from_secs(10);
        while !path.exists() {
            assert!(
                std::time::Instant::now() < deadline,
                "timed out waiting for {}",
                path.display()
            );
            thread::sleep(Duration::from_millis(5));
        }
    }

    fn helper(root: &std::path::Path, mode: &str) -> std::process::Child {
        Command::new(std::env::current_exe().unwrap())
            .arg("--ignored")
            .arg("--exact")
            .arg("brain::storage::permissions::tests::permission_process_helper")
            .arg("--nocapture")
            .env("CODING_BRAIN_PERMISSION_PROCESS_ROOT", root)
            .env("CODING_BRAIN_PERMISSION_PROCESS_MODE", mode)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap()
    }

    fn clone_database(source: &StoragePaths, target_root: &std::path::Path) -> StoragePaths {
        let connection = rusqlite::Connection::open(source.brain_db()).unwrap();
        connection
            .execute_batch("PRAGMA wal_checkpoint(TRUNCATE)")
            .unwrap();
        drop(connection);
        let target = StoragePaths::at(target_root);
        fs::create_dir_all(target.db_dir()).unwrap();
        fs::set_permissions(target.db_dir(), fs::Permissions::from_mode(0o700)).unwrap();
        fs::copy(source.brain_db(), target.brain_db()).unwrap();
        fs::set_permissions(target.brain_db(), fs::Permissions::from_mode(0o600)).unwrap();
        target
    }

    #[test]
    #[ignore]
    fn permission_process_helper() {
        let Some(root) = std::env::var_os("CODING_BRAIN_PERMISSION_PROCESS_ROOT") else {
            return;
        };
        let mode = std::env::var("CODING_BRAIN_PERMISSION_PROCESS_MODE").unwrap();
        let root = std::path::PathBuf::from(root);
        let paths = StoragePaths::at(&root);
        let request = admission("process-activity");
        let duration = if mode == "busy-delivery" {
            Duration::from_secs(1)
        } else {
            Duration::from_secs(5)
        };
        let mut db = open(&paths, duration);
        let Some(guard) = db.admit_permission(request.clone()).unwrap() else {
            fs::write(root.join("loser"), b"loser").unwrap();
            return;
        };
        fs::write(root.join("attempt-id"), guard.attempt_id().as_str()).unwrap();
        if mode == "hold" {
            fs::write(root.join("ready"), b"ready").unwrap();
            wait_for(&root.join("release"));
            return;
        }
        let commit_fault = if matches!(
            mode.as_str(),
            "before-delivery-commit" | "busy-delivery" | "successful-delivery"
        ) {
            None
        } else {
            Some(mode.as_str())
        };
        if let Some(fault) = commit_fault {
            unsafe { std::env::set_var("CODING_BRAIN_SQLITE_PERMISSION_FAULT", fault) };
        }
        let result = db.commit_permission(
            PreparedPermissionCommit::new(
                guard,
                proposal("process-decision"),
                terminal(&request, "process-decision"),
                PermissionAuthority {
                    transaction_id: format!("transaction-{mode}"),
                    action: PermissionAction::Allow,
                },
                PermissionEvidenceKind::ProviderAuthority,
                true,
            )
            .unwrap(),
        );
        if mode == "busy-delivery" {
            let committed = result.unwrap();
            fs::write(root.join("ready"), b"ready").unwrap();
            wait_for(&root.join("start-delivery"));
            unsafe {
                std::env::set_var(
                    "CODING_BRAIN_SQLITE_PERMISSION_FAULT",
                    "sleep-before-delivery-transaction",
                )
            };
            assert!(matches!(
                db.record_delivery(&committed, DeliveryEvidence::Delivered),
                Err(StorageError::Busy)
            ));
            fs::write(root.join("busy-returned"), b"busy").unwrap();
            return;
        }
        if mode == "successful-delivery" {
            let committed = result.unwrap();
            fs::write(root.join("provider-response"), MODELED_PROVIDER_RESPONSE).unwrap();
            db.record_delivery(&committed, DeliveryEvidence::Delivered)
                .unwrap();
            return;
        }
        if mode == "before-delivery-commit" {
            let committed = result.unwrap();
            fs::write(root.join("provider-response"), MODELED_PROVIDER_RESPONSE).unwrap();
            unsafe {
                std::env::set_var(
                    "CODING_BRAIN_SQLITE_PERMISSION_FAULT",
                    "before-delivery-commit",
                )
            };
            assert!(
                db.record_delivery(&committed, DeliveryEvidence::Delivered)
                    .is_err()
            );
        } else {
            assert!(result.is_err());
        }
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    #[ignore]
    fn live_fault_process_helper() {
        let Some(root) = std::env::var_os("CODING_BRAIN_PERMISSION_LIVE_FAULT_ROOT") else {
            return;
        };
        let root = std::path::PathBuf::from(root);
        let mode = std::env::var("CODING_BRAIN_PERMISSION_LIVE_FAULT_MODE").unwrap();
        let point = serde_json::from_str(
            &std::env::var("CODING_BRAIN_PERMISSION_LIVE_FAULT_POINT").unwrap(),
        )
        .unwrap();
        let activation = crate::brain::storage::FaultActivation {
            capability: std::env::var_os("CODING_BRAIN_PERMISSION_LIVE_FAULT_CAPABILITY")
                .unwrap()
                .into(),
            state_root: root.clone(),
            nonce: std::env::var("CODING_BRAIN_PERMISSION_LIVE_FAULT_NONCE").unwrap(),
            selection: crate::brain::storage::FaultSelection::Matrix(point),
            control_fd: std::env::var("CODING_BRAIN_PERMISSION_LIVE_FAULT_FD")
                .unwrap()
                .parse()
                .unwrap(),
        };
        crate::brain::storage::activate_fault(activation).unwrap();
        let paths = StoragePaths::at(&root);
        let request = admission("live-fault-activity");
        let mut db = open(&paths, Duration::from_secs(5));
        if mode == "admission" {
            let result = db.admit_permission(request);
            assert!(matches!(
                result,
                Err(StorageError::StorageFault {
                    operation: StorageOperation::Admission,
                    category: StorageFaultCategory::Full,
                })
            ));
            return;
        }
        let guard = db.admit_permission(request.clone()).unwrap().unwrap();
        fs::write(
            root.join("live-fault-attempt-id"),
            guard.attempt_id().as_str(),
        )
        .unwrap();
        let committed = db
            .commit_permission(
                PreparedPermissionCommit::new(
                    guard,
                    proposal("live-fault-decision"),
                    terminal(&request, "live-fault-decision"),
                    PermissionAuthority {
                        transaction_id: "live-fault-transaction".into(),
                        action: PermissionAction::Allow,
                    },
                    PermissionEvidenceKind::ProviderAuthority,
                    true,
                )
                .unwrap(),
            )
            .unwrap();
        if mode == "delivery" {
            let result = db.record_delivery(&committed, DeliveryEvidence::Delivered);
            assert!(matches!(
                result,
                Err(StorageError::CommitUncertain {
                    operation: StorageOperation::Delivery,
                    category: StorageFaultCategory::Io,
                })
            ));
        }
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn live_admission_write_fires_before_transaction_and_maps_full() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let mut fixture = LiveFaultFixture::new(
            root.path(),
            crate::brain::storage::FaultPoint::AdmissionWrite,
        );
        let status = fixture.spawn(root.path(), "admission").wait().unwrap();
        assert!(status.success(), "{status:?}");
        assert_eq!(
            fixture.marker(),
            b"CBRAIN-FAULT-V1\0admission-write\0before\0-\n"
        );
        let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM permission_attempts", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[cfg(feature = "fault-injection")]
    fn assert_live_commit_abort(
        point: crate::brain::storage::FaultPoint,
        marker: &[u8],
        expected_commits: i64,
    ) {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let mut fixture = LiveFaultFixture::new(root.path(), point);
        let status = fixture.spawn(root.path(), "commit").wait().unwrap();
        assert!(!status.success(), "fault helper unexpectedly survived");
        assert_eq!(fixture.marker(), marker);
        let attempt_id =
            AttemptId(fs::read_to_string(root.path().join("live-fault-attempt-id")).unwrap());
        let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM permission_attempts WHERE attempt_state = 'evaluating'",
                    [],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            1 - expected_commits
        );
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM permission_commits", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            expected_commits
        );
        let database = open(&paths, Duration::from_secs(2));
        if expected_commits == 0 {
            assert_eq!(
                database.permission_state(&attempt_id).unwrap(),
                PermissionState::Absent
            );
        } else {
            assert!(matches!(
                database.permission_state(&attempt_id).unwrap(),
                PermissionState::CommittedDeliveryUnknown(authority)
                    if authority.action == PermissionAction::Allow
            ));
        }
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn live_commit_points_bracket_the_sqlite_commit_return() {
        assert_live_commit_abort(
            crate::brain::storage::FaultPoint::CommitBeforeCall,
            b"CBRAIN-FAULT-V1\0commit-before-call\0before\0-\n",
            0,
        );
        assert_live_commit_abort(
            crate::brain::storage::FaultPoint::CommitAfterReturn,
            b"CBRAIN-FAULT-V1\0commit-after-return\0after\0-\n",
            1,
        );
    }

    #[cfg(feature = "fault-injection")]
    #[test]
    fn live_delivery_write_keeps_committed_authority_pending() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let mut fixture = LiveFaultFixture::new(
            root.path(),
            crate::brain::storage::FaultPoint::DeliveryWrite,
        );
        let status = fixture.spawn(root.path(), "delivery").wait().unwrap();
        assert!(status.success(), "{status:?}");
        assert_eq!(
            fixture.marker(),
            b"CBRAIN-FAULT-V1\0delivery-write\0before\0-\n"
        );
        let attempt_id =
            AttemptId(fs::read_to_string(root.path().join("live-fault-attempt-id")).unwrap());
        let database = open(&paths, Duration::from_secs(2));
        assert!(matches!(
            database.permission_state(&attempt_id).unwrap(),
            PermissionState::CommittedDeliveryUnknown(authority)
                if authority.action == PermissionAction::Allow
        ));
        let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM permission_commits", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
    }

    #[test]
    fn identical_admission_has_one_active_winner_and_sequential_attempts_are_distinct() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let request = admission("activity-1");
        let mut first_db = open(&paths, Duration::from_secs(2));
        let first = first_db.admit_permission(request.clone()).unwrap().unwrap();
        let mut contender_db = open(&paths, Duration::from_secs(2));
        assert!(
            contender_db
                .admit_permission(request.clone())
                .unwrap()
                .is_none()
        );
        let first_id = first.attempt_id().clone();
        drop(first);
        let second = contender_db.admit_permission(request).unwrap().unwrap();
        assert_ne!(first_id, *second.attempt_id());
        assert!(matches!(
            contender_db.permission_state(second.attempt_id()).unwrap(),
            PermissionState::Absent
        ));
    }

    #[test]
    fn extended_faults_are_mapped_at_permission_call_site_boundaries() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let mut db = open(&paths, Duration::from_secs(2));

        let admission_error = super::super::maintenance::with_sqlite_fault(
            "permission-admission-body",
            ffi::SQLITE_FULL,
            || db.admit_permission(admission("fault-admission")),
        )
        .unwrap_err();
        assert!(matches!(
            admission_error,
            StorageError::StorageFault {
                operation: StorageOperation::Admission,
                category: StorageFaultCategory::Full,
            }
        ));

        let commit_admission = admission("fault-commit");
        let guard = db
            .admit_permission(commit_admission.clone())
            .unwrap()
            .unwrap();
        let commit_error = super::super::maintenance::with_sqlite_fault(
            "permission-commit-commit",
            ffi::SQLITE_IOERR_FSYNC,
            || {
                db.commit_permission(
                    PreparedPermissionCommit::new(
                        guard,
                        proposal("fault-commit-decision"),
                        terminal(&commit_admission, "fault-commit-decision"),
                        PermissionAuthority {
                            transaction_id: "fault-commit-transaction".into(),
                            action: PermissionAction::Allow,
                        },
                        PermissionEvidenceKind::ProviderAuthority,
                        true,
                    )
                    .unwrap(),
                )
            },
        )
        .unwrap_err();
        assert!(matches!(
            commit_error,
            StorageError::CommitUncertain {
                operation: StorageOperation::Commit,
                category: StorageFaultCategory::Io,
            }
        ));

        let delivery_admission = admission("fault-delivery");
        let guard = db
            .admit_permission(delivery_admission.clone())
            .unwrap()
            .unwrap();
        let committed = db
            .commit_permission(
                PreparedPermissionCommit::new(
                    guard,
                    proposal("fault-delivery-decision"),
                    terminal(&delivery_admission, "fault-delivery-decision"),
                    PermissionAuthority {
                        transaction_id: "fault-delivery-transaction".into(),
                        action: PermissionAction::Allow,
                    },
                    PermissionEvidenceKind::ProviderAuthority,
                    true,
                )
                .unwrap(),
            )
            .unwrap();
        let delivery_error = super::super::maintenance::with_sqlite_fault(
            "permission-delivery-commit",
            ffi::SQLITE_IOERR_FSYNC,
            || db.record_delivery(&committed, DeliveryEvidence::Delivered),
        )
        .unwrap_err();
        assert!(matches!(
            delivery_error,
            StorageError::CommitUncertain {
                operation: StorageOperation::Delivery,
                category: StorageFaultCategory::Io,
            }
        ));
    }

    #[test]
    fn independent_request_shards_can_hold_inference_guards_concurrently() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let first = admission("activity-1");
        let lock_store =
            crate::brain::permission_request_lock::PermissionRequestLockStore::at(root.path());
        let first_shard = lock_store.shard_for(&first.lifecycle, &first.request_key);
        let mut second = admission("activity-2");
        for value in 0..10_000 {
            second.request_key = format!("{value:064x}");
            if lock_store.shard_for(&second.lifecycle, &second.request_key) != first_shard {
                break;
            }
        }
        assert_ne!(
            first_shard,
            lock_store.shard_for(&second.lifecycle, &second.request_key)
        );
        let mut first_db = open(&paths, Duration::from_secs(2));
        let mut second_db = open(&paths, Duration::from_secs(2));
        let first_guard = first_db.admit_permission(first).unwrap().unwrap();
        let second_guard = second_db.admit_permission(second).unwrap().unwrap();
        assert_ne!(first_guard.attempt_id(), second_guard.attempt_id());
    }

    #[test]
    fn separate_process_identical_admission_has_exactly_one_mutating_winner() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let mut holder = helper(root.path(), "hold");
        wait_for(&root.path().join("ready"));
        let contender = helper(root.path(), "hold").wait_with_output().unwrap();
        assert!(
            contender.status.success(),
            "{}",
            String::from_utf8_lossy(&contender.stderr)
        );
        wait_for(&root.path().join("loser"));
        let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM permission_attempts", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            1
        );
        drop(connection);
        fs::write(root.path().join("release"), b"release").unwrap();
        assert!(holder.wait().unwrap().success());
    }

    #[test]
    fn process_commit_faults_classify_fresh_as_absent_or_delivery_unknown_without_journal() {
        for (stage, committed) in [("before-commit", false), ("after-commit", true)] {
            let root = root();
            let paths = StoragePaths::at(root.path());
            drop(BrainDb::create_current(&paths).unwrap());
            let output = helper(root.path(), stage).wait_with_output().unwrap();
            assert!(
                output.status.success(),
                "{stage}: {}",
                String::from_utf8_lossy(&output.stderr)
            );
            let attempt_id = AttemptId(fs::read_to_string(root.path().join("attempt-id")).unwrap());
            let reopened = open(&paths, Duration::from_secs(2));
            let state = reopened.permission_state(&attempt_id).unwrap();
            assert_eq!(
                matches!(state, PermissionState::CommittedDeliveryUnknown(_)),
                committed,
                "{stage}"
            );
            assert!(!root.path().join("provider-response").exists(), "{stage}");
            assert!(!root.path().join("brain/permission-transactions").exists());
        }
    }

    #[test]
    fn abrupt_process_crash_after_commit_is_delivery_unknown_and_never_replayed() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let output = helper(root.path(), "abort-after-commit")
            .wait_with_output()
            .unwrap();
        assert!(!output.status.success());
        assert!(!root.path().join("provider-response").exists());
        let attempt_id = AttemptId(fs::read_to_string(root.path().join("attempt-id")).unwrap());
        let reopened = open(&paths, Duration::from_secs(2));
        assert!(matches!(
            reopened.permission_state(&attempt_id).unwrap(),
            PermissionState::CommittedDeliveryUnknown(_)
        ));
    }

    #[test]
    fn delivery_transaction_fault_retains_unknown_without_delivery_activity() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let output = helper(root.path(), "before-delivery-commit")
            .wait_with_output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(root.path().join("provider-response")).unwrap(),
            MODELED_PROVIDER_RESPONSE
        );
        let attempt_id = AttemptId(fs::read_to_string(root.path().join("attempt-id")).unwrap());
        let reopened = open(&paths, Duration::from_secs(2));
        assert!(matches!(
            reopened.permission_state(&attempt_id).unwrap(),
            PermissionState::CommittedDeliveryUnknown(_)
        ));
        let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM activity_events
                     WHERE permission_attempt_id = ?1
                       AND event_state IN ('delivered', 'delivery_failed')",
                    [attempt_id.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn modeled_provider_response_is_written_only_after_commit_and_then_delivered() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let output = helper(root.path(), "successful-delivery")
            .wait_with_output()
            .unwrap();
        assert!(
            output.status.success(),
            "{}",
            String::from_utf8_lossy(&output.stderr)
        );
        assert_eq!(
            fs::read(root.path().join("provider-response")).unwrap(),
            MODELED_PROVIDER_RESPONSE
        );
        let attempt_id = AttemptId(fs::read_to_string(root.path().join("attempt-id")).unwrap());
        let reopened = open(&paths, Duration::from_secs(2));
        assert!(matches!(
            reopened.permission_state(&attempt_id).unwrap(),
            PermissionState::Delivered(_)
        ));
    }

    #[test]
    fn modeled_provider_response_failure_records_delivery_failed() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let request = admission("activity-1");
        let mut db = open(&paths, Duration::from_secs(3));
        let guard = db.admit_permission(request.clone()).unwrap().unwrap();
        let committed = db
            .commit_permission(
                PreparedPermissionCommit::new(
                    guard,
                    proposal("decision-1"),
                    terminal(&request, "decision-1"),
                    PermissionAuthority {
                        transaction_id: "transaction-1".into(),
                        action: PermissionAction::Allow,
                    },
                    PermissionEvidenceKind::ProviderAuthority,
                    true,
                )
                .unwrap(),
            )
            .unwrap();
        let response_sink = root.path().join("provider-response");
        fs::create_dir(&response_sink).unwrap();
        assert!(fs::write(&response_sink, MODELED_PROVIDER_RESPONSE).is_err());
        db.record_delivery(&committed, DeliveryEvidence::Failed)
            .unwrap();
        assert!(matches!(
            db.permission_state(committed.attempt_id()).unwrap(),
            PermissionState::DeliveryFailed(_)
        ));
    }

    #[test]
    fn deterministic_safety_deny_is_authoritative_without_response_delivery() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let request = admission("activity-deny");
        let mut deny_proposal = proposal("decision-deny");
        deny_proposal.brain_action = "deny".into();
        deny_proposal.user_action = "hook_deny".into();
        let mut deny_terminal = terminal(&request, "decision-deny");
        deny_terminal.state = ActivityState::Denied;
        let mut db = open(&paths, Duration::from_secs(3));
        let guard = db.admit_permission(request.clone()).unwrap().unwrap();
        let committed = db
            .commit_permission(
                PreparedPermissionCommit::new(
                    guard,
                    deny_proposal,
                    deny_terminal,
                    PermissionAuthority {
                        transaction_id: "transaction-deny".into(),
                        action: PermissionAction::Deny,
                    },
                    PermissionEvidenceKind::DeterministicSafety,
                    false,
                )
                .unwrap(),
            )
            .unwrap();
        assert!(!committed.response_eligible());
        assert_eq!(
            db.permission_decision(committed.attempt_id()).unwrap(),
            Some(PermissionAuthority {
                transaction_id: "transaction-deny".into(),
                action: PermissionAction::Deny,
            })
        );
        assert!(matches!(
            db.record_delivery(&committed, DeliveryEvidence::Delivered),
            Err(StorageError::PermissionAttemptMismatch)
        ));
        let (delivery_state, response_eligible): (String, i64) = db
            .connection
            .query_row(
                "SELECT delivery_state, response_eligible FROM permission_commits
                 WHERE attempt_id = ?1",
                [committed.attempt_id().as_str()],
                |row| Ok((row.get(0)?, row.get(1)?)),
            )
            .unwrap();
        assert_eq!(
            (delivery_state.as_str(), response_eligible),
            ("not_required", 0)
        );

        let invalid_request = admission("activity-invalid");
        let invalid_guard = db
            .admit_permission(invalid_request.clone())
            .unwrap()
            .unwrap();
        assert!(matches!(
            PreparedPermissionCommit::new(
                invalid_guard,
                proposal("decision-invalid"),
                terminal(&invalid_request, "decision-invalid"),
                PermissionAuthority {
                    transaction_id: "transaction-invalid".into(),
                    action: PermissionAction::Allow,
                },
                PermissionEvidenceKind::DeterministicSafety,
                false,
            ),
            Err(StorageError::PermissionAttemptMismatch)
        ));
    }

    #[test]
    fn deterministic_safety_deny_cannot_be_response_eligible() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let request = admission("activity-response-eligible-deny");
        let mut deny_proposal = proposal("decision-response-eligible-deny");
        deny_proposal.brain_action = "deny".into();
        deny_proposal.user_action = "hook_deny".into();
        let mut deny_terminal = terminal(&request, "decision-response-eligible-deny");
        deny_terminal.state = ActivityState::Denied;
        let mut db = open(&paths, Duration::from_secs(3));
        let guard = db.admit_permission(request).unwrap().unwrap();

        assert!(matches!(
            PreparedPermissionCommit::new(
                guard,
                deny_proposal,
                deny_terminal,
                PermissionAuthority {
                    transaction_id: "transaction-response-eligible-deny".into(),
                    action: PermissionAction::Deny,
                },
                PermissionEvidenceKind::DeterministicSafety,
                true,
            ),
            Err(StorageError::PermissionAttemptMismatch)
        ));
    }

    #[test]
    fn fresh_reads_reject_response_eligible_deterministic_safety_corruption() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let request = admission("activity-corrupt-deterministic-deny");
        let mut deny_proposal = proposal("decision-corrupt-deterministic-deny");
        deny_proposal.brain_action = "deny".into();
        deny_proposal.user_action = "hook_deny".into();
        let mut deny_terminal = terminal(&request, "decision-corrupt-deterministic-deny");
        deny_terminal.state = ActivityState::Denied;
        let mut db = open(&paths, Duration::from_secs(3));
        let guard = db.admit_permission(request).unwrap().unwrap();
        let committed = db
            .commit_permission(
                PreparedPermissionCommit::new(
                    guard,
                    deny_proposal,
                    deny_terminal,
                    PermissionAuthority {
                        transaction_id: "transaction-corrupt-deterministic-deny".into(),
                        action: PermissionAction::Deny,
                    },
                    PermissionEvidenceKind::DeterministicSafety,
                    false,
                )
                .unwrap(),
            )
            .unwrap();
        let corrupted_capability = CommittedPermission {
            response_eligible: true,
            ..committed
        };
        drop(db);
        let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
        connection
            .execute_batch(
                "PRAGMA ignore_check_constraints = ON;
                 UPDATE permission_commits
                 SET response_eligible = 1, delivery_state = 'pending';",
            )
            .unwrap();
        drop(connection);

        let mut reopened = open(&paths, Duration::from_secs(3));
        assert!(matches!(
            reopened.permission_state(corrupted_capability.attempt_id()),
            Err(StorageError::InvalidStorage(_))
        ));
        assert!(matches!(
            reopened.permission_decision(corrupted_capability.attempt_id()),
            Err(StorageError::InvalidStorage(_))
        ));
        assert!(matches!(
            reopened.record_delivery(&corrupted_capability, DeliveryEvidence::Delivered),
            Err(StorageError::InvalidStorage(_))
        ));
    }

    #[test]
    fn permission_commit_and_lookup_ignore_more_than_sixteen_mib_of_history() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        let mut history_db = BrainDb::create_current(&paths).unwrap();
        let request = admission("activity-current");
        let history = (0..450)
            .map(|index| {
                let mut event = terminal(&request, &format!("history-decision-{index}"));
                event.activity_id = format!("history-activity-{index}");
                event.recorded_at_ms = 10 + index;
                event.session = None;
                event.state = ActivityState::Observed;
                event.project.project_id = ProjectId::Temporary("p".repeat(4096));
                event.project.cwd = format!("/{}", "c".repeat(4000)).into();
                event.project.label = Some("l".repeat(4096));
                event.tool = Some("t".repeat(4096));
                event.normalized_command = Some("n".repeat(4096));
                event.fingerprint = Some("f".repeat(4096));
                event.rule_id = Some("r".repeat(4096));
                event.reasoning = Some("e".repeat(4096));
                event.decision_id = Some(format!("{}-{index}", "d".repeat(4080)));
                event.supersedes = Some("s".repeat(4096));
                event
            })
            .collect::<Vec<_>>();
        history_db.append_activity_batch(&history).unwrap();
        let history_bytes: i64 = history_db
            .connection
            .query_row(
                "SELECT sum(length(event_payload)) FROM activity_events",
                [],
                |row| row.get(0),
            )
            .unwrap();
        assert!(
            history_bytes > 16 * 1024 * 1024,
            "history contains only {history_bytes} serialized bytes"
        );
        drop(history_db);

        let mut db = open(&paths, Duration::from_secs(5));
        let guard = db.admit_permission(request.clone()).unwrap().unwrap();
        let committed = db
            .commit_permission(
                PreparedPermissionCommit::new(
                    guard,
                    proposal("decision-current"),
                    terminal(&request, "decision-current"),
                    PermissionAuthority {
                        transaction_id: "transaction-current".into(),
                        action: PermissionAction::Allow,
                    },
                    PermissionEvidenceKind::ProviderAuthority,
                    true,
                )
                .unwrap(),
            )
            .unwrap();
        assert!(
            db.permission_decision(committed.attempt_id())
                .unwrap()
                .is_some()
        );
        let plan = db.explain_permission_lookup().unwrap();
        assert!(
            plan.contains("permission_attempts_request_active"),
            "{plan}"
        );
        assert!(!plan.contains("SCAN permission_attempts"), "{plan}");
    }

    #[test]
    fn corrupt_review_database_cannot_change_permission_or_delivery_authority() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        drop(ReviewDb::create_current(&paths).unwrap());
        let request = admission("activity-1");
        let mut db = open(&paths, Duration::from_secs(3));
        let guard = db.admit_permission(request.clone()).unwrap().unwrap();
        let committed = db
            .commit_permission(
                PreparedPermissionCommit::new(
                    guard,
                    proposal("decision-1"),
                    terminal(&request, "decision-1"),
                    PermissionAuthority {
                        transaction_id: "transaction-1".into(),
                        action: PermissionAction::Allow,
                    },
                    PermissionEvidenceKind::ProviderAuthority,
                    true,
                )
                .unwrap(),
            )
            .unwrap();
        fs::write(paths.review_db(), b"not sqlite").unwrap();
        assert!(
            ReviewDb::open_current(
                &paths,
                OpenRole::Hook,
                StorageDeadline::after(Duration::from_millis(250)),
            )
            .is_err()
        );
        assert!(
            db.permission_decision(committed.attempt_id())
                .unwrap()
                .is_some()
        );
        db.record_delivery(&committed, DeliveryEvidence::Delivered)
            .unwrap();
        assert!(matches!(
            db.permission_state(committed.attempt_id()).unwrap(),
            PermissionState::Delivered(_)
        ));
    }

    #[test]
    fn permission_failure_neither_creates_nor_mutates_review_storage() {
        for review_exists in [false, true] {
            let root = root();
            let paths = StoragePaths::at(root.path());
            drop(BrainDb::create_current(&paths).unwrap());
            let review_before = if review_exists {
                drop(ReviewDb::create_current(&paths).unwrap());
                Some(fs::read(paths.review_db()).unwrap())
            } else {
                assert!(!paths.review_db().exists());
                None
            };
            let request = admission("activity-1");
            let mut db = open(&paths, Duration::from_secs(3));
            let guard = db.admit_permission(request.clone()).unwrap().unwrap();
            let prepared = PreparedPermissionCommit::new(
                guard,
                proposal("decision-1"),
                terminal(&request, "decision-1"),
                PermissionAuthority {
                    transaction_id: "transaction-1".into(),
                    action: PermissionAction::Allow,
                },
                PermissionEvidenceKind::ProviderAuthority,
                true,
            )
            .unwrap();
            let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
            connection
                .execute("UPDATE permission_attempts SET tool_name = 'Write'", [])
                .unwrap();
            drop(connection);
            assert!(matches!(
                db.commit_permission(prepared),
                Err(StorageError::PermissionAttemptMismatch)
            ));
            match review_before {
                Some(bytes) => assert_eq!(fs::read(paths.review_db()).unwrap(), bytes),
                None => assert!(!paths.review_db().exists()),
            }
        }
    }

    #[test]
    fn atomic_commit_preserves_optional_tool_exact_authority_and_delivery_states() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let request = admission("activity-1");
        let mut db = open(&paths, Duration::from_secs(2));
        let guard = db.admit_permission(request.clone()).unwrap().unwrap();
        let committed = db
            .commit_permission(
                PreparedPermissionCommit::new(
                    guard,
                    proposal("decision-1"),
                    terminal(&request, "decision-1"),
                    PermissionAuthority {
                        transaction_id: "transaction-1".into(),
                        action: PermissionAction::Allow,
                    },
                    PermissionEvidenceKind::ProviderAuthority,
                    true,
                )
                .unwrap(),
            )
            .unwrap();
        assert_eq!(
            db.permission_decision(&committed.attempt_id).unwrap(),
            Some(PermissionAuthority {
                transaction_id: "transaction-1".into(),
                action: PermissionAction::Allow,
            })
        );
        assert!(matches!(
            db.permission_state(&committed.attempt_id).unwrap(),
            PermissionState::CommittedDeliveryUnknown(_)
        ));
        db.record_delivery(&committed, DeliveryEvidence::Delivered)
            .unwrap();
        assert!(matches!(
            db.permission_state(&committed.attempt_id).unwrap(),
            PermissionState::Delivered(_)
        ));
        let attempt_id = committed.attempt_id.clone();
        drop(db);
        let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
        connection
            .execute(
                "UPDATE decision_identities SET provider = 'claude'
                 WHERE decision_id = 'decision-1'",
                [],
            )
            .unwrap();
        drop(connection);
        let reopened = open(&paths, Duration::from_secs(2));
        assert!(matches!(
            reopened.permission_state(&attempt_id),
            Err(StorageError::InvalidStorage(_))
        ));
    }

    #[test]
    fn fresh_authority_reads_and_delivery_reject_every_corrupt_identity_or_timestamp() {
        for (label, mutation) in [
            (
                "request identity",
                "UPDATE permission_attempts SET request_identity_key = printf('%064x', 11)",
            ),
            (
                "request key",
                "UPDATE permission_attempts SET request_key = printf('%064x', 11)",
            ),
            (
                "attempt creation timestamp",
                "UPDATE permission_attempts SET created_at_ms = 4",
            ),
            (
                "attempt update timestamp",
                "UPDATE permission_attempts SET updated_at_ms = 2",
            ),
            (
                "decision timestamp",
                "UPDATE decision_identities SET decided_at_ms = 2",
            ),
            (
                "event timestamp",
                "UPDATE activity_events SET recorded_at_ms = 2 WHERE event_state = 'allowed'",
            ),
            (
                "event kind",
                "UPDATE activity_events SET event_kind = 'diagnostic'
                 WHERE event_state = 'allowed'",
            ),
            (
                "event source cursor range",
                "UPDATE activity_events SET source_cursor = 0 WHERE event_state = 'allowed'",
            ),
            (
                "event source cursor high water",
                "UPDATE activity_events SET source_cursor = 4 WHERE event_state = 'allowed'",
            ),
            (
                "commit timestamp",
                "UPDATE permission_commits SET committed_at_ms = 2",
            ),
            (
                "event payload bound",
                "UPDATE activity_events
                 SET event_payload = CAST(event_payload || printf('%65537s', '') AS BLOB)
                 WHERE event_state = 'allowed'",
            ),
        ] {
            let root = root();
            let paths = StoragePaths::at(root.path());
            drop(BrainDb::create_current(&paths).unwrap());
            let request = admission("activity-1");
            let mut db = open(&paths, Duration::from_secs(20));
            let guard = db.admit_permission(request.clone()).unwrap().unwrap();
            let committed = db
                .commit_permission(
                    PreparedPermissionCommit::new(
                        guard,
                        proposal("decision-1"),
                        terminal(&request, "decision-1"),
                        PermissionAuthority {
                            transaction_id: "transaction-1".into(),
                            action: PermissionAction::Allow,
                        },
                        PermissionEvidenceKind::ProviderAuthority,
                        true,
                    )
                    .unwrap(),
                )
                .unwrap();
            drop(db);
            let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
            connection
                .execute_batch(
                    "PRAGMA foreign_keys = OFF;
                     PRAGMA ignore_check_constraints = ON;",
                )
                .unwrap();
            connection.execute(mutation, []).unwrap();
            drop(connection);

            let mut reopened = open(&paths, Duration::from_secs(5));
            assert!(
                matches!(
                    reopened.permission_state(committed.attempt_id()),
                    Err(StorageError::InvalidStorage(_))
                ),
                "permission_state accepted corrupt {label}"
            );
            assert!(
                matches!(
                    reopened.permission_decision(committed.attempt_id()),
                    Err(StorageError::InvalidStorage(_))
                ),
                "permission_decision accepted corrupt {label}"
            );
            assert!(
                matches!(
                    reopened.record_delivery(&committed, DeliveryEvidence::Delivered),
                    Err(StorageError::InvalidStorage(_))
                ),
                "record_delivery accepted corrupt {label}"
            );
        }
    }

    #[test]
    fn committed_permission_without_tool_use_is_readable_by_decision_and_learning_apis() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let request = admission("activity-1");
        let mut db = open(&paths, Duration::from_secs(5));
        let guard = db.admit_permission(request.clone()).unwrap().unwrap();
        let committed = db
            .commit_permission(
                PreparedPermissionCommit::new(
                    guard,
                    proposal("decision-1"),
                    terminal(&request, "decision-1"),
                    PermissionAuthority {
                        transaction_id: "transaction-1".into(),
                        action: PermissionAction::Allow,
                    },
                    PermissionEvidenceKind::ProviderAuthority,
                    true,
                )
                .unwrap(),
            )
            .unwrap();

        let identity = db.decision_identity("decision-1").unwrap().unwrap();
        let DecisionIdentity::Permission { tool_use_id, .. } = identity else {
            panic!("committed permission materialized as an observation");
        };
        assert_eq!(tool_use_id, None);
        assert_eq!(
            db.decision_payload("decision-1")
                .unwrap()
                .unwrap()
                .source_cursor,
            committed.terminal_cursor()
        );
        let learned = db.learning_decisions(1, 1024 * 1024).unwrap();
        assert_eq!(learned.len(), 1);
        assert_eq!(learned[0].record.decision_id.as_deref(), Some("decision-1"));
    }

    #[test]
    fn commit_revalidates_every_typed_attempt_column_before_any_terminal_mutation() {
        for mutation in [
            "UPDATE permission_attempts SET request_key = printf('%064d', 0)",
            "UPDATE permission_attempts SET tool_name = 'Write'",
            "UPDATE permission_attempts SET cwd = X'2F6F74686572'",
            "UPDATE permission_attempts SET activity_id = 'other'",
        ] {
            let root = root();
            let paths = StoragePaths::at(root.path());
            drop(BrainDb::create_current(&paths).unwrap());
            let request = admission("activity-1");
            let mut db = open(&paths, Duration::from_secs(2));
            let guard = db.admit_permission(request.clone()).unwrap().unwrap();
            let attempt_id = guard.attempt_id().clone();
            let prepared = PreparedPermissionCommit::new(
                guard,
                proposal("decision-1"),
                terminal(&request, "decision-1"),
                PermissionAuthority {
                    transaction_id: "transaction-1".into(),
                    action: PermissionAction::Allow,
                },
                PermissionEvidenceKind::ProviderAuthority,
                true,
            )
            .unwrap();
            let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
            connection.execute(mutation, []).unwrap();
            drop(connection);
            assert!(matches!(
                db.commit_permission(prepared),
                Err(StorageError::PermissionAttemptMismatch)
            ));
            let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
            assert_eq!(
                connection
                    .query_row(
                        "SELECT count(*) FROM activity_events
                         WHERE permission_attempt_id = ?1 AND event_state IN ('allowed', 'denied')",
                        [attempt_id.as_str()],
                        |row| row.get::<_, i64>(0),
                    )
                    .unwrap(),
                0,
                "{mutation}"
            );
        }
    }

    #[test]
    fn attempt_and_delivery_capabilities_are_bound_to_the_opened_database_inode() {
        let first_root = root();
        let first_paths = StoragePaths::at(first_root.path());
        drop(BrainDb::create_current(&first_paths).unwrap());
        let request = admission("activity-1");
        let mut first = open(&first_paths, Duration::from_secs(3));
        let guard = first.admit_permission(request.clone()).unwrap().unwrap();
        let prepared = PreparedPermissionCommit::new(
            guard,
            proposal("decision-1"),
            terminal(&request, "decision-1"),
            PermissionAuthority {
                transaction_id: "transaction-1".into(),
                action: PermissionAction::Allow,
            },
            PermissionEvidenceKind::ProviderAuthority,
            true,
        )
        .unwrap();
        let second_root = root();
        let second_paths = clone_database(&first_paths, second_root.path());
        let mut second = open(&second_paths, Duration::from_secs(3));
        assert!(matches!(
            second.commit_permission(prepared),
            Err(StorageError::PermissionAttemptMismatch)
        ));

        let request = admission("activity-2");
        let guard = first.admit_permission(request.clone()).unwrap().unwrap();
        let committed = first
            .commit_permission(
                PreparedPermissionCommit::new(
                    guard,
                    proposal("decision-2"),
                    terminal(&request, "decision-2"),
                    PermissionAuthority {
                        transaction_id: "transaction-2".into(),
                        action: PermissionAction::Allow,
                    },
                    PermissionEvidenceKind::ProviderAuthority,
                    true,
                )
                .unwrap(),
            )
            .unwrap();
        let third_root = root();
        let third_paths = clone_database(&first_paths, third_root.path());
        let mut third = open(&third_paths, Duration::from_secs(3));
        assert!(matches!(
            third.record_delivery(&committed, DeliveryEvidence::Delivered),
            Err(StorageError::PermissionAttemptMismatch)
        ));
    }

    #[test]
    fn delivery_cannot_reset_the_original_absolute_deadline() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let request = admission("activity-1");
        let mut db = open(&paths, Duration::from_millis(200));
        let guard = db.admit_permission(request.clone()).unwrap().unwrap();
        let committed = db
            .commit_permission(
                PreparedPermissionCommit::new(
                    guard,
                    proposal("decision-1"),
                    terminal(&request, "decision-1"),
                    PermissionAuthority {
                        transaction_id: "transaction-1".into(),
                        action: PermissionAction::Allow,
                    },
                    PermissionEvidenceKind::ProviderAuthority,
                    true,
                )
                .unwrap(),
            )
            .unwrap();
        thread::sleep(Duration::from_millis(220));
        let mut reopened = open(&paths, Duration::from_secs(2));
        assert!(matches!(
            reopened.record_delivery(&committed, DeliveryEvidence::Delivered),
            Err(StorageError::Busy)
        ));
        assert!(matches!(
            reopened.permission_state(&committed.attempt_id).unwrap(),
            PermissionState::CommittedDeliveryUnknown(_)
        ));
    }

    #[test]
    fn delivery_busy_wait_uses_only_the_remaining_absolute_deadline() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let mut child = helper(root.path(), "busy-delivery");
        wait_for(&root.path().join("ready"));
        let blocker = rusqlite::Connection::open(paths.brain_db()).unwrap();
        blocker.execute_batch("BEGIN IMMEDIATE").unwrap();
        let started = std::time::Instant::now();
        fs::write(root.path().join("start-delivery"), b"start").unwrap();
        let status = child.wait().unwrap();
        let elapsed = started.elapsed();
        assert!(status.success());
        assert!(root.path().join("busy-returned").exists());
        assert!(
            elapsed < Duration::from_millis(1_250),
            "delivery busy wait reset the absolute deadline: {elapsed:?}"
        );
        blocker.execute_batch("ROLLBACK").unwrap();

        let attempt_id = AttemptId(fs::read_to_string(root.path().join("attempt-id")).unwrap());
        let reopened = open(&paths, Duration::from_secs(2));
        assert!(matches!(
            reopened.permission_state(&attempt_id).unwrap(),
            PermissionState::CommittedDeliveryUnknown(_)
        ));
        let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM activity_events
                     WHERE permission_attempt_id = ?1
                       AND event_state IN ('delivered', 'delivery_failed')",
                    [attempt_id.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn inference_time_exhausts_commit_deadline_without_authority_or_terminal_row() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let request = admission("activity-1");
        let mut db = open(&paths, Duration::from_millis(60));
        let guard = db.admit_permission(request.clone()).unwrap().unwrap();
        let attempt_id = guard.attempt_id().clone();
        let prepared = PreparedPermissionCommit::new(
            guard,
            proposal("decision-1"),
            terminal(&request, "decision-1"),
            PermissionAuthority {
                transaction_id: "transaction-1".into(),
                action: PermissionAction::Allow,
            },
            PermissionEvidenceKind::ProviderAuthority,
            true,
        )
        .unwrap();
        thread::sleep(Duration::from_millis(80));
        assert!(matches!(
            db.commit_permission(prepared),
            Err(StorageError::Busy)
        ));
        let reopened = open(&paths, Duration::from_secs(2));
        assert!(matches!(
            reopened.permission_state(&attempt_id).unwrap(),
            PermissionState::Absent
        ));
        let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
        assert_eq!(
            connection
                .query_row(
                    "SELECT count(*) FROM activity_events
                     WHERE permission_attempt_id = ?1 AND event_state IN ('allowed', 'denied')",
                    [attempt_id.as_str()],
                    |row| row.get::<_, i64>(0),
                )
                .unwrap(),
            0
        );
    }

    #[test]
    fn admission_rejects_noncanonical_request_keys_before_lock_or_database_mutation() {
        let root = root();
        let paths = StoragePaths::at(root.path());
        drop(BrainDb::create_current(&paths).unwrap());
        let mut db = open(&paths, Duration::from_secs(2));
        for request_key in ["short".into(), "A".repeat(64), "g".repeat(64)] {
            let mut request = admission("activity-1");
            request.request_key = request_key;
            assert!(matches!(
                db.admit_permission(request),
                Err(StorageError::InvalidStorage(_))
            ));
        }
        let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
        assert_eq!(
            connection
                .query_row("SELECT count(*) FROM permission_attempts", [], |row| row
                    .get::<_, i64>(0))
                .unwrap(),
            0
        );
    }

    #[test]
    fn prepared_commit_rejects_backward_or_overflowed_terminal_timestamps() {
        for terminal_at in [1, i64::MAX as u64 + 1] {
            let root = root();
            let paths = StoragePaths::at(root.path());
            drop(BrainDb::create_current(&paths).unwrap());
            let request = admission("activity-1");
            let mut db = open(&paths, Duration::from_secs(2));
            let guard = db.admit_permission(request.clone()).unwrap().unwrap();
            let mut terminal = terminal(&request, "decision-1");
            terminal.recorded_at_ms = terminal_at;
            assert!(matches!(
                PreparedPermissionCommit::new(
                    guard,
                    proposal("decision-1"),
                    terminal,
                    PermissionAuthority {
                        transaction_id: "transaction-1".into(),
                        action: PermissionAction::Allow,
                    },
                    PermissionEvidenceKind::ProviderAuthority,
                    true,
                ),
                Err(StorageError::PermissionAttemptMismatch)
            ));
            let connection = rusqlite::Connection::open(paths.brain_db()).unwrap();
            assert_eq!(
                connection
                    .query_row("SELECT count(*) FROM permission_commits", [], |row| row
                        .get::<_, i64>(0),)
                    .unwrap(),
                0
            );
        }
    }
}
