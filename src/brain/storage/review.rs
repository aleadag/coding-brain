use std::collections::{BTreeMap, BTreeSet, HashMap, HashSet};

use coding_brain_core::review_state::{
    MAX_REVIEW_KEYS, ReviewDisposition, ReviewKey, ReviewMutation, ReviewMutationRequest,
    ReviewMutationResult, ReviewSurface,
};
use rusqlite::{Connection, OptionalExtension, TransactionBehavior, params};

use super::{ActivityCursor, ReviewDb, StorageDeadline, StorageError};
use crate::brain::review_state::{ReviewStateError, mutate_surface};

const MAX_GROUP_ID_BYTES: usize = 512;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewEligibleOccurrence {
    surface: ReviewSurface,
    key: ReviewKey,
    group_id: String,
    source_cursor: ActivityCursor,
}

impl ReviewEligibleOccurrence {
    pub fn new(surface: ReviewSurface, key: ReviewKey, source_cursor: ActivityCursor) -> Self {
        Self {
            surface,
            key,
            group_id: key.to_string(),
            source_cursor,
        }
    }

    pub fn key(&self) -> ReviewKey {
        self.key
    }

    pub fn surface(&self) -> ReviewSurface {
        self.surface
    }

    pub fn group_id(&self) -> &str {
        &self.group_id
    }

    pub fn source_cursor(&self) -> ActivityCursor {
        self.source_cursor
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewEligibility {
    surface: ReviewSurface,
    source_high_water: Option<ActivityCursor>,
    occurrences: Vec<ReviewEligibleOccurrence>,
}

impl ReviewEligibility {
    pub fn try_new(
        surface: ReviewSurface,
        source_high_water: Option<ActivityCursor>,
        occurrences: Vec<ReviewEligibleOccurrence>,
    ) -> Result<Self, StorageError> {
        let evidence = Self {
            surface,
            source_high_water,
            occurrences,
        };
        evidence.validate()?;
        Ok(evidence)
    }

    pub fn surface(&self) -> ReviewSurface {
        self.surface
    }

    pub fn source_high_water(&self) -> Option<ActivityCursor> {
        self.source_high_water
    }

    pub fn occurrences(&self) -> &[ReviewEligibleOccurrence] {
        &self.occurrences
    }

    fn validate(&self) -> Result<(), StorageError> {
        if self.occurrences.len() > MAX_REVIEW_KEYS {
            return Err(StorageError::ReviewCapacityExceeded);
        }
        let high_water = self.source_high_water.map_or(0, ActivityCursor::get);
        let mut keys = HashSet::with_capacity(self.occurrences.len());
        let mut identities = HashSet::with_capacity(self.occurrences.len());
        for occurrence in &self.occurrences {
            if occurrence.surface != self.surface {
                return Err(StorageError::InvalidStorage(
                    "review occurrence and evidence surfaces disagree",
                ));
            }
            if occurrence.group_id.is_empty()
                || occurrence.group_id.len() > MAX_GROUP_ID_BYTES
                || occurrence.group_id.contains('\0')
            {
                return Err(StorageError::InvalidStorage(
                    "review group identity is out of range",
                ));
            }
            if occurrence.group_id != occurrence.key.to_string() {
                return Err(StorageError::InvalidStorage(
                    "review key disagrees with its group identity",
                ));
            }
            if occurrence.source_cursor.get() > high_water {
                return Err(StorageError::InvalidStorage(
                    "review cursor exceeds its source high-water",
                ));
            }
            if !keys.insert(occurrence.key) {
                return Err(StorageError::InvalidStorage(
                    "review evidence contains duplicate keys",
                ));
            }
            if !identities.insert((occurrence.group_id.as_str(), occurrence.source_cursor.get())) {
                return Err(StorageError::InvalidStorage(
                    "review evidence contains duplicate cursor identities",
                ));
            }
        }
        Ok(())
    }

    fn high_water_i64(&self) -> i64 {
        self.source_high_water
            .map(|cursor| cursor.get() as i64)
            .unwrap_or(0)
    }

    fn eligible_keys(&self) -> BTreeSet<ReviewKey> {
        self.occurrences
            .iter()
            .map(|occurrence| occurrence.key)
            .collect()
    }
}

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct ReviewSurfaceState {
    surface: ReviewSurface,
    surface_revision: u64,
    source_high_water: u64,
    dispositions: BTreeMap<ReviewKey, ReviewDisposition>,
    last_archive: BTreeSet<ReviewKey>,
}

impl ReviewSurfaceState {
    pub fn surface(&self) -> ReviewSurface {
        self.surface
    }

    pub fn surface_revision(&self) -> u64 {
        self.surface_revision
    }

    pub fn source_high_water(&self) -> u64 {
        self.source_high_water
    }

    pub fn disposition(&self, key: &ReviewKey) -> Option<ReviewDisposition> {
        self.dispositions.get(key).copied()
    }

    pub fn reviewed_count(&self) -> usize {
        self.count(ReviewDisposition::Reviewed)
    }

    pub fn archived_count(&self) -> usize {
        self.count(ReviewDisposition::Archived)
    }

    pub fn last_archive_count(&self) -> usize {
        self.last_archive.len()
    }

    pub fn dispositions(&self) -> impl Iterator<Item = (ReviewKey, ReviewDisposition)> + '_ {
        self.dispositions
            .iter()
            .map(|(key, disposition)| (*key, *disposition))
    }

    pub fn last_archive(&self) -> impl Iterator<Item = ReviewKey> + '_ {
        self.last_archive.iter().copied()
    }

