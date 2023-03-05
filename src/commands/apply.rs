use std::{path::PathBuf, collections::HashMap, hash::Hash};

use clap::{Arg, ArgAction, value_parser};
use opentelemetry::trace::SpanKind;
use tracing::{info_span, instrument};

use super::*;

#[derive(Debug)]
pub struct ApplyCommand {}

impl Command for ApplyCommand {
    fn name(&self) -> String {
        String::from("apply")
    }
    fn app(&self) -> clap::Command {
        clap::Command::new(self.name())
            .version("1.0")
            .about("applies a bootstrapping configuration to the local machine")
            .long_about("Reads the bootstrapping configuration passed in the --config parameter and attempts to apply it to the local machine.")
            .arg(Arg::new("config")
                    .short('c')
                    .long("config")
                    .env("BUCKLE_CONFIG")
                    .value_name("FOLDER")
                    .help("The path to your buckle configuration directory.")
                    .action(ArgAction::Set)
                    .value_parser(value_parser!(PathBuf))
                    .required(true))
    }
}

impl CommandRunnable for ApplyCommand {
    #[instrument(name = "command.apply", fields(otel.kind = ?SpanKind::Client), skip(self, matches), err)]
    fn run(&self, matches: &clap::ArgMatches) -> Result<i32, crate::errors::Error> {
        let config_dir: PathBuf =
            matches
                .get_one::<PathBuf>("config")
                .cloned()
                .ok_or_else(|| {
                    errors::user(
                        "No configuration directory provided.",
                        "Provide the --config directory when running this command.",
                    )
                })?;

        let mut output = crate::core::output::output();

        let config = crate::core::config::load_all_config(&config_dir.join("config"))?;
        for (key, val) in config.iter() {
            writeln!(output, " = config {key}={val}")?;
        }

        let secrets = crate::core::config::load_all_config(&config_dir.join("secrets"))?;
        for (key, _val) in secrets.iter() {
            writeln!(output, " = secret {key}=******")?;
        }

        let packages = crate::core::package::get_all_packages(&config_dir.join("packages"))?;

        for package in packages {
            let mut retries = 0;
            while retries <= package.retry.limit {
                match self.apply_package(&config, &secrets, &package) {
                    Ok(_) => {}
                    Err(err) => {
                        retries += 1;
                        if package.retry.limit >= retries {
                            return Err(err);
                        }
                        std::thread::sleep(package.retry.delay.into());
                    }
                }
            }
        }

        Ok(0)
    }
}

impl ApplyCommand {
    fn apply_package(&self, config: &HashMap<String, String>, secrets: &HashMap<String, String>, package: &crate::core::package::Package) -> Result<(), crate::errors::Error> {
        let mut output = crate::core::output::output();
        let _span = info_span!("package.apply", "package.id"=%package.id).entered();

        writeln!(output)?;
        writeln!(output, " + package '{}'", &package.id)?;

        let mut config = config.clone();
        for (key, val) in package.get_config()? {
            writeln!(output, "   = config {key}={val}")?;
            config.insert(key, val);
        }

        let mut secrets = secrets.clone();
        for (key, val) in package.get_secrets()? {
            writeln!(output, "   = secret {key}=******")?;
            secrets.insert(key, val);
        }

        let root_path = PathBuf::from("/");
        let files = package.get_files()?;
        for file in files {
            let target_path = package
                .files
                .get(&file.group)
                .map(|f| f.as_path())
                .unwrap_or(&root_path);
            writeln!(
                output,
                "   + {} '{}'",
                if file.is_template { "template" } else { "file" },
                target_path.join(&file.relative_path).display()
            )?;

            file.apply(target_path, &config, &secrets)?;
        }

        let tasks = package.get_tasks()?;
        for task in tasks {
            writeln!(output, "   + task '{}'", &task.name)?;
            task.run(&config, &secrets)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use mocktopus::mocking::{MockResult, Mockable};

    use crate::test::{get_test_data, test_tracing};

    use super::*;

    #[test]
    fn run() {
        let _guard = test_tracing();
        let temp = tempfile::tempdir().unwrap();

        let cmd = ApplyCommand {};

        let args = cmd.app().get_matches_from(vec![
            "apply",
            "--config",
            get_test_data().to_str().unwrap(),
        ]);

        let output = crate::core::output::mock();

        let temp_path = temp.path().to_owned();
        crate::core::file::File::apply.mock_safe(move |f, target, config, secrets| {
            let target = Box::leak(Box::new(temp_path.join(target.strip_prefix("/").unwrap())));

            MockResult::Continue((f, target, config, secrets))
        });

        crate::core::config::load_script_config.mock_safe(|interpreter, _file| {
            assert_eq!(interpreter, "pwsh");

            MockResult::Return(Ok("TESTING=yes".to_string()))
        });

        crate::core::script::run_script_task.mock_safe(|interpreter, _config, _file| {
            assert_eq!(interpreter, "pwsh");

            MockResult::Return(Ok(()))
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
