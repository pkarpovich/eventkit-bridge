use std::ffi::OsString;
use std::fs;
use std::future::Future;
use std::io;
use std::path::{Path, PathBuf};
use std::process::Stdio;
use std::time::Duration;

use tokio::process::Command;
use tokio::time::{self, Instant};

use crate::server::SHUTDOWN_GRACE;

/// The LaunchAgent label, which is also the app's bundle id.
pub const LABEL: &str = "dev.pkarpovich.eventkit-bridge";

/// The bundle id TCC keys the Calendars grant to; it must never change.
pub const BUNDLE_ID: &str = "dev.pkarpovich.eventkit-bridge";

/// How long one `launchctl` call may run before it is killed; above [`EXIT_TIMEOUT`], since
/// `bootout` may wait for the daemon to drain.
pub const LAUNCHCTL_TIMEOUT: Duration = Duration::from_secs(EXIT_TIMEOUT.as_secs() + 10);

const LAUNCHCTL: &str = "/bin/launchctl";
const LAUNCH_AGENTS_DIR: &str = "Library/LaunchAgents";
const LOG_RELATIVE_PATH: &str = "Library/Logs/eventkit-bridge.log";
const STDERR_TAIL: usize = 500;
const EXIT_TIMEOUT_MARGIN: Duration = Duration::from_secs(5);
const EXIT_TIMEOUT: Duration =
    Duration::from_secs(SHUTDOWN_GRACE.as_secs() + EXIT_TIMEOUT_MARGIN.as_secs());

/// Why installing or uninstalling the LaunchAgent failed.
#[derive(Debug, thiserror::Error)]
pub enum ServiceError {
    /// `$HOME` is unset or empty.
    #[error("HOME is not set, cannot locate ~/Library")]
    NoHome,
    /// A path that goes into the plist is not valid UTF-8.
    #[error("path is not valid UTF-8: {}", .0.display())]
    NonUtf8Path(PathBuf),
    /// `launchctl` could not be started.
    #[error("launchctl could not start: {0}")]
    Spawn(#[source] io::Error),
    /// `launchctl` ran past its deadline and was killed.
    #[error("launchctl {0} timed out")]
    Timeout(String),
    /// `launchctl` reported a failure where success was required.
    #[error("launchctl {command} failed: {}", exit_message(*.code, .stderr))]
    Failed {
        /// The `launchctl` subcommand.
        command: String,
        /// The exit code, `None` when a signal ended the process.
        code: Option<i32>,
        /// The last bytes `launchctl` wrote to stderr.
        stderr: String,
    },
    /// The agent was still loaded when the unload deadline passed.
    #[error("the agent is still loaded {} s after bootout{}", .waited.as_secs(), bootout_detail(.bootout))]
    StillLoaded {
        /// How long the unload waited.
        waited: Duration,
        /// What `bootout` reported.
        bootout: Exit,
    },
    /// A file or directory under `~/Library` could not be written or removed.
    #[error("{}: {source}", .path.display())]
    File {
        /// The path that failed.
        path: PathBuf,
        /// The underlying error.
        #[source]
        source: io::Error,
    },
}

fn exit_message(code: Option<i32>, stderr: &str) -> String {
    let status = match code {
        Some(code) => format!("exit code {code}"),
        None => "killed by a signal".to_owned(),
    };
    if stderr.is_empty() {
        return status;
    }
    format!("{status}: {stderr}")
}

fn bootout_detail(bootout: &Exit) -> String {
    match bootout {
        Exit::Success => String::new(),
        Exit::Failure { code, stderr } => {
            format!(" (bootout: {})", exit_message(*code, stderr))
        }
    }
}

/// How one `launchctl` call ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Exit {
    /// Exit code `0`.
    Success,
    /// Any other ending.
    Failure {
        /// The exit code, `None` when a signal ended the process.
        code: Option<i32>,
        /// The last bytes written to stderr.
        stderr: String,
    },
}

