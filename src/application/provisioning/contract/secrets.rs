use crate::domain::secret::{validate_chpasswd_secret, SecretString};
use serde::{Deserialize, Serialize};
use std::fmt;

use super::job::DeploymentError;
use crate::domain::machine::CreateUser;
use crate::domain::provisioning::DeploymentSpec;

pub struct UserSecret {
    username: String,
    password: Option<SecretString>,
}

impl UserSecret {
    pub fn new(username: String, password: String) -> Self {
        Self {
            username,
            password: (!password.is_empty()).then(|| SecretString::new(password)),
        }
    }

    fn validate(&self) -> Result<(), DeploymentError> {
        if let Some(password) = &self.password {
            validate_chpasswd_secret(password.expose_secret())
                .map_err(|error| DeploymentError::rejected(error.message("user password")))?;
        }
        Ok(())
    }

    fn into_password(self) -> Option<SecretString> {
        self.password
    }
}

impl fmt::Debug for UserSecret {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("UserSecret")
            .field("username", &self.username)
            .field("password", &self.password.as_ref().map(|_| "[REDACTED]"))
            .finish()
    }
}

pub struct DeploymentSecrets {
    root_password: Option<SecretString>,
    users: Vec<UserSecret>,
}

impl DeploymentSecrets {
    pub fn new(root_password: String, users: Vec<UserSecret>) -> Self {
        Self {
            root_password: (!root_password.is_empty()).then(|| SecretString::new(root_password)),
            users,
        }
    }

    pub fn validate_for(&self, users: &[CreateUser]) -> Result<(), DeploymentError> {
        if let Some(password) = &self.root_password {
            validate_chpasswd_secret(password.expose_secret())
                .map_err(|error| DeploymentError::rejected(error.message("root password")))?;
        }

        if self.users.len() != users.len()
            || self
                .users
                .iter()
                .zip(users)
                .any(|(secret, user)| secret.username != user.username)
        {
            return Err(DeploymentError::rejected(
                "deployment secrets do not match the requested user accounts",
            ));
        }
        for secret in &self.users {
            secret.validate()?;
        }
        Ok(())
    }

    pub(crate) fn has_account_changes(&self) -> bool {
        self.root_password.is_some() || !self.users.is_empty()
    }

    pub(crate) fn take_root_password(&mut self) -> Option<SecretString> {
        self.root_password.take()
    }

    pub(crate) fn take_user_password(
        &mut self,
        username: &str,
    ) -> Result<Option<SecretString>, DeploymentError> {
        let Some(index) = self
            .users
            .iter()
            .position(|secret| secret.username == username)
        else {
            return Err(DeploymentError::rejected(format!(
                "missing secret capsule entry for user {username:?}"
            )));
        };
        Ok(self.users.remove(index).into_password())
    }
}

impl fmt::Debug for DeploymentSecrets {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeploymentSecrets")
            .field(
                "root_password",
                &self.root_password.as_ref().map(|_| "[REDACTED]"),
            )
            .field("user_count", &self.users.len())
            .finish()
    }
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct DeploymentSecretsWire {
    #[serde(default, with = "crate::domain::secret::serde_secret::optional")]
    root_password: Option<SecretString>,
    users: Vec<UserSecretWire>,
}

#[derive(Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
struct UserSecretWire {
    username: String,
    #[serde(default, with = "crate::domain::secret::serde_secret::optional")]
    password: Option<SecretString>,
}

impl DeploymentSecrets {
    pub(crate) fn into_wire(self) -> DeploymentSecretsWire {
        DeploymentSecretsWire {
            root_password: self.root_password,
            users: self
                .users
                .into_iter()
                .map(|secret| UserSecretWire {
                    username: secret.username,
                    password: secret.password,
                })
                .collect(),
        }
    }
}

impl DeploymentSecretsWire {
    pub(crate) fn into_secrets(self) -> DeploymentSecrets {
        DeploymentSecrets {
            root_password: self.root_password,
            users: self
                .users
                .into_iter()
                .map(|secret| UserSecret {
                    username: secret.username,
                    password: secret.password,
                })
                .collect(),
        }
    }
}

impl fmt::Debug for DeploymentSecretsWire {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeploymentSecretsWire")
            .field(
                "root_password",
                &self.root_password.as_ref().map(|_| "[REDACTED]"),
            )
            .field("user_count", &self.users.len())
            .finish()
    }
}

pub struct DeploymentSubmission {
    request: DeploymentSpec,
    secrets: DeploymentSecrets,
}

impl DeploymentSubmission {
    pub fn new(request: DeploymentSpec, secrets: DeploymentSecrets) -> Self {
        Self { request, secrets }
    }

    pub fn request(&self) -> &DeploymentSpec {
        &self.request
    }

    pub(crate) fn into_parts(self) -> (DeploymentSpec, DeploymentSecrets) {
        (self.request, self.secrets)
    }

    pub(crate) fn validate_secrets(&self) -> Result<(), DeploymentError> {
        match &self.request {
            DeploymentSpec::Bootstrap(deployment) => self.secrets.validate_for(&deployment.users),
            DeploymentSpec::Import(_) | DeploymentSpec::Clone(_) | DeploymentSpec::Oci(_) => {
                if self.secrets.has_account_changes() {
                    return Err(DeploymentError::rejected(
                        "Only Bootstrap deployments accept account initialization secrets",
                    ));
                }
                Ok(())
            }
        }
    }
}

impl fmt::Debug for DeploymentSubmission {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("DeploymentSubmission")
            .field("request", &self.request)
            .field("secrets", &self.secrets)
            .finish()
    }
}

/// A submission rejected before a job is accepted still belongs to its caller.
/// Keeping it here allows retry without cloning or discarding its secrets.
#[derive(Debug, thiserror::Error)]
#[error("{error}")]
pub struct DeploymentStartError {
    #[source]
    error: DeploymentError,
    submission: Box<DeploymentSubmission>,
}

impl DeploymentStartError {
    pub(crate) fn new(error: DeploymentError, submission: DeploymentSubmission) -> Self {
        Self {
            error,
            submission: Box::new(submission),
        }
    }

    pub fn into_parts(self) -> (DeploymentError, DeploymentSubmission) {
        (self.error, *self.submission)
    }
}
