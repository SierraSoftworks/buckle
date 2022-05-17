use std::path::PathBuf;
use clap::Arg;
use tracing::{info_span, instrument};
use opentelemetry::trace::SpanKind;
use crate::errors;

use super::*;

#[derive(Debug)]
pub struct PlanCommand {}

impl Command for PlanCommand {
    fn name(&self) -> String {
        String::from("plan")
    }
    fn app<'a>(&self) -> clap::Command<'a> {
        clap::Command::new(&self.name())
            .version("1.0")
            .about("shows the planned strategy for bootstrapping the local machine")
            .long_about("Reads the bootstrapping configuration and shows how it would be executed if run against the local machine.")
            .arg(Arg::new("config")
                    .short('c')
                    .long("config")
                    .env("BUCKLE_CONFIG")
                    .value_name("FOLDER")
                    .help("The path to your buckle configuration directory.")
                    .takes_value(true))
    }
}

impl CommandRunnable for PlanCommand {
    #[instrument(name = "command.plan", fields(otel.kind = %SpanKind::Client), skip(self, matches), err)]
    fn run(
        &self,
        matches: &clap::ArgMatches,
    ) -> Result<i32, crate::errors::Error> {
        let config_dir: PathBuf = matches.value_of("config")
            .map(|p| p.into())
            .ok_or_else(|| errors::user("No configuration directory provided.", "Provide the --config directory when running this command."))?;

        let mut output = crate::core::output::output();
        
        let config = crate::core::config::load_all_config(&config_dir.join("config"))?;
        for (key, val) in config {
            writeln!(output, " = config {}={}", key, val)?;
        }
        
        let secrets = crate::core::config::load_all_config(&config_dir.join("secrets"))?;
        for (key, _val) in secrets {
            writeln!(output, " = secret {}=******", key)?;
        }

        let packages = crate::core::package::get_all_packages(&config_dir.join("packages"))?;

        for package in packages {
            let _span = info_span!("package.plan", "package.id"=%package.id).entered();
            writeln!(output)?;
            writeln!(output, " + package '{}'", &package.id)?;

            let config = package.get_config()?;
            for (key, val) in config {
                writeln!(output, "   = config {}={}", key, val)?;
            }

            let secrets = package.get_secrets()?;
            for (key, _val) in secrets {
                writeln!(output, "   = secret {}=******", key)?;
            }

            let root_path = PathBuf::from("/");
            let files = package.get_files()?;
            for file in files {
                let group = package.files.get(&file.group).map(|f| f.as_path()).unwrap_or(&root_path);
                writeln!(output, "   + {} '{}'", if file.is_template { "template" } else { "file" }, group.join(file.relative_path).display())?;
            }

            let tasks = package.get_tasks()?;
            for task in tasks {
                writeln!(output, "   + task '{}'", &task.name)?;
            }
        }

        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use crate::test::{get_test_data, test_tracing};
    use mocktopus::mocking::*;
    use super::*;

    #[test]
    fn run() {
        let _guard = test_tracing();
        
        let cmd = PlanCommand {};
        let args = cmd.app().get_matches_from(vec!["plan", "--config", get_test_data().to_str().unwrap()]);

        let output = crate::core::output::mock();

        crate::core::file::File::apply.mock_safe(|_f, _target, _config, _secrets| {
            panic!("The file should not have been written during the planning phase.");
        });

        match cmd.run(&args) {
            Ok(_) => {}
            Err(err) => panic!("{}", err.message()),
        }

        assert!(
            output.to_string().contains(" + package 'test1'"),
            "the output should contain the first package"
        );

        assert!(
            output.to_string().contains(" + package 'test2'"),
            "the output should contain the second package"
        );
    }
}
