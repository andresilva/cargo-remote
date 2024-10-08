use std::env;
use std::ffi::OsStr;
use std::path::PathBuf;
use std::process::{exit, Command, Stdio};

use itertools::Itertools;
use log::{debug, error, warn};
use structopt::StructOpt;
use toml::Value;

const PROGRESS_FLAG: &str = "--info=progress2";

mod patches;

/// Environment variables that are whitelisted to be forwarded from the process
/// env to the remote cargo instance.
const WHITELISTED_ENV_VARS: &[&str] = &["RUST_BACKTRACE", "RUST_LOG", "CARGO_INCREMENTAL"];

/// An enum that represents the commands that will be passed to the remote cargo
/// instance.
///
/// It uses `external_subcommand` to capture any given subcommand plus arguments.
/// This means that everything that is passed as subcommand it "blindly" passed
/// to the remote cargo instance and is required to be a valid cargo subcommand.
///
/// # Example
///
/// `cargo remote -r 100.100.100.100 build --release --all`
///
/// In this example `build --release --all` are passed to the remote cargo instance.
#[derive(StructOpt, Debug)]
enum RemoteCommands {
    #[structopt(external_subcommand)]
    Commands(Vec<String>),
}

impl RemoteCommands {
    /// Convert into the raw commands.
    fn into_commands(self) -> Vec<String> {
        let Self::Commands(cmds) = self;
        cmds
    }
}

#[derive(StructOpt, Debug)]
#[structopt(name = "cargo-remote", bin_name = "cargo")]
enum Opts {
    #[structopt(name = "remote")]
    Remote {
        /// Remote ssh build server.
        #[structopt(short = "r", long)]
        remote: Option<String>,

        /// Set remote environment variables. RUST_BACKTRACE, CC, LIB, etc.
        #[structopt(short = "b", long)]
        build_env: Vec<String>,

        /// Transfer the target folder or specific file from that folder back
        /// to the local machine.
        #[structopt(short = "c", long)]
        copy_back: Option<Option<String>>,

        /// Don't transfer the Cargo.lock file back to the local machine.
        #[structopt(long)]
        no_copy_lock: bool,

        /// Path to the manifest to execute.
        #[structopt(long, default_value = "Cargo.toml", parse(from_os_str))]
        manifest_path: PathBuf,

        /// Transfer hidden files and directories to the build server.
        #[structopt(short = "h", long = "transfer-hidden")]
        hidden: bool,

        /// Ignore Cargo patches.
        #[structopt(long)]
        ignore_patches: bool,

        #[structopt(flatten)]
        remote_commands: RemoteCommands,
    },
}

/// Tries to parse the file [`config_path`]. Logs warnings and returns [`None`] if errors occur
/// during reading or parsing, [`Some(Value)`] otherwise.
fn config_from_file(config_path: &PathBuf) -> Option<Value> {
    let config_file = std::fs::read_to_string(config_path)
        .map_err(|e| {
            warn!(
                "Can't parse config file '{}' (error: {})",
                config_path.display(),
                e
            );
        })
        .ok()?;

    let value = config_file
        .parse::<Value>()
        .map_err(|e| {
            warn!(
                "Can't parse config file '{}' (error: {})",
                config_path.display(),
                e
            );
        })
        .ok()?;

    Some(value)
}