/// Runs `launchctl` with the given arguments.
pub trait Launchctl {
    /// Runs one `launchctl` call and reports how it ended.
    fn run(&self, args: &[String]) -> impl Future<Output = Result<Exit, ServiceError>> + Send;
}

/// The real `launchctl`, with a deadline on every call.
#[derive(Debug, Clone)]
pub struct SystemLaunchctl {
    program: PathBuf,
    timeout: Duration,
}

impl SystemLaunchctl {
    /// Runs `/bin/launchctl` with [`LAUNCHCTL_TIMEOUT`].
    pub fn new() -> Self {
        Self {
            program: PathBuf::from(LAUNCHCTL),
            timeout: LAUNCHCTL_TIMEOUT,
        }
    }

    #[cfg(test)]
    fn with_program(program: PathBuf, timeout: Duration) -> Self {
        Self { program, timeout }
    }
}

impl Default for SystemLaunchctl {
    fn default() -> Self {
        Self::new()
    }
}

impl Launchctl for SystemLaunchctl {
    async fn run(&self, args: &[String]) -> Result<Exit, ServiceError> {
        let child = Command::new(&self.program)
            .args(args)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::piped())
            .kill_on_drop(true)
            .spawn()
            .map_err(ServiceError::Spawn)?;
        let Ok(output) = time::timeout(self.timeout, child.wait_with_output()).await else {
            let command = args.first().cloned().unwrap_or_default();
            return Err(ServiceError::Timeout(command));
        };
        let output = output.map_err(ServiceError::Spawn)?;
        if output.status.success() {
            return Ok(Exit::Success);
        }
        Ok(Exit::Failure {
            code: output.status.code(),
            stderr: stderr_tail(&output.stderr),
        })
    }
}

fn stderr_tail(bytes: &[u8]) -> String {
    let text = String::from_utf8_lossy(bytes);
    let text = text.trim();
    let mut start = text.len().saturating_sub(STDERR_TAIL);
    while !text.is_char_boundary(start) {
        start += 1;
    }
    text[start..].to_owned()
}

/// Where the LaunchAgent of one user lives.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Agent {
    /// The launchd domain, `gui/<uid>`.
    pub domain: String,
    /// `~/Library/LaunchAgents/<label>.plist`.
    pub plist: PathBuf,
    /// `~/Library/Logs/eventkit-bridge.log`, which receives stdout and stderr.
    pub log: PathBuf,
}

impl Agent {
    /// The agent of the user with `home` and `uid`.
    pub fn for_user(home: &Path, uid: u32) -> Self {
        Self {
            domain: format!("gui/{uid}"),
            plist: home.join(LAUNCH_AGENTS_DIR).join(format!("{LABEL}.plist")),
            log: home.join(LOG_RELATIVE_PATH),
        }
    }

    /// The agent of the current user, from `$HOME` and the real uid.
    pub fn current() -> Result<Self, ServiceError> {
        let home = home_dir(std::env::var_os("HOME"))?;
        let uid = rustix::process::getuid().as_raw();
        Ok(Self::for_user(&home, uid))
    }

    fn target(&self) -> String {
        format!("{}/{LABEL}", self.domain)
    }
}

fn home_dir(home: Option<OsString>) -> Result<PathBuf, ServiceError> {
    let Some(home) = home else {
        return Err(ServiceError::NoHome);
    };
    if home.is_empty() {
        return Err(ServiceError::NoHome);
    }
    Ok(PathBuf::from(home))
}

/// Where the binary sits relative to an app bundle.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum Housing {
    /// The binary is `<app>/Contents/MacOS/<binary>`.
    AppBundle,
    /// The binary is not inside an app bundle, so the Calendars grant will not survive an upgrade.
    Loose,
}

