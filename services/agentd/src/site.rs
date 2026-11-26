//! A host's site as browsers count it: its registrable domain (the public
//! suffix and one label more), from the system's Public Suffix List
//! (`/usr/share/publicsuffix`, Ubuntu's publicsuffix package, which libpsl
//! and so curl and wget already need).
//!
//! A plan's or a card's site grant covers the hosts of that domain: one
//! service spreads a task over several (web.postman.co and
//! apxlending.postman.co), and each used to be a card of its own. Two
//! people's pages on github.io, or two companies under co.uk, stay apart:
//! those suffixes are on the list. Without the list, a grant is its host and
//! the hosts under it, as before 0.21.1.

use std::collections::HashSet;
use std::sync::LazyLock;

/// The list's rules: plain (`co.uk`), wildcards (`*.ck`, kept as `ck`) and
/// exceptions (`!www.ck`, kept as `www.ck`).
#[derive(Default)]
pub struct Suffixes {
    rules: HashSet<String>,
    wildcards: HashSet<String>,
    exceptions: HashSet<String>,
}

impl Suffixes {
    /// The list's text: a rule a line, up to the first space; `//` comments.
    pub fn parse(text: &str) -> Self {
        let mut suffixes = Self::default();
        for line in text.lines() {
            let rule = line.split_whitespace().next().unwrap_or("").to_ascii_lowercase();
            if rule.is_empty() || rule.starts_with("//") {
                continue;
            }
            if let Some(name) = rule.strip_prefix('!') {
                suffixes.exceptions.insert(name.to_string());
            } else if let Some(name) = rule.strip_prefix("*.") {
                suffixes.wildcards.insert(name.to_string());
            } else {
                suffixes.rules.insert(rule);
            }
        }
        suffixes
    }

    /// The host's registrable domain. None for a public suffix itself
    /// (github.io, co.uk); for a punycode host (the list writes those rules
    /// in Unicode, so they wouldn't match and the domain could come out too
    /// wide); and for an IP address or a host with a port, which is its own
    /// site, as browsers count it (192.168.1.10 and 10.0.1.10 aren't one
    /// site called "1.10").
    pub fn registrable(&self, host: &str) -> Option<String> {
        let host = host.trim_end_matches('.').to_ascii_lowercase();
        let labels: Vec<&str> = host.split('.').collect();
        if labels.iter().any(|l| l.is_empty() || l.starts_with("xn--"))
            || host.contains(':')
            || labels
                .last()
                .is_some_and(|l| l.bytes().all(|b| b.is_ascii_digit()))
        {
            return None;
        }
        let n = labels.len();
        let tail = |i: usize| labels[i..].join(".");
        // Where the public suffix starts: an exception's parent, else the
        // longest rule that matches, else the last label (the default `*`).
        let start = (0..n)
            .find(|&i| self.exceptions.contains(&tail(i)))
            .map(|i| i + 1)
            .or_else(|| {
                (0..n).find(|&i| {
                    self.rules.contains(&tail(i)) || (i + 1 < n && self.wildcards.contains(&tail(i + 1)))
                })
            })
            .unwrap_or(n - 1);
        (start > 0).then(|| tail(start - 1))
    }
}

/// The system's list, read once; None where it isn't installed.
static SYSTEM: LazyLock<Option<Suffixes>> = LazyLock::new(|| {
    #[cfg(test)]
    {
        Some(Suffixes::parse(tests::LIST))
    }
    #[cfg(not(test))]
    {
        const LIST: &str = "/usr/share/publicsuffix/public_suffix_list.dat";
        std::fs::read_to_string(LIST)
            .ok()
            .map(|text| Suffixes::parse(&text))
    }
});

/// Whether grants can cover a whole domain here (the list is installed).
pub fn available() -> bool {
    SYSTEM.is_some()
}

/// The host's registrable domain, if the list knows it.
pub fn domain(host: &str) -> Option<String> {
    SYSTEM.as_ref()?.registrable(host)
}

/// Whether two hosts are the same site: the same registrable domain.
pub fn same(a: &str, b: &str) -> bool {
    domain(a).is_some_and(|domain| Some(domain) == self::domain(b))
}

#[cfg(test)]
pub(crate) mod tests {
    use super::*;

    /// Enough of the real list for the tests (and for the other modules'
    /// tests, which see it through `domain` and `same`).
    pub(crate) const LIST: &str = "\
// ===BEGIN ICANN DOMAINS===
com
co
uk
co.uk
io
dev
// A wildcard, and an exception to it.
*.ck
!www.ck
// ===BEGIN PRIVATE DOMAINS===
github.io
pages.dev
";

    #[test]
    fn a_host_s_site_is_its_registrable_domain() {
        let list = Suffixes::parse(LIST);
        let site = |host: &str| list.registrable(host);
        assert_eq!(site("web.postman.co").as_deref(), Some("postman.co"));
        assert_eq!(site("apxlending.postman.co").as_deref(), Some("postman.co"));
        assert_eq!(site("postman.co").as_deref(), Some("postman.co"));
        assert_eq!(site("identity.getpostman.com").as_deref(), Some("getpostman.com"));
        assert_eq!(site("news.bbc.co.uk").as_deref(), Some("bbc.co.uk"));
        // Someone's own pages on a shared suffix are their own site.
        assert_eq!(site("alice.github.io").as_deref(), Some("alice.github.io"));
        assert_eq!(site("x.alice.github.io").as_deref(), Some("alice.github.io"));
        assert_eq!(site("docs.pages.dev"), Some("docs.pages.dev".into()));
        // A suffix itself has none.
        assert_eq!(site("github.io"), None);
        assert_eq!(site("co.uk"), None);
        // Wildcards and exceptions, as the list's own tests have them.
        assert_eq!(site("a.b.ck").as_deref(), Some("a.b.ck"));
        assert_eq!(site("b.ck"), None);
        assert_eq!(site("www.ck").as_deref(), Some("www.ck"));
        assert_eq!(site("a.www.ck").as_deref(), Some("www.ck"));
        // An unknown top-level domain: the default rule, its last label.
        assert_eq!(site("a.b.example").as_deref(), Some("b.example"));
        // Punycode isn't widened, nor an address or a port.
        assert_eq!(site("a.xn--55qx5d.cn"), None);
        assert_eq!(site("192.168.1.10"), None);
        assert_eq!(site("10.20.1.10"), None);
        assert_eq!(site("[::1]"), None);
        assert_eq!(site("intranet.example.com:8443"), None);
        assert!(available() && same("web.postman.co", "apxlending.postman.co"));
        assert!(!same("alice.github.io", "mallory.github.io") && !same("www.postman.com", "postman.co"));
        assert!(!same("192.168.1.10", "10.20.1.10"));
    }
}
