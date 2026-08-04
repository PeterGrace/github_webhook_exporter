use crate::security::{CanonicalRepositoryName, RepositorySecret};

/// A positive SQLite repository row identifier.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
pub struct RepositoryId(i64);

impl RepositoryId {
    /// Constructs an identifier from a positive SQLite integer.
    ///
    /// Returns `None` when `value` is zero or negative.
    pub fn new(value: i64) -> Option<Self> {
        (value > 0).then_some(Self(value))
    }

    /// Returns the underlying SQLite integer.
    pub fn get(self) -> i64 {
        self.0
    }

    pub(crate) fn from_database(value: i64) -> Option<Self> {
        Self::new(value)
    }
}

/// An RFC 3339 UTC timestamp emitted by SQLite.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryTimestamp(String);

impl RepositoryTimestamp {
    /// Returns the persisted UTC timestamp text.
    pub fn as_str(&self) -> &str {
        &self.0
    }

    pub(crate) fn from_database(value: String) -> Self {
        Self(value)
    }
}

/// Public repository configuration fields that contain no secret material.
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct RepositoryMetadata {
    id: RepositoryId,
    full_name: CanonicalRepositoryName,
    enabled: bool,
    created_at: RepositoryTimestamp,
    updated_at: RepositoryTimestamp,
}

impl RepositoryMetadata {
    /// Returns the database identifier.
    pub fn id(&self) -> RepositoryId {
        self.id
    }

    /// Returns the canonical lowercase `owner/repository` name.
    pub fn full_name(&self) -> &str {
        self.full_name.as_str()
    }

    /// Returns whether webhook processing is enabled.
    pub fn enabled(&self) -> bool {
        self.enabled
    }

    /// Returns the creation timestamp.
    pub fn created_at(&self) -> &RepositoryTimestamp {
        &self.created_at
    }

    /// Returns the most recent mutation timestamp.
    pub fn updated_at(&self) -> &RepositoryTimestamp {
        &self.updated_at
    }

    pub(crate) fn from_database(
        id: RepositoryId,
        full_name: CanonicalRepositoryName,
        enabled: bool,
        created_at: String,
        updated_at: String,
    ) -> Self {
        Self {
            id,
            full_name,
            enabled,
            created_at: RepositoryTimestamp::from_database(created_at),
            updated_at: RepositoryTimestamp::from_database(updated_at),
        }
    }
}

/// Optional repository fields to change in one atomic store operation.
#[derive(Default)]
pub struct RepositoryMutation {
    pub(crate) full_name: Option<CanonicalRepositoryName>,
    pub(crate) webhook_secret: Option<RepositorySecret>,
    pub(crate) enabled: Option<bool>,
}

impl RepositoryMutation {
    /// Creates an empty mutation to populate through builder methods.
    pub fn new() -> Self {
        Self::default()
    }

    /// Sets the replacement canonical repository name.
    pub fn with_full_name(mut self, full_name: CanonicalRepositoryName) -> Self {
        self.full_name = Some(full_name);
        self
    }

    /// Sets a replacement plaintext secret for immediate encryption.
    pub fn with_webhook_secret(mut self, webhook_secret: RepositorySecret) -> Self {
        self.webhook_secret = Some(webhook_secret);
        self
    }

    /// Sets the replacement enabled state.
    pub fn with_enabled(mut self, enabled: bool) -> Self {
        self.enabled = Some(enabled);
        self
    }

    pub(crate) fn is_empty(&self) -> bool {
        self.full_name.is_none() && self.webhook_secret.is_none() && self.enabled.is_none()
    }
}

#[cfg(test)]
mod tests {
    use super::RepositoryId;

    #[test]
    fn repository_ids_are_positive_sqlite_integers() {
        assert_eq!(RepositoryId::new(1).map(RepositoryId::get), Some(1));
        assert_eq!(RepositoryId::new(0), None);
        assert_eq!(RepositoryId::new(-1), None);
    }
}
