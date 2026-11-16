//! shlv: a fast, resource-efficient bookmark store.

pub mod bulk;
pub mod config;
pub mod db;
#[cfg(feature = "fetch")]
pub mod extract;
#[cfg(feature = "fetch")]
pub mod fetch;
pub mod html;
pub mod netscape;
pub mod search;
pub mod store;
pub mod suggest;
pub mod tag;
pub mod transfer;
pub mod url;

pub use config::Config;
pub use store::{Added, Bookmark, Edit, ListQuery, NewBookmark, Store};
pub use suggest::{DomainRules, NonCanonicalRuleTags, RuleStats, Source, SuggestMode, Suggestion};
pub use tag::{
    CanonicalEntry, DeclaredEntry, DeclaredTags, ImportStats, NonCanonicalTags, Retagged,
    SyncStats, TagInfo, TagSort,
};
pub use transfer::{Format, ImportItem, ImportOptions, ImportSummary};
