//! Who may drive the workflow: the acceptance policy for third-party comments
//! and 👍 reactions.
//!
//! On a public repo anyone can comment or react, so ghwf only acts on input
//! whose author is the authenticated operator, an explicitly allow-listed user,
//! or a repo collaborator. Comments carry an `author_association`, so the
//! collaborator check is free for them; reactions don't, so collaborator status
//! for a 👍 is resolved against the repo's collaborator list (fetched lazily
//! and memoised).

use std::collections::{HashMap, HashSet};

use anyhow::Result;

use crate::github::{self, RepoRef};

/// Comment associations that count as "repo collaborator" for acceptance.
const COLLABORATOR_ASSOCIATIONS: [&str; 3] = ["OWNER", "MEMBER", "COLLABORATOR"];

/// A resolved acceptance policy. Construct once per process; the decision
/// methods are pure (collaborator sets are fetched up front via
/// [`AccessList::ensure_collaborators`]).
pub struct AccessList {
    /// The authenticated user's login, lowercased. Always accepted.
    me: String,
    /// `allowed_users` logins, lowercased.
    allowed: HashSet<String>,
    /// Per-repo collaborator login sets, lowercased; populated on demand.
    collaborators: HashMap<RepoRef, HashSet<String>>,
}

impl AccessList {
    /// Resolve from `allowed_users` plus the authenticated `gh` user (one
    /// `gh api user` call). No collaborator fetch happens here — that's lazy,
    /// via [`AccessList::ensure_collaborators`].
    pub fn resolve(allowed_users: &[String]) -> Result<Self> {
        let me = github::authenticated_user()?;
        Ok(Self::new(&me, allowed_users))
    }

    /// Build a policy directly from the authenticated login and `allowed_users`,
    /// with no collaborator sets fetched. Exposed for tests and `resolve`.
    pub fn new(me: &str, allowed_users: &[String]) -> Self {
        AccessList {
            me: me.to_ascii_lowercase(),
            allowed: allowed_users
                .iter()
                .map(|u| u.to_ascii_lowercase())
                .collect(),
            collaborators: HashMap::new(),
        }
    }

    /// Fetch and memoise the collaborator set for `repo` (idempotent). Needed
    /// before [`AccessList::accepts_reaction`] can honour the collaborator rule
    /// for that repo.
    pub fn ensure_collaborators(&mut self, repo: &RepoRef) -> Result<()> {
        if self.collaborators.contains_key(repo) {
            return Ok(());
        }
        let logins = github::fetch_collaborators(&repo.0, &repo.1)?;
        self.collaborators.insert(
            repo.clone(),
            logins.iter().map(|l| l.to_ascii_lowercase()).collect(),
        );
        Ok(())
    }

    /// Whether `login` is the operator or explicitly allow-listed (the checks
    /// that need no network and apply to both comments and reactions).
    fn is_self_or_allowed(&self, login: &str) -> bool {
        let login = login.to_ascii_lowercase();
        login == self.me || self.allowed.contains(&login)
    }

    /// Accept a comment by its author login and GitHub `author_association`.
    /// Pure: the association classifies collaborators with no API call.
    pub fn accepts_comment(&self, login: &str, association: &str) -> bool {
        self.is_self_or_allowed(login) || COLLABORATOR_ASSOCIATIONS.contains(&association)
    }

    /// Accept an issue for *automatic* selection by its author login and GitHub
    /// `author_association` — so a public repo's strangers can't get ghwf to
    /// auto-pick the issues they open (#93). Same rule as [`accepts_comment`]:
    /// the association classifies collaborators with no API call, since the
    /// open-issues listing carries it.
    pub fn accepts_issue(&self, login: &str, association: &str) -> bool {
        self.accepts_comment(login, association)
    }

    /// Accept a 👍 reaction by its author login, against the collaborator set
    /// for `repo`. Pure: returns false for an unknown collaborator if the set
    /// hasn't been fetched (call [`AccessList::ensure_collaborators`] first).
    pub fn accepts_reaction(&self, repo: &RepoRef, login: &str) -> bool {
        if self.is_self_or_allowed(login) {
            return true;
        }
        self.collaborators
            .get(repo)
            .is_some_and(|set| set.contains(&login.to_ascii_lowercase()))
    }

