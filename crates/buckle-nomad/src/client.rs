//! A small typed client over the Nomad HTTP API.
//!
//! Only what the primitives need: JSON in and out, `404` as "absent", the
//! `X-Nomad-Index` of a read, and errors that never carry a token, a request
//! body or the body of a variable response (Nomad returns the current
//! variable, items included, on a check-and-set conflict).

use crate::transport::{self, Address, Request, Response, Transport};
use buckle_core::Result;
use buckle_core::error::{system, user};
use buckle_core::secrets::redact::Redactor;
use secrecy::{ExposeSecret, SecretString};
use serde::Serialize;
use serde::de::DeserializeOwned;
use std::sync::Arc;

/// A Nomad API client bound to one cluster and credential.
#[derive(Clone)]
pub struct Nomad {
    transport: Arc<dyn Transport>,
    token: Option<Arc<SecretString>>,
    region: Option<String>,
    redactor: Option<Arc<Redactor>>,
}

impl std::fmt::Debug for Nomad {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Nomad")
            .field("transport", &self.transport)
            .field("token", &self.token.as_ref().map(|_| ".."))
            .field("region", &self.region)
            .finish()
    }
}

/// A read's value together with the index Nomad served it at.
#[derive(Debug, Clone)]
pub struct Indexed<T> {
    pub value: T,
    pub index: u64,
}

impl Nomad {
    /// A client for `addr` (`http://`, `https://` or `unix://`).
    pub fn connect(addr: &str, ca_pem: Option<&[u8]>) -> Result<Self> {
        Ok(Self::with_transport(transport::connect(
            &Address::parse(addr)?,
            ca_pem,
        )?))
    }

