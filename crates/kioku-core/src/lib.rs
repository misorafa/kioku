//! kioku-core: the store behind kioku — Markdown wiki in git (source of truth), SQLite
//! metadata, tantivy + lindera (`ja`) full-text search, sessions, observations, handoffs and
//! the rule-based session summary.
//!
//! The entry point is [`Store`]; every method is synchronous and blocking.

#![warn(missing_docs)]

pub mod config;
mod db;
pub mod digest;
pub mod error;
pub mod git;
pub mod handoff;
pub mod index;
pub mod layout;
pub mod page;
pub mod project;
pub mod render;
pub mod sanitize;
pub mod session;
pub mod store;
pub mod strings;
pub mod util;

pub use config::{ClientConfig, Config, ServerConfig};
pub use db::ProjectRow;
pub use digest::{FileCount, SessionDigest};
pub use error::{Error, Result};
pub use handoff::{Handoff, HandoffInput, HandoffSource};
pub use index::{Hit, SearchScope};
pub use layout::{DataDir, InitReport, init};
pub use page::{Frontmatter, Page, PageKind, PageScope};
pub use project::{ProjectIdentity, identify};
pub use session::{
    FinalizeResult, HANDOFF_STALE_TOOL_USES, NewObservation, Observation, ObservationKind,
    RecentSession, Session, SessionCounts, SessionInfo, SessionStartRequest, SessionStartResponse,
    SessionStatus,
};
pub use store::{StatusReport, Store, WritePageRequest};
pub use strings::Lang;

/// Crate version, for `/api/v1/health`.
pub const VERSION: &str = env!("CARGO_PKG_VERSION");
