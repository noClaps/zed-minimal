use anyhow::{Context as _, Result};
use clap::Parser;
use cli::{CliRequest, CliResponse, IpcHandshake, ipc::IpcOneShotServer};
use collections::HashMap;
use parking_lot::Mutex;
use std::{
    env, fs, io,
    path::{Path, PathBuf},
    process::ExitStatus,
    sync::Arc,
    thread::{self, JoinHandle},
};
use tempfile::NamedTempFile;
use util::paths::PathWithPosition;

struct Detect;

trait InstalledApp {
    fn zed_version_string(&self) -> String;
    fn launch(&self, ipc_url: String) -> anyhow::Result<()>;
    fn run_foreground(
        &self,
        ipc_url: String,
        user_data_dir: Option<&str>,
    ) -> io::Result<ExitStatus>;
    fn path(&self) -> PathBuf;
}

#[derive(Parser, Debug)]
#[command(
    name = "zed",
    disable_version_flag = true,
    before_help = "The Zed CLI binary.
This CLI is a separate binary that invokes Zed.

Examples:
    `zed`
          Simply opens Zed
    `zed --foreground`
          Runs in foreground (shows all logs)
    `zed path-to-your-project`
          Open your project in Zed
    `zed -n path-to-file `
          Open file/folder in a new window",
    after_help = "To read from stdin, append '-', e.g. 'ps axf | zed -'"
)]
struct Args {
    /// Wait for all of the given paths to be opened/closed before exiting.
    #[arg(short, long)]
    wait: bool,
    /// Add files to the currently open workspace
    #[arg(short, long, overrides_with = "new")]
    add: bool,
    /// Create a new workspace
    #[arg(short, long, overrides_with = "add")]
    new: bool,
    /// Sets a custom directory for all user data (e.g., database, extensions, logs).
    /// This overrides the default platform-specific data directory location.
    /// On macOS, the default is `~/Library/Application Support/Zed`.
    #[arg(long, value_name = "DIR")]
    user_data_dir: Option<String>,
    /// The paths to open in Zed (space-separated).
    ///
    /// Use `path:line:column` syntax to open a file at the given line and column.
    paths_with_position: Vec<String>,
    /// Print Zed's version and the app path.
    #[arg(short, long)]
    version: bool,
    /// Run zed in the foreground (useful for debugging)
    #[arg(long)]
    foreground: bool,
    /// Custom path to Zed.app or the zed binary
    #[arg(long)]
    zed: Option<PathBuf>,
    /// Run zed in dev-server mode
    #[arg(long)]
    dev_server_token: Option<String>,
    /// Not supported in Zed CLI, only supported on Zed binary
    /// Will attempt to give the correct command to run
    #[arg(long)]
    system_specs: bool,
    /// Uninstall Zed from user system
    #[arg(long)]
    uninstall: bool,
}

fn parse_path_with_position(argument_str: &str) -> anyhow::Result<String> {
    let canonicalized = match Path::new(argument_str).canonicalize() {
        Ok(existing_path) => PathWithPosition::from_path(existing_path),
        Err(_) => {
            let path = PathWithPosition::parse_str(argument_str);
            let curdir = env::current_dir().context("retrieving current directory")?;
            path.map_path(|path| match fs::canonicalize(&path) {
                Ok(path) => Ok(path),
                Err(e) => {
                    if let Some(mut parent) = path.parent() {
                        if parent == Path::new("") {
                            parent = &curdir
                        }
                        match fs::canonicalize(parent) {
                            Ok(parent) => Ok(parent.join(path.file_name().unwrap())),
                            Err(_) => Err(e),
                        }
                    } else {
                        Err(e)
                    }
                }
            })
        }
        .with_context(|| format!("parsing as path with position {argument_str}"))?,
    };
    Ok(canonicalized.to_string(|path| path.to_string_lossy().to_string()))
}

