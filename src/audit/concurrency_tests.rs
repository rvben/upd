use super::{Ecosystem, MAX_CONCURRENT_PACKAGES, MAX_CONCURRENT_REQUESTS, OsvClient, Package};
use serde_json::{Value, json};
use std::sync::Arc;
use std::time::Duration;
use wiremock::matchers::{method, path, path_regex};
use wiremock::{Mock, MockServer, ResponseTemplate};

fn packages(n: usize) -> Vec<Package> {
    (0..n)
        .map(|i| Package {
            name: format!("pkg-{i}"),
            version: "1.0.0".into(),
            ecosystem: Ecosystem::Npm,
        })
        .collect()
}

async fn batch(server: &MockServer, n: usize, per_package: usize) {
    let results: Vec<_> = (0..n)
        .map(|i| {
            let vulns: Vec<_> = (0..per_package)
                .map(|j| json!({"id": format!("ADV-{i}-{j}")}))
                .collect();
            json!({"vulns": vulns})
        })
        .collect();
    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(json!({"results":results})))
        .mount(server)
        .await;
}

async fn detail_count(server: &MockServer) -> usize {
    server
        .received_requests()
        .await
        .unwrap()
        .iter()
        .filter(|r| r.url.path().starts_with("/vulns/"))
        .count()
}

async fn wait_for_details(server: &MockServer, n: usize) {
    loop {
        if detail_count(server).await >= n {
            return;
        }
        tokio::time::sleep(Duration::from_millis(5)).await;
    }
}

#[tokio::test]
async fn detail_limit_is_shared_across_packages_and_concurrent_audits() {
    let server = MockServer::start().await;
    batch(&server, 40, 2).await;
    Mock::given(method("GET"))
        .and(path_regex("^/vulns/"))
        .respond_with(ResponseTemplate::new(503).set_delay(Duration::from_secs(30)))
        .mount(&server)
        .await;
    let client = Arc::new(OsvClient::with_base_url(server.uri()));
    let run = || {
        let client = client.clone();
        tokio::spawn(async move { client.check_packages(&packages(40)).await })
    };
    let first = run();
    let second = run();
    let filled = tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_details(&server, MAX_CONCURRENT_REQUESTS),
    )
    .await;
    let overflow = tokio::time::timeout(
        Duration::from_millis(100),
        wait_for_details(&server, MAX_CONCURRENT_REQUESTS + 1),
    )
    .await;
    first.abort();
    second.abort();
    assert!(first.await.unwrap_err().is_cancelled());
    assert!(second.await.unwrap_err().is_cancelled());
    assert!(filled.is_ok(), "independent packages did not overlap");
    assert!(
        overflow.is_err(),
        "detail limit multiplied across packages or calls"
    );
    // Dropping the request futures must release permits; a subsequent audit
    // can enter the transport even while old server responses remain blocked.
    let before = detail_count(&server).await;
    let retry = run();
    let restarted = tokio::time::timeout(
        Duration::from_secs(5),
        wait_for_details(&server, before + MAX_CONCURRENT_REQUESTS),
    )
    .await;
    retry.abort();
    assert!(retry.await.unwrap_err().is_cancelled());
    assert!(restarted.is_ok(), "cancellation leaked detail permits");
}

#[tokio::test]
async fn slow_first_package_does_not_hold_later_packages_outside_the_window() {
    let server = MockServer::start().await;
    let n = MAX_CONCURRENT_PACKAGES + 5;
    batch(&server, n, 1).await;
    Mock::given(method("GET"))
        .and(path_regex("^/vulns/"))
        .respond_with(|req: &wiremock::Request| {
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":req.url.path().rsplit('/').next().unwrap()}))
        })
        .mount(&server)
        .await;
    Mock::given(method("GET"))
        .and(path("/vulns/ADV-0-0"))
        .respond_with(
            ResponseTemplate::new(200)
                .set_body_json(json!({"id":"ADV-0-0"}))
                .set_delay(Duration::from_secs(30)),
        )
        .with_priority(1)
        .mount(&server)
        .await;
    let client = OsvClient::with_base_url(server.uri());
    let run = tokio::spawn(async move { client.check_packages(&packages(n)).await });
    let last_path = format!("/vulns/ADV-{}-0", n - 1);
    let progressed = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            if server
                .received_requests()
                .await
                .unwrap()
                .iter()
                .any(|r| r.url.path() == last_path.as_str())
            {
                break;
            }
            tokio::time::sleep(Duration::from_millis(5)).await;
        }
    })
    .await;
    let still_waiting = !run.is_finished();
    run.abort();
    assert!(run.await.unwrap_err().is_cancelled());
    assert!(
        progressed.is_ok(),
        "later packages waited for the slow first result"
    );
    assert!(still_waiting, "slow response was not actually held open");
}

#[tokio::test]
async fn results_keep_package_order_scoped_fixes_and_detail_failure_fallback() {
    let server = MockServer::start().await;
    let results = json!({"results": [
        {"vulns": [{"id": "SLOW"}]},
        {"vulns": [{"id": "FAST"}]},
        {"vulns": []},
        {"vulns": [{"id": "FAILED"}]}
    ]});
    Mock::given(method("POST"))
        .and(path("/querybatch"))
        .respond_with(ResponseTemplate::new(200).set_body_json(results))
        .mount(&server)
        .await;
    let affected: Vec<Value> = [(0, "2.0.0"), (1, "3.0.0")]
        .into_iter()
        .map(|(i, fixed)| {
            json!({
                "package": {"name": format!("pkg-{i}"), "ecosystem": "npm"},
                "ranges": [{"type": "SEMVER", "events": [
                    {"introduced": "0"}, {"fixed": fixed}
                ]}]
            })
        })
        .collect();
    for (id, delay) in [("SLOW", 100), ("FAST", 0)] {
        Mock::given(method("GET"))
            .and(path(format!("/vulns/{id}")))
            .respond_with(
                ResponseTemplate::new(200)
                    .set_body_json(json!({"id":id,"affected":affected}))
                    .set_delay(Duration::from_millis(delay)),
            )
            .mount(&server)
            .await;
    }
    Mock::given(method("GET"))
        .and(path("/vulns/FAILED"))
        .respond_with(ResponseTemplate::new(503))
        .mount(&server)
        .await;
    let client = OsvClient::with_base_url(server.uri());
    let result = client.check_packages(&packages(4)).await.unwrap();
    assert!(result.errors.is_empty());
    assert_eq!(result.safe_count, 1);
    assert_eq!(
        result
            .vulnerable
            .iter()
            .map(|r| r.package.name.as_str())
            .collect::<Vec<_>>(),
        ["pkg-0", "pkg-1", "pkg-3"]
    );
    assert_eq!(
        result.vulnerable[0].vulnerabilities[0]
            .fixed_version
            .as_deref(),
        Some("2.0.0")
    );
    assert_eq!(
        result.vulnerable[1].vulnerabilities[0]
            .fixed_version
            .as_deref(),
        Some("3.0.0")
    );
    let fallback = &result.vulnerable[2].vulnerabilities[0];
    assert_eq!(fallback.id, "FAILED");
    assert!(fallback.summary.is_none());
    assert!(fallback.fixed_version.is_none());
    assert_eq!(detail_count(&server).await, 3);
}