/// Tells whether `binary` is the executable of an `.app` bundle.
pub fn housing(binary: &Path) -> Housing {
    let Some(macos) = binary.parent() else {
        return Housing::Loose;
    };
    let Some(contents) = macos.parent() else {
        return Housing::Loose;
    };
    let Some(app) = contents.parent() else {
        return Housing::Loose;
    };
    if macos.file_name() != Some("MacOS".as_ref()) {
        return Housing::Loose;
    }
    if contents.file_name() != Some("Contents".as_ref()) {
        return Housing::Loose;
    }
    if app.extension() != Some("app".as_ref()) {
        return Housing::Loose;
    }
    Housing::AppBundle
}

/// Renders the LaunchAgent plist that runs `binary` and sends its output to `log`.
pub fn render_plist(binary: &str, log: &str) -> String {
    let binary = escape_xml(binary);
    let log = escape_xml(log);
    let exit_timeout = EXIT_TIMEOUT.as_secs();
    format!(
        r#"<?xml version="1.0" encoding="UTF-8"?>
<!DOCTYPE plist PUBLIC "-//Apple//DTD PLIST 1.0//EN" "http://www.apple.com/DTDs/PropertyList-1.0.dtd">
<plist version="1.0">
<dict>
	<key>Label</key>
	<string>{LABEL}</string>
	<key>ProgramArguments</key>
	<array>
		<string>{binary}</string>
	</array>
	<key>RunAtLoad</key>
	<true/>
	<key>KeepAlive</key>
	<dict>
		<key>PathState</key>
		<dict>
			<key>{binary}</key>
			<true/>
		</dict>
	</dict>
	<key>ExitTimeOut</key>
	<integer>{exit_timeout}</integer>
	<key>AssociatedBundleIdentifiers</key>
	<array>
		<string>{BUNDLE_ID}</string>
	</array>
	<key>StandardOutPath</key>
	<string>{log}</string>
	<key>StandardErrorPath</key>
	<string>{log}</string>
</dict>
</plist>
"#
    )
}

fn escape_xml(value: &str) -> String {
    let mut out = String::with_capacity(value.len());
    for ch in value.chars() {
        match ch {
            '&' => out.push_str("&amp;"),
            '<' => out.push_str("&lt;"),
            '>' => out.push_str("&gt;"),
            '"' => out.push_str("&quot;"),
            '\'' => out.push_str("&apos;"),
            other => out.push(other),
        }
    }
    out
}

/// How long `install` and `uninstall` wait for an unloaded agent to disappear.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct UnloadWait {
    /// The longest wait after `bootout`.
    pub timeout: Duration,
    /// The pause between two `launchctl print` checks.
    pub poll: Duration,
}

impl Default for UnloadWait {
    fn default() -> Self {
        Self {
            timeout: EXIT_TIMEOUT,
            poll: Duration::from_millis(100),
        }
    }
}

/// What `install` did.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Installed {
    /// The written plist.
    pub plist: PathBuf,
    /// The program the agent runs.
    pub program: PathBuf,
    /// Whether the program sits inside an app bundle.
    pub housing: Housing,
}

/// What `uninstall` did.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Uninstalled {
    /// The agent was unloaded or its plist removed.
    Removed,
    /// There was no loaded agent and no plist.
    NothingInstalled,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Unload {
    Unloaded,
    NotLoaded,
}

/// Installs and uninstalls the LaunchAgent through a [`Launchctl`].
#[derive(Debug, Clone)]
pub struct Service<L> {
    launchctl: L,
    agent: Agent,
    wait: UnloadWait,
}

impl<L: Launchctl> Service<L> {
    /// A service for `agent` that calls `launchctl` and waits per `wait`.
    pub fn new(launchctl: L, agent: Agent, wait: UnloadWait) -> Self {
        Self {
            launchctl,
            agent,
            wait,
        }
    }

