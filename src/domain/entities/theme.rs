//! Theme identity is independent of the filesystem and HTTP representation.
use crate::common::errors::DomainError;

#[derive(Debug, Clone, PartialEq, Eq, PartialOrd, Ord)]
pub struct ThemeId(String);

impl ThemeId {
    pub fn parse(raw: &str) -> Result<Self, DomainError> {
        if raw.is_empty()
            || raw.len() > 64
            || !raw.as_bytes()[0].is_ascii_alphanumeric()
            || !raw
                .bytes()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == b'-')
        {
            return Err(DomainError::validation_error("Invalid theme identifier"));
        }
        Ok(Self(raw.to_owned()))
    }
    pub fn as_str(&self) -> &str {
        &self.0
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    #[test]
    fn theme_identifiers_are_path_independent() {
        for id in ["default", "sober", "a-2", "0"] {
            assert!(ThemeId::parse(id).is_ok());
        }
        for id in ["", "../a", ".", "A", "a/b", "a\\b", "-a", "é"] {
            assert!(ThemeId::parse(id).is_err());
        }
        assert!(ThemeId::parse(&"a".repeat(64)).is_ok());
        assert!(ThemeId::parse(&"a".repeat(65)).is_err());
    }
}
