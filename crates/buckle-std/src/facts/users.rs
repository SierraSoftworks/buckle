//! The `users` fact: local accounts.

use async_trait::async_trait;
use buckle_core::facts::FactProvider;
use buckle_core::task::Cx;
use buckle_core::{Platforms, Result};
use serde_json::{Value, json};
use std::time::Duration;

/// Lists the local accounts.
///
/// Keys: `accounts`, a list of `{name, uid, gid, home, shell}`, and `names`,
/// the account names as a flat list for selectors
/// (`host.facts.users.names contains "deploy"`). Refreshed every ten minutes.
pub struct UsersFacts;

#[async_trait]
impl FactProvider for UsersFacts {
    const NAME: &'static str = "users";
    const TTL: Duration = Duration::from_secs(600);
    const PLATFORMS: Platforms = Platforms::ALL;
    const DESCRIPTION: &'static str = "Local user accounts (name, uid, gid, home, shell).";

    #[tracing::instrument(name = "facts.users", skip_all)]
    async fn collect(&self, cx: &Cx) -> Result<Value> {
        let users = cx.system.list_users().await?;
        let accounts: Vec<Value> = users
            .iter()
            .map(|u| {
                json!({
                    "name": u.name,
                    "uid": u.uid,
                    "gid": u.gid,
                    "home": u.home.display().to_string(),
                    "shell": u.shell.display().to_string(),
                })
            })
            .collect();
        Ok(json!({
            "accounts": accounts,
            "names": users.iter().map(|u| u.name.clone()).collect::<Vec<_>>(),
        }))
    }
}

buckle_core::register_facts!(UsersFacts);

#[cfg(test)]
mod tests {
    use super::*;
    use crate::tasks::sys_exec::testkit::cx;
    use buckle_core::system::UserInfo;
    use buckle_core::testing::FakeSystem;

    fn user(name: &str, uid: u32, shell: &str) -> UserInfo {
        UserInfo {
            name: name.into(),
            uid,
            gid: uid,
            home: format!("/home/{name}").into(),
            shell: shell.into(),
            gecos: String::new(),
        }
    }

    #[tokio::test]
    async fn lists_accounts_and_names() {
        let sys = FakeSystem::new()
            .with_user(user("root", 0, "/bin/bash"))
            .with_user(user("sierra", 1000, "/usr/bin/zsh"));
        let f = UsersFacts.collect(&cx(&sys)).await.unwrap();
        assert_eq!(f["names"], json!(["root", "sierra"]));
        assert_eq!(
            f["accounts"][1],
            json!({"name": "sierra", "uid": 1000, "gid": 1000, "home": "/home/sierra", "shell": "/usr/bin/zsh"})
        );
    }

    #[tokio::test]
    async fn no_accounts_is_an_empty_list() {
        let f = UsersFacts.collect(&cx(&FakeSystem::new())).await.unwrap();
        assert_eq!(f["accounts"], json!([]));
    }
}
