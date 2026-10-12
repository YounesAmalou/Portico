use std::{fmt, str::FromStr};

use thiserror::Error;

#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Host {
    alias: String,
    hostname: String,
    user: Option<String>,
    port: Option<u16>,
    identity_file: Option<String>,
    proxy_jump: Option<String>,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub enum Change<T> {
    #[default]
    Keep,
    Set(T),
    Clear,
}

#[derive(Clone, Debug, Default, Eq, PartialEq)]
pub struct HostChanges {
    pub alias: Change<String>,
    pub hostname: Change<String>,
    pub user: Change<String>,
    pub port: Change<u16>,
    pub identity_file: Change<String>,
    pub proxy_jump: Change<String>,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum ClearField {
    User,
    Port,
    Identity,
    Jump,
}

/// A user-editable host field, as validated before it reaches the managed fragment.
#[derive(Clone, Copy, Debug, Eq, PartialEq)]
pub enum HostField {
    Alias,
    Hostname,
    User,
    Port,
    IdentityFile,
    ProxyJump,
}

impl fmt::Display for HostField {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        formatter.write_str(match self {
            Self::Alias => "alias",
            Self::Hostname => "hostname",
            Self::User => "user",
            Self::Port => "port",
            Self::IdentityFile => "identity file",
            Self::ProxyJump => "proxy jump",
        })
    }
}

impl HostField {
    /// Checks the text of one field, so forms and the CLI report exactly what `Host::new` would.
    ///
    /// Empty optional fields are the caller's concern: an omitted field is `None`, never `""`.
    /// ProxyJump is only checked for shape; OpenSSH validates its destination grammar.
    pub fn validate(self, value: &str) -> Result<(), HostError> {
        match self.rejection(value) {
            Some(reason) => Err(HostError::Invalid {
                field: self,
                reason,
            }),
            None => Ok(()),
        }
    }

    fn rejection(self, value: &str) -> Option<&'static str> {
        match self {
            Self::Alias => (!is_alias(value)).then_some(
                "must start with a letter or digit and contain only letters, digits, '.', '_' or '-'",
            ),
            Self::Hostname | Self::User => single_value_rejection(value),
            Self::Port => parse_port_text(value).err(),
            Self::IdentityFile => {
                if value.trim().is_empty() {
                    Some("must not be blank")
                } else if value.chars().any(char::is_control) {
                    Some("must not contain control characters")
                } else if value.trim() != value {
                    Some("must not start or end with whitespace")
                } else {
                    None
                }
            }
            Self::ProxyJump => {
                if value.trim().is_empty() {
                    Some("must not be blank")
                } else if value.chars().any(char::is_control) {
                    Some("must not contain control characters")
                } else if value.chars().any(char::is_whitespace) {
                    Some("must not contain whitespace")
                } else {
                    None
                }
            }
        }
    }
}

/// Parses port text exactly as the host model accepts it: digits only, 1 through 65535.
pub fn parse_port(value: &str) -> Result<u16, HostError> {
    parse_port_text(value).map_err(|reason| HostError::Invalid {
        field: HostField::Port,
        reason,
    })
}

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HostError {
    #[error("invalid {field}: {reason}")]
    Invalid {
        field: HostField,
        reason: &'static str,
    },
    #[error("cannot clear required field: {0}")]
    RequiredField(&'static str),
}

impl FromStr for ClearField {
    type Err = String;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value {
            "user" => Ok(Self::User),
            "port" => Ok(Self::Port),
            "identity" => Ok(Self::Identity),
            "jump" => Ok(Self::Jump),
            _ => Err(format!("unknown clear field: {value}")),
        }
    }
}

impl HostChanges {
    pub fn from_clear_fields(fields: impl IntoIterator<Item = ClearField>) -> Self {
        let mut changes = Self::default();
        for field in fields {
            match field {
                ClearField::User => changes.user = Change::Clear,
                ClearField::Port => changes.port = Change::Clear,
                ClearField::Identity => changes.identity_file = Change::Clear,
                ClearField::Jump => changes.proxy_jump = Change::Clear,
            }
        }
        changes
    }
}

impl Host {
    pub fn new(
        alias: impl Into<String>,
        hostname: impl Into<String>,
        user: Option<String>,
        port: Option<u16>,
        identity_file: Option<String>,
        proxy_jump: Option<String>,
    ) -> Result<Self, HostError> {
        let host = Self {
            alias: alias.into(),
            hostname: hostname.into(),
            user,
            port,
            identity_file,
            proxy_jump,
        };
        host.validate()?;
        Ok(host)
    }

    pub fn alias(&self) -> &str {
        &self.alias
    }
    pub fn hostname(&self) -> &str {
        &self.hostname
    }
    pub fn user(&self) -> Option<&str> {
        self.user.as_deref()
    }
    pub fn port(&self) -> Option<u16> {
        self.port
    }
    pub fn identity_file(&self) -> Option<&str> {
        self.identity_file.as_deref()
    }
    pub fn proxy_jump(&self) -> Option<&str> {
        self.proxy_jump.as_deref()
    }

    pub fn apply(&self, changes: HostChanges) -> Result<Self, HostError> {
        let changed = Self {
            alias: apply_required(&self.alias, changes.alias, "alias")?,
            hostname: apply_required(&self.hostname, changes.hostname, "hostname")?,
            user: apply_optional(&self.user, changes.user),
            port: apply_optional(&self.port, changes.port),
            identity_file: apply_optional(&self.identity_file, changes.identity_file),
            proxy_jump: apply_optional(&self.proxy_jump, changes.proxy_jump),
        };
        changed.validate()?;
        Ok(changed)
    }

