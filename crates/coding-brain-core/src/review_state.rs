use std::collections::BTreeSet;
use std::fmt;

use serde::de::{self, Visitor};
use serde::{Deserialize, Deserializer, Serialize, Serializer};
use sha2::{Digest, Sha256};

pub const MAX_REVIEW_KEYS: usize = 10_000;
pub const MAX_REVIEW_STATE_BYTES: usize = 8 * 1024 * 1024;

const REVIEW_KEY_DOMAIN: &[u8] = b"review-item:v1";

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewSurface {
    Attention,
    Review,
    Diagnostics,
    Recent,
}

impl ReviewSurface {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Attention => "attention",
            Self::Review => "review",
            Self::Diagnostics => "diagnostics",
            Self::Recent => "recent",
        }
    }

    pub fn supports_archive(self) -> bool {
        self != Self::Recent
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ReviewDisposition {
    Reviewed,
    Archived,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord, Hash)]
pub struct ReviewKey([u8; 32]);

impl ReviewKey {
    pub fn derive(surface: ReviewSurface, source_identity: &[u8]) -> Self {
        let mut hash = Sha256::new();
        hash.update(REVIEW_KEY_DOMAIN);
        hash.update(surface.as_str().as_bytes());
        hash.update((source_identity.len() as u64).to_be_bytes());
        hash.update(source_identity);
        Self(hash.finalize().into())
    }

    pub fn as_bytes(&self) -> &[u8; 32] {
        &self.0
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewKeySetError {
    DuplicateKey,
}

pub fn derive_review_key_set<I, S>(
    surface: ReviewSurface,
    source_identities: I,
) -> Result<BTreeSet<ReviewKey>, ReviewKeySetError>
where
    I: IntoIterator<Item = S>,
    S: AsRef<[u8]>,
{
    let mut keys = BTreeSet::new();
    for identity in source_identities {
        if !keys.insert(ReviewKey::derive(surface, identity.as_ref())) {
            return Err(ReviewKeySetError::DuplicateKey);
        }
    }
    Ok(keys)
}

impl fmt::Display for ReviewKey {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        for byte in self.0 {
            write!(formatter, "{byte:02x}")?;
        }
        Ok(())
    }
}

impl Serialize for ReviewKey {
    fn serialize<S>(&self, serializer: S) -> Result<S::Ok, S::Error>
    where
        S: Serializer,
    {
        serializer.serialize_str(&self.to_string())
    }
}

impl<'de> Deserialize<'de> for ReviewKey {
    fn deserialize<D>(deserializer: D) -> Result<Self, D::Error>
    where
        D: Deserializer<'de>,
    {
        deserializer.deserialize_str(ReviewKeyVisitor)
    }
}

struct ReviewKeyVisitor;

impl Visitor<'_> for ReviewKeyVisitor {
    type Value = ReviewKey;

    fn expecting(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("exactly 64 lowercase hexadecimal characters")
    }

    fn visit_str<E>(self, value: &str) -> Result<Self::Value, E>
    where
        E: de::Error,
    {
        let bytes = value.as_bytes();
        if bytes.len() != 64 || !bytes.iter().all(u8::is_ascii_hexdigit) {
            return Err(E::invalid_value(de::Unexpected::Str(value), &self));
        }
        if bytes.iter().any(u8::is_ascii_uppercase) {
            return Err(E::invalid_value(de::Unexpected::Str(value), &self));
        }
        let mut key = [0_u8; 32];
        for (index, pair) in bytes.chunks_exact(2).enumerate() {
            key[index] = (hex_nibble(pair[0]) << 4) | hex_nibble(pair[1]);
        }
        Ok(ReviewKey(key))
    }
}