    fn count(&self, disposition: ReviewDisposition) -> usize {
        self.dispositions
            .values()
            .filter(|candidate| **candidate == disposition)
            .count()
    }
}

struct LoadedSurface {
    state: ReviewSurfaceState,
    last_archive_revision: Option<u64>,
    row_revisions: BTreeMap<ReviewKey, u64>,
}

impl ReviewDb {
    pub fn read_surface(
        &self,
        evidence: &ReviewEligibility,
    ) -> Result<ReviewSurfaceState, StorageError> {
        evidence.validate()?;
        apply_deadline(&self.connection, self.deadline)?;
        Ok(load_surface(&self.connection, evidence, self.deadline)?.state)
    }

    pub fn mutate(
        &mut self,
        request: &ReviewMutationRequest,
        evidence: &ReviewEligibility,
    ) -> Result<ReviewMutationResult, StorageError> {
        self.mutate_inner(request, evidence).map_err(|error| {
            super::maintenance::map_storage_error(super::StorageOperation::Review, false, error)
        })
    }

    fn mutate_inner(
        &mut self,
        request: &ReviewMutationRequest,
        evidence: &ReviewEligibility,
    ) -> Result<ReviewMutationResult, StorageError> {
        request
            .validate()
            .map_err(StorageError::InvalidReviewRequest)?;
        evidence.validate()?;
        if request.surface != evidence.surface {
            return Err(StorageError::InvalidStorage(
                "review request and evidence surfaces disagree",
            ));
        }

        apply_deadline(&self.connection, self.deadline)?;
        let transaction = self
            .connection
            .transaction_with_behavior(TransactionBehavior::Immediate)?;
        super::maintenance::sqlite_fault("review-body")?;
        let mut loaded = load_surface(&transaction, evidence, self.deadline)?;
        if loaded.state.surface_revision >= i64::MAX as u64 {
            return Err(StorageError::ReviewRevisionOverflow);
        }
        let previous_dispositions = loaded.state.dispositions.clone();
        let previous_last_archive = loaded.state.last_archive.clone();
        let mut revision = loaded.state.surface_revision;
        let result = mutate_surface(
            request,
            &evidence.eligible_keys(),
            &mut revision,
            &mut loaded.state.dispositions,
            &mut loaded.state.last_archive,
        )
        .map_err(map_review_error)?;
        loaded.state.surface_revision = revision;

        let last_archive_revision = match &request.operation {
            ReviewMutation::SetDisposition {
                disposition: ReviewDisposition::Archived,
                ..
            }
            | ReviewMutation::ArchiveAllReviewed { .. } => {
                (!loaded.state.last_archive.is_empty()).then_some(revision)
            }
            ReviewMutation::UndoLastArchive { .. } => None,
            ReviewMutation::SetDisposition {
                disposition: ReviewDisposition::Reviewed,
                ..
            } => {
                if loaded.state.last_archive.is_empty() {
                    None
                } else if loaded.state.last_archive == previous_last_archive {
                    loaded.last_archive_revision
                } else {
                    return Err(StorageError::InvalidStorage(
                        "review archive metadata changed unexpectedly",
                    ));
                }
            }
        };

        replace_surface_marks(
            &transaction,
            evidence,
            &loaded.state,
            &previous_dispositions,
            &loaded.row_revisions,
            revision,
            self.deadline,
        )?;
        let changed = transaction.execute(
            "UPDATE review_meta
             SET revision = ?1,
                 source_high_water = max(source_high_water, ?2),
                 last_archive_revision = ?3
             WHERE surface = ?4",
            params![
                revision as i64,
                evidence.high_water_i64(),
                last_archive_revision.map(|value| value as i64),
                evidence.surface.as_str(),
            ],
        )?;
        if changed != 1 {
            return Err(StorageError::InvalidStorage(
                "review metadata update count disagrees",
            ));
        }
        verify_counts(
            &transaction,
            evidence.surface,
            &result,
            last_archive_revision,
            self.deadline,
        )?;
        ensure_deadline(self.deadline)?;
        super::activity::commit_before_deadline(
            self.deadline,
            super::StorageOperation::Review,
            || {
                super::maintenance::sqlite_fault("review-commit")?;
                transaction.commit()
            },
        )?;
        Ok(result)
    }
}

