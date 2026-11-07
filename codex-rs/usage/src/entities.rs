//! Closed, content-free values observed at the runtime approval boundary.
use serde::Deserialize;
use serde::Serialize;

use crate::Result;
use crate::StorageError;

/// Version of the phase/activity mapping registered by this collector.
pub const USAGE_TAXONOMY_VERSION: u64 = 1;

macro_rules! values {
    ($(#[$meta:meta])* $name:ident { $($(#[$variant_meta:meta])* $variant:ident => $value:literal),+ $(,)? }) => {
        $(#[$meta])*
        #[derive(Clone, Copy, Debug, Deserialize, Eq, PartialEq, Serialize)]
        pub enum $name {
            $($(#[$variant_meta])* #[serde(rename = $value)] $variant),+
        }
        impl $name {
            /// Return the stable content-free storage value.
            pub const fn as_str(self) -> &'static str {
                match self { $(Self::$variant => $value),+ }
            }
        }
        impl std::str::FromStr for $name {
            type Err = StorageError;
            fn from_str(value: &str) -> Result<Self> {
                match value {
                    $($value => Ok(Self::$variant),)+
                    _ => Err(StorageError::InvalidData(concat!("invalid ", stringify!($name)).into())),
                }
            }
        }
    };
}

values!(
    /// An observed approval result. Request text, policies and rules are excluded.
    UsageApprovalDecision {
        /// This request was approved.
        Approved => "approved",
        /// Matching requests were approved for the current session.
        ApprovedForSession => "approved_for_session",
        /// An execution-policy amendment was approved; its content is excluded.
        ApprovedExecpolicyAmendment => "approved_execpolicy_amendment",
        /// An MCP-policy amendment was approved; its content is excluded.
        ApprovedMcpPolicyAmendment => "approved_mcp_policy_amendment",
        /// A network allow decision was observed without retaining its host.
        NetworkAllowed => "network_allowed",
        /// A network deny decision was observed without retaining its host.
        NetworkDenied => "network_denied",
        /// The request was denied; the rejection text is excluded.
        Denied => "denied",
        /// The reviewer timed out.
        TimedOut => "timed_out",
        /// Review was cancelled or interrupted.
        Cancelled => "cancelled",
    }
);

values!(
    /// The boundary that actually supplied the approval result.
    UsageApprovalSource {
        /// A configured permission hook supplied the result.
        Config => "config",
        /// The automatic reviewer supplied the result.
        AutomatedReviewer => "automated_reviewer",
        /// The user supplied the result.
        User => "user",
        /// An existing session approval avoided a new review.
        SessionCache => "session_cache",
    }
);