    /// Unloads any existing agent, writes the plist for `binary` and bootstraps it.
    pub async fn install(&self, binary: &Path) -> Result<Installed, ServiceError> {
        let Agent { domain, plist, log } = &self.agent;
        let program = utf8(binary)?;
        let plist_text = render_plist(program, utf8(log)?);
        let plist_path = utf8(plist)?;
        self.unload().await?;
        create_parent(plist)?;
        create_parent(log)?;
        fs::write(plist, plist_text).map_err(|source| ServiceError::File {
            path: plist.clone(),
            source,
        })?;
        let args = [
            "bootstrap".to_owned(),
            domain.clone(),
            plist_path.to_owned(),
        ];
        self.require_success(&args).await?;
        Ok(Installed {
            plist: plist.clone(),
            program: binary.to_path_buf(),
            housing: housing(binary),
        })
    }

    /// Unloads the agent and removes its plist; the config is left alone.
    pub async fn uninstall(&self) -> Result<Uninstalled, ServiceError> {
        let unload = self.unload().await?;
        let plist = &self.agent.plist;
        let removed = match fs::remove_file(plist) {
            Ok(()) => true,
            Err(err) if err.kind() == io::ErrorKind::NotFound => false,
            Err(source) => {
                return Err(ServiceError::File {
                    path: plist.clone(),
                    source,
                });
            }
        };
        match (unload, removed) {
            (Unload::NotLoaded, false) => Ok(Uninstalled::NothingInstalled),
            (Unload::NotLoaded, true) => Ok(Uninstalled::Removed),
            (Unload::Unloaded, true) => Ok(Uninstalled::Removed),
            (Unload::Unloaded, false) => Ok(Uninstalled::Removed),
        }
    }

    async fn unload(&self) -> Result<Unload, ServiceError> {
        if !self.loaded().await? {
            return Ok(Unload::NotLoaded);
        }
        let bootout = self
            .launchctl
            .run(&["bootout".to_owned(), self.agent.target()])
            .await?;
        let started = Instant::now();
        loop {
            if !self.loaded().await? {
                return Ok(Unload::Unloaded);
            }
            if started.elapsed() >= self.wait.timeout {
                return Err(ServiceError::StillLoaded {
                    waited: self.wait.timeout,
                    bootout,
                });
            }
            time::sleep(self.wait.poll).await;
        }
    }

    async fn loaded(&self) -> Result<bool, ServiceError> {
        let exit = self
            .launchctl
            .run(&["print".to_owned(), self.agent.target()])
            .await?;
        match exit {
            Exit::Success => Ok(true),
            Exit::Failure { .. } => Ok(false),
        }
    }

    async fn require_success(&self, args: &[String]) -> Result<(), ServiceError> {
        match self.launchctl.run(args).await? {
            Exit::Success => Ok(()),
            Exit::Failure { code, stderr } => Err(ServiceError::Failed {
                command: args[0].clone(),
                code,
                stderr,
            }),
        }
    }
}

fn utf8(path: &Path) -> Result<&str, ServiceError> {
    let Some(text) = path.to_str() else {
        return Err(ServiceError::NonUtf8Path(path.to_path_buf()));
    };
    Ok(text)
}

fn create_parent(path: &Path) -> Result<(), ServiceError> {
    let Some(parent) = path.parent() else {
        return Ok(());
    };
    fs::create_dir_all(parent).map_err(|source| ServiceError::File {
        path: parent.to_path_buf(),
        source,
    })
}

#[cfg(test)]
mod tests {
    use std::collections::VecDeque;
    use std::sync::Mutex;

    use plist::Value;

    use super::*;
    use crate::fake_ekctl::Fake;

    const UID: u32 = 501;
    const TARGET: &str = "gui/501/dev.pkarpovich.eventkit-bridge";

    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Call {
        args: Vec<String>,
        plist: Option<String>,
    }

    struct Recording {
        plist: PathBuf,
        prints: Mutex<VecDeque<Exit>>,
        bootout: Exit,
        bootstrap: Exit,
        calls: Mutex<Vec<Call>>,
    }