    /// Accept a 👍 by author against *any* fetched collaborator set, ignoring
    /// which repo it's on. For the `wait` wake-gate, where erring toward an
    /// extra wake is harmless (`work-on` re-checks per-repo and is
    /// authoritative). Call [`AccessList::ensure_collaborators`] for the repos
    /// of interest first.
    pub fn accepts_reaction_any(&self, login: &str) -> bool {
        if self.is_self_or_allowed(login) {
            return true;
        }
        let login = login.to_ascii_lowercase();
        self.collaborators.values().any(|set| set.contains(&login))
    }

    /// Whether a 👍 from `login` still needs a collaborator lookup to decide —
    /// i.e. it isn't already accepted by the operator/allow-list rule. Lets the
    /// caller skip fetching collaborators when nothing hinges on it.
    pub fn reaction_needs_collaborators(&self, login: &str) -> bool {
        !self.is_self_or_allowed(login)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn repo() -> RepoRef {
        ("octo".to_string(), "repo".to_string())
    }

    #[test]
    fn authenticated_user_always_accepted() {
        let access = AccessList::new("Me", &[]);
        // Case-insensitive, and regardless of association.
        assert!(access.accepts_comment("me", "NONE"));
        assert!(access.accepts_reaction(&repo(), "ME"));
    }

    #[test]
    fn allowed_users_accepted_case_insensitively() {
        let access = AccessList::new("me", &["OctoCat".to_string()]);
        assert!(access.accepts_comment("octocat", "NONE"));
        assert!(access.accepts_reaction(&repo(), "OCTOCAT"));
        assert!(!access.accepts_comment("stranger", "NONE"));
    }

    #[test]
    fn collaborator_associations_accepted_for_comments() {
        let access = AccessList::new("me", &[]);
        for assoc in ["OWNER", "MEMBER", "COLLABORATOR"] {
            assert!(access.accepts_comment("someone", assoc), "{assoc}");
        }
        for assoc in ["CONTRIBUTOR", "FIRST_TIME_CONTRIBUTOR", "NONE", ""] {
            assert!(!access.accepts_comment("someone", assoc), "{assoc}");
        }
    }

    #[test]
    fn issues_accept_operator_allowed_and_collaborator_authors() {
        let access = AccessList::new("me", &["friend".to_string()]);
        // Operator and allow-listed authors, regardless of association.
        assert!(access.accepts_issue("me", "NONE"));
        assert!(access.accepts_issue("Friend", "NONE"));
        // Collaborator associations are accepted with no list fetch.
        for assoc in ["OWNER", "MEMBER", "COLLABORATOR"] {
            assert!(access.accepts_issue("someone", assoc), "{assoc}");
        }
        // A stranger with no qualifying association is rejected.
        for assoc in ["CONTRIBUTOR", "FIRST_TIME_CONTRIBUTOR", "NONE", ""] {
            assert!(!access.accepts_issue("stranger", assoc), "{assoc}");
        }
    }

    #[test]
    fn reactions_need_the_collaborator_set() {
        let mut access = AccessList::new("me", &[]);
        // Without the set fetched, a stranger's 👍 is rejected.
        assert!(!access.accepts_reaction(&repo(), "collab"));
        assert!(access.reaction_needs_collaborators("collab"));
        // Inject a collaborator set (stands in for the API fetch).
        access
            .collaborators
            .insert(repo(), ["collab".to_string()].into_iter().collect());
        assert!(access.accepts_reaction(&repo(), "Collab"));
        assert!(!access.accepts_reaction(&repo(), "stranger"));
    }

    #[test]
    fn self_and_allowed_skip_the_collaborator_lookup() {
        let access = AccessList::new("me", &["friend".to_string()]);
        assert!(!access.reaction_needs_collaborators("me"));
        assert!(!access.reaction_needs_collaborators("Friend"));
        assert!(access.reaction_needs_collaborators("stranger"));
    }
}