fn load_surface(
    connection: &Connection,
    evidence: &ReviewEligibility,
    deadline: Option<StorageDeadline>,
) -> Result<LoadedSurface, StorageError> {
    apply_deadline(connection, deadline)?;
    let meta = connection
        .query_row(
            "SELECT revision, source_high_water, last_archive_revision
             FROM review_meta WHERE surface = ?1",
            [evidence.surface.as_str()],
            |row| {
                Ok((
                    row.get::<_, i64>(0)?,
                    row.get::<_, i64>(1)?,
                    row.get::<_, Option<i64>>(2)?,
                ))
            },
        )
        .optional()?;
    let Some((revision, stored_high_water, last_archive_revision)) = meta else {
        return Err(StorageError::InvalidStorage(
            "review surface metadata is missing",
        ));
    };
    let revision = checked_nonnegative(revision, "review revision is out of range")?;
    let stored_high_water = checked_nonnegative(
        stored_high_water,
        "review source high-water is out of range",
    )?;
    let last_archive_revision = last_archive_revision
        .map(|value| checked_positive(value, "review archive revision is out of range"))
        .transpose()?;
    if last_archive_revision.is_some_and(|value| value > revision) {
        return Err(StorageError::InvalidStorage(
            "review archive revision exceeds surface revision",
        ));
    }
    if evidence.surface == ReviewSurface::Recent && last_archive_revision.is_some() {
        return Err(StorageError::InvalidStorage(
            "Recent contains archive metadata",
        ));
    }
    if evidence.high_water_i64() < stored_high_water as i64 {
        return Err(StorageError::InvalidStorage(
            "review evidence source high-water decreased",
        ));
    }

    let exact = evidence
        .occurrences
        .iter()
        .map(|occurrence| {
            (
                (occurrence.group_id.as_str(), occurrence.source_cursor.get()),
                occurrence.key,
            )
        })
        .collect::<HashMap<_, _>>();
    apply_deadline(connection, deadline)?;
    let mut statement = connection.prepare(
        "SELECT group_id, source_cursor, disposition, revision
         FROM review_marks INDEXED BY sqlite_autoindex_review_marks_1
         WHERE surface = ?1
         ORDER BY group_id, source_cursor
         LIMIT ?2",
    )?;
    let mut rows = statement.query(params![
        evidence.surface.as_str(),
        MAX_REVIEW_KEYS as i64 + 1
    ])?;
    let mut row_count = 0;
    let mut dispositions = BTreeMap::new();
    let mut last_archive = BTreeSet::new();
    let mut row_revisions = BTreeMap::new();
    while let Some(row) = rows.next()? {
        row_count += 1;
        if row_count > MAX_REVIEW_KEYS {
            return Err(StorageError::ReviewCapacityExceeded);
        }
        let group_id = row.get::<_, String>(0)?;
        if group_id.is_empty() || group_id.len() > MAX_GROUP_ID_BYTES || group_id.contains('\0') {
            return Err(StorageError::InvalidStorage(
                "stored review group identity is out of range",
            ));
        }
        let source_cursor = checked_positive(
            row.get::<_, i64>(1)?,
            "stored review cursor is out of range",
        )?;
        if source_cursor > stored_high_water {
            return Err(StorageError::InvalidStorage(
                "stored review cursor exceeds its source high-water",
            ));
        }
        let disposition = parse_disposition(&row.get::<_, String>(2)?)?;
        if evidence.surface == ReviewSurface::Recent && disposition == ReviewDisposition::Archived {
            return Err(StorageError::InvalidStorage(
                "Recent contains archived review state",
            ));
        }
        let row_revision = checked_positive(
            row.get::<_, i64>(3)?,
            "stored review mark revision is out of range",
        )?;
        if row_revision > revision {
            return Err(StorageError::InvalidStorage(
                "stored review mark revision exceeds surface revision",
            ));
        }
        if let Some(key) = exact.get(&(group_id.as_str(), source_cursor)) {
            dispositions.insert(*key, disposition);
            row_revisions.insert(*key, row_revision);
            if disposition == ReviewDisposition::Archived
                && last_archive_revision == Some(row_revision)
            {
                last_archive.insert(*key);
            }
        }
        ensure_deadline(deadline)?;
    }
    ensure_deadline(deadline)?;
    Ok(LoadedSurface {
        state: ReviewSurfaceState {
            surface: evidence.surface,
            surface_revision: revision,
            source_high_water: stored_high_water,
            dispositions,
            last_archive,
        },
        last_archive_revision,
        row_revisions,
    })
}

