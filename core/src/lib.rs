//! Shared foundation for the CRM stack.
//!
//! Every binary in this workspace depends on this crate, so the writer
//! (`bot`), the agent-facing server (`mcp`) and the dashboard cannot drift
//! apart on the document schema or the index settings.

pub mod error;
pub mod id;
pub mod index;
pub mod market;
pub mod meili;
pub mod model;
pub mod request;

pub use error::{Error, Result};
pub use market::{MarketConfig, Profile, Vertical};
pub use model::{Account, AccountStatus, Activity, ActivityType, Company, Doc, Signal, SignalKind};
pub use request::{CompanyRequest, RequestStatus};