fn main() -> Result<()> {
    // Intercept version designators
    if let Some(channel) = std::env::args().nth(1).filter(|arg| arg.starts_with("--")) {
        // When the first argument is a name of a release channel, we're going to spawn off the CLI of that version, with trailing args passed along.
        use std::str::FromStr as _;

        if let Ok(channel) = release_channel::ReleaseChannel::from_str(&channel[2..]) {
            return mac_os::spawn_channel_cli(channel, std::env::args().skip(2).collect());
        }
    }
    let args = Args::parse();

    // Set custom data directory before any path operations
    let user_data_dir = args.user_data_dir.clone();
    if let Some(dir) = &user_data_dir {
        paths::set_custom_data_dir(dir);
    }

    let app = Detect::detect(args.zed.as_deref()).context("Bundle detection")?;

    if args.version {
        println!("{}", app.zed_version_string());
        return Ok(());
    }

    if args.system_specs {
        let path = app.path();
        let msg = [
            "The `--system-specs` argument is not supported in the Zed CLI, only on Zed binary.",
            "To retrieve the system specs on the command line, run the following command:",
            &format!("{} --system-specs", path.display()),
        ];
        anyhow::bail!(msg.join("\n"));
    }

    if args.uninstall {
        static UNINSTALL_SCRIPT: &[u8] = include_bytes!("../../../script/uninstall.sh");

        let tmp_dir = tempfile::tempdir()?;
        let script_path = tmp_dir.path().join("uninstall.sh");
        fs::write(&script_path, UNINSTALL_SCRIPT)?;

        use std::os::unix::fs::PermissionsExt as _;
        fs::set_permissions(&script_path, fs::Permissions::from_mode(0o755))?;

        let status = std::process::Command::new("sh")
            .arg(&script_path)
            .env("ZED_CHANNEL", &*release_channel::RELEASE_CHANNEL_NAME)
            .status()
            .context("Failed to execute uninstall script")?;

        std::process::exit(status.code().unwrap_or(1));
    }

    let (server, server_name) =
        IpcOneShotServer::<IpcHandshake>::new().context("Handshake before Zed spawn")?;
    let url = format!("zed-cli://{server_name}");

    let open_new_workspace = if args.new {
        Some(true)
    } else if args.add {
        Some(false)
    } else {
        None
    };

    let env = Some(std::env::vars().collect::<HashMap<_, _>>());

    let exit_status = Arc::new(Mutex::new(None));
    let mut paths = vec![];
    let mut urls = vec![];
    let mut stdin_tmp_file: Option<fs::File> = None;
    let mut anonymous_fd_tmp_files = vec![];

    for path in args.paths_with_position.iter() {
        if path.starts_with("zed://")
            || path.starts_with("http://")
            || path.starts_with("https://")
            || path.starts_with("file://")
        {
            urls.push(path.to_string());
        } else if path == "-" && args.paths_with_position.len() == 1 {
            let file = NamedTempFile::new()?;
            paths.push(file.path().to_string_lossy().to_string());
            let (file, _) = file.keep()?;
            stdin_tmp_file = Some(file);
        } else if let Some(file) = anonymous_fd(path) {
            let tmp_file = NamedTempFile::new()?;
            paths.push(tmp_file.path().to_string_lossy().to_string());
            let (tmp_file, _) = tmp_file.keep()?;
            anonymous_fd_tmp_files.push((file, tmp_file));
        } else {
            paths.push(parse_path_with_position(path)?)
        }
    }

    let sender: JoinHandle<anyhow::Result<()>> = thread::spawn({
        let exit_status = exit_status.clone();
        let user_data_dir_for_thread = user_data_dir.clone();
        move || {
            let (_, handshake) = server.accept().context("Handshake after Zed spawn")?;
            let (tx, rx) = (handshake.requests, handshake.responses);

            tx.send(CliRequest::Open {
                paths,
                urls,
                wait: args.wait,
                open_new_workspace,
                env,
                user_data_dir: user_data_dir_for_thread,
            })?;

            while let Ok(response) = rx.recv() {
                match response {
                    CliResponse::Ping => {}
                    CliResponse::Stdout { message } => println!("{message}"),
                    CliResponse::Stderr { message } => eprintln!("{message}"),
                    CliResponse::Exit { status } => {
                        exit_status.lock().replace(status);
                        return Ok(());
                    }
                }
            }

            Ok(())
        }
    });

    let stdin_pipe_handle: Option<JoinHandle<anyhow::Result<()>>> =
        stdin_tmp_file.map(|tmp_file| {
            thread::spawn(move || {
                let stdin = std::io::stdin().lock();
                if io::IsTerminal::is_terminal(&stdin) {
                    return Ok(());
                }
                return pipe_to_tmp(stdin, tmp_file);
            })
        });

    let anonymous_fd_pipe_handles: Vec<JoinHandle<anyhow::Result<()>>> = anonymous_fd_tmp_files
        .into_iter()
        .map(|(file, tmp_file)| thread::spawn(move || pipe_to_tmp(file, tmp_file)))
        .collect();

    if args.foreground {
        app.run_foreground(url, user_data_dir.as_deref())?;
    } else {
        app.launch(url)?;
        sender.join().unwrap()?;
        if let Some(handle) = stdin_pipe_handle {
            handle.join().unwrap()?;
        }
        for handle in anonymous_fd_pipe_handles {
            handle.join().unwrap()?;
        }
    }

    if let Some(exit_status) = exit_status.lock().take() {
        std::process::exit(exit_status);
    }
    Ok(())
}