fn replace_surface_marks(
    connection: &Connection,
    evidence: &ReviewEligibility,
    state: &ReviewSurfaceState,
    previous_dispositions: &BTreeMap<ReviewKey, ReviewDisposition>,
    previous_row_revisions: &BTreeMap<ReviewKey, u64>,
    revision: u64,
    deadline: Option<StorageDeadline>,
) -> Result<(), StorageError> {
    ensure_deadline(deadline)?;
    connection.execute(
        "DELETE FROM review_marks WHERE surface = ?1",
        [evidence.surface.as_str()],
    )?;
    let by_key = evidence
        .occurrences
        .iter()
        .map(|occurrence| (occurrence.key, occurrence))
        .collect::<BTreeMap<_, _>>();
    for (key, disposition) in &state.dispositions {
        ensure_deadline(deadline)?;
        let occurrence = by_key.get(key).ok_or(StorageError::InvalidStorage(
            "review state contains a key outside current evidence",
        ))?;
        let row_revision = if previous_dispositions.get(key) == Some(disposition) {
            previous_row_revisions
                .get(key)
                .copied()
                .ok_or(StorageError::InvalidStorage(
                    "review mark revision is missing",
                ))?
        } else {
            revision
        };
        connection.execute(
            "INSERT INTO review_marks (
                surface, group_id, source_cursor, disposition, revision
             ) VALUES (?1, ?2, ?3, ?4, ?5)",
            params![
                evidence.surface.as_str(),
                occurrence.group_id,
                occurrence.source_cursor.get() as i64,
                disposition_name(*disposition),
                row_revision as i64,
            ],
        )?;
    }
    ensure_deadline(deadline)?;
    Ok(())
}

