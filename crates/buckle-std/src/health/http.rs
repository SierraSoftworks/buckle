//! The `http` health check.

use async_trait::async_trait;
use buckle_core::health::{HealthCheck, HealthStatus};
use buckle_core::secrets::SecretField;
use buckle_core::system::HttpRequest;
use buckle_core::task::Cx;
use schemars::JsonSchema;
use serde::{Deserialize, Serialize};
use std::collections::BTreeMap;
use std::time::Duration;

/// Requests a URL and checks the response.
///
/// `method` defaults to `GET`; `headers` are sent as given. By default any
/// 2xx status passes; `expect.status` requires one exact status, and
/// `expect.body_contains` requires the response body to contain a string.
/// `tls.insecure: true` accepts any certificate, for local endpoints with
/// self-signed certificates. A connection error or timeout fails the check
/// (the service is not answering); `timeout` defaults to 10s.
///
/// Used for the Nomad agent health endpoint, Caddy's admin API or the
/// OpenTelemetry collector's health extension.
pub struct HttpCheck;

#[derive(Debug, Clone, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Spec {
    /// The URL to request.
    pub url: String,
    /// The HTTP method; defaults to `GET`.
    #[serde(default = "default_method")]
    pub method: String,
    /// Request headers. Values may reference secrets (`{{ secrets.token }}`).
    #[serde(default)]
    pub headers: BTreeMap<String, SecretField>,
    /// What the response must look like.
    #[serde(default)]
    pub expect: Expect,
    /// TLS options.
    #[serde(default)]
    pub tls: Tls,
    /// How long to wait for the response; defaults to 10s.
    #[serde(default = "default_timeout", with = "humantime_serde")]
    #[schemars(with = "String")]
    pub timeout: Duration,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Expect {
    /// The exact status code required; any 2xx when omitted.
    #[serde(default)]
    pub status: Option<u16>,
    /// A string the response body must contain.
    #[serde(default)]
    pub body_contains: Option<String>,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize, JsonSchema)]
#[serde(deny_unknown_fields)]
pub struct Tls {
    /// Accept any TLS certificate.
    #[serde(default)]
    pub insecure: bool,
}

fn default_method() -> String {
    "GET".into()
}

fn default_timeout() -> Duration {
    Duration::from_secs(10)
}

#[async_trait]
impl HealthCheck for HttpCheck {
    type Spec = Spec;
    const KIND: &'static str = "http";
    const DESCRIPTION: &'static str =
        "Request a URL and check the status code and, optionally, the body.";

    #[tracing::instrument(name = "health.http", skip_all, fields(url = %spec.url))]
    async fn check(&self, cx: &Cx, spec: &Spec) -> HealthStatus {
        // Reasons can quote command output or response bodies: scrub known secrets.
        super::redacted(cx, self.probe(cx, spec).await)
    }
}

impl HttpCheck {
    async fn probe(&self, cx: &Cx, spec: &Spec) -> HealthStatus {
        let method = spec.method.to_ascii_uppercase();
        let headers = match crate::tasks::hostkit::resolve_headers(cx, &spec.headers) {
            Ok(h) => h,
            Err(e) => {
                return HealthStatus::unknown(format!(
                    "the request headers could not be rendered: {}",
                    e.description()
                ));
            }
        };
        let request = HttpRequest {
            method: method.clone(),
            url: spec.url.clone(),
            headers,
            body: None,
            timeout: Some(spec.timeout),
            insecure: spec.tls.insecure,
        };
        let response = match cx.system.http(request).await {
            Ok(r) => r,
            Err(e) => {
                return HealthStatus::fail(format!(
                    "{method} {} failed: {}",
                    spec.url,
                    e.description()
                ));
            }
        };
        match spec.expect.status {
            Some(expected) if response.status != expected => {
                return HealthStatus::fail(format!(
                    "{method} {} returned status {} (expected {expected})",
                    spec.url, response.status
                ));
            }
            None if !(200..300).contains(&response.status) => {
                return HealthStatus::fail(format!(
                    "{method} {} returned status {} (expected 2xx)",
                    spec.url, response.status
                ));
            }
            _ => {}
        }
        if let Some(needle) = &spec.expect.body_contains
            && !String::from_utf8_lossy(&response.body).contains(needle.as_str())
        {
            return HealthStatus::fail(format!(
                "{method} {} returned status {} but the body does not contain \"{needle}\"",
                spec.url, response.status
            ));
        }
        HealthStatus::Pass
    }
}