fn main() {
    simple_logger::SimpleLogger::new()
        .with_level(log::LevelFilter::Error)
        .env()
        .init()
        .unwrap();

    let Opts::Remote {
        remote,
        mut build_env,
        copy_back,
        no_copy_lock,
        manifest_path,
        hidden,
        ignore_patches,
        remote_commands,
    } = Opts::from_args();

    let mut metadata_cmd = cargo_metadata::MetadataCommand::new();
    metadata_cmd.manifest_path(manifest_path).no_deps();

    let project_metadata = match metadata_cmd.exec() {
        Ok(m) => m,
        Err(cargo_metadata::Error::CargoMetadata { stderr }) => {
            error!("Cargo Metadata execution failed:\n{}", stderr);
            exit(1)
        }
        Err(e) => {
            error!("Cargo Metadata failed:\n{:?}", e);
            exit(1)
        }
    };
    let project_dir = project_metadata.workspace_root.clone().into_std_path_buf();
    debug!("Project dir: {:?}", project_dir);

    let mut manifest_path = project_dir.clone();
    manifest_path.push("Cargo.toml");
    log::info!("Manifest_path: {:?}", manifest_path);

    let project_name = project_metadata
        .packages
        .iter()
        .find(|p| p.manifest_path == manifest_path)
        .map_or_else(
            || {
                debug!("No metadata found. Setting the remote dir name like the local. Or use --manifest_path for execute");
                project_dir.file_name().unwrap()
            },
            |p| OsStr::new(&p.name),
        );

    let build_path_folder = "~/remote-builds";
    let build_path = format!("{}/{}/", build_path_folder, project_name.to_string_lossy());

    debug!("Project name: {:?}", project_name);
    let configs = vec![
        config_from_file(&project_dir.join(".cargo-remote.toml")),
        xdg::BaseDirectories::with_prefix("cargo-remote")
            .ok()
            .and_then(|base| base.find_config_file("cargo-remote.toml"))
            .and_then(|p| config_from_file(&p)),
    ];

    // TODO: move Opts::Remote fields into own type and implement complete_from_config(&mut self, config: &Value)
    let build_server = remote
        .or_else(|| {
            configs
                .into_iter()
                .flat_map(|config| config.and_then(|c| c["remote"].as_str().map(String::from)))
                .next()
        })
        .unwrap_or_else(|| {
            error!("No remote build server was defined (use config file or --remote flag)");
            exit(-3);
        });

    debug!("Transferring sources to build server.");
    // transfer project to build server
    copy_to_remote(
        &format!("{}/", project_dir.display()),
        &format!("{}:{}", build_server, build_path),
        hidden,
    )
    .unwrap_or_else(|e| {
        error!("Failed to transfer project to build server (error: {})", e);
        exit(-4);
    });

    if !ignore_patches {
        patches::handle_patches(&build_path, &build_server, manifest_path, hidden).unwrap_or_else(
            |err| {
                log::error!("Could not transfer patched workspaces to remote: {}", err);
            },
        );
    } else {
        log::debug!("Potential patches will be ignored due to command line flag.");
    }

    debug!("Build ENV: {:?}", build_env);
    debug!("Build path: {:?}", build_path);

    build_env.extend(
        env::vars()
            .filter(|(k, _v)| WHITELISTED_ENV_VARS.iter().any(|w| w == k))
            .map(|(k, v)| format!(r#"{}="{}""#, k, v)),
    );

    let build_command = format!(
        "cd {}; eval $(direnv export bash); {} cargo {}",
        build_path,
        build_env.into_iter().join(" "),
        remote_commands.into_commands().join(" "),
    );

    debug!("Starting build process.");
    let output = Command::new("ssh")
        .arg("-t")
        .arg(&build_server)
        .arg(build_command)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::inherit())
        .output()
        .unwrap_or_else(|e| {
            error!("Failed to run cargo command remotely (error: {})", e);
            exit(-5);
        });

    if let Some(file_name) = copy_back {
        debug!("Transferring artifacts back to client.");
        let file_name = file_name.unwrap_or_else(String::new);
        Command::new("rsync")
            .arg("--links")
            .arg("--recursive")
            .arg("--quiet")
            .arg("--delete")
            .arg("--compress")
            .arg(PROGRESS_FLAG)
            .arg(format!(
                "{}:{}target/{}",
                build_server, build_path, file_name
            ))
            .arg(format!("{}/target/{}", project_dir.display(), file_name))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdin(Stdio::inherit())
            .output()
            .unwrap_or_else(|e| {
                error!(
                    "Failed to transfer target back to local machine (error: {})",
                    e
                );
                exit(-6);
            });
    }

    if !no_copy_lock {
        debug!("Transferring Cargo.lock file back to client.");
        Command::new("rsync")
            .arg("--links")
            .arg("--recursive")
            .arg("--quiet")
            .arg("--delete")
            .arg("--compress")
            .arg(PROGRESS_FLAG)
            .arg(format!("{}:{}/Cargo.lock", build_server, build_path))
            .arg(format!("{}/Cargo.lock", project_dir.display()))
            .stdout(Stdio::inherit())
            .stderr(Stdio::inherit())
            .stdin(Stdio::inherit())
            .output()
            .unwrap_or_else(|e| {
                error!(
                    "Failed to transfer Cargo.lock back to local machine (error: {})",
                    e
                );
                exit(-7);
            });
    }

    if !output.status.success() {
        exit(output.status.code().unwrap_or(1))
    }
}

pub fn copy_to_remote(
    local_dir: &str,
    remote_dir: &str,
    hidden: bool,
) -> Result<std::process::Output, std::io::Error> {
    let mut rsync_to = Command::new("rsync");
    rsync_to
        .arg("--links")
        .arg("--recursive")
        .arg("--quiet")
        .arg("--delete")
        .arg("--compress")
        .arg(PROGRESS_FLAG)
        .arg("--exclude")
        .arg("target");

    if !hidden {
        rsync_to.arg("--exclude").arg(".*");
    }

    rsync_to
        .arg("--rsync-path")
        .arg("mkdir -p remote-builds && rsync")
        .arg(local_dir)
        .arg(remote_dir)
        .stdout(Stdio::inherit())
        .stderr(Stdio::inherit())
        .stdin(Stdio::inherit())
        .output()
}
