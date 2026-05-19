use omnifs_host::runtime::capability::{CapabilityChecker, CapabilityGrants};

fn grants(domains: &[&str]) -> CapabilityGrants {
    CapabilityGrants {
        domains: domains.iter().copied().map(String::from).collect(),
        git_repos: vec![],
        max_memory_mb: 64,
        needs_git: false,
        unix_sockets: Vec::new(),
    }
}

#[test]
fn test_allowed_domain() {
    let checker = CapabilityChecker::new(grants(&["api.github.com"]));
    assert!(
        checker
            .check_url("https://api.github.com/repos/foo/bar")
            .is_ok()
    );
}

#[test]
fn test_denied_domain() {
    let checker = CapabilityChecker::new(grants(&["api.github.com"]));
    let result = checker.check_url("https://evil.com/steal");
    assert!(result.is_err());
}

#[test]
fn test_http_denied_by_default() {
    let checker = CapabilityChecker::new(grants(&["api.github.com"]));
    let result = checker.check_url("http://api.github.com/repos");
    assert!(result.is_err());
}

#[test]
fn test_private_ip_denied() {
    let checker = CapabilityChecker::new(grants(&["localhost"]));
    let result = checker.check_url("https://127.0.0.1/secret");
    assert!(result.is_err());
}

#[test]
fn test_private_ip_v6_denied() {
    let checker = CapabilityChecker::new(grants(&["localhost"]));
    let result = checker.check_url("https://[::1]/secret");
    assert!(result.is_err());
}

#[test]
fn test_link_local_denied() {
    let checker = CapabilityChecker::new(grants(&["*"]));
    let result = checker.check_url("https://169.254.169.254/metadata");
    assert!(result.is_err());
}

#[test]
fn test_git_repo_allowed() {
    let grants = CapabilityGrants {
        domains: vec![],
        git_repos: vec!["github.com/*".to_string()],
        max_memory_mb: 64,
        needs_git: true,
        unix_sockets: Vec::new(),
    };
    let checker = CapabilityChecker::new(grants);
    assert!(checker.check_git_url("github.com/rust-lang/rust").is_ok());
}

#[test]
fn test_git_repo_denied() {
    let grants = CapabilityGrants {
        domains: vec![],
        git_repos: vec!["github.com/myorg/*".to_string()],
        max_memory_mb: 64,
        needs_git: true,
        unix_sockets: Vec::new(),
    };
    let checker = CapabilityChecker::new(grants);
    let result = checker.check_git_url("github.com/other/repo");
    assert!(result.is_err());
}

#[test]
fn test_git_denied_when_not_granted() {
    let checker = CapabilityChecker::new(grants(&[]));
    let result = checker.check_git_url("github.com/foo/bar");
    assert!(result.is_err());
}
