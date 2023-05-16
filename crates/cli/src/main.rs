use anyhow::{anyhow, Context, Result};
use clap::Parser;
use cli::{CliRequest, CliResponse, IpcHandshake, FORCE_CLI_MODE_ENV_VAR_NAME};
use core_foundation::{
    array::{CFArray, CFIndex},
    string::kCFStringEncodingUTF8,
    url::{CFURLCreateWithBytes, CFURL},
};
use core_services::{kLSLaunchDefaults, LSLaunchURLSpec, LSOpenFromURLSpec, TCFType};
use ipc_channel::ipc::{IpcOneShotServer, IpcReceiver, IpcSender};
use serde::Deserialize;
use std::{
    ffi::OsStr,
    fs::{self, OpenOptions},
    io,
    path::{Path, PathBuf},
    ptr,
};

#[derive(Parser)]
#[clap(name = "zed", global_setting(clap::AppSettings::NoAutoVersion))]
struct Args {
    /// Wait for all of the given paths to be closed before exiting.
    #[clap(short, long)]
    wait: bool,
    /// A sequence of space-separated paths that you want to open.
    #[clap()]
    paths: Vec<PathBuf>,
    /// Print Zed's version and the app path.
    #[clap(short, long)]
    version: bool,
    /// Custom Zed.app path
    #[clap(short, long)]
    bundle_path: Option<PathBuf>,
}

#[derive(Debug, Deserialize)]
struct InfoPlist {
    #[serde(rename = "CFBundleShortVersionString")]
    bundle_short_version_string: String,
}

fn main() -> Result<()> {
    let args = Args::parse();

    let bundle = Bundle::detect(args.bundle_path.as_deref()).context("Bundle detection")?;

    if args.version {
        println!("{}", bundle.zed_version_string());
        return Ok(());
    }

    for path in args.paths.iter() {
        if !path.exists() {
            touch(path.as_path())?;
        }
    }

    let (tx, rx) = bundle.launch()?;

    tx.send(CliRequest::Open {
        paths: args
            .paths
            .into_iter()
            .map(|path| fs::canonicalize(path).map_err(|error| anyhow!(error)))
            .collect::<Result<Vec<PathBuf>>>()?,
        wait: args.wait,
    })?;

    while let Ok(response) = rx.recv() {
        match response {
            CliResponse::Ping => {}
            CliResponse::Stdout { message } => println!("{message}"),
            CliResponse::Stderr { message } => eprintln!("{message}"),
            CliResponse::Exit { status } => std::process::exit(status),
        }
    }

    Ok(())
}

enum Bundle {
    App {
        app_bundle: PathBuf,
        plist: InfoPlist,
    },
    LocalPath {
        executable: PathBuf,
        plist: InfoPlist,
    },
}

impl Bundle {
    fn detect(args_bundle_path: Option<&Path>) -> anyhow::Result<Self> {
        let bundle_path = if let Some(bundle_path) = args_bundle_path {
            bundle_path
                .canonicalize()
                .with_context(|| format!("Args bundle path {bundle_path:?} canonicalization"))?
        } else {
            locate_bundle().context("bundle autodiscovery")?
        };

        match bundle_path.extension().and_then(|ext| ext.to_str()) {
            Some("app") => {
                let plist_path = bundle_path.join("Contents/Info.plist");
                let plist = plist::from_file::<_, InfoPlist>(&plist_path).with_context(|| {
                    format!("Reading *.app bundle plist file at {plist_path:?}")
                })?;
                Ok(Self::App {
                    app_bundle: bundle_path,
                    plist,
                })
            }
            _ => {
                println!("Bundle path {bundle_path:?} has no *.app extension, attempting to locate a dev build");
                let plist_path = bundle_path
                    .parent()
                    .with_context(|| format!("Bundle path {bundle_path:?} has no parent"))?
                    .join("WebRTC.framework/Resources/Info.plist");
                let plist = plist::from_file::<_, InfoPlist>(&plist_path)
                    .with_context(|| format!("Reading dev bundle plist file at {plist_path:?}"))?;
                Ok(Self::LocalPath {
                    executable: bundle_path,
                    plist,
                })
            }
        }
    }

