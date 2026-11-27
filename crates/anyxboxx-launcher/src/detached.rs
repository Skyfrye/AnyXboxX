//! Starting the game so that it outlives the launcher: a detached process (no console, its own
//! process group), its output written to a log file in `logs` next to the launcher (else under
//! `%LOCALAPPDATA%\AnyXboxX-diffbase\logs`) instead of pipes the launcher would have to hold. The
//! log starts with the command and its VIctory settings. The launcher then waits briefly and may
//! close.

use std::fs::File;
use std::io::{self, Write};
use std::path::{Path, PathBuf};
use std::process::{Child, Command, Stdio};
use std::time::{Duration, Instant};

/// `DETACHED_PROCESS`: the child gets no console of its own and does not share the launcher's.
pub const DETACHED_PROCESS: u32 = 0x0000_0008;
/// `CREATE_NEW_PROCESS_GROUP`: a console Ctrl+C of the launcher does not reach the game.
pub const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
/// `CREATE_NO_WINDOW`: a console child (victory.exe, cargo) opens no window.
pub const CREATE_NO_WINDOW: u32 = 0x0800_0000;
/// How long the launcher waits for the game to be running before it lets go of it.
pub const UP_WAIT: Duration = Duration::from_secs(10);
const POLL: Duration = Duration::from_millis(200);

/// The creation flags of the detached game: the three constants above, together.
pub fn spawn_flags() -> u32 {
    DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP | CREATE_NO_WINDOW
}

/// Where the game logs go: `logs` next to the launcher when that folder can be written, else
/// `%LOCALAPPDATA%\AnyXboxX-diffbase\logs`; `None` when neither is known.
pub fn log_dir(
    launcher_dir: Option<&Path>,
    local_app_data: Option<&std::ffi::OsStr>,
) -> Option<PathBuf> {
    launcher_dir
        .map(|dir| dir.join("logs"))
        .filter(|logs| writable(logs))
        .or_else(|| {
            local_app_data.filter(|v| !v.is_empty()).map(|local| {
                PathBuf::from(local)
                    .join(crate::store::APP_DIR)
                    .join("logs")
            })
        })
}

/// Whether files can be made in `dir` (made first when missing).
fn writable(dir: &Path) -> bool {
    let probe = dir.join(".write-test");
    let ok = std::fs::create_dir_all(dir).is_ok() && std::fs::write(&probe, b"").is_ok();
    let _ = std::fs::remove_file(&probe);
    ok
}

/// The first lines of a game log: what was started and with which VIctory settings.
pub fn header(command: &Command) -> String {
    let mut text = format!(
        "== {}\n== {}",
        crate::about::line(),
        command.get_program().to_string_lossy()
    );
    for arg in command.get_args() {
        text.push(' ');
        text.push_str(&arg.to_string_lossy());
    }
    text.push('\n');
    for (key, value) in command.get_envs() {
        let key = key.to_string_lossy();
        if key.starts_with("VICTORY_") || key.starts_with("VKD3D_") {
            let value = value.map_or(String::new(), |v| v.to_string_lossy().into_owned());
            text.push_str(&format!("== {key}={value}\n"));
        }
    }
    text.push_str("==\n");
    text
}

/// `game-<unix seconds>.log` in `dir`: one file per launch.
pub fn game_log_path(dir: &Path, unix_seconds: u64) -> PathBuf {
    dir.join(format!("game-{unix_seconds}.log"))
}

/// Starts `command` detached, with stdin closed and both output streams written to `log`
/// (created, or truncated, along with its folder).
pub fn spawn_game(mut command: Command, log: &Path) -> io::Result<Child> {
    if let Some(dir) = log.parent() {
        std::fs::create_dir_all(dir)?;
    }
    let mut file = File::create(log)?;
    file.write_all(header(&command).as_bytes())?;
    let errors = file.try_clone()?;
    command
        .stdin(Stdio::null())
        .stdout(Stdio::from(file))
        .stderr(Stdio::from(errors));
    apply_flags(&mut command);
    command.spawn()
}

