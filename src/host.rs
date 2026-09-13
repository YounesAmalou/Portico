use std::str::FromStr;

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

#[derive(Clone, Debug, Error, Eq, PartialEq)]
pub enum HostError {
    #[error("invalid alias: {0}")]
    InvalidAlias(String),
    #[error("invalid hostname: {0}")]
    InvalidHostname(String),
    #[error("invalid user: {0}")]
    InvalidUser(String),
    #[error("invalid port: {0}")]
    InvalidPort(u16),
    #[error("invalid identity file: {0}")]
    InvalidIdentity(String),
    #[error("invalid proxy jump: {0}")]
    InvalidJump(String),
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
        if self.alias.is_empty()
            || !self
                .alias
                .chars()
                .next()
                .is_some_and(|c| c.is_ascii_alphanumeric())
            || !self
                .alias
                .chars()
                .all(|c| c.is_ascii_alphanumeric() || ".-_".contains(c))
        {
            return Err(HostError::InvalidAlias(self.alias.clone()));
        }
        if invalid_single_value(&self.hostname) {
            return Err(HostError::InvalidHostname(self.hostname.clone()));
        }
        if let Some(user) = &self.user
            && invalid_single_value(user)
        {
            return Err(HostError::InvalidUser(user.clone()));
        }
        if let Some(port) = self.port
            && port == 0
        {
            return Err(HostError::InvalidPort(port));
        }
        if let Some(identity) = &self.identity_file
            && invalid_argument(identity)
        {
            return Err(HostError::InvalidIdentity(identity.clone()));
        }
        if let Some(jump) = &self.proxy_jump
            && invalid_argument(jump)
        {
            return Err(HostError::InvalidJump(jump.clone()));
        }
        Ok(())
    }
}

fn invalid_single_value(value: &str) -> bool {
    value.is_empty() || value.chars().any(|c| c.is_whitespace() || c.is_control())
}

fn invalid_argument(value: &str) -> bool {
    value.is_empty()
        || value.chars().any(char::is_control)
        || value.contains('\n')
        || value.contains('\r')
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
            Err(HostError::InvalidAlias(_))
        ));
        assert!(matches!(
            Host::new("good", "example.com", None, Some(0), None, None),
            Err(HostError::InvalidPort(0))
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
}
