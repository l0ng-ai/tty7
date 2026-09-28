//! Reading a repository with whichever signed-in account can see it.
//!
//! `gh` keeps several github.com accounts but lends out one at a time — the
//! active one. A private repository owned by a work organisation reads as a
//! 404 to a personal account, and flipping `gh auth switch` back and forth
//! for the panel's sake flips it for every repository at once.
//!
//! [`FallbackTransport`] asks the active account first. When the answer is
//! one another account could do better on — a 404 (GitHub's "not found, or
//! not yours to see"), a 401, a 403 such as SSO enforcement — it tries the
//! others in turn, and remembers, per repository owner, the account that got
//! through, so the next request for that owner goes to it directly.
//!
//! The other accounts are only listed when first needed: listing them means
//! `gh auth status`, which checks each one against GitHub.

use std::collections::HashMap;
use std::sync::{Arc, Mutex, OnceLock};

use super::api::{ApiError, Reply, Transport};

/// One account's way in, and its login for the log.
pub struct Account {
    pub login: String,
    pub transport: Arc<dyn Transport>,
}

/// Lists the accounts past the first. Blocking; called at most once.
pub type LoadOthers = Box<dyn FnOnce() -> Vec<Account> + Send>;

pub struct FallbackTransport {
    first: Arc<dyn Transport>,
    load: Mutex<Option<LoadOthers>>,
    others: OnceLock<Vec<Account>>,
    /// Owner (lowercased) → the account that last read it: 0 is `first`,
    /// `i` is `others[i - 1]`.
    by_owner: Mutex<HashMap<String, usize>>,
}

impl FallbackTransport {
    pub fn new(first: Arc<dyn Transport>, load_others: LoadOthers) -> FallbackTransport {
        FallbackTransport {
            first,
            load: Mutex::new(Some(load_others)),
            others: OnceLock::new(),
            by_owner: Mutex::new(HashMap::new()),
        }
    }

    fn others(&self) -> &[Account] {
        self.others.get_or_init(|| {
            let load = self.load.lock().unwrap_or_else(|e| e.into_inner()).take();
            load.map(|f| f()).unwrap_or_default()
        })
    }

    fn account(&self, i: usize) -> Option<&dyn Transport> {
        match i {
            0 => Some(&*self.first),
            i => self.others().get(i - 1).map(|a| &*a.transport),
        }
    }

    fn call(
        &self,
        path: &str,
        send: impl Fn(&dyn Transport) -> Result<Reply, ApiError>,
    ) -> Result<Reply, ApiError> {
        let Some(owner) = owner_of(path) else {
            return send(&*self.first);
        };
        let remembered = self
            .by_owner
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .get(&owner)
            .copied();
        // A remembered index always exists — the list never shrinks — but
        // falling back to the active account costs nothing.
        let (start, account) = remembered
            .and_then(|i| Some((i, self.account(i)?)))
            .unwrap_or((0, &*self.first));
        let err = match send(account) {
            Err(e) if another_account_might_help(&e) => e,
            reply => return reply,
        };
        for i in (0..=self.others().len()).filter(|&i| i != start) {
            let Some(account) = self.account(i) else {
                continue;
            };
            if let Ok(reply) = send(account) {
                let login = match i {
                    0 => "the active gh account",
                    i => &self.others()[i - 1].login,
                };
                log::info!("github: reading {owner}'s repositories as {login}");
                self.by_owner
                    .lock()
                    .unwrap_or_else(|e| e.into_inner())
                    .insert(owner, i);
                return Ok(reply);
            }
        }
        // Nobody got through: say what the account that was asked first
        // heard.
        Err(err)
    }
}

impl Transport for FallbackTransport {
    fn get(&self, path: &str) -> Result<Reply, ApiError> {
        self.call(path, |t| t.get(path))
    }

    fn get_full(&self, path: &str) -> Result<Reply, ApiError> {
        self.call(path, |t| t.get_full(path))
    }

    fn authenticated(&self) -> bool {
        self.first.authenticated()
    }
}

/// Errors a different account's token could turn into an answer. A spent
/// rate limit, a network failure or a bad body would come out the same.
fn another_account_might_help(e: &ApiError) -> bool {
    matches!(
        e,
        ApiError::NotFound | ApiError::Unauthorized | ApiError::Forbidden(_)
    )
}

