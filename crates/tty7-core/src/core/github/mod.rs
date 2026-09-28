//! Read-only GitHub: which repository a working tree belongs to, whose token
//! to read it with, and the handful of REST calls the right panel's GitHub tab
//! makes.
//!
//! Framework-free like the rest of this crate. The HTTPS half ([`http`]) is
//! behind the `github` feature, which only the GUI turns on — the headless
//! server never talks to GitHub.

pub mod accounts;
pub mod api;
#[cfg(feature = "github")]
pub mod http;
pub mod markdown;
pub mod model;
pub mod remote;
pub mod token;

pub use api::{ApiError, ListPage, ListQuery, Reply, Transport};
pub use model::{Comment, Detail, Item, ItemState, Kind, Label, PrFile, PullInfo, StateFilter};
pub use remote::{GitHubRemote, RepoSlug};
pub use token::{Token, TokenSource};
