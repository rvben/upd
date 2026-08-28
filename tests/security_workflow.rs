//! Static safety contracts for the repository's dogfooded security audit.

const WORKFLOW: &str = include_str!("../.github/workflows/security-audit.yml");
const CONFIG: &str = include_str!("../.updrc.toml");

#[test]
fn security_audit_dogfoods_the_checked_out_revision() {
    assert!(WORKFLOW.contains("cargo build --locked --bin upd"));
    assert!(WORKFLOW.contains("target/debug/upd audit --format sarif ."));
    assert!(WORKFLOW.contains("security-events: write"));
    assert!(WORKFLOW.contains("category: upd-dependency-audit"));
}

#[test]
fn security_audit_uploads_before_enforcing_findings() {
    let audit = WORKFLOW
        .find("name: Audit repository dependencies")
        .unwrap();
    let upload = WORKFLOW
        .find("name: Upload audit results to code scanning")
        .unwrap();
    let enforce = WORKFLOW.find("name: Enforce audit result").unwrap();

    assert!(audit < upload && upload < enforce);
    assert!(WORKFLOW.contains("echo \"exit-code=$audit_exit\" >> \"$GITHUB_OUTPUT\""));
    assert!(
        WORKFLOW.contains("github.event.pull_request.head.repo.full_name == github.repository")
    );
}

#[test]
fn synthetic_vulnerable_fixture_is_excluded_from_repository_scans() {
    assert!(CONFIG.contains("benchmarks/fixtures/vendor/**"));
    assert!(!WORKFLOW.contains("--no-ignore"));
}
