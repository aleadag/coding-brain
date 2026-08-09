use std::collections::HashSet;
use std::fmt;
use std::path::Path;

use coding_brain_core::brain_activity::{
    ACTIVITY_SCHEMA_VERSION, ActivityEvent, ActivityState, MAX_ACTIVITY_EVENT_BYTES,
};
use coding_brain_core::lifecycle::{LifecycleIdentity, MAX_ID_BYTES, PermissionDisposition};
use serde::de::{self, MapAccess, SeqAccess, Visitor};
use serde::{Deserialize, Serialize};

use crate::brain::decisions::{HookDecisionRecord, validate_hook_decision_record};

const JOURNAL_SCHEMA_VERSION: u32 = 2;
const MAX_JOURNAL_BYTES: usize = 1024 * 1024;

#[derive(Clone, Debug, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PermissionTransactionJournal {
    pub schema_version: u32,
    pub transaction_id: String,
    pub proposal: HookDecisionRecord,
    pub terminal: ActivityEvent,
    pub lifecycle_identity: LifecycleIdentity,
    pub request_key: String,
    pub disposition: PermissionDisposition,
    pub allow_requires_lifecycle_authority: bool,
}

pub(crate) fn validate_journal(journal: &PermissionTransactionJournal) -> Result<(), ()> {
    if !matches!(journal.schema_version, 1 | JOURNAL_SCHEMA_VERSION)
        || !valid_id(&journal.transaction_id)
        || !validate_hook_decision_record(&journal.proposal)
        || !valid_id(&journal.terminal.activity_id)
        || journal.request_key.len() != 64
        || !journal
            .request_key
            .bytes()
            .all(|byte| byte.is_ascii_hexdigit() && !byte.is_ascii_uppercase())
        || journal.terminal.schema_version != ACTIVITY_SCHEMA_VERSION
        || !journal.terminal.state.is_terminal()
        || !journal.terminal.has_consistent_payload()
        || journal.terminal.clone().normalized() != journal.terminal
        || serde_json::to_vec(&journal.terminal).map_or(true, |serialized| {
            serialized.len() > MAX_ACTIVITY_EVENT_BYTES
        })
        || serde_json::to_vec(&journal.proposal)
            .map_or(true, |serialized| serialized.len() > MAX_JOURNAL_BYTES)
    {
        return Err(());
    }

    let Some(session) = journal.terminal.session.as_ref() else {
        return Err(());
    };
    if journal.proposal.provider != journal.lifecycle_identity.provider()
        || session.provider != journal.lifecycle_identity.provider()
        || journal.proposal.session_id != journal.lifecycle_identity.session_id()
        || session.session_id != journal.lifecycle_identity.session_id()
        || session.provider_session_id.as_deref()
            != journal.lifecycle_identity.provider_session_id()
        || journal.proposal.turn_id != journal.lifecycle_identity.turn_id().unwrap_or_default()
        || session.turn_id.as_deref() != journal.lifecycle_identity.turn_id()
        || session.cwd != journal.lifecycle_identity.cwd()
        || journal.terminal.project.cwd != journal.lifecycle_identity.cwd()
        || session.project_id != journal.terminal.project.project_id
        || journal.terminal.project.label.as_deref() != Some(journal.proposal.project.as_str())
        || journal.terminal.tool.as_deref() != Some(journal.proposal.tool.as_str())
        || journal.terminal.decision_id.as_deref() != Some(journal.proposal.decision_id.as_str())
        || !action_matches_terminal(&journal.proposal.brain_action, journal.terminal.state)
        || journal.allow_requires_lifecycle_authority
            != (journal.terminal.state == ActivityState::Allowed)
        || !matches!(
            (journal.terminal.state, journal.disposition),
            (
                ActivityState::Allowed | ActivityState::Denied,
                PermissionDisposition::Decided
            ) | (
                ActivityState::Abstained | ActivityState::Error,
                PermissionDisposition::NeedsInput
            )
        )
    {
        return Err(());
    }

    let rebuilt_identity = LifecycleIdentity::try_new_with_provider_session(
        journal.lifecycle_identity.provider(),
        journal.lifecycle_identity.session_id().to_owned(),
        journal
            .lifecycle_identity
            .provider_session_id()
            .map(str::to_owned),
        journal.lifecycle_identity.turn_id().map(str::to_owned),
        journal
            .lifecycle_identity
            .transcript_path()
            .map(Path::to_owned),
        journal.lifecycle_identity.cwd().to_owned(),
    )
    .map_err(|_| ())?;
    (rebuilt_identity == journal.lifecycle_identity)
        .then_some(())
        .ok_or(())
}

fn valid_id(value: &str) -> bool {
    !value.is_empty() && value.len() <= MAX_ID_BYTES && !value.chars().any(char::is_control)
}

fn action_matches_terminal(action: &str, state: ActivityState) -> bool {
    matches!(
        (action, state),
        ("approve", ActivityState::Allowed)
            | ("deny", ActivityState::Denied)
            | ("approve" | "deny" | "abstain", ActivityState::Abstained)
            | ("abstain", ActivityState::Error)
    )
}

pub(crate) fn decode_exact_journal(bytes: &[u8]) -> Option<PermissionTransactionJournal> {
    let raw_numbers = serde_json::from_slice::<RawJournalNumbers<'_>>(bytes).ok()?;
    if !raw_numbers.are_lossless() {
        return None;
    }
    let encoded = decode_exact_json(bytes)?;
    let journal: PermissionTransactionJournal = serde_json::from_value(encoded.clone()).ok()?;
    (serde_json::to_value(&journal).ok()? == encoded).then_some(journal)
}