    pub fn with_transport(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            token: None,
            region: None,
            redactor: None,
        }
    }

    /// Sends `X-Nomad-Token` on every request. Over the task API socket no
    /// token is needed: Nomad attaches the workload identity itself.
    pub fn token(mut self, token: SecretString) -> Self {
        self.token = Some(Arc::new(token));
        self
    }

    pub fn region(mut self, region: impl Into<String>) -> Self {
        self.region = Some(region.into());
        self
    }

    /// Scrubs known secret values from every error message, so an API that
    /// echoes a value back in an error cannot leak it.
    pub fn redactor(mut self, redactor: Arc<Redactor>) -> Self {
        self.redactor = Some(redactor);
        self
    }

    /// Sends a request and returns the raw response, whatever its status.
    pub async fn send(
        &self,
        method: &'static str,
        path: &str,
        body: Option<Vec<u8>>,
    ) -> Result<Response> {
        let mut headers = Vec::new();
        if let Some(token) = &self.token {
            headers.push(("X-Nomad-Token".to_owned(), token.expose_secret().to_owned()));
        }
        let path = match &self.region {
            Some(region) => with_query(path, "region", region),
            None => path.to_owned(),
        };
        let resp = self
            .transport
            .send(Request {
                method,
                path,
                headers,
                body,
            })
            .await
            .map_err(|e| self.scrub(e))?;
        Ok(resp)
    }

    /// `GET path`, decoding JSON; `None` on 404.
    pub async fn get<T: DeserializeOwned>(&self, path: &str) -> Result<Option<T>> {
        Ok(self.get_indexed(path).await?.map(|i| i.value))
    }

    /// `GET path` with its `X-Nomad-Index`; `None` on 404.
    pub async fn get_indexed<T: DeserializeOwned>(&self, path: &str) -> Result<Option<Indexed<T>>> {
        let resp = self.send("GET", path, None).await?;
        if resp.status == 404 {
            return Ok(None);
        }
        let resp = self.ok("GET", path, resp)?;
        let index = resp
            .header("x-nomad-index")
            .and_then(|i| i.parse().ok())
            .unwrap_or(0);
        Ok(Some(Indexed {
            value: self.decode("GET", path, &resp)?,
            index,
        }))
    }

    /// `method path` with a JSON body, decoding the JSON answer.
    pub async fn write<B: Serialize + ?Sized, T: DeserializeOwned>(
        &self,
        method: &'static str,
        path: &str,
        body: &B,
    ) -> Result<T> {
        let bytes = serde_json::to_vec(body).map_err(|e| {
            human_errors::wrap_system(e, "A Nomad request could not be encoded.", &[])
        })?;
        let resp = self.send(method, path, Some(bytes)).await?;
        let resp = self.ok(method, path, resp)?;
        self.decode(method, path, &resp)
    }

    /// `DELETE path`; a 404 counts as done.
    pub async fn delete(&self, path: &str) -> Result<()> {
        let resp = self.send("DELETE", path, None).await?;
        if resp.status == 404 {
            return Ok(());
        }
        self.ok("DELETE", path, resp)?;
        Ok(())
    }

    /// Maps a non-2xx response to an error. Bodies of variable responses
    /// never appear in it.
    pub fn ok(&self, method: &str, path: &str, resp: Response) -> Result<Response> {
        if (200..300).contains(&resp.status) {
            return Ok(resp);
        }
        let shown_path = path.split('?').next().unwrap_or(path);
        let detail = if is_variable_path(path) {
            String::new()
        } else {
            let text = String::from_utf8_lossy(&resp.body);
            let text = text.trim();
            if text.is_empty() {
                String::new()
            } else {
                format!(": {}", truncate(text, 500))
            }
        };
        let message = self.scrub_text(&format!(
            "Nomad answered {} to {method} {shown_path}{detail}",
            resp.status
        ));
        Err(match resp.status {
            401 | 403 => user(
                message,
                &[
                    "Check that the executor's credential is valid and that its policy grants this operation.",
                ],
            ),
            400 | 404 | 409 | 422 => user(
                message,
                &["Check the workload's spec files against Nomad's documentation."],
            ),
            _ => system(
                message,
                &["Check the Nomad servers' logs; retry once the cluster is healthy."],
            ),
        })
    }

    fn decode<T: DeserializeOwned>(&self, method: &str, path: &str, resp: &Response) -> Result<T> {
        let body = if resp.body.is_empty() {
            b"null".as_slice()
        } else {
            &resp.body
        };
        serde_json::from_slice(body).map_err(|e| {
            let shown_path = path.split('?').next().unwrap_or(path);
            system(
                format!("Nomad's answer to {method} {shown_path} could not be read ({e})."),
                &["Check that the Nomad version is supported (1.7 or later)."],
            )
        })
    }

    fn scrub_text(&self, text: &str) -> String {
        match &self.redactor {
            Some(r) => r.redact(text).into_inner(),
            None => text.to_owned(),
        }
    }

    fn scrub(&self, err: buckle_core::Error) -> buckle_core::Error {
        match &self.redactor {
            Some(_) => user(self.scrub_text(&err.message()), &[]),
            None => err,
        }
    }
}

fn is_variable_path(path: &str) -> bool {
    path.starts_with("/v1/var/") || path.starts_with("/v1/vars")
}

fn truncate(text: &str, max: usize) -> &str {
    match text.char_indices().nth(max) {
        Some((i, _)) => &text[..i],
        None => text,
    }
}

/// Percent-encodes one path segment (a job id, a policy name).
pub fn seg(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, SEGMENT).to_string()
}

/// Percent-encodes a slash-separated path (a variable path), keeping the slashes.
pub fn path(s: &str) -> String {
    s.split('/').map(seg).collect::<Vec<_>>().join("/")
}

const SEGMENT: &percent_encoding::AsciiSet = &percent_encoding::NON_ALPHANUMERIC
    .remove(b'-')
    .remove(b'.')
    .remove(b'_')
    .remove(b'~');

