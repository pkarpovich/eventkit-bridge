use std::path::PathBuf;
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;

use crate::config::AccountId;
use crate::mail::MessageId;
use crate::subprocess::{self, Output, RunError};

/// Where macOS keeps `osascript`.
pub const OSASCRIPT: &str = "/usr/bin/osascript";
/// How long one Mail call may run before it is killed.
pub const DEADLINE: Duration = Duration::from_secs(30);

const NOT_PERMITTED: i32 = -1743;
const NO_SUCH_OBJECT: i32 = -1728;
const EVENT_TIMED_OUT: i32 = -1712;

/// The AppleScript every junk call runs; values reach it only through `argv`.
pub const SCRIPT: &str = r#"on run argv
  set {acctId, sourcePath, msgId, targetPath, junkValue} to argv
  tell application "Mail"
    with timeout of 20 seconds
      set acct to account id acctId
      set m to first message of mailbox sourcePath of acct whose id is (msgId as integer)
      set rfcId to message id of m
      set junk mail status of m to (junkValue is "true")
      if sourcePath is targetPath then return msgId
      if rfcId is missing value or rfcId is "" then
        move m to mailbox targetPath of acct
        return ""
      end if
      set known to id of (messages of mailbox targetPath of acct whose message id is rfcId)
      move m to mailbox targetPath of acct
      set giveUp to (current date) + 5
      repeat
        repeat with hit in (id of (messages of mailbox targetPath of acct whose message id is rfcId))
          if known does not contain (contents of hit) then return (contents of hit) as text
        end repeat
        if (current date) > giveUp then return ""
        delay 0.2
      end repeat
    end timeout
  end tell
end run"#;

/// Why Mail did not mark and move a message.
#[derive(Debug, thiserror::Error)]
pub enum ScriptError {
    /// `osascript` could not be started.
    #[error("osascript could not start: {0}")]
    Spawn(#[source] std::io::Error),
    /// Reading `osascript`'s output or waiting for it failed.
    #[error("osascript i/o failed: {0}")]
    Io(#[source] std::io::Error),
    /// `osascript` wrote more than the stdout cap and was killed.
    #[error("output too large")]
    OutputTooLarge,
    /// The user has not allowed EventKitBridge to control Mail (`-1743`).
    #[error("Mail automation not permitted")]
    NotPermitted,
    /// Mail has no such account, mailbox or message (`-1728`).
    #[error("message not found")]
    NotFound,
    /// Mail did not answer an Apple Event in time (`-1712`), or `osascript` ran past its deadline.
    #[error("Mail did not answer")]
    Timeout,
    /// `osascript` failed for another reason.
    #[error("{}", failure_message(*.code))]
    Failed {
        /// The AppleScript error number, `None` when stderr carries none.
        code: Option<i32>,
    },
    /// `osascript` succeeded but printed neither an id nor an empty line.
    #[error("unexpected osascript output")]
    UnexpectedOutput,
}

impl From<RunError> for ScriptError {
    fn from(err: RunError) -> Self {
        match err {
            RunError::Spawn(source) => ScriptError::Spawn(source),
            RunError::Io(source) => ScriptError::Io(source),
            RunError::Timeout => ScriptError::Timeout,
            RunError::OutputTooLarge => ScriptError::OutputTooLarge,
        }
    }
}

fn failure_message(code: Option<i32>) -> String {
    match code {
        Some(code) => format!("Mail failed with error {code}"),
        None => "Mail failed".to_owned(),
    }
}

/// The junk status a message is given.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JunkStatus {
    /// Junk.
    Junk,
    /// Not junk.
    NotJunk,
}

impl From<bool> for JunkStatus {
    fn from(junk: bool) -> Self {
        match junk {
            true => JunkStatus::Junk,
            false => JunkStatus::NotJunk,
        }
    }
}

impl JunkStatus {
    /// Whether this status is junk.
    pub fn is_junk(self) -> bool {
        match self {
            JunkStatus::Junk => true,
            JunkStatus::NotJunk => false,
        }
    }

    fn as_str(self) -> &'static str {
        match self {
            JunkStatus::Junk => "true",
            JunkStatus::NotJunk => "false",
        }
    }
}

/// One message to mark and move, addressed as the Envelope Index stores it.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct JunkMove {
    /// The account holding the message.
    pub account: AccountId,
    /// The decoded path of its current mailbox, exact case.
    pub source: String,
    /// The message's current id.
    pub id: MessageId,
    /// The decoded path of the mailbox to move it to, exact case.
    pub target: String,
    /// The junk status to set.
    pub status: JunkStatus,
}

/// The message's id after the move; `None` when the moved copy did not show up in time.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct Moved(pub Option<MessageId>);

