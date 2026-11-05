//! A small client over the Kubernetes API: JSON in and out, `404` as
//! "absent", server-side apply under the `buckle` field manager, API
//! discovery for arbitrary kinds, and errors that never carry a token or a
//! request body.

use crate::config::{Endpoint, HostCredential};
use crate::object::ObjectKey;
use crate::transport::{HttpTransport, Request, Response, Transport};
use buckle_core::Result;
use buckle_core::error::{system, user};
use secrecy::{ExposeSecret, SecretString};
use serde::Deserialize;
use serde_json::Value;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};

/// The field manager every Buckle apply uses: ownership of each field is
/// visible in the objects' `managedFields`.
pub const FIELD_MANAGER: &str = "buckle";

/// The content type of a server-side apply (JSON is YAML).
pub const APPLY_PATCH: &str = "application/apply-patch+yaml";
pub const MERGE_PATCH: &str = "application/merge-patch+json";

/// A resource type as discovery reports it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ApiResource {
    /// `apps/v1`, or `v1` for the core group.
    pub api_version: String,
    pub kind: String,
    /// The plural used in paths (`deployments`).
    pub plural: String,
    pub namespaced: bool,
}

impl ApiResource {
    /// The collection or object path: `/apis/apps/v1/namespaces/x/deployments/y`.
    pub fn path(&self, namespace: &str, name: Option<&str>) -> String {
        let root = if self.api_version.contains('/') {
            format!("/apis/{}", self.api_version)
        } else {
            format!("/api/{}", self.api_version)
        };
        let mut out = root;
        if self.namespaced && !namespace.is_empty() {
            out.push_str(&format!("/namespaces/{}", seg(namespace)));
        }
        out.push('/');
        out.push_str(&self.plural);
        if let Some(name) = name {
            out.push('/');
            out.push_str(&seg(name));
        }
        out
    }
}

type Discovery = HashMap<String, Option<Arc<Vec<ApiResource>>>>;

/// A Kubernetes API client bound to one cluster and credential.
#[derive(Clone)]
pub struct Kube {
    transport: Arc<dyn Transport>,
    token: Option<Arc<SecretString>>,
    discovery: Arc<Mutex<Discovery>>,
    /// Serialises this executor's index writes (workloads apply
    /// concurrently and would otherwise keep losing the resourceVersion race).
    pub(crate) index_lock: Arc<tokio::sync::Mutex<()>>,
}

impl std::fmt::Debug for Kube {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.debug_struct("Kube")
            .field("transport", &self.transport)
            .field("token", &self.token.as_ref().map(|_| ".."))
            .finish()
    }
}

#[derive(Deserialize)]
struct Status {
    #[serde(default)]
    message: String,
    #[serde(default)]
    reason: String,
}

impl Kube {
    /// A client for `endpoint`, carrying the host's own credential.
    pub fn connect(endpoint: Endpoint) -> Result<Self> {
        let transport = HttpTransport::new(&endpoint.server, &endpoint.tls)?;
        let kube = Self::with_transport(Arc::new(transport));
        Ok(match endpoint.credential {
            HostCredential::Token(t) => kube.token(t),
            HostCredential::None | HostCredential::ClientCertificate => kube,
        })
    }

    pub fn with_transport(transport: Arc<dyn Transport>) -> Self {
        Self {
            transport,
            token: None,
            discovery: Arc::new(Mutex::new(HashMap::new())),
            index_lock: Arc::new(tokio::sync::Mutex::new(())),
        }
    }

    /// Sends `Authorization: Bearer` on every request.
    pub fn token(mut self, token: SecretString) -> Self {
        self.token = Some(Arc::new(token));
        self
    }

    /// Sends a request and returns the raw response, whatever its status.
    pub async fn send(
        &self,
        method: &'static str,
        path: &str,
        content_type: Option<&'static str>,
        body: Option<Vec<u8>>,
    ) -> Result<Response> {
        let mut headers = Vec::new();
        if let Some(token) = &self.token {
            headers.push((
                "Authorization".to_owned(),
                format!("Bearer {}", token.expose_secret()),
            ));
        }
        self.transport
            .send(Request {
                method,
                path: path.to_owned(),
                headers,
                content_type,
                body,
            })
            .await
    }

