use std::fs;
use std::os::unix::fs::PermissionsExt;
use std::path::{Path, PathBuf};
use std::time::Duration;

use tempfile::TempDir;

use crate::ekctl::Runner;

pub(crate) struct Fake {
    _dir: TempDir,
    program: PathBuf,
    log: PathBuf,
}

impl Fake {
    pub(crate) fn new(body: &str) -> Self {
        let dir = tempfile::tempdir().unwrap();
        let log = dir.path().join("log");
        let source = dir.path().join("script");
        let program = dir.path().join("ekctl");
        fs::write(
            &source,
            format!("#!/bin/sh\nLOG='{}'\n{body}\n", log.display()),
        )
        .unwrap();
        let copied = std::process::Command::new("cp")
            .arg(&source)
            .arg(&program)
            .status()
            .unwrap();
        assert!(copied.success());
        fs::set_permissions(&program, fs::Permissions::from_mode(0o755)).unwrap();
        Self {
            _dir: dir,
            program,
            log,
        }
    }

    pub(crate) fn printing(fixture_name: &str) -> Self {
        Self::new(&format!("cat '{}'", fixture(fixture_name).display()))
    }

    pub(crate) fn recording(fixture_name: &str) -> Self {
        Self::new(&format!(
            "for arg in \"$@\"; do printf '%s\\0' \"$arg\" >> \"$LOG\"; done\ncat '{}'",
            fixture(fixture_name).display()
        ))
    }

    pub(crate) fn recording_json(json: &str) -> Self {
        Self::new(&format!(
            "for arg in \"$@\"; do printf '%s\\0' \"$arg\" >> \"$LOG\"; done\ncat <<'JSON'\n{json}\nJSON"
        ))
    }

    pub(crate) fn scripted(responses: &[(&str, &str)]) -> Self {
        let mut body = String::from("printf '%s\\n' \"$*\" >> \"$LOG\"\ncase \"$1 $2\" in\n");
        for (command, json) in responses {
            let pattern = match *command {
                "free" => "free\\ *".to_owned(),
                command => format!("'{command}'"),
            };
            body.push_str(&format!("  {pattern}) cat <<'JSON'\n{json}\nJSON\n  ;;\n"));
        }
        body.push_str("  *) echo '{\"status\":\"error\",\"error\":\"unexpected call\"}' ;;\nesac");
        Self::new(&body)
    }

    pub(crate) fn calls(&self) -> Vec<String> {
        let mut calls = Vec::new();
        for line in self.log().lines() {
            calls.push(line.to_owned());
        }
        calls
    }

    pub(crate) fn runner(&self) -> Runner {
        self.runner_with_timeout(Duration::from_secs(10))
    }

    pub(crate) fn runner_with_timeout(&self, timeout: Duration) -> Runner {
        Runner::new(self.program.clone(), timeout)
    }

    pub(crate) fn log(&self) -> String {
        fs::read_to_string(&self.log).unwrap_or_default()
    }

    pub(crate) fn recorded_args(&self) -> Vec<String> {
        let mut args = Vec::new();
        for arg in self.log().split_terminator('\0') {
            args.push(arg.to_owned());
        }
        args
    }
}

pub(crate) fn fixture_text(name: &str) -> String {
    fs::read_to_string(fixture(name)).unwrap()
}

pub(crate) fn fixture(name: &str) -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("fixtures")
        .join(name)
}
