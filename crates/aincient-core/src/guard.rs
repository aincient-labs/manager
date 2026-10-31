//! What a destructive confirmation has to say, and how hard it is to give
//! (m2-8, D-07).
//!
//! With local and remote sites in one list, the costliest mistake is changing
//! the wrong one. So every destructive confirm names where the site runs ("on
//! this computer", "on <host>"), and a change to a **live remote site** — one
//! that is running and serving its live site, not stopped or frozen — needs
//! the site's slug typed back. Both front-ends (the CLI's `confirm_change` and
//! the GUI's confirm dialog) read the same [`ChangeGuard`].

use serde::Serialize;

use crate::ops::Status;
use crate::sites::Site;
use crate::Stack;

/// Where a change lands, and whether its slug must be typed to confirm it.
#[derive(Debug, Clone, PartialEq, Eq, Serialize)]
pub struct ChangeGuard {
    /// What `--site` takes here — the word typed to confirm.
    pub slug: String,
    /// The site's display name.
    pub label: String,
    /// The server's ssh target; `None` for a site on this computer.
    pub host: Option<String>,
    /// The site runs on a server and is live: confirming takes the typed slug.
    pub typed_slug: bool,
}

impl ChangeGuard {
    /// A site on this computer: a plain yes/no, as it always was.
    pub fn local(stack: &Stack) -> Self {
        ChangeGuard {
            slug: stack.slug.clone().unwrap_or_default(),
            label: stack
                .label
                .clone()
                .unwrap_or_else(|| "your site".to_string()),
            host: None,
            typed_slug: false,
        }
    }

    /// A site on a server; `live` per [`is_live`]. A local `site` is
    /// [`ChangeGuard::local`].
    pub fn for_site(site: &Site, live: bool) -> Self {
        match &site.remote {
            None => Self::local(&site.stack()),
            Some(remote) => ChangeGuard {
                slug: site.slug.clone(),
                label: site.label.clone(),
                host: Some(remote.host.clone()),
                typed_slug: live,
            },
        }
    }

    /// "on this computer" or "on <host>".
    pub fn place(&self) -> String {
        match &self.host {
            None => "on this computer".to_string(),
            Some(host) => format!("on {host}"),
        }
    }

    /// "<label> on <place>" — what `{site}` stands for in [`Self::question`].
    pub fn site_words(&self) -> String {
        format!("{} {}", self.label, self.place())
    }

    /// `template` with `{site}` replaced by [`Self::site_words`]:
    /// `"Restore {site}?"` → `"Restore Blog on vps.example.com?"`.
    pub fn question(&self, template: &str) -> String {
        template.replace("{site}", &self.site_words())
    }

    /// Whether `typed` confirms the change: anything goes when no slug is
    /// asked for, else exactly the slug (surrounding spaces ignored).
    pub fn accepts(&self, typed: &str) -> bool {
        !self.typed_slug || typed.trim() == self.slug
    }
}

/// Whether a remote site is live: running, whatever visitors are served. A
/// frozen site counts too: a restore there wipes what editors did, the next
/// freeze publishes it, and after a rest every engine site is frozen. `status`
/// is what the server answered, `None` when it couldn't be asked. **Unknown
/// counts as live**: a guard that relaxes because a probe failed guards
/// nothing.
pub fn is_live(status: Option<&Status>) -> bool {
    status.is_none_or(|st| st.running)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::sites::{KeyMode, Site};
    use crate::Channel;
    use std::path::Path;

    fn status(running: bool) -> Status {
        Status {
            installed: true,
            running,
            reachable: running,
            console_url: String::new(),
            site_url: String::new(),
            image: String::new(),
            channel: Channel::Stable,
            image_digest: None,
            version: None,
            site_id: None,
        }
    }

    #[test]
    fn a_running_site_is_live_and_unknown_counts_as_live() {
        assert!(is_live(Some(&status(true))));
        assert!(is_live(None), "server unreachable");
    }

    #[test]
    fn a_stopped_site_is_not_live() {
        assert!(!is_live(Some(&status(false))));
    }

    #[test]
    fn a_remote_guard_names_the_host_and_asks_for_the_slug_only_when_live() {
        let site = Site::new_remote(
            Path::new("/home/u"),
            "blog",
            Some("Blog"),
            "vps.example.com",
            None,
            KeyMode::Ssh,
        );
        let live = ChangeGuard::for_site(&site, true);
        assert_eq!(
            live.question("Restore {site}?"),
            "Restore Blog on vps.example.com?"
        );
        assert!(live.typed_slug);
        assert!(!live.accepts("y"));
        assert!(!live.accepts("Blog"));
        assert!(live.accepts(" blog "));
        let resting = ChangeGuard::for_site(&site, false);
        assert!(!resting.typed_slug);
        assert!(resting.accepts(""));
    }

    #[test]
    fn a_local_guard_says_this_computer_and_never_asks_for_the_slug() {
        let stack = Stack {
            home: "/home/u/.atelier/sites/shop".into(),
            slug: Some("shop".into()),
            label: Some("Shop".into()),
            project: None,
        };
        let g = ChangeGuard::local(&stack);
        assert_eq!(
            g.question("Restore {site}?"),
            "Restore Shop on this computer?"
        );
        assert!(!g.typed_slug);
        let bare = ChangeGuard::local(&Stack {
            home: "/home/u/.atelier".into(),
            slug: None,
            label: None,
            project: None,
        });
        assert_eq!(bare.site_words(), "your site on this computer");
    }
}