    /// `GET path`, decoding JSON; `None` on 404.
    pub async fn get(&self, path: &str) -> Result<Option<Value>> {
        let resp = self.send("GET", path, None, None).await?;
        if resp.status == 404 {
            return Ok(None);
        }
        let resp = self.ok("GET", path, resp)?;
        decode(path, &resp.body).map(Some)
    }

    /// Server-side apply of `body` (an object, as JSON) at its object path,
    /// forcing ownership of the fields it sets. `dry_run` changes nothing
    /// and returns the object as it would be. A `409` is returned as
    /// [`Applied::Conflict`].
    pub async fn apply(&self, path: &str, body: Vec<u8>, dry_run: bool) -> Result<Applied> {
        let mut url = format!("{path}?fieldManager={FIELD_MANAGER}&force=true");
        if dry_run {
            url.push_str("&dryRun=All");
        }
        let resp = self
            .send("PATCH", &url, Some(APPLY_PATCH), Some(body))
            .await?;
        if resp.status == 409 {
            return Ok(Applied::Conflict(status_message(&resp)));
        }
        let resp = self.ok("PATCH", path, resp)?;
        decode(path, &resp.body).map(Applied::Object)
    }

    /// A JSON merge patch (an action's annotation, a rollback).
    pub async fn merge_patch(&self, path: &str, patch: &Value) -> Result<Value> {
        let resp = self
            .send(
                "PATCH",
                &format!("{path}?fieldManager={FIELD_MANAGER}"),
                Some(MERGE_PATCH),
                Some(serde_json::to_vec(patch).unwrap_or_default()),
            )
            .await?;
        let resp = self.ok("PATCH", path, resp)?;
        decode(path, &resp.body)
    }

    /// `PUT path` (a replace under a resourceVersion precondition); `false`
    /// when the object changed in between (`409`).
    pub async fn replace(&self, path: &str, body: &Value) -> Result<bool> {
        let resp = self
            .send(
                "PUT",
                &format!("{path}?fieldManager={FIELD_MANAGER}"),
                Some("application/json"),
                Some(serde_json::to_vec(body).unwrap_or_default()),
            )
            .await?;
        if resp.status == 409 {
            return Ok(false);
        }
        self.ok("PUT", path, resp)?;
        Ok(true)
    }

    /// `POST path`; `false` when the object already exists (`409`).
    pub async fn create(&self, path: &str, body: &Value) -> Result<bool> {
        let resp = self
            .send(
                "POST",
                &format!("{path}?fieldManager={FIELD_MANAGER}"),
                Some("application/json"),
                Some(serde_json::to_vec(body).unwrap_or_default()),
            )
            .await?;
        if resp.status == 409 {
            return Ok(false);
        }
        self.ok("POST", path, resp)?;
        Ok(true)
    }

    /// `DELETE path` in the background; `false` when it was already gone.
    pub async fn delete(&self, path: &str) -> Result<bool> {
        let resp = self
            .send(
                "DELETE",
                &format!("{path}?propagationPolicy=Background"),
                None,
                None,
            )
            .await?;
        if resp.status == 404 {
            return Ok(false);
        }
        self.ok("DELETE", path, resp)?;
        Ok(true)
    }