    fn plist(&self) -> &InfoPlist {
        match self {
            Self::App { plist, .. } => plist,
            Self::LocalPath { plist, .. } => plist,
        }
    }

    fn path(&self) -> &Path {
        match self {
            Self::App { app_bundle, .. } => app_bundle,
            Self::LocalPath {
                executable: excutable,
                ..
            } => excutable,
        }
    }

    fn launch(&self) -> anyhow::Result<(IpcSender<CliRequest>, IpcReceiver<CliResponse>)> {
        let (server, server_name) =
            IpcOneShotServer::<IpcHandshake>::new().context("Handshake before Zed spawn")?;
        let url = format!("zed-cli://{server_name}");

        match self {
            Self::App { app_bundle, .. } => {
                let app_path = app_bundle;

                let status = unsafe {
                    let app_url = CFURL::from_path(app_path, true)
                        .with_context(|| format!("invalid app path {app_path:?}"))?;
                    let url_to_open = CFURL::wrap_under_create_rule(CFURLCreateWithBytes(
                        ptr::null(),
                        url.as_ptr(),
                        url.len() as CFIndex,
                        kCFStringEncodingUTF8,
                        ptr::null(),
                    ));
                    let urls_to_open = CFArray::from_copyable(&[url_to_open.as_concrete_TypeRef()]);
                    LSOpenFromURLSpec(
                        &LSLaunchURLSpec {
                            appURL: app_url.as_concrete_TypeRef(),
                            itemURLs: urls_to_open.as_concrete_TypeRef(),
                            passThruParams: ptr::null(),
                            launchFlags: kLSLaunchDefaults,
                            asyncRefCon: ptr::null_mut(),
                        },
                        ptr::null_mut(),
                    )
                };

                anyhow::ensure!(
                    status == 0,
                    "cannot start app bundle {}",
                    self.zed_version_string()
                );
            }
            Self::LocalPath { executable, .. } => {
                let executable_parent = executable
                    .parent()
                    .with_context(|| format!("Executable {executable:?} path has no parent"))?;
                let subprocess_stdout_file =
                    fs::File::create(executable_parent.join("zed_dev.log"))
                        .with_context(|| format!("Log file creation in {executable_parent:?}"))?;
                let subprocess_stdin_file =
                    subprocess_stdout_file.try_clone().with_context(|| {
                        format!("Cloning descriptor for file {subprocess_stdout_file:?}")
                    })?;
                let mut command = std::process::Command::new(executable);
                let command = command
                    .env(FORCE_CLI_MODE_ENV_VAR_NAME, "")
                    .stderr(subprocess_stdout_file)
                    .stdout(subprocess_stdin_file)
                    .arg(url);

                command
                    .spawn()
                    .with_context(|| format!("Spawning {command:?}"))?;
            }
        }

        let (_, handshake) = server.accept().context("Handshake after Zed spawn")?;
        Ok((handshake.requests, handshake.responses))
    }

    fn zed_version_string(&self) -> String {
        let is_dev = matches!(self, Self::LocalPath { .. });
        format!(
            "Zed {}{} – {}",
            self.plist().bundle_short_version_string,
            if is_dev { " (dev)" } else { "" },
            self.path().display(),
        )
    }
}

fn touch(path: &Path) -> io::Result<()> {
    match OpenOptions::new().create(true).write(true).open(path) {
        Ok(_) => Ok(()),
        Err(e) => Err(e),
    }
}

fn locate_bundle() -> Result<PathBuf> {
    let cli_path = std::env::current_exe()?.canonicalize()?;
    let mut app_path = cli_path.clone();
    while app_path.extension() != Some(OsStr::new("app")) {
        if !app_path.pop() {
            return Err(anyhow!("cannot find app bundle containing {:?}", cli_path));
        }
    }
    Ok(app_path)
}
