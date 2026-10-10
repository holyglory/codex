use codex_login::AuthManagerLease;
use codex_login::CodexAuth;
use codex_protocol::auth::AuthMode;

/// Non-secret authentication identity used to partition in-memory plugin state.
///
/// This deliberately excludes bearer tokens, API keys, headers, and other credential material.
#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) struct PluginAuthIdentity {
    mode: Option<PluginAuthMode>,
    account_id: Option<String>,
    chatgpt_user_id: Option<String>,
    is_workspace_account: bool,
}

#[derive(Clone, Hash, PartialEq, Eq)]
pub(crate) struct PluginAuthCacheIdentity {
    backend: String,
    auth: PluginAuthIdentity,
}

impl PluginAuthCacheIdentity {
    pub(crate) fn new(backend: &str, auth: Option<&CodexAuth>) -> Self {
        Self::from_identity(backend, PluginAuthIdentity::from_auth(auth))
    }

    pub(crate) fn from_identity(backend: &str, auth: PluginAuthIdentity) -> Self {
        Self {
            backend: backend.trim_end_matches('/').to_string(),
            auth,
        }
    }

    pub(crate) fn auth_identity(&self) -> &PluginAuthIdentity {
        &self.auth
    }
}

impl PluginAuthIdentity {
    pub(crate) fn from_auth(auth: Option<&CodexAuth>) -> Self {
        Self {
            mode: auth.map(CodexAuth::api_auth_mode).map(PluginAuthMode::from),
            account_id: auth.and_then(CodexAuth::get_account_id),
            chatgpt_user_id: auth.and_then(CodexAuth::get_chatgpt_user_id),
            is_workspace_account: auth.is_some_and(CodexAuth::is_workspace_account),
        }
    }
}

#[derive(Clone, Copy, Hash, PartialEq, Eq)]
enum PluginAuthMode {
    ApiKey,
    Chatgpt,
    ChatgptAuthTokens,
    Headers,
    AgentIdentity,
    PersonalAccessToken,
    BedrockApiKey,
    BedrockAccessKeys,
}

impl From<AuthMode> for PluginAuthMode {
    fn from(mode: AuthMode) -> Self {
        match mode {
            AuthMode::ApiKey => Self::ApiKey,
            AuthMode::Chatgpt => Self::Chatgpt,
            AuthMode::ChatgptAuthTokens => Self::ChatgptAuthTokens,
            AuthMode::Headers => Self::Headers,
            AuthMode::AgentIdentity => Self::AgentIdentity,
            AuthMode::PersonalAccessToken => Self::PersonalAccessToken,
            AuthMode::BedrockApiKey => Self::BedrockApiKey,
            AuthMode::BedrockAccessKeys => Self::BedrockAccessKeys,
        }
    }
}

/// An authenticated background plugin job owns both the operation lease and its auth snapshot.
#[derive(Clone)]
pub(crate) struct AuthenticatedPluginJob {
    identity: PluginAuthIdentity,
    auth: Option<CodexAuth>,
    _auth_lease: AuthManagerLease,
}

impl AuthenticatedPluginJob {
    pub(crate) fn new(auth_lease: AuthManagerLease, auth: Option<CodexAuth>) -> Self {
        Self {
            identity: PluginAuthIdentity::from_auth(auth.as_ref()),
            auth,
            _auth_lease: auth_lease,
        }
    }

    pub(crate) fn identity(&self) -> &PluginAuthIdentity {
        &self.identity
    }

    pub(crate) fn auth(&self) -> Option<&CodexAuth> {
        self.auth.as_ref()
    }

    pub(crate) fn cloned_auth(&self) -> Option<CodexAuth> {
        self.auth.clone()
    }
}
