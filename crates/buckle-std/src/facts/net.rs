//! The `net` fact: hostname, FQDN, interfaces and the default route.

use super::{opt_str, probe, probe_json};
use async_trait::async_trait;
use buckle_core::facts::FactProvider;
use buckle_core::task::Cx;
use buckle_core::{Platforms, Result};
use serde_json::{Value, json};
use std::time::Duration;

/// Describes the host's network identity.
///
/// Keys: `hostname`, `fqdn` (`hostname -f`, `null` when it cannot be
/// resolved), `interfaces` (a list of `{name, state, mac, mtu, addresses:
/// [{family, address, prefix, scope}]}` from `ip -j addr`; empty when `ip` is
/// unavailable), `addresses` (every global-scope address as a flat list, for
/// selectors) and `default_route` (`{gateway, interface, source}` from `ip -j
/// route show default`, `null` when there is none). Refreshed every five
/// minutes.
pub struct NetFacts;

/// Parses `ip -j addr`.
pub(crate) fn parse_interfaces(json: &Value) -> Vec<Value> {
    let Some(items) = json.as_array() else {
        return vec![];
    };
    items
        .iter()
        .filter_map(|i| {
            let name = i.get("ifname")?.as_str()?;
            let addresses: Vec<Value> = i
                .get("addr_info")
                .and_then(Value::as_array)
                .map(|infos| {
                    infos
                        .iter()
                        .filter_map(|a| {
                            Some(json!({
                                "family": a.get("family")?.as_str()?,
                                "address": a.get("local")?.as_str()?,
                                "prefix": a.get("prefixlen").cloned().unwrap_or(Value::Null),
                                "scope": a.get("scope").cloned().unwrap_or(Value::Null),
                            }))
                        })
                        .collect()
                })
                .unwrap_or_default();
            Some(json!({
                "name": name,
                "state": i.get("operstate").cloned().unwrap_or(Value::Null),
                "mac": i.get("address").cloned().unwrap_or(Value::Null),
                "mtu": i.get("mtu").cloned().unwrap_or(Value::Null),
                "addresses": addresses,
            }))
        })
        .collect()
}

/// The first default route of `ip -j route show default`.
pub(crate) fn parse_default_route(json: &Value) -> Value {
    json.as_array()
        .and_then(|routes| routes.first())
        .map(|r| {
            json!({
                "gateway": r.get("gateway").cloned().unwrap_or(Value::Null),
                "interface": r.get("dev").cloned().unwrap_or(Value::Null),
                "source": r.get("prefsrc").cloned().unwrap_or(Value::Null),
            })
        })
        .unwrap_or(Value::Null)
}

#[async_trait]
impl FactProvider for NetFacts {
    const NAME: &'static str = "net";
    const TTL: Duration = Duration::from_secs(300);
    const PLATFORMS: Platforms = Platforms::LINUX;
    const DESCRIPTION: &'static str = "Hostname, FQDN, network interfaces and the default route.";

    #[tracing::instrument(name = "facts.net", skip_all)]
    async fn collect(&self, cx: &Cx) -> Result<Value> {
        let fqdn = probe(cx, "hostname", &["-f"])
            .await
            .map(|f| f.trim().to_owned())
            .filter(|f| !f.is_empty());
        let interfaces = probe_json(cx, "ip", &["-j", "addr"])
            .await
            .map(|j| parse_interfaces(&j))
            .unwrap_or_default();
        let addresses: Vec<Value> = interfaces
            .iter()
            .flat_map(|i| i["addresses"].as_array().cloned().unwrap_or_default())
            .filter(|a| a["scope"] == "global")
            .map(|a| a["address"].clone())
            .collect();
        let default_route = probe_json(cx, "ip", &["-j", "route", "show", "default"])
            .await
            .map(|j| parse_default_route(&j))
            .unwrap_or(Value::Null);
        Ok(json!({
            "hostname": cx.system.hostname(),
            "fqdn": opt_str(fqdn),
            "interfaces": interfaces,
            "addresses": addresses,
            "default_route": default_route,
        }))
    }
}

buckle_core::register_facts!(NetFacts);