/// The owner in `/repos/{owner}/…`, lowercased — GitHub logins are
/// case-insensitive.
fn owner_of(path: &str) -> Option<String> {
    let owner = path.strip_prefix("/repos/")?.split(['/', '?']).next()?;
    (!owner.is_empty()).then(|| owner.to_ascii_lowercase())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::core::github::api::tests::Fixture;
    use std::sync::atomic::{AtomicUsize, Ordering};

    const PATH: &str = "/repos/Work/secret/issues?page=1";
    const OTHER_REPO: &str = "/repos/work/other/issues?page=1";
    const MINE: &str = "/repos/me/public/issues?page=1";

    fn fixture(paths: &[&str]) -> Arc<Fixture> {
        let mut f = Fixture::new();
        for p in paths {
            f.on(p, "[]", false);
        }
        Arc::new(f)
    }

    struct Setup {
        t: FallbackTransport,
        first: Arc<Fixture>,
        work: Arc<Fixture>,
        loads: Arc<AtomicUsize>,
    }

    /// `first` sees only `MINE`; the second account, "work", sees the work
    /// organisation's repositories.
    fn setup() -> Setup {
        let first = fixture(&[MINE]);
        let work = fixture(&[PATH, OTHER_REPO]);
        let loads = Arc::new(AtomicUsize::new(0));
        let (w, l) = (work.clone(), loads.clone());
        let t = FallbackTransport::new(
            first.clone(),
            Box::new(move || {
                l.fetch_add(1, Ordering::SeqCst);
                vec![
                    Account {
                        login: "nobody".into(),
                        transport: fixture(&[]),
                    },
                    Account {
                        login: "work".into(),
                        transport: w,
                    },
                ]
            }),
        );
        Setup {
            t,
            first,
            work,
            loads,
        }
    }

    fn asked(f: &Fixture) -> usize {
        f.asked.lock().unwrap().len()
    }

    #[test]
    fn what_the_active_account_can_read_never_lists_the_others() {
        let s = setup();
        assert!(s.t.get(MINE).is_ok());
        assert_eq!(s.loads.load(Ordering::SeqCst), 0);
        assert_eq!(asked(&s.work), 0);
    }

    #[test]
    fn a_404_is_retried_with_the_other_accounts_until_one_reads_it() {
        let s = setup();
        assert!(s.t.get(PATH).is_ok());
        assert_eq!(asked(&s.first), 1);
        assert_eq!(asked(&s.work), 1);
    }

    #[test]
    fn the_account_that_got_through_is_asked_first_for_that_owner_next_time() {
        let s = setup();
        s.t.get(PATH).unwrap();
        // Another repository of the same owner, spelled in another case:
        // straight to "work", the active account is not bothered again.
        s.t.get_full(OTHER_REPO).unwrap();
        assert_eq!(asked(&s.first), 1);
        assert_eq!(asked(&s.work), 2);
        // Other owners still start from the active account.
        s.t.get(MINE).unwrap();
        assert_eq!(asked(&s.first), 2);
        assert_eq!(s.loads.load(Ordering::SeqCst), 1, "listed once");
    }

    #[test]
    fn a_remembered_account_that_cannot_read_a_repository_falls_back_again() {
        const SHARED: &str = "/repos/work/shared/issues";
        let first = fixture(&[SHARED]);
        let work = fixture(&[PATH]);
        let w = work.clone();
        let t = FallbackTransport::new(
            first.clone(),
            Box::new(move || {
                vec![Account {
                    login: "work".into(),
                    transport: w,
                }]
            }),
        );
        t.get(PATH).unwrap();
        // Remembered: "work". This one only the active account sees.
        t.get(SHARED).unwrap();
        assert_eq!(asked(&work), 2);
        assert_eq!(asked(&first), 2);
        // And now the active account is the one remembered.
        t.get(SHARED).unwrap();
        assert_eq!(asked(&work), 2);
    }

    #[test]
    fn when_nobody_can_read_it_the_first_error_is_reported() {
        let s = setup();
        assert_eq!(s.t.get("/repos/ghost/x").err(), Some(ApiError::NotFound));
        // Everyone was asked, once.
        assert_eq!(asked(&s.first), 1);
        assert_eq!(asked(&s.work), 1);
    }

    #[test]
    fn a_rate_limit_is_not_retried_as_someone_else() {
        let mut first = Fixture::new();
        first
            .replies
            .insert(PATH.into(), Err(ApiError::RateLimited { reset: Some(9) }));
        let loads = Arc::new(AtomicUsize::new(0));
        let l = loads.clone();
        let t = FallbackTransport::new(
            Arc::new(first),
            Box::new(move || {
                l.fetch_add(1, Ordering::SeqCst);
                Vec::new()
            }),
        );
        assert_eq!(
            t.get(PATH).err(),
            Some(ApiError::RateLimited { reset: Some(9) })
        );
        assert_eq!(loads.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn owners_come_from_repo_paths_only() {
        assert_eq!(
            owner_of("/repos/L0ng-AI/tty7/pulls"),
            Some("l0ng-ai".into())
        );
        assert_eq!(owner_of("/repos/x?y"), Some("x".into()));
        assert_eq!(owner_of("/user"), None);
        assert_eq!(owner_of("/repos//x"), None);
    }
}