buckle_core::register_health!(HttpCheck);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::{cx, spec};
    use buckle_core::system::HttpResponse;
    use buckle_core::testing::FakeSystem;
    use rstest::rstest;
    use serde_json::json;

    fn server(status: u16, body: &'static str) -> FakeSystem {
        FakeSystem::new().on_http("http://127.0.0.1:4646/", move |_| HttpResponse {
            status,
            headers: vec![],
            body: body.as_bytes().to_vec(),
        })
    }

    fn url() -> serde_json::Value {
        json!("http://127.0.0.1:4646/v1/agent/health")
    }

    #[rstest]
    #[case::ok(200, "{}", json!({"url": url()}), None)]
    #[case::any_2xx_passes(204, "", json!({"url": url()}), None)]
    #[case::exact_status(404, "", json!({"url": url(), "expect": {"status": 404}}), None)]
    #[case::body_matches(200, "{\"status\":\"ok\"}", json!({"url": url(), "expect": {"body_contains": "\"ok\""}}), None)]
    #[case::server_error(503, "down", json!({"url": url()}), Some("returned status 503 (expected 2xx)"))]
    #[case::redirect_is_not_2xx(302, "", json!({"url": url()}), Some("status 302"))]
    #[case::wrong_status(200, "", json!({"url": url(), "expect": {"status": 204}}), Some("returned status 200 (expected 204)"))]
    #[case::body_missing(200, "{\"status\":\"degraded\"}", json!({"url": url(), "expect": {"body_contains": "\"ok\""}}), Some("the body does not contain"))]
    #[tokio::test]
    async fn status_and_body_expectations(
        #[case] status: u16,
        #[case] body: &'static str,
        #[case] spec_json: serde_json::Value,
        #[case] failure: Option<&str>,
    ) {
        let sys = server(status, body);
        let result = HttpCheck.check(&cx(&sys), &spec(spec_json)).await;
        match failure {
            None => assert_eq!(result, HealthStatus::Pass),
            Some(needle) => match result {
                HealthStatus::Fail { reason } => assert!(reason.contains(needle), "{reason}"),
                other => panic!("expected failure containing {needle:?}, got {other:?}"),
            },
        }
    }

    #[tokio::test]
    async fn unreachable_endpoints_fail_with_the_url() {
        let result = HttpCheck
            .check(
                &cx(&FakeSystem::new()),
                &spec(json!({"url": "http://127.0.0.1:9/"})),
            )
            .await;
        match result {
            HealthStatus::Fail { reason } => assert!(
                reason.starts_with("GET http://127.0.0.1:9/ failed"),
                "{reason}"
            ),
            other => panic!("unexpected {other:?}"),
        }
    }

    #[tokio::test]
    async fn the_request_carries_method_headers_timeout_and_tls_settings() {
        let sys = server(200, "");
        let s: Spec = spec(json!({
            "url": url(),
            "method": "head",
            "headers": {"X-Token": "abc"},
            "tls": {"insecure": true},
            "timeout": "3s",
        }));
        assert_eq!(HttpCheck.check(&cx(&sys), &s).await, HealthStatus::Pass);
        let req = &sys.http_requests()[0];
        assert_eq!(req.method, "HEAD");
        assert_eq!(req.headers, vec![("X-Token".to_owned(), "abc".to_owned())]);
        assert_eq!(req.timeout, Some(Duration::from_secs(3)));
        assert!(req.insecure);
        // Defaults.
        let d: Spec = spec(json!({"url": url()}));
        assert_eq!(
            (d.method.as_str(), d.timeout, d.tls.insecure),
            ("GET", Duration::from_secs(10), false)
        );
    }
}
