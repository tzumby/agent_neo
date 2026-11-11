//! Sites the agent's shell may reach, beyond the deployment's list
//! (docs/architecture.md §6.20).
//!
//! The shell reaches the internet only through Squid, which allows the hosts
//! Ansible lists (`EGRESS_BASE_HOSTS`, read-only here) and the ones the user
//! allowed: from a card in the chat when the agent asks (`request_access`), or
//! in Settings. Those live in the `egress_hosts` table and are written to a
//! file Squid reads (`EGRESS_USER_HOSTS`, in a directory only agentd and Squid
//! can read), after which Squid reloads its configuration. The agent's own
//! commands can't touch that file or Squid (guard.rs: touches_egress_policy).
//!
//! Exact host names only: no wildcards, IP addresses or internal names. Squid
//! refuses internal addresses whatever the name resolves to.

use std::path::Path;
use std::sync::LazyLock;

use anyhow::{Context, Result, bail};

/// At most this many hosts the user allowed.
pub const MAX_HOSTS: usize = 100;
/// At most this many hosts in one request.
pub const MAX_PER_REQUEST: usize = 5;

/// Names that only mean something inside a network.
const INTERNAL_SUFFIXES: &[&str] = &[
    "localhost",
    "local",
    "internal",
    "lan",
    "home",
    "corp",
    "intranet",
    "home.arpa",
    "in-addr.arpa",
    "ip6.arpa",
    "ts.net",
];

/// A host name as Squid's `dstdomain` takes it, from what the model or the
/// user wrote: a bare name, or a URL whose host is taken. Err says what's
/// wrong, in words for the model or the app.
pub fn check_host(raw: &str) -> Result<String, String> {
    let raw = raw.trim();
    if raw.is_empty() {
        return Err("no host given".into());
    }
    let host = if raw.contains("://") {
        url::Url::parse(raw)
            .ok()
            .and_then(|u| u.host_str().map(str::to_string))
            .ok_or_else(|| format!("{raw:?} isn't a URL with a host"))?
    } else {
        // "api.example.com:443" or "api.example.com/path": the name only.
        raw.split(['/', '?', '#']).next().unwrap_or("").to_string()
    };
    let host = host
        .rsplit_once(':')
        .filter(|(_, port)| port.chars().all(|c| c.is_ascii_digit()) && !port.is_empty())
        .map_or(host.as_str(), |(name, _)| name)
        .trim_end_matches('.')
        .to_ascii_lowercase();
    if host.starts_with('.') || host.contains('*') {
        return Err(format!("{raw:?}: give one exact host, not a wildcard"));
    }
    if host.parse::<std::net::IpAddr>().is_ok() || host.starts_with('[') {
        return Err(format!("{raw:?}: give a host name, not an IP address"));
    }
    static LABEL: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"^[a-z0-9]([a-z0-9-]{0,61}[a-z0-9])?$").expect("valid regex"));
    let labels: Vec<&str> = host.split('.').collect();
    if host.len() > 253 || labels.len() < 2 || !labels.iter().all(|l| LABEL.is_match(l)) {
        return Err(format!("{raw:?} isn't a public host name"));
    }
    // The last label is a top-level domain: letters (or punycode).
    let tld = labels[labels.len() - 1];
    if !(tld.starts_with("xn--") || tld.chars().all(|c| c.is_ascii_lowercase())) {
        return Err(format!("{raw:?} isn't a public host name"));
    }
    if INTERNAL_SUFFIXES
        .iter()
        .any(|suffix| host == *suffix || host.ends_with(&format!(".{suffix}")))
    {
        return Err(format!("{raw:?} is an internal name"));
    }
    Ok(host)
}

/// Whether Squid's `dstdomain` list already lets `host` through: an entry
/// equal to it, or `.example.com` for example.com and everything under it.
pub fn covered(host: &str, list: &[String]) -> bool {
    list.iter().any(|entry| match entry.strip_prefix('.') {
        Some(domain) => host == domain || host.ends_with(&format!(".{domain}")),
        None => host == entry,
    })
}

