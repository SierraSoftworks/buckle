use super::errors;
use clap::ArgMatches;
use std::sync::Arc;
use std::{io::Write, vec::Vec};

mod apply;
mod plan;

pub trait Command: Send + Sync {
    fn name(&self) -> String;
    fn app(&self) -> clap::Command;
}

pub trait CommandRunnable: Command {
    fn run(&self, matches: &ArgMatches) -> Result<i32, errors::Error>;
}

pub fn commands() -> Vec<Arc<dyn CommandRunnable>> {
    vec![
        Arc::new(apply::ApplyCommand {}),
        Arc::new(plan::PlanCommand {}),
    ]
}