    impl Recording {
        fn new(plist: &Path, prints: &[Exit]) -> Self {
            Self {
                plist: plist.to_path_buf(),
                prints: Mutex::new(prints.iter().cloned().collect()),
                bootout: Exit::Success,
                bootstrap: Exit::Success,
                calls: Mutex::new(Vec::new()),
            }
        }

        fn calls(&self) -> Vec<Call> {
            self.calls.lock().unwrap().clone()
        }

        fn subcommands(&self) -> Vec<String> {
            let mut out = Vec::new();
            for Call { args, plist: _ } in self.calls() {
                out.push(args[0].clone());
            }
            out
        }
    }

    impl Launchctl for &Recording {
        async fn run(&self, args: &[String]) -> Result<Exit, ServiceError> {
            let plist = fs::read_to_string(&self.plist).ok();
            self.calls.lock().unwrap().push(Call {
                args: args.to_vec(),
                plist,
            });
            let exit = match args[0].as_str() {
                "print" => self
                    .prints
                    .lock()
                    .unwrap()
                    .pop_front()
                    .unwrap_or_else(not_found),
                "bootout" => self.bootout.clone(),
                "bootstrap" => self.bootstrap.clone(),
                other => panic!("unexpected launchctl {other}"),
            };
            Ok(exit)
        }
    }

    fn not_found() -> Exit {
        Exit::Failure {
            code: Some(113),
            stderr: "Could not find service".to_owned(),
        }
    }

    #[test]
    fn unload_waits_out_the_daemon_drain() {
        let UnloadWait { timeout, poll: _ } = UnloadWait::default();
        assert!(timeout > SHUTDOWN_GRACE);
        assert!(LAUNCHCTL_TIMEOUT > timeout);
    }

    fn fast() -> UnloadWait {
        UnloadWait {
            timeout: Duration::from_millis(200),
            poll: Duration::from_millis(5),
        }
    }

    struct Home {
        dir: tempfile::TempDir,
        agent: Agent,
    }

    impl Home {
        fn new() -> Self {
            let dir = tempfile::tempdir().unwrap();
            let agent = Agent::for_user(dir.path(), UID);
            Self { dir, agent }
        }

        fn binary(&self) -> PathBuf {
            self.dir
                .path()
                .join("Applications/EventKitBridge.app/Contents/MacOS/eventkit-bridge")
        }