fn verify_counts(
    connection: &Connection,
    surface: ReviewSurface,
    expected: &ReviewMutationResult,
    last_archive_revision: Option<u64>,
    deadline: Option<StorageDeadline>,
) -> Result<(), StorageError> {
    ensure_deadline(deadline)?;
    let actual = connection.query_row(
        "SELECT
            sum(CASE WHEN disposition = 'reviewed' THEN 1 ELSE 0 END),
            sum(CASE WHEN disposition = 'archived' THEN 1 ELSE 0 END),
            sum(CASE WHEN disposition = 'archived' AND revision = ?1 THEN 1 ELSE 0 END)
         FROM review_marks WHERE surface = ?2",
        params![
            last_archive_revision.map(|value| value as i64),
            surface.as_str()
        ],
        |row| {
            Ok((
                row.get::<_, Option<i64>>(0)?.unwrap_or(0),
                row.get::<_, Option<i64>>(1)?.unwrap_or(0),
                row.get::<_, Option<i64>>(2)?.unwrap_or(0),
            ))
        },
    )?;
    ensure_deadline(deadline)?;
    if actual
        != (
            expected.reviewed_count as i64,
            expected.archived_count as i64,
            expected.last_archive_count as i64,
        )
    {
        return Err(StorageError::InvalidStorage(
            "review mutation counts disagree with persisted rows",
        ));
    }
    Ok(())
}

fn map_review_error(error: ReviewStateError) -> StorageError {
    match error {
        ReviewStateError::InvalidRequest(error) => StorageError::InvalidReviewRequest(error),
        ReviewStateError::StaleRevision => StorageError::StaleReviewRevision,
        ReviewStateError::TargetNotEligible => StorageError::ReviewTargetNotEligible,
        ReviewStateError::CountMismatch => StorageError::ReviewCountMismatch,
        ReviewStateError::DispositionConflict => StorageError::ReviewDispositionConflict,
        ReviewStateError::CapacityExceeded => StorageError::ReviewCapacityExceeded,
        ReviewStateError::RevisionOverflow => StorageError::ReviewRevisionOverflow,
        _ => StorageError::InvalidStorage("pure review mutation failed unexpectedly"),
    }
}

fn parse_disposition(value: &str) -> Result<ReviewDisposition, StorageError> {
    match value {
        "reviewed" => Ok(ReviewDisposition::Reviewed),
        "archived" => Ok(ReviewDisposition::Archived),
        _ => Err(StorageError::InvalidStorage(
            "stored review disposition is unsupported",
        )),
    }
}

fn disposition_name(value: ReviewDisposition) -> &'static str {
    match value {
        ReviewDisposition::Reviewed => "reviewed",
        ReviewDisposition::Archived => "archived",
    }
}

fn checked_nonnegative(value: i64, reason: &'static str) -> Result<u64, StorageError> {
    u64::try_from(value).map_err(|_| StorageError::InvalidStorage(reason))
}

fn checked_positive(value: i64, reason: &'static str) -> Result<u64, StorageError> {
    let value = checked_nonnegative(value, reason)?;
    (value != 0)
        .then_some(value)
        .ok_or(StorageError::InvalidStorage(reason))
}

fn apply_deadline(
    connection: &Connection,
    deadline: Option<StorageDeadline>,
) -> Result<(), StorageError> {
    if let Some(deadline) = deadline {
        deadline.apply(connection)?;
    }
    Ok(())
}

fn ensure_deadline(deadline: Option<StorageDeadline>) -> Result<(), StorageError> {
    if let Some(deadline) = deadline {
        deadline.ensure_remaining()?;
    }
    Ok(())
}
