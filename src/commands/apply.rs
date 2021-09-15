use std::path::PathBuf;

use tracing::instrument;
use opentelemetry::trace::SpanKind;

use super::*;

#[derive(Debug)]
pub struct ApplyCommand {}

impl Command for ApplyCommand {
    fn name(&self) -> String {
        String::from("apply")
    }
    fn app<'a>(&self) -> clap::App<'a> {
        App::new(&self.name())
            .version("1.0")
            .about("applies a bootstrapping configuration to the local machine")
            .long_about("Reads the bootstrapping configuration passed in the --config parameter and attempts to apply it to the local machine.")
    }
}

impl CommandRunnable for ApplyCommand {
    #[instrument(name = "command.apply", fields(otel.kind = %SpanKind::Client), skip(self, matches), err)]
    fn run(
        &self,
        matches: &clap::ArgMatches,
    ) -> Result<i32, crate::errors::Error> {
        let config_dir: PathBuf = matches.value_of("config")
        .map(|p| p.into())
        .ok_or(errors::user("No configuration directory provided.", "Provide the --config directory when running this command."))?;

        let config = crate::core::config::load_all_config(&config_dir.join("config"))?;

        let mut output = crate::core::output::output();

        for (key, val) in config.iter() {
            writeln!(output, " = config {}={}", key, val)?;
        }

        let packages = crate::core::package::get_all_packages(&config_dir.join("packages"))?;

        for package in packages {
            writeln!(output, "")?;
            writeln!(output, " + package '{}'", &package.id)?;

            let mut config = config.clone();
            for (key, val) in package.get_config()? {
                writeln!(output, "   = config {}={}", key, val)?;
                config.insert(key, val);
            }

            let root_path = PathBuf::from("/");
            let files = package.get_files()?;
            for file in files {
                let target_path = package.files.get(&file.group).map(|f| f.as_path()).unwrap_or(&root_path);
                writeln!(output, "   + {} '{}'", if file.is_template { "template" } else { "file" }, target_path.join(&file.relative_path).display())?;

                file.apply(target_path, &config)?;
            }

            let tasks = package.get_tasks()?;
            for task in tasks {
                writeln!(output, "   + task '{}'", &task.name)?;
                task.run(&config)?;
            }
        }
        Ok(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn run() {
        let args = ArgMatches::default();

        let output = crate::core::output::mock();

        let cmd = ApplyCommand {};
        match cmd.run(&args) {
            Ok(_) => {}
            Err(err) => panic!("{}", err.message()),
        }

        assert!(
            output.to_string().contains("shell"),
            "the output should contain the default app"
        );
    }
}