/// The file Squid reads: one host per line. Written whole and renamed into
/// place, so Squid never reads half of it. True if it changed.
pub fn write_list(path: &Path, hosts: &[String]) -> Result<bool> {
    let mut text =
        String::from("# Sites the user allowed in the app. agentd writes this file; don't edit it.\n");
    for host in hosts {
        text.push_str(host);
        text.push('\n');
    }
    if std::fs::read_to_string(path).is_ok_and(|old| old == text) {
        return Ok(false);
    }
    let dir = path.parent().context("the hosts file has no directory")?;
    let temp = dir.join(".allowed-hosts.new");
    std::fs::write(&temp, &text).with_context(|| format!("writing {}", temp.display()))?;
    // Squid reads it through the directory's group (Ansible sets it).
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(0o640))?;
    }
    std::fs::rename(&temp, path).with_context(|| format!("replacing {}", path.display()))?;
    Ok(true)
}

/// Squid reads its configuration (and the hosts file) again. Connections in
/// progress carry on.
pub fn reload_squid() -> Result<()> {
    let status = std::process::Command::new("sudo")
        .args(["-n", "squid", "-k", "reconfigure"])
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .status()
        .context("running squid -k reconfigure")?;
    if !status.success() {
        bail!("squid -k reconfigure exited with {status}");
    }
    Ok(())
}

/// What a command's output says when Squid refused a host that isn't listed
/// (curl, wget, git, pip and requests, Node, Squid's own error page).
pub fn refused_by_proxy(output: &str) -> bool {
    static REFUSED: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)CONNECT tunnel failed, response 403|HTTP code 403 from proxy|Tunnel connection failed: 403|ERR_ACCESS_DENIED|Proxy tunneling failed: Forbidden",
        )
        .expect("valid regex")
    });
    REFUSED.is_match(output)
}