fn pipe_to_tmp(mut src: impl io::Read, mut dest: fs::File) -> Result<()> {
    let mut buffer = [0; 8 * 1024];
    loop {
        let bytes_read = match src.read(&mut buffer) {
            Err(err) if err.kind() == io::ErrorKind::Interrupted => continue,
            res => res?,
        };
        if bytes_read == 0 {
            break;
        }
        io::Write::write_all(&mut dest, &buffer[..bytes_read])?;
    }
    io::Write::flush(&mut dest)?;
    Ok(())
}

fn anonymous_fd(path: &str) -> Option<fs::File> {
    use std::os::{
        fd::{self, FromRawFd},
        unix::fs::FileTypeExt,
    };

    let fd_str = path.strip_prefix("/dev/fd/")?;

    let metadata = fs::metadata(path).ok()?;
    let file_type = metadata.file_type();
    if !file_type.is_fifo() && !file_type.is_socket() {
        return None;
    }
    let fd: fd::RawFd = fd_str.parse().ok()?;
    let file = unsafe { fs::File::from_raw_fd(fd) };
    return Some(file);
}

mod mac_os {
    use anyhow::{Context as _, Result};
    use core_foundation::{
        array::{CFArray, CFIndex},
        base::TCFType as _,
        string::kCFStringEncodingUTF8,
        url::{CFURL, CFURLCreateWithBytes},
    };
    use core_services::{LSLaunchURLSpec, LSOpenFromURLSpec, kLSLaunchDefaults};
    use serde::Deserialize;
    use std::{
        ffi::OsStr,
        fs, io,
        path::{Path, PathBuf},
        process::{Command, ExitStatus},
        ptr,
    };

    use cli::FORCE_CLI_MODE_ENV_VAR_NAME;

    use crate::{Detect, InstalledApp};

    #[derive(Debug, Deserialize)]
    struct InfoPlist {
        #[serde(rename = "CFBundleShortVersionString")]
        bundle_short_version_string: String,
    }

    enum Bundle {
        App {
            app_bundle: PathBuf,
            plist: InfoPlist,
        },
        LocalPath {
            executable: PathBuf,
        },
    }

    fn locate_bundle() -> Result<PathBuf> {
        let cli_path = std::env::current_exe()?.canonicalize()?;
        let mut app_path = cli_path.clone();
        while app_path.extension() != Some(OsStr::new("app")) {
            anyhow::ensure!(
                app_path.pop(),
                "cannot find app bundle containing {cli_path:?}"
            );
        }
        Ok(app_path)
    }

