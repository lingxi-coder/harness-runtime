//! Side queries (stateless one-shot LLM calls) and forked agents (full agent
//! loops that share the parent's prompt cache via byte-exact reuse of
//! [`CacheSafeParams`]).
//!
//! See spec §20 (Side Query & Forked Agent) for the architecture. The two
//! primitives live in the same crate because both are "non-main" entry
//! points to the LLM and need uniform telemetry tagging via [`QuerySource`].
//!
//! - [`side_query`]: one-shot, stateless, no tools-in-loop.
//! - [`forked_agent`]: full subagent loop, byte-exact cache reuse.
//! - [`cache_safe_params`]: the shared slot the host writes after every
//!   successful main-loop turn so forks can read the latest snapshot.

#![forbid(unsafe_code)]

pub mod cache_safe_params;
pub mod forked_agent;
pub mod provider_side_query;
pub mod purposes;
pub mod side_query;
pub mod vision_delegation;

pub use cache_safe_params::{CacheSafeParams, CacheSafeParamsSlot};
pub use forked_agent::{
    ForkError, ForkPurpose, ForkedAgentRequest, ForkedAgentResult, ForkedAgentRunner,
};
pub use provider_side_query::ProviderSideQueryClient;
pub use purposes::QuerySource;
pub use side_query::{
    CanonicalSideQueryRequest, SideQueryClient, SideQueryError, SideQueryEstimate,
    SideQueryRequest, SideQueryResponse, StrictStructuredQueryRequest,
    StrictStructuredQueryResponse,
};
pub use vision_delegation::{
    collect_media, collect_media_fingerprints, covered_fingerprints,
    filter_messages_to_fingerprints, prepare_delegation, prepare_media_for_nonvision,
    rewrite_media_for_nonvision, DelegationMedia, PreparedDelegation, VisionDelegationResult,
    VisionDelegationService, VisionPacket, MAX_DECODED_BYTES_PER_QUERY, MAX_MEDIA_PER_QUERY,
    MAX_MEDIA_PER_REQUEST, PROMPT_VERSION,
};