#[cfg(windows)]
fn apply_flags(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    command.creation_flags(spawn_flags());
}

#[cfg(not(windows))]
fn apply_flags(_command: &mut Command) {}

/// Waits up to `wait` for the child to be running. `Err` with its exit code when it already
/// ended (the game died at once: the log says why). `Ok` when it is still alive at the end.
pub fn wait_until_up(child: &mut Child, wait: Duration) -> io::Result<Result<(), i32>> {
    let started = Instant::now();
    loop {
        if let Some(status) = child.try_wait()? {
            return Ok(Err(status.code().unwrap_or(1)));
        }
        let elapsed = started.elapsed();
        if elapsed >= wait {
            return Ok(Ok(()));
        }
        std::thread::sleep(POLL.min(wait.saturating_sub(elapsed)));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_spawn_flags_are_the_three_documented_constants() {
        assert_eq!(spawn_flags(), 0x0000_0008 | 0x0000_0200 | 0x0800_0000);
        assert_eq!(DETACHED_PROCESS, 0x8);
        assert_eq!(CREATE_NEW_PROCESS_GROUP, 0x200);
        assert_eq!(CREATE_NO_WINDOW, 0x0800_0000);
    }

    #[test]
    fn logs_live_next_to_the_launcher_else_under_appdata() {
        let launcher = tempfile::tempdir().expect("tempdir");
        let local = std::ffi::OsStr::new(r"C:\Users\me\AppData\Local");
        let near = log_dir(Some(launcher.path()), Some(local)).expect("dir");
        assert_eq!(near, launcher.path().join("logs"));
        assert!(near.is_dir());
        let a_file = launcher.path().join("a-file");
        std::fs::write(&a_file, b"x").expect("write");
        let dir = log_dir(Some(&a_file), Some(local)).expect("dir");
        assert!(dir.ends_with(Path::new("AnyXboxX-diffbase").join("logs")));
        assert!(log_dir(None, None).is_none());
        assert!(log_dir(None, Some(std::ffi::OsStr::new(""))).is_none());
        let log = game_log_path(&dir, 1_795_654_414);
        assert_eq!(
            log.file_name().and_then(|n| n.to_str()),
            Some("game-1795654414.log")
        );
    }

    #[test]
    fn a_child_that_exits_at_once_is_reported_with_its_code() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("logs").join("game-test.log");
        let mut command = Command::new(std::env::current_exe().expect("exe"));
        command.arg("--definitely-not-a-test-name-xyz");
        let mut child = spawn_game(command, &log).expect("spawn");
        let outcome = wait_until_up(&mut child, Duration::from_secs(5)).expect("wait");
        assert!(
            outcome.is_err(),
            "an unknown option ends the test binary at once"
        );
        assert!(
            log.is_file(),
            "the log file is created before the child starts"
        );
    }

    #[test]
    fn the_log_starts_with_the_command_and_its_victory_settings() {
        let mut command = Command::new("victory.exe");
        command
            .args(["run", "game.exe"])
            .env("VICTORY_PANEL", "1")
            .env("PATH", "x");
        let text = header(&command);
        assert!(text.contains("victory.exe run game.exe"));
        assert!(text.contains("== VICTORY_PANEL=1\n"));
        assert!(!text.contains("PATH="));
    }

    #[test]
    fn a_running_child_is_up_after_the_wait() {
        let dir = tempfile::tempdir().expect("tempdir");
        let log = dir.path().join("game.log");
        let mut command = Command::new(std::env::current_exe().expect("exe"));
        command.args(["--ignored", "--exact", "detached::tests::sleeper"]);
        let mut child = spawn_game(command, &log).expect("spawn");
        let outcome = wait_until_up(&mut child, Duration::from_millis(400)).expect("wait");
        let _ = child.kill();
        let _ = child.wait();
        assert_eq!(outcome, Ok(()));
    }

    /// Helper of the test above: a test that waits long enough to still be running. Run only by name.
    #[test]
    #[ignore]
    fn sleeper() {
        std::thread::sleep(Duration::from_secs(30));
    }
}
