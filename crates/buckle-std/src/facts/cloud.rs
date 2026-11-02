//! The `cloud` fact: which cloud (if any) this instance runs in, from its metadata service.

use async_trait::async_trait;
use buckle_core::facts::FactProvider;
use buckle_core::system::HttpRequest;
use buckle_core::task::Cx;
use buckle_core::{Platforms, Result};
use serde_json::{Value, json};
use std::time::Duration;

/// Detects the cloud provider by asking the link-local instance metadata
/// service (IMDS) at `169.254.169.254`, with a two second timeout per probe.
///
/// On Oracle Cloud (`oci`) the fact is `{provider, region, shape, id,
/// fault_domain, display_name}`; on Azure (`azure`) it is `{provider, region,
/// sku, id, name, zone}`, where `sku` is the VM size. Anywhere else, and
/// whenever the metadata service is unreachable or returns something
/// unexpected, the fact is `null`: a broken IMDS must never fail a run, so
/// select with `host.facts.cloud.provider == "oci"`. Refreshed hourly.
pub struct CloudFacts;

const IMDS_TIMEOUT: Duration = Duration::from_secs(2);
const OCI_URL: &str = "http://169.254.169.254/opc/v2/instance/";
const AZURE_URL: &str = "http://169.254.169.254/metadata/instance?api-version=2021-02-01";

fn text(v: &Value) -> Value {
    v.as_str().map(|s| json!(s)).unwrap_or(Value::Null)
}

/// Parses the OCI instance document; `None` when it is not one.
pub(crate) fn parse_oci(doc: &Value) -> Option<Value> {
    let region = doc.get("canonicalRegionName")?;
    Some(json!({
        "provider": "oci",
        "region": text(region),
        "shape": text(&doc["shape"]),
        "id": text(&doc["id"]),
        "fault_domain": text(&doc["faultDomain"]),
        "display_name": text(&doc["displayName"]),
    }))
}

/// Parses the Azure instance document; `None` when it is not one.
pub(crate) fn parse_azure(doc: &Value) -> Option<Value> {
    let compute = doc.get("compute")?;
    compute.get("location")?;
    Some(json!({
        "provider": "azure",
        "region": text(&compute["location"]),
        "sku": text(&compute["vmSize"]),
        "id": text(&compute["vmId"]),
        "name": text(&compute["name"]),
        "zone": text(&compute["zone"]),
    }))
}

async fn fetch(cx: &Cx, request: HttpRequest) -> Option<Value> {
    let response = cx.system.http(request).await.ok()?;
    if !(200..300).contains(&response.status) {
        return None;
    }
    serde_json::from_slice(&response.body).ok()
}

#[async_trait]
impl FactProvider for CloudFacts {
    const NAME: &'static str = "cloud";
    const TTL: Duration = Duration::from_secs(3600);
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str =
        "Cloud provider details (oci, azure) from the instance metadata service; null elsewhere.";

    #[tracing::instrument(name = "facts.cloud", skip_all)]
    async fn collect(&self, cx: &Cx) -> Result<Value> {
        let oci = fetch(
            cx,
            HttpRequest::get(OCI_URL)
                .header("Authorization", "Bearer Oracle")
                .timeout(IMDS_TIMEOUT),
        );
        let azure = fetch(
            cx,
            HttpRequest::get(AZURE_URL)
                .header("Metadata", "true")
                .timeout(IMDS_TIMEOUT),
        );
        let (oci, azure) = tokio::join!(oci, azure);
        Ok(oci
            .as_ref()
            .and_then(parse_oci)
            .or_else(|| azure.as_ref().and_then(parse_azure))
            .unwrap_or(Value::Null))
    }
}