/// Appends `key=value` to a path's query string.
pub fn with_query(path: &str, key: &str, value: &str) -> String {
    let sep = if path.contains('?') { '&' } else { '?' };
    format!("{path}{sep}{key}={}", seg(value))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::testing::mock;
    use wiremock::matchers::{header, method, path as wpath, query_param};
    use wiremock::{Mock, ResponseTemplate};

    #[tokio::test]
    async fn reads_decode_json_and_keep_the_index() {
        let (server, nomad) = mock().await;
        Mock::given(method("GET"))
            .and(wpath("/v1/job/paste"))
            .and(header("X-Nomad-Token", "t0k"))
            .respond_with(
                ResponseTemplate::new(200)
                    .insert_header("X-Nomad-Index", "42")
                    .set_body_json(serde_json::json!({"ID": "paste"})),
            )
            .mount(&server)
            .await;
        let nomad = nomad.token(SecretString::from("t0k"));
        let got: Indexed<serde_json::Value> =
            nomad.get_indexed("/v1/job/paste").await.unwrap().unwrap();
        assert_eq!(got.index, 42);
        assert_eq!(got.value["ID"], "paste");
        assert!(!format!("{nomad:?}").contains("t0k"));
    }

    #[tokio::test]
    async fn not_found_is_absent() {
        let (server, nomad) = mock().await;
        Mock::given(method("GET"))
            .respond_with(ResponseTemplate::new(404).set_body_string("job not found"))
            .mount(&server)
            .await;
        assert!(
            nomad
                .get::<serde_json::Value>("/v1/job/x")
                .await
                .unwrap()
                .is_none()
        );
        nomad.delete("/v1/job/x").await.unwrap();
    }

    #[tokio::test]
    async fn variable_error_bodies_never_reach_the_message() {
        let (server, nomad) = mock().await;
        Mock::given(method("PUT"))
            .and(wpath("/v1/var/nomad/jobs/atuin"))
            .and(query_param("cas", "3"))
            .respond_with(
                ResponseTemplate::new(409)
                    .set_body_json(serde_json::json!({"Items": {"DB": "hunter2"}})),
            )
            .mount(&server)
            .await;
        let err = nomad
            .write::<_, serde_json::Value>(
                "PUT",
                "/v1/var/nomad/jobs/atuin?cas=3",
                &serde_json::json!({}),
            )
            .await
            .unwrap_err();
        let shown = format!("{} {}", err.message(), err.description());
        assert!(shown.contains("409"), "{shown}");
        assert!(
            !shown.contains("hunter2") && !shown.contains("cas=3"),
            "{shown}"
        );
    }

    #[tokio::test]
    async fn other_error_bodies_are_shown_and_scrubbed() {
        use buckle_core::secrets::{InstallKey, Secret, SecretStore};
        let (server, nomad) = mock().await;
        Mock::given(method("POST"))
            .respond_with(ResponseTemplate::new(400).set_body_string("bad value hunter2 in job"))
            .mount(&server)
            .await;
        let mut store = SecretStore::new();
        store.insert(Secret::new(
            "db",
            "hunter2".into(),
            &InstallKey::from_bytes([1; 32]),
        ));
        let nomad = nomad.redactor(Arc::new(Redactor::new(&store)));
        let err = nomad
            .write::<_, serde_json::Value>("POST", "/v1/jobs", &serde_json::json!({}))
            .await
            .unwrap_err();
        assert!(err.message().contains("bad value"), "{}", err.message());
        assert!(!err.message().contains("hunter2"), "{}", err.message());
        assert!(err.is(buckle_core::error::Kind::User));
    }

    #[rstest::rstest]
    #[case("nomad/jobs/atuin", "nomad/jobs/atuin")]
    #[case("a b/c?d", "a%20b/c%3Fd")]
    fn variable_paths_keep_their_slashes(#[case] input: &str, #[case] expected: &str) {
        assert_eq!(path(input), expected);
    }
}
