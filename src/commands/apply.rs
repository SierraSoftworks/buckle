use std::{collections::HashMap, path::PathBuf};

use clap::{Arg, ArgAction, value_parser};
use tracing::{info_span, instrument};
use tracing_batteries::prelude::opentelemetry::trace::SpanKind;

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
    #[instrument(name = "command.apply", fields(otel.kind = ?SpanKind::Client), skip(self, core, matches), err)]
    fn run(&self, core: &Core, matches: &clap::ArgMatches) -> Result<i32, crate::errors::Error> {
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

        let mut output = core.output().writer();

        let config = crate::core::config::load_all_config(core, &config_dir.join("config"))?;
        for (key, val) in config.iter() {
            writeln!(output, " = config {key}={val}")?;
        }

        let secrets = crate::core::config::load_all_config(core, &config_dir.join("secrets"))?;
        for (key, _val) in secrets.iter() {
            writeln!(output, " = secret {key}=******")?;
        }

        let packages = crate::core::package::get_all_packages(&config_dir.join("packages"))?;

        for package in packages {
            let mut retries = 0;
            while retries <= package.retry.limit {
                match self.apply_package(core, &config, &secrets, &package) {
                    Ok(_) => {
                        break;
                    }
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
    fn apply_package(&self, core: &Core, config: &HashMap<String, String>, secrets: &HashMap<String, String>, package: &crate::core::package::Package) -> Result<(), crate::errors::Error> {
        let mut output = core.output().writer();
        let _span = info_span!("package.apply", "package.id"=%package.id).entered();

        writeln!(output)?;
        writeln!(output, " + package '{}'", &package.id)?;

        let mut config = config.clone();
        for (key, val) in package.get_config(core)? {
            writeln!(output, "   = config {key}={val}")?;
            config.insert(key, val);
        }

        let mut secrets = secrets.clone();
        for (key, val) in package.get_secrets(core)? {
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

            file.apply(core, target_path, &config, &secrets)?;
        }

        let tasks = package.get_tasks()?;
        for task in tasks {
            writeln!(output, "   + task '{}'", &task.name)?;
            task.run(core, &config, &secrets)?;
        }

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use crate::core::file::{FileWriter, LocalFileWriter};
    use crate::core::output::mocks::OutputBuffer;
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

        let output = OutputBuffer::default();
        let temp_path = temp.path().to_owned();

        let core = Core::builder()
            .with_mock_output(|mock| {
                let output = output.clone();
                mock.expect_writer()
                    .returning(move || Box::new(output.clone()));
            })
            .with_mock_files(|mock| {
                // Redirect every file write into the temporary directory so that
                // the real template/copy logic runs without touching `/`.
                mock.expect_apply()
                    .returning(move |file, target, config, secrets| {
                        let target = temp_path.join(target.strip_prefix("/").unwrap());
                        LocalFileWriter.apply(file, &target, config, secrets)
                    });
            })
            .with_mock_launcher(|mock| {
                mock.expect_load_config()
                    .withf(|interpreter, _file| interpreter == "pwsh")
                    .returning(|_, _| Ok("TESTING=yes".to_string()));

                mock.expect_run_task()
                    .withf(|interpreter, _env, _file| interpreter == "pwsh")
                    .returning(|_, _, _| Ok(()));
            })
            .build();

        match cmd.run(&core, &args) {
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

        assert!(
            temp.path().join("etc/test.conf/test.conf").exists(),
            "the templated file should have been written into the temporary directory"
        );
    }
}