#[cfg(test)]
pub(crate) mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::cx;
    use buckle_core::testing::FakeSystem;

    pub const IP_ADDR: &str = r#"[{"ifindex":1,"ifname":"lo","flags":["LOOPBACK","UP","LOWER_UP"],"mtu":65536,"qdisc":"noqueue","operstate":"UNKNOWN","group":"default","txqlen":1000,"link_type":"loopback","address":"00:00:00:00:00:00","broadcast":"00:00:00:00:00:00","addr_info":[{"family":"inet","local":"127.0.0.1","prefixlen":8,"scope":"host","label":"lo","valid_life_time":4294967295,"preferred_life_time":4294967295},{"family":"inet6","local":"::1","prefixlen":128,"scope":"host","valid_life_time":4294967295,"preferred_life_time":4294967295}]},{"ifindex":2,"ifname":"eth0","flags":["BROADCAST","MULTICAST","UP","LOWER_UP"],"mtu":1500,"qdisc":"fq_codel","operstate":"UP","group":"default","txqlen":1000,"link_type":"ether","address":"02:00:17:0a:3b:4c","broadcast":"ff:ff:ff:ff:ff:ff","addr_info":[{"family":"inet","local":"10.0.0.12","prefixlen":24,"broadcast":"10.0.0.255","scope":"global","dynamic":true,"label":"eth0","valid_life_time":86312,"preferred_life_time":86312},{"family":"inet6","local":"fe80::17ff:fe0a:3b4c","prefixlen":64,"scope":"link","valid_life_time":4294967295,"preferred_life_time":4294967295}]},{"ifindex":3,"ifname":"tailscale0","flags":["POINTOPOINT","MULTICAST","NOARP","UP","LOWER_UP"],"mtu":1280,"qdisc":"fq_codel","operstate":"UNKNOWN","group":"default","txqlen":500,"link_type":"none","addr_info":[{"family":"inet","local":"100.101.102.103","prefixlen":32,"scope":"global","label":"tailscale0","valid_life_time":4294967295,"preferred_life_time":4294967295}]}]"#;

    pub const IP_ROUTE: &str = r#"[{"dst":"default","gateway":"10.0.0.1","dev":"eth0","protocol":"dhcp","prefsrc":"10.0.0.12","metric":100,"flags":[]}]"#;

    #[tokio::test]
    async fn collects_interfaces_addresses_and_default_route() {
        let sys = FakeSystem::new()
            .with_hostname("web-1")
            .on_command_ok("hostname", &["-f"], "web-1.example.com\n")
            .on_command_ok("ip", &["-j", "addr"], IP_ADDR)
            .on_command_ok("ip", &["-j", "route"], IP_ROUTE);
        let f = NetFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f["hostname"], "web-1");
        assert_eq!(f["fqdn"], "web-1.example.com");
        let ifaces = f["interfaces"].as_array().unwrap();
        assert_eq!(ifaces.len(), 3);
        assert_eq!(ifaces[1]["name"], "eth0");
        assert_eq!(ifaces[1]["state"], "UP");
        assert_eq!(ifaces[1]["mac"], "02:00:17:0a:3b:4c");
        assert_eq!(ifaces[1]["mtu"], 1500);
        assert_eq!(ifaces[1]["addresses"][0]["address"], "10.0.0.12");
        assert_eq!(ifaces[1]["addresses"][0]["prefix"], 24);
        assert_eq!(f["addresses"], json!(["10.0.0.12", "100.101.102.103"]));
        assert_eq!(f["default_route"]["gateway"], "10.0.0.1");
        assert_eq!(f["default_route"]["interface"], "eth0");
        assert_eq!(f["default_route"]["source"], "10.0.0.12");
    }

    #[tokio::test]
    async fn degrades_to_empty_values_without_iproute2() {
        let sys = FakeSystem::new().with_hostname("lonely");
        let f = NetFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f["hostname"], "lonely");
        assert!(f["fqdn"].is_null());
        assert_eq!(f["interfaces"], json!([]));
        assert!(f["default_route"].is_null());
    }

    #[test]
    fn tolerant_parsers() {
        assert!(parse_interfaces(&json!({"not": "an array"})).is_empty());
        assert_eq!(
            parse_interfaces(&json!([{"ifname": "x"}, {"nope": 1}])).len(),
            1
        );
        assert!(parse_default_route(&json!([])).is_null());
        // Routes without a gateway (point-to-point links) still report the device.
        let r = parse_default_route(&json!([{"dst": "default", "dev": "wg0"}]));
        assert_eq!(r["interface"], "wg0");
        assert!(r["gateway"].is_null());
    }
}