pub(crate) fn decode_exact_json(bytes: &[u8]) -> Option<serde_json::Value> {
    let mut deserializer = serde_json::Deserializer::from_slice(bytes);
    let UniqueJsonValue(value) = UniqueJsonValue::deserialize(&mut deserializer).ok()?;
    deserializer.end().ok()?;
    Some(value)
}

#[derive(Deserialize)]
struct RawJournalNumbers<'a> {
    #[serde(borrow)]
    proposal: RawProposalNumbers<'a>,
    #[serde(borrow)]
    terminal: Option<RawTerminalNumbers<'a>>,
}

#[derive(Deserialize)]
struct RawProposalNumbers<'a> {
    #[serde(borrow)]
    brain_confidence: &'a serde_json::value::RawValue,
    #[serde(borrow)]
    brain_threshold: Option<&'a serde_json::value::RawValue>,
}

pub(crate) fn hook_decision_numbers_are_lossless(bytes: &[u8]) -> bool {
    serde_json::from_slice::<RawProposalNumbers<'_>>(bytes)
        .is_ok_and(|numbers| numbers.are_lossless())
}

impl RawProposalNumbers<'_> {
    fn are_lossless(&self) -> bool {
        lossless_f64_token(self.brain_confidence)
            && self.brain_threshold.is_none_or(lossless_f64_token)
    }
}

#[derive(Deserialize)]
struct RawTerminalNumbers<'a> {
    #[serde(borrow)]
    confidence: Option<&'a serde_json::value::RawValue>,
    #[serde(borrow)]
    threshold: Option<&'a serde_json::value::RawValue>,
}

impl RawJournalNumbers<'_> {
    fn are_lossless(&self) -> bool {
        self.proposal.are_lossless()
            && self.terminal.as_ref().is_none_or(|terminal| {
                terminal.confidence.is_none_or(lossless_f64_token)
                    && terminal.threshold.is_none_or(lossless_f64_token)
            })
    }
}

fn lossless_f64_token(token: &serde_json::value::RawValue) -> bool {
    let token = token.get();
    let Ok(value) = token.parse::<f64>() else {
        return false;
    };
    let Some(round_trip) = serde_json::Number::from_f64(value) else {
        return false;
    };
    normalized_decimal(token) == normalized_decimal(&round_trip.to_string())
}

fn normalized_decimal(value: &str) -> Option<(bool, String, i64)> {
    let (negative, unsigned) = value
        .strip_prefix('-')
        .map_or((false, value), |unsigned| (true, unsigned));
    let exponent_index = unsigned.find(['e', 'E']);
    let (mantissa, explicit_exponent) = match exponent_index {
        Some(index) => (
            &unsigned[..index],
            unsigned[index + 1..].parse::<i64>().ok()?,
        ),
        None => (unsigned, 0),
    };
    let (integer, fraction) = mantissa.split_once('.').unwrap_or((mantissa, ""));
    let fraction_len = i64::try_from(fraction.len()).ok()?;
    let mut digits = String::with_capacity(integer.len().checked_add(fraction.len())?);
    digits.push_str(integer);
    digits.push_str(fraction);
    if !digits.bytes().all(|byte| byte.is_ascii_digit()) {
        return None;
    }
    let Some(first_nonzero) = digits.bytes().position(|byte| byte != b'0') else {
        return Some((false, String::new(), 0));
    };
    let last_nonzero = digits.bytes().rposition(|byte| byte != b'0')?;
    let trailing_zeroes = i64::try_from(digits.len().checked_sub(last_nonzero + 1)?).ok()?;
    let exponent = explicit_exponent
        .checked_sub(fraction_len)?
        .checked_add(trailing_zeroes)?;
    Some((
        negative,
        digits[first_nonzero..=last_nonzero].to_owned(),
        exponent,
    ))
}

struct UniqueJsonValue(serde_json::Value);

impl<'de> Deserialize<'de> for UniqueJsonValue {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: serde::Deserializer<'de>,
    {
        deserializer.deserialize_any(UniqueJsonValueVisitor)
    }
}

struct UniqueJsonValueVisitor;

impl<'de> Visitor<'de> for UniqueJsonValueVisitor {
    type Value = UniqueJsonValue;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("a JSON value without duplicate object keys")
    }

    fn visit_bool<E>(self, value: bool) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_i64<E>(self, value: i64) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_u64<E>(self, value: u64) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_f64<E>(self, value: f64) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        serde_json::Number::from_f64(value)
            .map(serde_json::Value::Number)
            .map(UniqueJsonValue)
            .ok_or_else(|| E::custom("non-finite JSON number"))
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_string<E>(self, value: String) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(value.into()))
    }

    fn visit_unit<E>(self) -> Result<Self::Value, E> {
        Ok(UniqueJsonValue(serde_json::Value::Null))
    }

    fn visit_seq<A>(self, mut sequence: A) -> Result<Self::Value, A::Error>
    where
        A: SeqAccess<'de>,
    {
        let mut values = Vec::new();
        while let Some(UniqueJsonValue(value)) = sequence.next_element()? {
            values.push(value);
        }
        Ok(UniqueJsonValue(serde_json::Value::Array(values)))
    }

    fn visit_map<A>(self, mut object: A) -> Result<Self::Value, A::Error>
    where
        A: MapAccess<'de>,
    {
        let mut keys = HashSet::new();
        let mut values = serde_json::Map::new();
        while let Some(key) = object.next_key::<String>()? {
            if !keys.insert(key.clone()) {
                return Err(de::Error::custom("duplicate JSON object key"));
            }
            let UniqueJsonValue(value) = object.next_value()?;
            values.insert(key, value);
        }
        Ok(UniqueJsonValue(serde_json::Value::Object(values)))
    }
}
