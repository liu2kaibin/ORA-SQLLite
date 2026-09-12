use std::num::NonZeroU16;

use crate::{DbError, ErrorCode, Result};

use super::SqlSpan;

pub(crate) const MAX_IDENTIFIER_BYTES: usize = 128;

/// An owned SQL identifier normalized according to Oracle's quoting rules.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) struct Identifier {
    normalized: String,
    quoted: bool,
    span: SqlSpan,
}

impl Identifier {
    pub(crate) fn unquoted(value: impl AsRef<str>, span: SqlSpan) -> Result<Self> {
        Ok(Self {
            normalized: normalize_unquoted(value.as_ref())?,
            quoted: false,
            span,
        })
    }

    pub(crate) fn quoted(value: impl AsRef<str>, span: SqlSpan) -> Result<Self> {
        let normalized = value.as_ref();
        validate_identifier_length(normalized)?;
        Ok(Self {
            normalized: normalized.to_owned(),
            quoted: true,
            span,
        })
    }

    pub(crate) fn as_str(&self) -> &str {
        &self.normalized
    }

    pub(crate) const fn is_quoted(&self) -> bool {
        self.quoted
    }

    pub(crate) const fn span(&self) -> SqlSpan {
        self.span
    }
}

/// The only parameter styles supported by the Phase 4B frontend.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub(crate) enum ParameterStyle {
    Positional,
    Named,
}

/// A normalized, owned Oracle-style bind parameter key.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub(crate) enum ParameterKey {
    Position(NonZeroU16),
    Name(String),
}

impl ParameterKey {
    pub(crate) fn parse(value: &str, _span: SqlSpan) -> Result<Self> {
        let Some(body) = value.strip_prefix(':') else {
            return Err(invalid_parameter("parameter must begin with a colon"));
        };
        if body.is_empty() || body.starts_with('"') {
            return Err(invalid_parameter(
                "parameter name must be an unquoted identifier",
            ));
        }

        if body.bytes().all(|byte| byte.is_ascii_digit()) {
            if body.starts_with('0') {
                return Err(invalid_parameter(
                    "positional parameter must not start with zero",
                ));
            }
            let position = body
                .parse::<u16>()
                .ok()
                .and_then(NonZeroU16::new)
                .ok_or_else(|| invalid_parameter("positional parameter is outside 1..=65535"))?;
            return Ok(Self::Position(position));
        }

        if !is_unquoted_identifier(body) {
            return Err(invalid_parameter(
                "parameter name must be an unquoted identifier",
            ));
        }
        Ok(Self::Name(normalize_unquoted(body)?))
    }

    pub(crate) const fn style(&self) -> ParameterStyle {
        match self {
            Self::Position(_) => ParameterStyle::Positional,
            Self::Name(_) => ParameterStyle::Named,
        }
    }

    pub(crate) fn name(&self) -> Option<&str> {
        match self {
            Self::Position(_) => None,
            Self::Name(name) => Some(name),
        }
    }
}

fn normalize_unquoted(value: &str) -> Result<String> {
    validate_identifier_length(value)?;
    Ok(value.to_ascii_uppercase())
}

fn validate_identifier_length(value: &str) -> Result<()> {
    if value.is_empty() || value.len() > MAX_IDENTIFIER_BYTES {
        return Err(DbError::new(
            ErrorCode::Bind,
            "identifier must contain 1 through 128 UTF-8 bytes",
        ));
    }
    Ok(())
}

fn is_unquoted_identifier(value: &str) -> bool {
    let mut chars = value.chars();
    let Some(first) = chars.next() else {
        return false;
    };
    if !is_identifier_start(first) {
        return false;
    }
    chars.all(is_identifier_continue)
}

fn is_identifier_start(character: char) -> bool {
    character.is_ascii_alphabetic() || character == '_' || !character.is_ascii()
}

fn is_identifier_continue(character: char) -> bool {
    is_identifier_start(character) || character.is_ascii_digit() || matches!(character, '$' | '#')
}

fn invalid_parameter(message: &str) -> DbError {
    DbError::new(ErrorCode::Bind, message)
}

#[cfg(test)]
mod tests {
    use super::{Identifier, ParameterKey, ParameterStyle};
    use crate::{ErrorCode, sql::SqlSpan};
    use std::num::NonZeroU16;

    #[test]
    fn oracle_identifier_normalization_is_byte_bounded() {
        fn span() -> SqlSpan {
            SqlSpan::new(0, 2).unwrap()
        }

        assert_eq!(
            Identifier::unquoted("users", span()).unwrap().as_str(),
            "USERS"
        );
        assert_eq!(
            Identifier::quoted("Users", span()).unwrap().as_str(),
            "Users"
        );
        assert!(Identifier::unquoted("a".repeat(127), span()).is_ok());
        assert!(Identifier::unquoted("a".repeat(128), span()).is_ok());
        assert_eq!(
            Identifier::unquoted("a".repeat(129), span())
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
        assert!(Identifier::quoted("界".repeat(42), span()).is_ok());
        assert_eq!(
            Identifier::quoted("界".repeat(43), span())
                .unwrap_err()
                .code(),
            ErrorCode::Bind
        );
    }

    #[test]
    fn positional_parameter_keys_reject_zero_leading_zero_and_overflow() {
        fn span() -> SqlSpan {
            SqlSpan::new(0, 2).unwrap()
        }

        assert!(ParameterKey::parse(":1", span()).is_ok());
        assert_eq!(
            ParameterKey::parse(":65535", span()).unwrap(),
            ParameterKey::Position(NonZeroU16::new(65_535).unwrap())
        );
        for invalid in [":0", ":01", ":65536"] {
            assert_eq!(
                ParameterKey::parse(invalid, span()).unwrap_err().code(),
                ErrorCode::Bind
            );
        }
    }

    #[test]
    fn identifier_and_named_parameter_normalization_are_ascii_only() {
        let span = SqlSpan::new(0, 1).unwrap();
        let name = Identifier::unquoted("i\u{307}x", span).unwrap();
        assert_eq!(name.as_str(), "I\u{307}X");
        assert!(!name.is_quoted());
        assert_eq!(name.span(), span);
        assert_eq!(
            ParameterKey::parse(":mixed_Name", span).unwrap().style(),
            ParameterStyle::Named
        );
        assert_eq!(
            ParameterKey::parse(":mixed_Name", span).unwrap().name(),
            Some("MIXED_NAME")
        );
        assert_eq!(
            ParameterKey::parse(":\"named\"", span).unwrap_err().code(),
            ErrorCode::Bind
        );
    }
}