        fn service<'a>(&self, launchctl: &'a Recording) -> Service<&'a Recording> {
            Service::new(launchctl, self.agent.clone(), fast())
        }
    }

    fn args(items: &[&str]) -> Vec<String> {
        let mut out = Vec::new();
        for item in items {
            out.push((*item).to_owned());
        }
        out
    }

    fn dict(value: &Value) -> &plist::Dictionary {
        value.as_dictionary().unwrap()
    }

    #[test]
    fn agent_paths_for_user() {
        let agent = Agent::for_user(Path::new("/Users/me"), UID);
        assert_eq!(
            agent,
            Agent {
                domain: "gui/501".to_owned(),
                plist: PathBuf::from(
                    "/Users/me/Library/LaunchAgents/dev.pkarpovich.eventkit-bridge.plist"
                ),
                log: PathBuf::from("/Users/me/Library/Logs/eventkit-bridge.log"),
            }
        );
        assert_eq!(agent.target(), TARGET);
    }

    #[test]
    fn home_must_be_set() {
        let Err(ServiceError::NoHome) = home_dir(None) else {
            panic!("unset HOME accepted");
        };
        let Err(ServiceError::NoHome) = home_dir(Some(OsString::new())) else {
            panic!("empty HOME accepted");
        };
        assert_eq!(
            home_dir(Some(OsString::from("/Users/me"))).unwrap(),
            PathBuf::from("/Users/me")
        );
    }

    #[test]
    fn rendered_plist_is_valid() {
        let binary = "/Applications/EventKitBridge.app/Contents/MacOS/eventkit-bridge";
        let log = "/Users/me/Library/Logs/eventkit-bridge.log";
        let text = render_plist(binary, log);
        let value = Value::from_reader_xml(text.as_bytes()).unwrap();
        let root = dict(&value);
        assert_eq!(
            root.get("Label").unwrap().as_string(),
            Some("dev.pkarpovich.eventkit-bridge")
        );
        let program = root.get("ProgramArguments").unwrap().as_array().unwrap();
        assert_eq!(program.len(), 1);
        assert_eq!(program[0].as_string(), Some(binary));
        assert_eq!(root.get("RunAtLoad").unwrap().as_boolean(), Some(true));
        let keep_alive = dict(root.get("KeepAlive").unwrap());
        let path_state = dict(keep_alive.get("PathState").unwrap());
        assert_eq!(path_state.len(), 1);
        assert_eq!(path_state.get(binary).unwrap().as_boolean(), Some(true));
        let exit_timeout = root.get("ExitTimeOut").unwrap().as_unsigned_integer();
        assert_eq!(exit_timeout, Some(30));
        assert!(Duration::from_secs(30) > SHUTDOWN_GRACE);
        let bundles = root
            .get("AssociatedBundleIdentifiers")
            .unwrap()
            .as_array()
            .unwrap();
        assert_eq!(bundles.len(), 1);
        assert_eq!(bundles[0].as_string(), Some(BUNDLE_ID));
        assert_eq!(root.get("StandardOutPath").unwrap().as_string(), Some(log));
        assert_eq!(
            root.get("StandardErrorPath").unwrap().as_string(),
            Some(log)
        );
        assert_eq!(root.len(), 8);
    }

    #[test]
    fn rendered_plist_escapes_paths() {
        let binary = "/Users/me/Tom & Jerry <dev>/\"x\"/'y'/eventkit-bridge";
        let text = render_plist(binary, "/tmp/a&b.log");
        let value = Value::from_reader_xml(text.as_bytes()).unwrap();
        let root = dict(&value);
        let program = root.get("ProgramArguments").unwrap().as_array().unwrap();
        assert_eq!(program[0].as_string(), Some(binary));
        let keep_alive = dict(root.get("KeepAlive").unwrap());
        assert!(dict(keep_alive.get("PathState").unwrap()).contains_key(binary));
        assert_eq!(
            root.get("StandardOutPath").unwrap().as_string(),
            Some("/tmp/a&b.log")
        );
    }

    #[test]
    fn housing_inside_app_bundle() {
        assert_eq!(
            housing(Path::new(
                "/Applications/EventKitBridge.app/Contents/MacOS/eventkit-bridge"
            )),
            Housing::AppBundle
        );
        assert_eq!(
            housing(Path::new("/Users/me/dist/Other.app/Contents/MacOS/x")),
            Housing::AppBundle
        );
    }

    #[test]
    fn housing_outside_app_bundle() {
        for path in [
            "/opt/homebrew/bin/eventkit-bridge",
            "/Users/me/src/target/release/eventkit-bridge",
            "/Applications/EventKitBridge.app/Contents/Resources/eventkit-bridge",
            "/Applications/EventKitBridge.app/Other/MacOS/eventkit-bridge",
            "/Applications/EventKitBridge/Contents/MacOS/eventkit-bridge",
            "/Applications/EventKitBridge.app/eventkit-bridge",
            "Contents/MacOS/eventkit-bridge",
            "eventkit-bridge",
            "/",
        ] {
            assert_eq!(housing(Path::new(path)), Housing::Loose, "{path}");
        }
    }

    #[tokio::test]
    async fn fresh_install_writes_then_bootstraps() {
        let home = Home::new();
        let launchctl = Recording::new(&home.agent.plist, &[]);
        let binary = home.binary();
        let installed = home.service(&launchctl).install(&binary).await.unwrap();
        let plist = home.agent.plist.to_str().unwrap();
        let rendered = render_plist(binary.to_str().unwrap(), home.agent.log.to_str().unwrap());
        assert_eq!(
            launchctl.calls(),
            vec![
                Call {
                    args: args(&["print", TARGET]),
                    plist: None,
                },
                Call {
                    args: args(&["bootstrap", "gui/501", plist]),
                    plist: Some(rendered.clone()),
                },
            ]
        );
        assert_eq!(fs::read_to_string(&home.agent.plist).unwrap(), rendered);
        assert!(home.agent.log.parent().unwrap().is_dir());
        assert_eq!(
            installed,
            Installed {
                plist: home.agent.plist.clone(),
                program: binary.clone(),
                housing: Housing::AppBundle,
            }
        );
    }

    #[tokio::test]
    async fn reinstall_unloads_waits_writes_and_bootstraps_in_order() {
        let home = Home::new();
        fs::create_dir_all(home.agent.plist.parent().unwrap()).unwrap();
        fs::write(&home.agent.plist, "old").unwrap();
        let launchctl = Recording::new(
            &home.agent.plist,
            &[Exit::Success, Exit::Success, Exit::Success, not_found()],
        );
        let binary = home.binary();
        home.service(&launchctl).install(&binary).await.unwrap();
        let rendered = render_plist(binary.to_str().unwrap(), home.agent.log.to_str().unwrap());
        let old = Some("old".to_owned());
        assert_eq!(
            launchctl.calls(),
            vec![
                Call {
                    args: args(&["print", TARGET]),
                    plist: old.clone(),
                },
                Call {
                    args: args(&["bootout", TARGET]),
                    plist: old.clone(),
                },
                Call {
                    args: args(&["print", TARGET]),
                    plist: old.clone(),
                },
                Call {
                    args: args(&["print", TARGET]),
                    plist: old.clone(),
                },
                Call {
                    args: args(&["print", TARGET]),
                    plist: old,
                },
                Call {
                    args: args(&["bootstrap", "gui/501", home.agent.plist.to_str().unwrap()]),
                    plist: Some(rendered),
                },
            ]
        );
    }

    #[tokio::test]
    async fn install_gives_up_when_agent_stays_loaded() {
        let home = Home::new();
        let mut launchctl = Recording::new(&home.agent.plist, &vec![Exit::Success; 1000]);
        launchctl.bootout = Exit::Failure {
            code: Some(5),
            stderr: "Input/output error".to_owned(),
        };
        let err = home
            .service(&launchctl)
            .install(&home.binary())
            .await
            .unwrap_err();
        let ServiceError::StillLoaded { .. } = err else {
            panic!("unexpected error {err:?}");
        };
        assert!(err.to_string().contains("Input/output error"), "{err}");
        assert!(!launchctl.subcommands().contains(&"bootstrap".to_owned()));
        assert!(!home.agent.plist.exists());
    }

    #[tokio::test]
    async fn install_reports_bootstrap_failure() {
        let home = Home::new();
        let mut launchctl = Recording::new(&home.agent.plist, &[]);
        launchctl.bootstrap = Exit::Failure {
            code: Some(5),
            stderr: "Bootstrap failed: 5: Input/output error".to_owned(),
        };
        let err = home
            .service(&launchctl)
            .install(&home.binary())
            .await
            .unwrap_err();
        assert_eq!(
            err.to_string(),
            "launchctl bootstrap failed: exit code 5: Bootstrap failed: 5: Input/output error"
        );
    }

    #[tokio::test]
    async fn install_reports_loose_binary() {
        let home = Home::new();
        let launchctl = Recording::new(&home.agent.plist, &[]);
        let binary = home.dir.path().join("target/release/eventkit-bridge");
        let installed = home.service(&launchctl).install(&binary).await.unwrap();
        assert_eq!(installed.housing, Housing::Loose);
    }

    #[tokio::test]
    async fn uninstall_unloads_and_removes_plist() {
        let home = Home::new();
        fs::create_dir_all(home.agent.plist.parent().unwrap()).unwrap();
        fs::write(&home.agent.plist, "old").unwrap();
        let launchctl = Recording::new(&home.agent.plist, &[Exit::Success, Exit::Success]);
        let result = home.service(&launchctl).uninstall().await.unwrap();
        assert_eq!(result, Uninstalled::Removed);
        assert_eq!(
            launchctl.subcommands(),
            args(&["print", "bootout", "print", "print"])
        );
        assert!(!home.agent.plist.exists());
    }

    #[tokio::test]
    async fn uninstall_removes_plist_of_unloaded_agent() {
        let home = Home::new();
        fs::create_dir_all(home.agent.plist.parent().unwrap()).unwrap();
        fs::write(&home.agent.plist, "old").unwrap();
        let launchctl = Recording::new(&home.agent.plist, &[]);
        let result = home.service(&launchctl).uninstall().await.unwrap();
        assert_eq!(result, Uninstalled::Removed);
        assert_eq!(launchctl.subcommands(), args(&["print"]));
        assert!(!home.agent.plist.exists());
    }

    #[tokio::test]
    async fn uninstall_without_agent() {
        let home = Home::new();
        let launchctl = Recording::new(&home.agent.plist, &[]);
        let result = home.service(&launchctl).uninstall().await.unwrap();
        assert_eq!(result, Uninstalled::NothingInstalled);
    }

    #[tokio::test]
    async fn system_launchctl_reports_success() {
        let fake = Fake::new("printf '%s\\n' \"$@\" > \"$LOG\"");
        let launchctl =
            SystemLaunchctl::with_program(fake.program().to_path_buf(), LAUNCHCTL_TIMEOUT);
        let exit = launchctl.run(&args(&["print", TARGET])).await.unwrap();
        assert_eq!(exit, Exit::Success);
        assert_eq!(fake.log(), format!("print\n{TARGET}\n"));
    }

    #[tokio::test]
    async fn system_launchctl_reports_failure_with_stderr() {
        let fake = Fake::new("echo 'Could not find service' >&2\nexit 113");
        let launchctl =
            SystemLaunchctl::with_program(fake.program().to_path_buf(), LAUNCHCTL_TIMEOUT);
        let exit = launchctl.run(&args(&["print", TARGET])).await.unwrap();
        assert_eq!(
            exit,
            Exit::Failure {
                code: Some(113),
                stderr: "Could not find service".to_owned(),
            }
        );
    }

    #[tokio::test]
    async fn system_launchctl_kills_at_deadline() {
        let fake = Fake::new("sleep 5\necho done > \"$LOG\"");
        let launchctl =
            SystemLaunchctl::with_program(fake.program().to_path_buf(), Duration::from_millis(200));
        let started = Instant::now();
        let err = launchctl
            .run(&args(&["bootout", TARGET]))
            .await
            .unwrap_err();
        let ServiceError::Timeout(command) = err else {
            panic!("unexpected error {err:?}");
        };
        assert_eq!(command, "bootout");
        assert!(started.elapsed() < Duration::from_secs(3));
        time::sleep(Duration::from_millis(300)).await;
        assert_eq!(fake.log(), "");
    }

    #[tokio::test]
    async fn system_launchctl_missing_binary() {
        let dir = tempfile::tempdir().unwrap();
        let launchctl =
            SystemLaunchctl::with_program(dir.path().join("launchctl"), LAUNCHCTL_TIMEOUT);
        let err = launchctl.run(&args(&["print", TARGET])).await.unwrap_err();
        let ServiceError::Spawn(_) = err else {
            panic!("unexpected error {err:?}");
        };
    }

    #[test]
    fn stderr_tail_keeps_the_end() {
        let long = format!("{}é{}", "a".repeat(600), "b".repeat(10));
        let tail = stderr_tail(long.as_bytes());
        assert!(tail.len() <= STDERR_TAIL);
        assert!(tail.ends_with("bbbbbbbbbb"));
    }
}