/// Runs [`SCRIPT`] through `osascript`, one call at a time.
#[derive(Debug, Clone)]
pub struct Runner {
    program: PathBuf,
    timeout: Duration,
    lock: Arc<Mutex<()>>,
}

impl Runner {
    /// A runner for the `osascript` at `program`, killing any call that outlives `timeout`.
    pub fn new(program: PathBuf, timeout: Duration) -> Self {
        Self {
            program,
            timeout,
            lock: Arc::default(),
        }
    }

    /// Sets the junk status of one message and moves it to its target, waiting for every other
    /// call through this runner or its clones to end first.
    pub async fn junk(&self, request: &JunkMove) -> Result<Moved, ScriptError> {
        let _guard = self.lock.lock().await;
        let Output {
            stdout,
            stderr,
            status,
        } = subprocess::run(&self.program, &argv(request), self.timeout).await?;
        if !status.success() {
            return Err(classify(&stderr));
        }
        parse_moved(&stdout)
    }
}

fn argv(request: &JunkMove) -> Vec<String> {
    let JunkMove {
        account,
        source,
        id,
        target,
        status,
    } = request;
    vec![
        "-e".to_owned(),
        SCRIPT.to_owned(),
        account.as_str().to_owned(),
        source.clone(),
        id.to_string(),
        target.clone(),
        status.as_str().to_owned(),
    ]
}

fn classify(stderr: &[u8]) -> ScriptError {
    match error_code(stderr) {
        Some(NOT_PERMITTED) => ScriptError::NotPermitted,
        Some(NO_SUCH_OBJECT) => ScriptError::NotFound,
        Some(EVENT_TIMED_OUT) => ScriptError::Timeout,
        code => ScriptError::Failed { code },
    }
}

fn error_code(stderr: &[u8]) -> Option<i32> {
    let text = String::from_utf8_lossy(stderr);
    let text = text.trim_end().strip_suffix(')')?;
    let (_, code) = text.rsplit_once('(')?;
    code.parse().ok()
}

