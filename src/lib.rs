//! shlv: a fast, resource-efficient bookmark store.

#[cfg(feature = "ai")]
pub mod ai;
pub mod batch;
pub mod bulk;
pub mod choose;
pub mod config;
pub mod db;
pub mod draft;
#[cfg(feature = "fetch")]
pub mod extract;
#[cfg(feature = "fetch")]
pub mod fetch;
pub mod health;
pub mod html;
#[cfg(feature = "model")]
pub mod model;
pub mod netscape;
pub mod proposal;
pub mod review;
pub mod search;
pub mod seed;
pub mod store;
pub mod style;
pub mod suggest;
pub mod tag;
pub mod transfer;
pub mod url;

pub use config::Config;
pub use proposal::{
    Decided, Diverted, Proposal, ProposalSource, ProposalState, ProposeStats, StrictTags,
};
pub use seed::{Candidate, Decision, Hint, Interest, SeedName, Seeded};
pub use store::{
    Added, Bookmark, Deleted, Edit, Edited, ListQuery, MissingBookmarks, NewBookmark, Store,
    TagSource,
};
pub use style::{Separator, StyleConfig, TagStyle};
pub use suggest::{
    BackfillCandidate, DomainRules, NonCanonicalRuleTags, OfferStats, RuleStats, Source,
    SuggestMode, Suggestion,
};
pub use tag::{
    CanonicalEntry, DeclaredEntry, DeclaredTags, ImportStats, NonCanonicalTags, Pinned, Retagged,
    SyncStats, TagInfo, TagSort,
};
pub use transfer::{Format, ImportItem, ImportOptions, ImportSummary};
