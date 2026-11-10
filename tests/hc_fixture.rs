//! The subprocess backend must be private, reusable on restart, and explicitly cleaned.
#![cfg(unix)]

pub mod support;

use serde_json::Value;
use std::{
    fs,
    path::Path,
    process::{Command, Stdio},
    time::{Duration, Instant, SystemTime, UNIX_EPOCH},
};

fn command(runtime: &Path, args: &[&str]) -> Command {
    let mut command = Command::new("/bin/sh");
    command
        .args(["-c", "exec hc \"$@\"", "hc-fixture"])
        .args(args);
    command
        .env("HC_SOCKET", "/must-not-connect.sock")
        .env("HC_TERMINAL_ID", "t999")
        .env("HC_TERMINAL_SOCKET", "/must-not-parent.sock")
        .env("HERDR_HC_SOURCE_PROFILE", "must-not-activate");
    support::hc::configure(&mut command, runtime);
    command
}

fn json(runtime: &Path, args: &[&str]) -> Value {
    let output = command(runtime, args).output().unwrap();
    assert!(
        output.status.success(),
        "hc {args:?}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).expect("hc JSON")
}

#[test]
fn scratch_cores_are_distinct_reusable_and_cleanup_stops_only_the_registered_core() {
    let base = std::env::temp_dir().join(format!(
        "hc-fixture-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ));
    let one = base.join("one/runtime");
    let two = base.join("two/runtime");
    let args = [
        "run",
        "--bg",
        "--json",
        "--",
        "/bin/sh",
        "-c",
        "read -r hold",
    ];
    let first = json(&one, &args);
    let second = json(&two, &args);
    assert_eq!(first["address"], "t1", "{first}");
    assert_eq!(second["address"], "t1", "{second}");
    let running = |runtime: &Path| {
        let mut terminal = Value::Null;
        assert!(
            support::wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
                terminal = json(runtime, &["get", "t1", "--json"]);
                terminal["pid"].as_i64().is_some()
            }),
            "program did not start: {terminal}"
        );
        terminal["pid"].as_i64().unwrap() as libc::pid_t
    };
    let first_pid = running(&one);
    let second_pid = running(&two);
    support::hc::cleanup(&base.join("never-registered")).unwrap();
    assert_ne!(first_pid, second_pid);
    assert_eq!(json(&one, &["get", "t1", "--json"])["pid"], first_pid);
    assert_eq!(json(&two, &["get", "t1", "--json"])["pid"], second_pid);
    support::hc::cleanup(&one).unwrap();
    assert!(
        support::wait_until(
            Duration::from_secs(5),
            Duration::from_millis(20),
            || unsafe { libc::kill(first_pid, 0) } != 0
        ),
        "first scratch program survived cleanup"
    );
    assert_eq!(
        json(&two, &["get", "t1", "--json"])["pid"],
        second_pid,
        "cleanup crossed fixture identities"
    );
    support::hc::cleanup(&two).unwrap();
    assert!(
        support::wait_until(
            Duration::from_secs(5),
            Duration::from_millis(20),
            || unsafe { libc::kill(second_pid, 0) } != 0
        ),
        "second scratch program survived cleanup"
    );
    support::unregister_runtime_dir(&one);
    support::unregister_runtime_dir(&two);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn scratch_environment_keeps_explicit_home_and_path_without_global_changes() {
    let base = std::env::temp_dir().join(format!("hc-fixture-env-{}", std::process::id()));
    let runtime = base.join("runtime");
    let keys = [
        "HOME",
        "PATH",
        "HC_SESSION",
        "HC_SOCKET",
        "HC_TERMINAL_ID",
        "HC_TERMINAL_SOCKET",
        "HERDR_HC_SOURCE_PROFILE",
    ];
    let before: Vec<_> = keys.iter().map(std::env::var_os).collect();
    let mut command = portable_pty::CommandBuilder::new("/bin/sh");
    command.env("HOME", base.join("explicit-home"));
    command.env("PATH", "/test-only-bin:/usr/bin:/bin");
    command.env("HC_SOCKET", "/must-not-connect.sock");
    command.env("HC_TERMINAL_SOCKET", "/must-not-parent.sock");
    command.env("HERDR_HC_SOURCE_PROFILE", "must-not-activate");
    support::hc::configure_pty(&mut command, &runtime);
    assert_eq!(
        command.get_env("HOME"),
        Some(base.join("explicit-home").as_os_str())
    );
    let paths: Vec<_> = std::env::split_paths(command.get_env("PATH").unwrap()).collect();
    assert!(paths[0].join("hc").is_file());
    assert_eq!(paths[1], Path::new("/test-only-bin"));
    assert_eq!(
        command.get_env("HC_SESSION"),
        Some(std::ffi::OsStr::new("fixture"))
    );
    assert!(command.get_env("HC_SOCKET").is_none());
    assert!(command.get_env("HC_TERMINAL_SOCKET").is_none());
    assert!(command.get_env("HERDR_HC_SOURCE_PROFILE").is_none());
    let mut command = Command::new("/bin/sh");
    command
        .env("HOME", base.join("explicit-home"))
        .env("PATH", "/test-only-bin:/usr/bin:/bin");
    support::hc::configure(&mut command, &runtime);
    let env: std::collections::HashMap<_, _> = command.get_envs().collect();
    assert_eq!(
        env[std::ffi::OsStr::new("HOME")],
        Some(base.join("explicit-home").as_os_str())
    );
    assert_eq!(
        std::env::split_paths(env[std::ffi::OsStr::new("PATH")].unwrap())
            .nth(1)
            .unwrap(),
        Path::new("/test-only-bin")
    );
    for key in [
        "HC_SOCKET",
        "HC_TERMINAL_ID",
        "HC_TERMINAL_SOCKET",
        "HERDR_HC_SOURCE_PROFILE",
    ] {
        assert_eq!(env[std::ffi::OsStr::new(key)], None);
    }
    assert_eq!(
        keys.iter().map(std::env::var_os).collect::<Vec<_>>(),
        before
    );
    support::cleanup_test_base(&base);
}

fn scratch(label: &str) -> std::path::PathBuf {
    std::env::temp_dir().join(format!(
        "hc-{label}-{}-{}",
        std::process::id(),
        SystemTime::now()
            .duration_since(UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    ))
}

fn kept(runtime: &Path, marker: &Path) -> Value {
    json(
        runtime,
        &[
            "run",
            "--keep",
            "--bg",
            "--json",
            "--",
            "/bin/sh",
            "-c",
            "printf 'spawn\\n' >> \"$1\"; read -r hold",
            "hc-fixture",
            marker.to_str().unwrap(),
        ],
    );
    let mut terminal = Value::Null;
    assert!(
        support::wait_until(Duration::from_secs(10), Duration::from_millis(20), || {
            terminal = json(runtime, &["get", "t1", "--json"]);
            terminal["pid"].is_number() && marker.exists()
        }),
        "kept program did not start: {terminal}"
    );
    let status = json(runtime, &["server", "status", "--json"]);
    let root = support::hc::private_cli(runtime)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    serde_json::json!({"root":root,"program":terminal["pid"],"core":status["pid"],"holders":status["holders"]})
}

fn assert_program_gone(info: &Value) {
    let pid = info["program"].as_i64().unwrap() as libc::pid_t;
    assert!(
        support::wait_until(
            Duration::from_secs(5),
            Duration::from_millis(20),
            || unsafe { libc::kill(pid, 0) } != 0
        ),
        "private program {pid} survived cleanup: {info}"
    );
}

#[test]
fn cleanup_stops_kept_programs_and_recovering_holders_without_new_launches() {
    let base = scratch("kept");
    let runtime = base.join("runtime");
    let marker = base.join("spawns");
    let info = kept(&runtime, &marker);
    let root = Path::new(info["root"].as_str().unwrap());
    assert!(!info["holders"].as_array().unwrap().is_empty(), "{info}");
    // A successful 'no core running' is not proof that its holder/programs are gone.
    assert_eq!(
        unsafe { libc::kill(info["core"].as_i64().unwrap() as libc::pid_t, libc::SIGKILL) },
        0
    );
    support::hc::cleanup(&runtime).unwrap();
    assert_program_gone(&info);
    assert!(!root.exists());
    assert_eq!(
        fs::read_to_string(&marker).unwrap(),
        "spawn\n",
        "cleanup launched a replacement program"
    );
    support::hc::cleanup(&runtime).unwrap();
    support::unregister_runtime_dir(&runtime);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn cleanup_covers_all_private_sessions_including_max_length_holder_paths() {
    use std::os::unix::fs::PermissionsExt as _;
    let base = scratch("sessions");
    let runtime = base.join("runtime");
    let mut programs = Vec::new();
    for session in ["fixture".to_owned(), "s".repeat(64)] {
        let run = command(
            &runtime,
            &["run", "--keep", "--bg", "--json", "--", "/bin/cat"],
        )
        .env("HC_SESSION", &session)
        .output()
        .unwrap();
        assert!(
            run.status.success(),
            "{session}: {}",
            String::from_utf8_lossy(&run.stderr)
        );
        let mut terminal = Value::Null;
        assert!(support::wait_until(
            Duration::from_secs(10),
            Duration::from_millis(25),
            || {
                let output = command(&runtime, &["get", "t1", "--json"])
                    .env("HC_SESSION", &session)
                    .output()
                    .unwrap();
                terminal = serde_json::from_slice(&output.stdout).unwrap();
                terminal["pid"].is_number()
            }
        ));
        programs.push(serde_json::json!({"program":terminal["pid"]}));
    }
    let root = support::hc::private_cli(&runtime)
        .parent()
        .unwrap()
        .parent()
        .unwrap()
        .to_path_buf();
    assert_eq!(
        fs::metadata(&root).unwrap().permissions().mode() & 0o777,
        0o700
    );
    support::hc::cleanup(&runtime).unwrap();
    for info in programs {
        assert_program_gone(&info);
    }
    assert!(!root.exists());
    support::unregister_runtime_dir(&runtime);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn failed_or_timed_out_stop_retains_ownership_and_evidence_for_retry() {
    let base = scratch("failed-cleanup");
    let runtime = base.join("runtime");
    let info = kept(&runtime, &base.join("spawns"));
    let root = Path::new(info["root"].as_str().unwrap());
    let wrapper = root.join("bin/hc");
    let original = fs::read(&wrapper).unwrap();
    for body in [
        "exit 23\n".to_owned(),
        format!(
            "printf '%s\\n' \"$$\" > '{}'\nexec /bin/sleep 30\n",
            base.join("stop.pid").display()
        ),
        "exit 0\n".to_owned(),
    ] {
        fs::write(&wrapper, format!("#!/bin/sh\n{body}")).unwrap();
        let started = Instant::now();
        let result = support::hc::cleanup(&runtime);
        // Restore the real stopper before any assertion can trigger the panic hook.
        fs::write(&wrapper, &original).unwrap();
        let error = result.expect_err("failed stop discarded the private core");
        assert!(
            started.elapsed() < Duration::from_secs(25),
            "cleanup exceeded its bound"
        );
        assert!(
            error.to_string().contains(root.to_str().unwrap()),
            "{error}"
        );
        assert!(root.exists());
        assert_eq!(
            support::hc::private_cli(&runtime),
            wrapper,
            "cleanup lost its registered root"
        );
        assert_eq!(
            json(&runtime, &["get", "t1", "--json"])["pid"],
            info["program"]
        );
        if body.contains("sleep") {
            assert_eq!(error.kind(), std::io::ErrorKind::TimedOut);
            let pid: libc::pid_t = fs::read_to_string(base.join("stop.pid"))
                .unwrap()
                .trim()
                .parse()
                .unwrap();
            assert_ne!(
                unsafe { libc::kill(pid, 0) },
                0,
                "timed-out stopper survived"
            );
        }
    }
    support::hc::cleanup(&runtime).unwrap();
    assert_program_gone(&info);
    assert!(!root.exists());
    support::unregister_runtime_dir(&runtime);
    fs::remove_dir_all(base).unwrap();
}

#[test]
fn process_exit_panic_and_signal_hooks_clean_only_their_private_cores() {
    const MODE: &str = "HERDR_HC_FIXTURE_EXIT_MODE";
    const BASE: &str = "HERDR_HC_FIXTURE_EXIT_BASE";
    if let Ok(mode) = std::env::var(MODE) {
        let base = std::path::PathBuf::from(std::env::var_os(BASE).unwrap());
        let runtime = base.join("runtime");
        let info = if mode == "cleanup-error" {
            let wrapper = support::hc::private_cli(&runtime);
            let root = wrapper.parent().unwrap().parent().unwrap();
            fs::create_dir_all(root.join("state/herdr-core/fixture")).unwrap();
            fs::write(&wrapper, "#!/bin/sh\nexit 23\n").unwrap();
            let other = kept(&base.join("other/runtime"), &base.join("spawns"));
            serde_json::json!({"root":root,"other":other}) // The broken root itself never started a core.
        } else {
            kept(&runtime, &base.join("spawns"))
        };
        fs::write(base.join("owned.json"), serde_json::to_vec(&info).unwrap()).unwrap();
        match mode.as_str() {
            "panic" => panic!("intentional fixture cleanup probe"),
            "sigint" | "sigterm" => {
                unsafe {
                    libc::raise(if mode == "sigint" {
                        libc::SIGINT
                    } else {
                        libc::SIGTERM
                    })
                };
                std::thread::park();
                panic!("cleanup signal did not exit");
            }
            _ => std::process::exit(0), // Exercise atexit without Rust Drop unwinding.
        }
    }
    let base = scratch("hooks");
    let other = base.join("other/runtime");
    let protected = kept(&other, &base.join("protected-spawns"));
    for mode in ["exit", "panic", "sigint", "sigterm", "cleanup-error"] {
        let child_base = base.join(mode);
        let mut child = Command::new(std::env::current_exe().unwrap())
            .args([
                "--exact",
                "process_exit_panic_and_signal_hooks_clean_only_their_private_cores",
                "--nocapture",
            ])
            .env(MODE, mode)
            .env(BASE, &child_base)
            .stdout(Stdio::piped())
            .stderr(Stdio::piped())
            .spawn()
            .unwrap();
        support::register_spawned_herdr_pid(Some(child.id()));
        assert!(
            support::wait_until(Duration::from_secs(40), Duration::from_millis(25), || child
                .try_wait()
                .unwrap()
                .is_some()),
            "fixture child {mode} did not exit"
        );
        support::unregister_spawned_herdr_pid(Some(child.id()));
        let output = child.wait_with_output().unwrap();
        let info: Value =
            serde_json::from_slice(&fs::read(child_base.join("owned.json")).unwrap()).unwrap();
        let root = Path::new(info["root"].as_str().unwrap());
        assert_eq!(
            output.status.success(),
            mode == "exit",
            "{mode}: {}",
            String::from_utf8_lossy(&output.stderr)
        );
        if mode == "cleanup-error" {
            assert!(root.exists(), "failed atexit discarded evidence");
            assert!(String::from_utf8_lossy(&output.stderr).contains("retained"));
            assert_program_gone(&info["other"]);
            assert!(
                !Path::new(info["other"]["root"].as_str().unwrap()).exists(),
                "one failed core prevented other registered cleanup"
            );
            fs::remove_dir_all(root).unwrap(); // Only this broken root never started a core/holder.
        } else {
            assert_program_gone(&info);
            assert!(!root.exists(), "{mode} did not remove its private core");
            assert_eq!(
                fs::read_to_string(child_base.join("spawns")).unwrap(),
                "spawn\n"
            );
        }
        assert_eq!(
            json(&other, &["get", "t1", "--json"])["pid"],
            protected["program"],
            "{mode} crossed fixture ownership"
        );
    }
    support::hc::cleanup(&other).unwrap();
    assert_program_gone(&protected);
    support::unregister_runtime_dir(&other);
    fs::remove_dir_all(base).unwrap();
}