    /// The resource type of `api_version`/`kind`, from the API's discovery
    /// documents (cached per client). `None` when the cluster does not serve
    /// it (yet: a CRD the same apply creates).
    pub async fn resource(&self, api_version: &str, kind: &str) -> Result<Option<ApiResource>> {
        let cached = self.discovery.lock().unwrap().get(api_version).cloned();
        let list = match cached {
            Some(list) => list,
            None => {
                let path = if api_version.contains('/') {
                    format!("/apis/{api_version}")
                } else {
                    format!("/api/{api_version}")
                };
                let list = self.get(&path).await?.map(|doc| {
                    Arc::new(
                        doc["resources"]
                            .as_array()
                            .into_iter()
                            .flatten()
                            .filter_map(|r| {
                                let plural = r["name"].as_str()?;
                                if plural.contains('/') {
                                    return None;
                                }
                                Some(ApiResource {
                                    api_version: api_version.to_owned(),
                                    kind: r["kind"].as_str()?.to_owned(),
                                    plural: plural.to_owned(),
                                    namespaced: r["namespaced"].as_bool().unwrap_or(false),
                                })
                            })
                            .collect::<Vec<_>>(),
                    )
                });
                // A missing group version is not cached: a CRD may add it.
                if list.is_some() {
                    self.discovery
                        .lock()
                        .unwrap()
                        .insert(api_version.to_owned(), list.clone());
                }
                list
            }
        };
        Ok(list.and_then(|l| l.iter().find(|r| r.kind == kind).cloned()))
    }

    /// The resource type of `key`, or a plan error naming it.
    pub async fn resource_of(&self, key: &ObjectKey) -> Result<ApiResource> {
        self.resource(&key.api_version, &key.kind)
            .await?
            .ok_or_else(|| {
                user(
                    format!(
                        "The cluster does not serve {} {} ({key}).",
                        key.api_version, key.kind
                    ),
                    &["Check the manifest's apiVersion and kind, or apply the CustomResourceDefinition first."],
                )
            })
    }

    /// Forgets cached discovery (after a CRD was applied).
    pub fn refresh_discovery(&self) {
        self.discovery.lock().unwrap().clear();
    }

    /// Turns a non-2xx response into an error naming the request (never its
    /// body or token).
    pub fn ok(&self, method: &str, path: &str, resp: Response) -> Result<Response> {
        if (200..300).contains(&resp.status) {
            return Ok(resp);
        }
        let message = status_message(&resp);
        let path = path.split('?').next().unwrap_or(path);
        Err(match resp.status {
            401 => user(
                format!("The Kubernetes API refused the executor's credential ({method} {path})."),
                &[
                    "Check the cluster's identity and trust setup: for buckle_jwt, the API server's authentication config must trust Buckle's issuer.",
                ],
            ),
            403 => user(
                format!("The executor may not {method} {path}: {message}"),
                &[
                    "Grant the executor's user or group this permission through RBAC (`buckle:deploy` for applies).",
                ],
            ),
            400 | 404 | 409 | 422 => user(
                format!("The Kubernetes API rejected {method} {path}: {message}"),
                &["Fix the manifest and plan again."],
            ),
            status => system(
                format!("The Kubernetes API answered {status} to {method} {path}: {message}"),
                &["The API server may be restarting; retry shortly."],
            ),
        })
    }
}

/// The outcome of a server-side apply.
#[derive(Debug, Clone)]
pub enum Applied {
    Object(Value),
    /// The object changed under a resourceVersion precondition, or another
    /// manager's conflict could not be forced.
    Conflict(String),
}

fn status_message(resp: &Response) -> String {
    match serde_json::from_slice::<Status>(&resp.body) {
        Ok(s) if !s.message.is_empty() => s.message,
        Ok(s) if !s.reason.is_empty() => s.reason,
        _ => format!("HTTP {}", resp.status),
    }
}

fn decode(path: &str, body: &[u8]) -> Result<Value> {
    serde_json::from_slice(body).map_err(|e| {
        buckle_core::error::wrap_system(
            e,
            format!("The Kubernetes API answered {path} with something other than JSON."),
            &[],
        )
    })
}

/// Percent-encodes one path segment.
pub fn seg(s: &str) -> String {
    percent_encoding::utf8_percent_encode(s, percent_encoding::NON_ALPHANUMERIC)
        .to_string()
        .replace("%2D", "-")
        .replace("%2E", ".")
        .replace("%5F", "_")
}

/// Appends `key=value` to a path's query, percent-encoding the value.
pub fn with_query(path: &str, key: &str, value: &str) -> String {
    let sep = if path.contains('?') { '&' } else { '?' };
    format!(
        "{path}{sep}{key}={}",
        percent_encoding::utf8_percent_encode(value, percent_encoding::NON_ALPHANUMERIC)
    )
}
