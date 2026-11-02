//! `buckle update [--to VERSION]`: replaces this binary with a release from
//! GitHub (see `buckle_agent::update` for how the download is verified and
//! swapped in).

use buckle_agent::update::{GitHubUpdater, UpdateOutcome, Updater};

pub async fn run(args: super::UpdateArgs) -> buckle_core::Result<i32> {
    let updater = GitHubUpdater::new(crate::telemetry::version());
    match updater.update(args.to_version.as_deref()).await? {
        UpdateOutcome::Updated { version } => {
            println!(
                "Updated Buckle to {version}. Restart running agents (the service manager does this after `buckle agent` upgrades itself)."
            );
        }
        UpdateOutcome::UpToDate { version } => {
            println!("Buckle {version} is already installed.");
        }
    }
    Ok(0)
}
