use std::fmt;

use thiserror::Error;
use uuid::Uuid;

/// A validated GitHub webhook delivery UUID.
///
/// The identifier is stored as a UUID rather than untrusted header text. Persistence code encodes
/// it in normalized lowercase hyphenated form without allocating a temporary string.
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct DeliveryId(Uuid);

impl DeliveryId {
    /// Parses a GitHub delivery identifier as a UUID.
    ///
    /// # Errors
    ///
    /// Returns [`DeliveryIdError`] when `value` is not a valid UUID representation.
    pub fn parse(value: &str) -> Result<Self, DeliveryIdError> {
        Uuid::parse_str(value)
            .map(Self)
            .map_err(|_| DeliveryIdError)
    }

    pub(crate) fn encode_lower<'buffer>(&self, buffer: &'buffer mut [u8]) -> &'buffer str {
        self.0.as_hyphenated().encode_lower(buffer)
    }
}

impl fmt::Debug for DeliveryId {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str("DeliveryId([REDACTED])")
    }
}

/// A malformed GitHub webhook delivery identifier.
#[derive(Clone, Copy, Debug, Error, PartialEq, Eq)]
#[error("delivery identifier is not a valid UUID")]
pub struct DeliveryIdError;

#[cfg(test)]
mod tests {
    use super::DeliveryId;

    const CANONICAL_DELIVERY_ID: &str = "550e8400-e29b-41d4-a716-446655440000";

    #[test]
    fn delivery_id_parses_and_normalizes_valid_uuid() {
        let delivery_id =
            DeliveryId::parse(CANONICAL_DELIVERY_ID).expect("canonical delivery UUID is valid");
        let mut buffer = uuid::Uuid::encode_buffer();

        assert_eq!(delivery_id.encode_lower(&mut buffer), CANONICAL_DELIVERY_ID);
    }

    #[test]
    fn delivery_id_debug_output_is_redacted() {
        let delivery_id =
            DeliveryId::parse(CANONICAL_DELIVERY_ID).expect("canonical delivery UUID is valid");

        let rendered = format!("{delivery_id:?}");

        assert!(!rendered.contains(CANONICAL_DELIVERY_ID));
    }

    #[test]
    fn delivery_id_rejects_malformed_uuid_values() {
        for invalid in [
            "not-a-uuid",
            "550e8400-e29b-41d4-a716-44665544000",
            "550e8400-e29b-41d4-a716-4466554400000",
        ] {
            assert!(DeliveryId::parse(invalid).is_err(), "accepted {invalid}");
        }
    }
}