fn parse_moved(stdout: &[u8]) -> Result<Moved, ScriptError> {
    let Ok(text) = std::str::from_utf8(stdout) else {
        return Err(ScriptError::UnexpectedOutput);
    };
    let text = text.trim();
    if text.is_empty() {
        return Ok(Moved(None));
    }
    let Some(id) = MessageId::parse(text) else {
        return Err(ScriptError::UnexpectedOutput);
    };
    Ok(Moved(Some(id)))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::fake_ekctl::Fake;

    const ACCOUNT: &str = "6A1F9C2E-3B4D-4E5F-8A9B-0C1D2E3F4A5B";
    const RECORD: &str = "for arg in \"$@\"; do printf '%s\\0' \"$arg\" >> \"$LOG\"; done";

    fn request(source: &str, target: &str, status: JunkStatus) -> JunkMove {
        JunkMove {
            account: AccountId::parse(ACCOUNT).unwrap(),
            source: source.to_owned(),
            id: MessageId::new(383621).unwrap(),
            target: target.to_owned(),
            status,
        }
    }

    fn runner(fake: &Fake) -> Runner {
        Runner::new(fake.program().to_owned(), Duration::from_secs(10))
    }

    fn failing(stderr: &str) -> Fake {
        Fake::new(&format!("cat >&2 <<'ERR'\n{stderr}\nERR\nexit 1"))
    }

    async fn junk_error(fake: &Fake) -> ScriptError {
        runner(fake)
            .junk(&request("INBOX", "[Gmail]/Spam", JunkStatus::Junk))
            .await
            .unwrap_err()
    }

    #[tokio::test]
    async fn passes_values_after_the_script() {
        let fake = Fake::new(&format!("{RECORD}\necho 383622"));
        let source = "Folders/\"Quoted\" [Box] 'it''s' $HOME";
        let target = "[Gmail]/Spam";
        let moved = runner(&fake)
            .junk(&request(source, target, JunkStatus::Junk))
            .await
            .unwrap();
        assert_eq!(moved, Moved(MessageId::new(383622)));
        assert_eq!(
            fake.recorded_args(),
            ["-e", SCRIPT, ACCOUNT, source, "383621", target, "true"]
        );
    }

    #[tokio::test]
    async fn not_junk_passes_false() {
        let fake = Fake::new(&format!("{RECORD}\necho 383621"));
        let moved = runner(&fake)
            .junk(&request("Inbox", "Inbox", JunkStatus::NotJunk))
            .await
            .unwrap();
        assert_eq!(moved, Moved(MessageId::new(383621)));
        assert_eq!(
            fake.recorded_args(),
            ["-e", SCRIPT, ACCOUNT, "Inbox", "383621", "Inbox", "false"]
        );
    }

    #[test]
    fn runs_the_system_osascript_with_a_thirty_second_deadline() {
        assert_eq!(OSASCRIPT, "/usr/bin/osascript");
        assert_eq!(DEADLINE, Duration::from_secs(30));
    }

    #[test]
    fn script_reads_every_value_from_argv() {
        assert!(SCRIPT.starts_with("on run argv\n"));
        assert!(SCRIPT.contains("set {acctId, sourcePath, msgId, targetPath, junkValue} to argv"));
        assert!(!SCRIPT.contains(ACCOUNT));
        assert!(!SCRIPT.contains("Spam"));
        assert!(!SCRIPT.contains("INBOX"));
    }

    #[tokio::test]
    async fn empty_output_is_a_copy_not_found() {
        let fake = Fake::new("echo");
        let moved = runner(&fake)
            .junk(&request("INBOX", "[Gmail]/Spam", JunkStatus::Junk))
            .await
            .unwrap();
        assert_eq!(moved, Moved(None));
    }

    #[tokio::test]
    async fn garbage_output_is_refused() {
        for output in ["abc", "0", "-5", "383622 383623", "missing value"] {
            let fake = Fake::new(&format!("echo '{output}'"));
            let err = junk_error(&fake).await;
            let ScriptError::UnexpectedOutput = err else {
                panic!("{output:?}: unexpected error: {err:?}");
            };
        }
    }

    #[tokio::test]
    async fn automation_denied() {
        let fake =
            failing("0:120: execution error: Not authorized to send Apple events to Mail. (-1743)");
        let err = junk_error(&fake).await;
        let ScriptError::NotPermitted = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[tokio::test]
    async fn message_gone() {
        let fake = failing(
            "0:215: execution error: Mail got an error: Can’t get mailbox \"[Gmail]/Spam\" of account id \"x\". (-1728)",
        );
        let err = junk_error(&fake).await;
        let ScriptError::NotFound = err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(err.to_string(), "message not found");
    }

    #[tokio::test]
    async fn apple_event_timeout() {
        let fake =
            failing("0:301: execution error: Mail got an error: AppleEvent timed out. (-1712)");
        let err = junk_error(&fake).await;
        let ScriptError::Timeout = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[tokio::test]
    async fn other_errors_keep_only_the_code() {
        let fake = failing(
            "0:88: execution error: Mail got an error: Secret Subject \"[Gmail]/Spam\" (-10000)",
        );
        let err = junk_error(&fake).await;
        let ScriptError::Failed { code } = &err else {
            panic!("unexpected error: {err:?}");
        };
        assert_eq!(*code, Some(-10000));
        let message = err.to_string();
        assert_eq!(message, "Mail failed with error -10000");
        assert!(!message.contains("Spam"));
    }

    #[tokio::test]
    async fn errors_without_a_code() {
        for stderr in ["", "syntax error: oops", "(abc)", "trailing (-1743) text"] {
            let fake = failing(stderr);
            let err = junk_error(&fake).await;
            let ScriptError::Failed { code: None } = err else {
                panic!("{stderr:?}: unexpected error: {err:?}");
            };
        }
    }

    #[tokio::test]
    async fn deadline_kills_osascript() {
        let fake = Fake::new("sleep 5\necho 1");
        let runner = Runner::new(fake.program().to_owned(), Duration::from_millis(200));
        let started = std::time::Instant::now();
        let err = runner
            .junk(&request("INBOX", "[Gmail]/Spam", JunkStatus::Junk))
            .await
            .unwrap_err();
        let ScriptError::Timeout = err else {
            panic!("unexpected error: {err:?}");
        };
        assert!(started.elapsed() < Duration::from_secs(3));
    }

    #[tokio::test]
    async fn missing_program() {
        let dir = tempfile::tempdir().unwrap();
        let runner = Runner::new(dir.path().join("osascript"), Duration::from_secs(5));
        let err = runner
            .junk(&request("INBOX", "[Gmail]/Spam", JunkStatus::Junk))
            .await
            .unwrap_err();
        let ScriptError::Spawn(_) = err else {
            panic!("unexpected error: {err:?}");
        };
    }

    #[tokio::test]
    async fn calls_run_one_at_a_time() {
        let fake = Fake::new("echo start >> \"$LOG\"\nsleep 0.2\necho end >> \"$LOG\"\necho 7");
        let first = runner(&fake);
        let second = first.clone();
        let one = request("INBOX", "[Gmail]/Spam", JunkStatus::Junk);
        let (a, b) = tokio::join!(first.junk(&one), second.junk(&one));
        assert_eq!(a.unwrap(), Moved(MessageId::new(7)));
        assert_eq!(b.unwrap(), Moved(MessageId::new(7)));
        assert_eq!(fake.calls(), ["start", "end", "start", "end"]);
    }
}
