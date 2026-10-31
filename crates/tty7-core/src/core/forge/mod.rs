//! Read-only forges: which repository a working tree belongs to, whose token
//! to read it with, and the handful of REST calls the right panel's forge tab
//! makes. What the forges share is here; each one's REST calls, JSON and
//! sign-in are in a module of its own — [`github`].
//!
//! Framework-free like the rest of this crate. The HTTPS half ([`http`]) is
//! behind the `forge` feature, which only the GUI turns on — the headless
//! server never talks to a forge.

pub mod api;
pub mod github;
#[cfg(feature = "forge")]
pub mod http;
pub mod markdown;
pub mod model;
pub mod remote;
pub mod stack;
pub mod token;

pub use api::{ApiError, ListPage, ListQuery, Reply, Transport};
pub use model::{
    Check, CheckState, Checks, Comment, Detail, Item, ItemState, Kind, Label, MergeState, PrFile,
    PullInfo, PullRefs, Readiness, ReviewState, Reviewer, StateFilter, readiness,
};
pub use remote::{Forge, ForgeRemote, RepoSlug};
pub use token::{Token, TokenSource};