fn hex_nibble(byte: u8) -> u8 {
    match byte {
        b'0'..=b'9' => byte - b'0',
        b'a'..=b'f' => byte - b'a' + 10,
        _ => unreachable!("review key validation accepts only lowercase hexadecimal"),
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewTarget {
    pub surface: ReviewSurface,
    pub display_id: String,
    pub new_member_keys: Vec<ReviewKey>,
    pub reviewed_member_keys: Vec<ReviewKey>,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct SurfaceReviewProjection {
    pub revision: u64,
    pub items: Vec<ReviewTarget>,
    pub new_count: usize,
    pub reviewed_count: usize,
    pub last_archive_count: usize,
}

#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct BrainReviewProjection {
    pub attention: SurfaceReviewProjection,
    pub review: SurfaceReviewProjection,
    pub diagnostics: SurfaceReviewProjection,
    pub recent: SurfaceReviewProjection,
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub enum ReviewMutation {
    SetDisposition {
        keys: BTreeSet<ReviewKey>,
        disposition: ReviewDisposition,
    },
    ArchiveAllReviewed {
        expected_count: usize,
    },
    UndoLastArchive {
        expected_count: usize,
    },
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewMutationRequest {
    pub surface: ReviewSurface,
    pub expected_surface_revision: u64,
    pub operation: ReviewMutation,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ReviewRequestError {
    InvalidKeyCount,
    InvalidExpectedCount,
    UnsupportedOperation,
}

impl ReviewMutationRequest {
    pub fn validate(&self) -> Result<(), ReviewRequestError> {
        match &self.operation {
            ReviewMutation::SetDisposition { keys, disposition } => {
                if keys.is_empty() || keys.len() > MAX_REVIEW_KEYS {
                    return Err(ReviewRequestError::InvalidKeyCount);
                }
                if self.surface == ReviewSurface::Recent
                    && *disposition == ReviewDisposition::Archived
                {
                    return Err(ReviewRequestError::UnsupportedOperation);
                }
            }
            ReviewMutation::ArchiveAllReviewed { expected_count }
            | ReviewMutation::UndoLastArchive { expected_count } => {
                if !self.surface.supports_archive() {
                    return Err(ReviewRequestError::UnsupportedOperation);
                }
                if *expected_count > MAX_REVIEW_KEYS {
                    return Err(ReviewRequestError::InvalidExpectedCount);
                }
            }
        }
        Ok(())
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReviewMutationResult {
    pub surface: ReviewSurface,
    pub surface_revision: u64,
    pub reviewed_count: usize,
    pub archived_count: usize,
    pub last_archive_count: usize,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn review_keys_are_surface_separated_and_fixed_width() {
        let attention = ReviewKey::derive(ReviewSurface::Attention, b"activity-1");
        let recent = ReviewKey::derive(ReviewSurface::Recent, b"activity-1");
        assert_ne!(attention, recent);
        assert_eq!(attention.to_string().len(), 64);
        assert_eq!(
            attention,
            ReviewKey::derive(ReviewSurface::Attention, b"activity-1")
        );
    }

    #[test]
    fn review_key_sets_reject_duplicate_source_identities() {
        assert_eq!(
            derive_review_key_set(
                ReviewSurface::Attention,
                [b"activity-1".as_slice(), b"activity-1".as_slice()],
            ),
            Err(ReviewKeySetError::DuplicateKey)
        );
    }

    #[test]
    fn review_key_sets_remain_surface_separated() {
        let attention =
            derive_review_key_set(ReviewSurface::Attention, [b"activity-1".as_slice()]).unwrap();
        let recent =
            derive_review_key_set(ReviewSurface::Recent, [b"activity-1".as_slice()]).unwrap();

        assert!(attention.is_disjoint(&recent));
    }

    #[test]
    fn recent_rejects_archive_operations() {
        let request = ReviewMutationRequest {
            surface: ReviewSurface::Recent,
            expected_surface_revision: 0,
            operation: ReviewMutation::SetDisposition {
                keys: [ReviewKey::derive(ReviewSurface::Recent, b"recent-1")]
                    .into_iter()
                    .collect(),
                disposition: ReviewDisposition::Archived,
            },
        };
        assert_eq!(
            request.validate(),
            Err(ReviewRequestError::UnsupportedOperation)
        );
    }

    #[test]
    fn review_key_serde_rejects_noncanonical_strings() {
        let key = ReviewKey::derive(ReviewSurface::Review, b"decision-1");
        let encoded = serde_json::to_string(&key).unwrap();
        assert_eq!(serde_json::from_str::<ReviewKey>(&encoded).unwrap(), key);
        assert!(
            serde_json::from_str::<ReviewKey>(&format!("\"{}\"", key.to_string().to_uppercase()))
                .is_err()
        );
        assert!(serde_json::from_str::<ReviewKey>("\"abc\"").is_err());
        assert!(
            serde_json::from_str::<ReviewKey>(&format!("\"{}g\"", &key.to_string()[1..])).is_err()
        );
    }

    #[test]
    fn set_disposition_rejects_empty_and_oversized_requests() {
        let empty = ReviewMutationRequest {
            surface: ReviewSurface::Attention,
            expected_surface_revision: 0,
            operation: ReviewMutation::SetDisposition {
                keys: Default::default(),
                disposition: ReviewDisposition::Reviewed,
            },
        };
        assert_eq!(empty.validate(), Err(ReviewRequestError::InvalidKeyCount));

        let keys = (0..=MAX_REVIEW_KEYS)
            .map(|index| ReviewKey::derive(ReviewSurface::Attention, &index.to_be_bytes()))
            .collect();
        let oversized = ReviewMutationRequest {
            surface: ReviewSurface::Attention,
            expected_surface_revision: 0,
            operation: ReviewMutation::SetDisposition {
                keys,
                disposition: ReviewDisposition::Reviewed,
            },
        };
        assert_eq!(
            oversized.validate(),
            Err(ReviewRequestError::InvalidKeyCount)
        );
    }
}