    fn validate(&self) -> Result<(), HostError> {
        HostField::Alias.validate(&self.alias)?;
        HostField::Hostname.validate(&self.hostname)?;
        if let Some(user) = &self.user {
            HostField::User.validate(user)?;
        }
        if let Some(port) = self.port {
            HostField::Port.validate(&port.to_string())?;
        }
        if let Some(identity) = &self.identity_file {
            HostField::IdentityFile.validate(identity)?;
        }
        if let Some(jump) = &self.proxy_jump {
            HostField::ProxyJump.validate(jump)?;
        }
        Ok(())
    }
}

fn is_alias(value: &str) -> bool {
    value
        .chars()
        .next()
        .is_some_and(|c| c.is_ascii_alphanumeric())
        && value
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c))
}

fn single_value_rejection(value: &str) -> Option<&'static str> {
    if value.is_empty() {
        Some("must not be empty")
    } else if value.chars().any(|c| c.is_whitespace() || c.is_control()) {
        Some("must not contain whitespace or control characters")
    } else {
        None
    }
}

fn parse_port_text(value: &str) -> Result<u16, &'static str> {
    const REASON: &str = "must be an integer from 1 through 65535";
    if value.is_empty() || !value.bytes().all(|byte| byte.is_ascii_digit()) {
        return Err(REASON);
    }
    value.parse().ok().filter(|port| *port != 0).ok_or(REASON)
}

fn apply_required(
    current: &str,
    change: Change<String>,
    field: &'static str,
) -> Result<String, HostError> {
    match change {
        Change::Keep => Ok(current.to_owned()),
        Change::Set(value) => Ok(value),
        Change::Clear => Err(HostError::RequiredField(field)),
    }
}

fn apply_optional<T: Clone>(current: &Option<T>, change: Change<T>) -> Option<T> {
    match change {
        Change::Keep => current.clone(),
        Change::Set(value) => Some(value),
        Change::Clear => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn host() -> Host {
        Host::new("dev-vps", "203.0.113.8", None, None, None, None).unwrap()
    }

    #[test]
    fn validates_alias_and_port() {
        assert!(matches!(
            Host::new("-bad", "example.com", None, None, None, None),
            Err(HostError::Invalid {
                field: HostField::Alias,
                ..
            })
        ));
        assert!(matches!(
            Host::new("good", "example.com", None, Some(0), None, None),
            Err(HostError::Invalid {
                field: HostField::Port,
                ..
            })
        ));
    }

    #[test]
    fn applies_only_requested_changes() {
        let changed = host()
            .apply(HostChanges {
                user: Change::Set("deploy".into()),
                port: Change::Set(2222),
                ..HostChanges::default()
            })
            .unwrap();

        assert_eq!(changed.alias(), "dev-vps");
        assert_eq!(changed.hostname(), "203.0.113.8");
        assert_eq!(changed.user(), Some("deploy"));
        assert_eq!(changed.port(), Some(2222));
    }

    #[test]
    fn clear_rejects_required_fields() {
        let changes = HostChanges::from_clear_fields([ClearField::User]);
        assert_eq!(host().apply(changes).unwrap().user(), None);
        assert!("hostname".parse::<ClearField>().is_err());
    }

    fn rejected(field: HostField, value: &str) -> bool {
        matches!(
            field.validate(value),
            Err(HostError::Invalid { field: rejected, .. }) if rejected == field
        )
    }

    #[test]
    fn identity_allows_inner_spaces_but_rejects_controls_blanks_and_padding() {
        let accepted = Host::new(
            "dev-vps",
            "203.0.113.8",
            None,
            None,
            Some("/keys/my deploy key".into()),
            None,
        )
        .unwrap();
        assert_eq!(accepted.identity_file(), Some("/keys/my deploy key"));

        for value in [
            "",
            "   ",
            " /key",
            "/key ",
            "/keys/bad\nkey",
            "/keys/bad\tkey",
        ] {
            assert!(rejected(HostField::IdentityFile, value), "{value:?}");
        }
    }

    #[test]
    fn proxy_jump_rejects_whitespace_controls_and_blanks() {
        assert!(
            HostField::ProxyJump
                .validate("bastion,user@jump:2222")
                .is_ok()
        );
        for value in [
            "",
            "  ",
            " bastion",
            "bastion ",
            "bastion host",
            "bastion\thost",
        ] {
            assert!(rejected(HostField::ProxyJump, value), "{value:?}");
        }
    }

    #[test]
    fn hostname_and_user_reject_blank_padded_and_inner_whitespace() {
        for field in [HostField::Hostname, HostField::User] {
            assert!(field.validate("example").is_ok());
            for value in ["", " ", " example", "example ", "exa mple"] {
                assert!(rejected(field, value), "{field}: {value:?}");
            }
        }
    }

    #[test]
    fn port_text_is_strict_digits_in_range() {
        assert_eq!(parse_port("22").unwrap(), 22);
        assert_eq!(parse_port("65535").unwrap(), 65535);
        for value in ["", "0", "65536", "+22", "-1", " 22", "2 2", "22a"] {
            assert!(parse_port(value).is_err(), "{value:?}");
            assert!(rejected(HostField::Port, value), "{value:?}");
        }
    }

    #[test]
    fn errors_name_the_field_and_constraint_without_the_value() {
        let message = Host::new(
            "dev",
            "example",
            None,
            None,
            Some(" /secret/key".into()),
            None,
        )
        .unwrap_err()
        .to_string();
        assert_eq!(
            message,
            "invalid identity file: must not start or end with whitespace"
        );
    }
}