/// What a command's output says when it tried to connect on its own, not
/// through the proxy, and the guard firewall stopped it: no DNS, or a refused
/// connection to somewhere other than this machine (a local server being down
/// is something else). Adding a host to the proxy's list can't help these.
pub fn connected_directly(output: &str) -> bool {
    static NO_DNS: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)Could not resolve host|Temporary failure in name resolution|Name or service not known|no such host|getaddrinfo (EAI_AGAIN|ENOTFOUND)|unable to resolve host|failed to lookup address|UnknownHostException|\blookup \S+ on \S+:53\b",
        )
        .expect("valid regex")
    });
    // Go, curl, Node, ssh, Python: each names where it tried to connect.
    static REFUSED: LazyLock<regex::Regex> = LazyLock::new(|| {
        regex::Regex::new(
            r"(?i)dial tcp (\S+): connect: connection refused|Failed to connect to (\S+) port \d+|connect ECONNREFUSED (\S+)|connect to host (\S+) port \d+: Connection refused|Pool\(host='([^']+)'.*Connection refused",
        )
        .expect("valid regex")
    });
    let this_machine = |address: &str| {
        let host = address.trim_start_matches('[');
        ["localhost", "127.", "::1", "0.0.0.0"]
            .iter()
            .any(|start| host.starts_with(start))
    };
    NO_DNS.is_match(output)
        || REFUSED.captures_iter(output).any(|found| {
            found
                .iter()
                .skip(1)
                .flatten()
                .next()
                .is_some_and(|address| !this_machine(address.as_str()))
        })
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn takes_exact_public_host_names_only() {
        for (given, host) in [
            ("api.example.com", "api.example.com"),
            ("  API.Example.COM. ", "api.example.com"),
            ("https://hooks.example.io/x?y=1", "hooks.example.io"),
            ("registry.example.dev:443", "registry.example.dev"),
            ("pkg.example.com/simple", "pkg.example.com"),
            ("xn--bcher-kva.example", "xn--bcher-kva.example"),
        ] {
            assert_eq!(check_host(given).as_deref(), Ok(host), "{given}");
        }
        for bad in [
            "",
            ".example.com",
            "*.example.com",
            "10.0.0.5",
            "[::1]",
            "http://169.254.169.254/latest",
            "localhost",
            "intranet",
            "db.internal",
            "printer.local",
            "router.home.arpa",
            "desktop.tail1234.ts.net",
            "exa mple.com",
            "-bad.example.com",
            "example.123",
            "file:///etc/passwd",
        ] {
            assert!(check_host(bad).is_err(), "{bad:?} was accepted");
        }
    }

    #[test]
    fn a_leading_dot_covers_the_domain_and_below() {
        let list = vec![".github.com".to_string(), "pypi.org".to_string()];
        assert!(covered("github.com", &list));
        assert!(covered("api.github.com", &list));
        assert!(!covered("notgithub.com", &list));
        assert!(covered("pypi.org", &list));
        assert!(!covered("files.pypi.org", &list));
    }

    #[test]
    fn writes_the_list_only_when_it_changes() {
        let dir = std::env::temp_dir().join(format!("agentd-access-{}", uuid::Uuid::new_v4().simple()));
        std::fs::create_dir_all(&dir).unwrap();
        let path = dir.join("allowed-hosts.txt");
        let hosts = vec!["api.example.com".to_string()];
        assert!(write_list(&path, &hosts).unwrap());
        assert!(!write_list(&path, &hosts).unwrap());
        let text = std::fs::read_to_string(&path).unwrap();
        assert!(text.starts_with('#') && text.ends_with("api.example.com\n"));
        assert!(write_list(&path, &[]).unwrap());
        std::fs::remove_dir_all(dir).unwrap();
    }

    #[test]
    fn recognises_the_proxy_refusing_a_host() {
        assert!(refused_by_proxy("curl: (56) CONNECT tunnel failed, response 403"));
        assert!(refused_by_proxy(
            "fatal: unable to access 'https://x.example/': Received HTTP code 403 from proxy after CONNECT"
        ));
        assert!(refused_by_proxy(
            "ProxyError('Cannot connect to proxy.', OSError('Tunnel connection failed: 403 Forbidden'))"
        ));
        assert!(!refused_by_proxy(
            "HTTP/1.1 403 Forbidden\n{\"error\":\"bad token\"}"
        ));
        assert!(!refused_by_proxy("all good"));
    }

    #[test]
    fn recognises_a_tool_connecting_on_its_own() {
        for output in [
            // flyctl's Depot builder, dialling a build machine by address.
            "rpc error: code = Unavailable desc = connection error: desc = \"transport: Error while dialing: \
             dial tcp 149.248.212.172:443: connect: connection refused\"",
            "Get \"https://api.fly.io/\": dial tcp: lookup api.fly.io on 127.0.0.53:53: read udp \
             127.0.0.1:40000->127.0.0.53:53: read: connection refused",
            "dial tcp: lookup api.fly.io: no such host",
            "curl: (6) Could not resolve host: example.com",
            "curl: (7) Failed to connect to 1.2.3.4 port 443 after 0 ms: Couldn't connect to server",
            "ssh: Could not resolve hostname github.com: Temporary failure in name resolution",
            "ssh: connect to host github.com port 22: Connection refused",
            "wget: unable to resolve host address 'example.com'",
            "Error: getaddrinfo EAI_AGAIN registry.example.com",
            "Error: connect ECONNREFUSED 1.2.3.4:443",
            "socket.gaierror: [Errno -2] Name or service not known",
            "HTTPSConnectionPool(host='api.example.com', port=443): Max retries exceeded with url: / (Caused by \
             NewConnectionError('Failed to establish a new connection: [Errno 111] Connection refused'))",
        ] {
            assert!(connected_directly(output), "{output}");
        }
        for output in [
            // This machine: a local server that isn't up, or the proxy itself.
            "curl: (7) Failed to connect to localhost port 3000 after 0 ms: Couldn't connect to server",
            "fatal: unable to access 'https://github.com/': Failed to connect to 127.0.0.1 port 3128",
            "dial tcp 127.0.0.1:5432: connect: connection refused",
            "dial tcp [::1]:8080: connect: connection refused",
            "Error: connect ECONNREFUSED 127.0.0.1:3000",
            "HTTPConnectionPool(host='localhost', port=8000): Max retries exceeded (Caused by \
             NewConnectionError('[Errno 111] Connection refused'))",
            // The proxy's own refusal is request_access's.
            "curl: (56) CONNECT tunnel failed, response 403",
            "all good",
        ] {
            assert!(!connected_directly(output), "{output}");
        }
    }
}