buckle_core::register_facts!(CloudFacts);

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::cx;
    use buckle_core::system::HttpResponse;
    use buckle_core::testing::FakeSystem;

    pub const OCI: &str = r#"{
  "availabilityDomain": "TqEJ:UK-LONDON-1-AD-1",
  "faultDomain": "FAULT-DOMAIN-2",
  "compartmentId": "ocid1.compartment.oc1..aaaaaaaaexample",
  "displayName": "web-1",
  "hostname": "web-1",
  "id": "ocid1.instance.oc1.uk-london-1.anwgiljrexample",
  "image": "ocid1.image.oc1.uk-london-1.aaaaaaaaexample",
  "metadata": {"ssh_authorized_keys": "ssh-ed25519 AAAA... ops"},
  "region": "lhr",
  "canonicalRegionName": "uk-london-1",
  "ociAdName": "uk-london-1-ad-1",
  "regionInfo": {"realmKey": "oc1", "realmDomainComponent": "oraclecloud.com", "regionKey": "LHR", "regionIdentifier": "uk-london-1"},
  "shape": "VM.Standard.A1.Flex",
  "shapeConfig": {"ocpus": 2.0, "memoryInGBs": 12.0, "networkingBandwidthInGbps": 2.0, "maxVnicAttachments": 2},
  "state": "Running",
  "timeCreated": 1717000000000
}"#;

    const AZURE: &str = r#"{
  "compute": {
    "azEnvironment": "AzurePublicCloud",
    "location": "uksouth",
    "name": "web-az-1",
    "offer": "0001-com-ubuntu-server-jammy",
    "osType": "Linux",
    "platformFaultDomain": "0",
    "resourceGroupName": "rg-fleet",
    "sku": "22_04-lts-gen2",
    "subscriptionId": "00000000-0000-0000-0000-000000000000",
    "vmId": "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9",
    "vmSize": "Standard_B2ps_v2",
    "zone": "1"
  },
  "network": {"interface": [{"ipv4": {"ipAddress": [{"privateIpAddress": "10.1.0.4", "publicIpAddress": ""}]}}]}
}"#;

    fn body(
        status: u16,
        body: &'static str,
    ) -> impl Fn(&HttpRequest) -> HttpResponse + Send + Sync + 'static {
        move |_| HttpResponse {
            status,
            headers: vec![],
            body: body.as_bytes().to_vec(),
        }
    }

    #[tokio::test]
    async fn detects_oci_with_the_oracle_bearer_header() {
        let sys = FakeSystem::new().on_http_ok(OCI_URL, OCI);
        let f = CloudFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(
            f,
            json!({
                "provider": "oci",
                "region": "uk-london-1",
                "shape": "VM.Standard.A1.Flex",
                "id": "ocid1.instance.oc1.uk-london-1.anwgiljrexample",
                "fault_domain": "FAULT-DOMAIN-2",
                "display_name": "web-1",
            })
        );
        let oci = sys
            .http_requests()
            .into_iter()
            .find(|r| r.url == OCI_URL)
            .unwrap();
        assert_eq!(
            oci.headers,
            vec![("Authorization".to_owned(), "Bearer Oracle".to_owned())]
        );
        assert_eq!(oci.timeout, Some(IMDS_TIMEOUT));
    }

    #[tokio::test]
    async fn detects_azure_with_the_metadata_header() {
        let sys = FakeSystem::new()
            .on_http(OCI_URL, body(404, "not found"))
            .on_http_ok(AZURE_URL, AZURE);
        let f = CloudFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f["provider"], "azure");
        assert_eq!(f["region"], "uksouth");
        assert_eq!(f["sku"], "Standard_B2ps_v2");
        assert_eq!(f["id"], "0a1b2c3d-4e5f-6071-8293-a4b5c6d7e8f9");
        assert_eq!(f["name"], "web-az-1");
        let az = sys
            .http_requests()
            .into_iter()
            .find(|r| r.url == AZURE_URL)
            .unwrap();
        assert_eq!(az.headers, vec![("Metadata".to_owned(), "true".to_owned())]);
    }

    #[tokio::test]
    async fn null_everywhere_else() {
        // Nothing answers (FakeSystem errors for unrouted URLs, like a connect timeout).
        assert!(
            CloudFacts
                .collect(&cx(&FakeSystem::new()))
                .await
                .unwrap()
                .is_null()
        );
        // Something answers with a page that is not instance metadata.
        let sys = FakeSystem::new()
            .on_http(OCI_URL, body(200, "<html>captive portal</html>"))
            .on_http(AZURE_URL, body(200, "{\"unrelated\": true}"));
        assert!(CloudFacts.collect(&cx(&sys)).await.unwrap().is_null());
        // Errors from the metadata service.
        let sys = FakeSystem::new()
            .on_http(OCI_URL, body(500, "oops"))
            .on_http(AZURE_URL, body(403, "denied"));
        assert!(CloudFacts.collect(&cx(&sys)).await.unwrap().is_null());
    }
}