    impl Detect {
        pub fn detect(path: Option<&Path>) -> anyhow::Result<impl InstalledApp> {
            let bundle_path = if let Some(bundle_path) = path {
                bundle_path
                    .canonicalize()
                    .with_context(|| format!("Args bundle path {bundle_path:?} canonicalization"))?
            } else {
                locate_bundle().context("bundle autodiscovery")?
            };

            match bundle_path.extension().and_then(|ext| ext.to_str()) {
                Some("app") => {
                    let plist_path = bundle_path.join("Contents/Info.plist");
                    let plist =
                        plist::from_file::<_, InfoPlist>(&plist_path).with_context(|| {
                            format!("Reading *.app bundle plist file at {plist_path:?}")
                        })?;
                    Ok(Bundle::App {
                        app_bundle: bundle_path,
                        plist,
                    })
                }
                _ => Ok(Bundle::LocalPath {
                    executable: bundle_path,
                }),
            }
        }
    }

    impl InstalledApp for Bundle {
        fn zed_version_string(&self) -> String {
            format!("Zed {} – {}", self.version(), self.path().display(),)
        }

        fn launch(&self, url: String) -> anyhow::Result<()> {
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
                        // equivalent to: open zed-cli:... -a /Applications/Zed\ Preview.app
                        let urls_to_open =
                            CFArray::from_copyable(&[url_to_open.as_concrete_TypeRef()]);
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
                    let subprocess_stdout_file = fs::File::create(
                        executable_parent.join("zed_dev.log"),
                    )
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

            Ok(())
        }

        fn run_foreground(
            &self,
            ipc_url: String,
            user_data_dir: Option<&str>,
        ) -> io::Result<ExitStatus> {
            let path = match self {
                Bundle::App { app_bundle, .. } => app_bundle.join("Contents/MacOS/zed"),
                Bundle::LocalPath { executable, .. } => executable.clone(),
            };

            let mut cmd = std::process::Command::new(path);
            cmd.arg(ipc_url);
            if let Some(dir) = user_data_dir {
                cmd.arg("--user-data-dir").arg(dir);
            }
            cmd.status()
        }

        fn path(&self) -> PathBuf {
            match self {
                Bundle::App { app_bundle, .. } => app_bundle.join("Contents/MacOS/zed").clone(),
                Bundle::LocalPath { executable, .. } => executable.clone(),
            }
        }
    }

    impl Bundle {
        fn version(&self) -> String {
            match self {
                Self::App { plist, .. } => plist.bundle_short_version_string.clone(),
                Self::LocalPath { .. } => "<development>".to_string(),
            }
        }

        fn path(&self) -> &Path {
            match self {
                Self::App { app_bundle, .. } => app_bundle,
                Self::LocalPath { executable, .. } => executable,
            }
        }
    }

    pub(super) fn spawn_channel_cli(
        channel: release_channel::ReleaseChannel,
        leftover_args: Vec<String>,
    ) -> Result<()> {
        use anyhow::bail;

        let app_id_prompt = format!("id of app \"{}\"", channel.display_name());
        let app_id_output = Command::new("osascript")
            .arg("-e")
            .arg(&app_id_prompt)
            .output()?;
        if !app_id_output.status.success() {
            bail!("Could not determine app id for {}", channel.display_name());
        }
        let app_name = String::from_utf8(app_id_output.stdout)?.trim().to_owned();
        let app_path_prompt = format!("kMDItemCFBundleIdentifier == '{app_name}'");
        let app_path_output = Command::new("mdfind").arg(app_path_prompt).output()?;
        if !app_path_output.status.success() {
            bail!(
                "Could not determine app path for {}",
                channel.display_name()
            );
        }
        let app_path = String::from_utf8(app_path_output.stdout)?.trim().to_owned();
        let cli_path = format!("{app_path}/Contents/MacOS/cli");
        Command::new(cli_path).args(leftover_args).spawn()?;
        Ok(())
    }
}
