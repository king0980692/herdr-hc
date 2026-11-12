use super::harness::*;

#[test]
fn agent_explain_missing_file_reports_json_error() {
    let base = unique_test_dir();
    let missing = base.join("missing-screen.txt");
    let output = run_named_cli(
        &base.join("config"),
        &base.join("runtime"),
        &[
            "agent",
            "explain",
            "--file",
            missing.to_str().unwrap(),
            "--agent",
            "claude",
            "--json",
        ],
    );

    assert_eq!(output.status.code(), Some(1));
    assert!(output.stdout.is_empty());
    let error: serde_json::Value = serde_json::from_slice(&output.stderr).unwrap();
    assert_eq!(error["id"], "cli:agent:explain");
    assert_eq!(error["error"]["code"], "agent_explain_file_read_failed");
    assert!(error["error"]["message"]
        .as_str()
        .unwrap()
        .contains(missing.to_str().unwrap()));
}

fn write_delayed_shell_and_fake_pi(
    base: &Path,
    shell_delay_seconds: &str,
) -> (PathBuf, PathBuf, PathBuf) {
    use std::os::unix::fs::PermissionsExt;

    let bin = base.join("bin");
    let delayed_shell = bin.join("delayed-shell");
    let fake_pi = bin.join("pi");
    let invocations = base.join("pi-invocations");
    fs::create_dir_all(&bin).unwrap();
    fs::write(
        &delayed_shell,
        format!("#!/bin/sh\n/bin/sleep {shell_delay_seconds}\nexec /bin/sh\n"),
    )
    .unwrap();
    fs::write(
        &fake_pi,
        format!(
            "#!/bin/sh\nprintf '%s\\n' \"$@\" >> '{}'\nprintf '\\033]7501;state=idle:app=pi\\007'\nwhile IFS= read -r _prompt; do :; done\n",
            invocations.display(),
        ),
    )
    .unwrap();
    fs::set_permissions(&delayed_shell, fs::Permissions::from_mode(0o755)).unwrap();
    fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o755)).unwrap();
    (bin, delayed_shell, invocations)
}

#[test]
fn agent_start_waits_for_a_new_pane_shell_to_finish_initializing() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let (bin, delayed_shell, invocations) = write_delayed_shell_and_fake_pi(&base, "0.4");
    let config = format!(
        "onboarding = false\n[terminal]\ndefault_shell = {:?}\nshell_mode = \"non_login\"\n",
        delayed_shell.to_str().unwrap()
    );
    let herdr = spawn_herdr_with_config(
        &config_home,
        &runtime_dir,
        &socket_path,
        Some(&bin),
        &config,
    );
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let seed = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let seed_workspace = seed["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap();
    let created = run_cli_json(
        &socket_path,
        &[
            "workspace",
            "create",
            "--cwd",
            base.to_str().unwrap(),
            "--no-focus",
        ],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"].as_str().unwrap();
    let terminal_id = created["result"]["root_pane"]["terminal_id"]
        .as_str()
        .unwrap();
    assert!(!created["result"]["root_pane"]["focused"].as_bool().unwrap());

    let started = run_cli_json(
        &socket_path,
        &[
            "agent",
            "start",
            "worker",
            "--kind",
            "pi",
            "--pane",
            pane_id,
            "--timeout",
            "8000",
            "--",
            "--no-context-files",
            "--no-skills",
            "--no-extensions",
        ],
    );

    assert_eq!(started["result"]["agent"]["terminal_id"], terminal_id);
    assert_eq!(started["result"]["agent"]["pane_id"], pane_id);
    assert_eq!(started["result"]["agent"]["interactive_ready"], true);
    assert_eq!(
        fs::read_to_string(&invocations).unwrap(),
        "--no-context-files\n--no-skills\n--no-extensions\n"
    );
    assert_eq!(
        run_cli_json(&socket_path, &["workspace", "list"])["result"]["workspaces"]
            .as_array()
            .unwrap()
            .iter()
            .find(|workspace| workspace["workspace_id"] == seed_workspace)
            .unwrap()["focused"],
        true
    );

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn agent_start_stops_retrying_when_the_pane_shell_stays_busy() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let (bin, delayed_shell, invocations) = write_delayed_shell_and_fake_pi(&base, "2.3");
    let config = format!(
        "onboarding = false\n[terminal]\ndefault_shell = {:?}\nshell_mode = \"non_login\"\n",
        delayed_shell.to_str().unwrap()
    );
    let herdr = spawn_herdr_with_config(
        &config_home,
        &runtime_dir,
        &socket_path,
        Some(&bin),
        &config,
    );
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"].as_str().unwrap();

    let started_at = Instant::now();
    let unavailable = run_cli(
        &socket_path,
        &[
            "agent",
            "start",
            "worker",
            "--kind",
            "pi",
            "--pane",
            pane_id,
            "--timeout",
            "8000",
        ],
    );
    assert_eq!(unavailable.status.code(), Some(1));
    let error: serde_json::Value = serde_json::from_slice(&unavailable.stderr).unwrap();
    assert_eq!(error["error"]["code"], "agent_pane_busy");
    assert!(started_at.elapsed() >= Duration::from_secs(2));
    assert!(started_at.elapsed() < Duration::from_secs(4));
    assert!(!invocations.exists());

    let retried = run_cli_json(
        &socket_path,
        &[
            "agent",
            "start",
            "worker",
            "--kind",
            "pi",
            "--pane",
            pane_id,
            "--timeout",
            "8000",
        ],
    );
    assert_eq!(retried["result"]["type"], "agent_started");
    assert_eq!(fs::read_to_string(&invocations).unwrap(), "\n");

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn agent_start_command_works() {
    use std::os::unix::fs::PermissionsExt;

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let bin = base.join("bin");
    let captured_args = base.join("pi-args");
    let captured_prompts = base.join("pi-prompts");
    fs::create_dir_all(&bin).unwrap();
    let fake_pi = bin.join("pi");
    // The fake agent reports its own state on its terminal, as a real one does.
    // States the test dictates arrive through a control file it polls. The pane's
    // PATH holds only this directory, so external tools are named absolutely.
    let status_control = base.join("pi-status");
    fs::write(
        &fake_pi,
        r#"#!/bin/sh
printf '%s\n' "$@" > 'ARGS'
report() { printf '\033]7501;state=%s:app=pi\007' "$1"; }
(
  while :; do
    if [ -f 'CONTROL' ]; then
      IFS= read -r state < 'CONTROL'
      /bin/rm -f 'CONTROL'
      report "$state"
    fi
    /bin/sleep 0.02
  done
) &
report idle
while IFS= read -r prompt; do
  case "$prompt" in
    "do not transition") continue ;;
    "done churn")
      report done
      report idle
      continue
      ;;
    "block after submit")
      report blocked
      continue
      ;;
  esac
  report working
  # hc publishes a back-to-back working/idle pair, but Herdr's projection keeps
  # only the latest state, so `agent prompt --wait` misses an instantaneous
  # working phase. Hold it briefly; whether Herdr should observe it is open.
  /bin/sleep 0.2
  report idle
  printf '%s\n' "$prompt" >> 'PROMPTS'
done
"#
        .replace("ARGS", captured_args.to_str().unwrap())
        .replace("CONTROL", status_control.to_str().unwrap())
        .replace("PROMPTS", captured_prompts.to_str().unwrap()),
    )
    .unwrap();
    fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o755)).unwrap();

    let herdr = spawn_herdr_with_path(&config_home, &runtime_dir, &socket_path, Some(&bin));
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    run_cli_json(&socket_path, &["pane", "rename", &pane_id, "shell-pane"]);
    let before = run_cli_json(&socket_path, &["pane", "list"]);
    let before_topology = pane_topology_snapshot(&before);

    let missing = run_cli(
        &socket_path,
        &[
            "agent", "start", "missing", "--kind", "pi", "--pane", "w999:p1",
        ],
    );
    assert_eq!(missing.status.code(), Some(1));
    let missing: serde_json::Value = serde_json::from_slice(&missing.stderr).unwrap();
    assert_eq!(missing["error"]["code"], "agent_pane_not_found");
    assert_eq!(
        pane_topology_snapshot(&run_cli_json(&socket_path, &["pane", "list"])),
        before_topology
    );

    for unsafe_arg in ["tab\tcompletion", "escape\x1b[201~"] {
        let rejected = run_cli(
            &socket_path,
            &[
                "agent",
                "start",
                "invalid-argument",
                "--kind",
                "pi",
                "--pane",
                &pane_id,
                "--",
                unsafe_arg,
            ],
        );
        assert_eq!(rejected.status.code(), Some(1));
        let error: serde_json::Value = serde_json::from_slice(&rejected.stderr).unwrap();
        assert_eq!(error["error"]["code"], "invalid_agent_argument");
    }

    // An invalid timeout is never retried, so the shell must already be available.
    wait_for_pane_shell(&socket_path, &pane_id);
    for invalid_timeout in ["3000", "300001", "18446744073709551615"] {
        let rejected = run_cli(
            &socket_path,
            &[
                "agent",
                "start",
                "invalid-timeout",
                "--kind",
                "pi",
                "--pane",
                &pane_id,
                "--timeout",
                invalid_timeout,
            ],
        );
        assert_eq!(rejected.status.code(), Some(1));
        let error: serde_json::Value = serde_json::from_slice(&rejected.stderr).unwrap();
        assert_eq!(error["error"]["code"], "invalid_agent_timeout");
    }

    let started = run_cli_json(
        &socket_path,
        &[
            "agent",
            "start",
            "main",
            "--kind",
            "pi",
            "--pane",
            &pane_id,
            "--timeout",
            "8000",
            "--",
            "--name",
            "scratch",
            "--no-session",
        ],
    );
    assert_eq!(started["result"]["type"], "agent_started");
    assert_eq!(started["result"]["agent"]["name"], "main");
    assert_eq!(started["result"]["agent"]["agent"], "pi");
    assert_eq!(started["result"]["agent"]["pane_id"], pane_id);
    assert_eq!(
        run_cli_json(&socket_path, &["pane", "get", &pane_id])["result"]["pane"]["label"],
        "shell-pane"
    );
    assert_eq!(started["result"]["argv"][0], "pi");
    assert_eq!(started["result"]["argv"][1], "--name");
    assert_eq!(started["result"]["argv"][2], "scratch");
    assert_eq!(started["result"]["argv"][3], "--no-session");
    assert_eq!(
        fs::read_to_string(&captured_args).unwrap(),
        "--name\nscratch\n--no-session\n"
    );

    let literal_flag_prompt = run_cli(&socket_path, &["agent", "prompt", "main", "--wait"]);
    assert!(
        literal_flag_prompt.status.success(),
        "flag-shaped prompt was not treated literally: {}",
        String::from_utf8_lossy(&literal_flag_prompt.stderr)
    );
    let literal_flag_prompt: serde_json::Value =
        serde_json::from_slice(&literal_flag_prompt.stdout).unwrap();
    assert_eq!(literal_flag_prompt["result"]["type"], "agent_prompted");
    assert!(wait_until(
        Duration::from_secs(2),
        Duration::from_millis(25),
        || captured_prompts.exists()
    ));

    let after = run_cli_json(&socket_path, &["pane", "list"]);
    assert_eq!(pane_topology_snapshot(&after), before_topology);

    let prompts_before_blocked = fs::read(&captured_prompts).unwrap();
    let report_agent = |state: &str| {
        let pending = status_control.with_extension("next");
        fs::write(&pending, format!("{state}\n")).unwrap();
        fs::rename(&pending, &status_control).unwrap();
        wait_until(Duration::from_secs(5), Duration::from_millis(25), || {
            run_cli_json(&socket_path, &["agent", "get", "main"])["result"]["agent"]["agent_status"]
                == state
        })
    };
    assert!(report_agent("blocked"));
    assert_eq!(
        run_cli_json(&socket_path, &["agent", "get", "main"])["result"]["agent"]["agent_status"],
        "blocked"
    );

    let blocked_prompt = run_cli(
        &socket_path,
        &[
            "agent",
            "prompt",
            "main",
            "must not be submitted",
            "--wait",
            "--timeout",
            "2000",
        ],
    );
    assert_eq!(blocked_prompt.status.code(), Some(1));
    let blocked_prompt: serde_json::Value = serde_json::from_slice(&blocked_prompt.stderr).unwrap();
    assert_eq!(blocked_prompt["error"]["code"], "agent_blocked");
    thread::sleep(Duration::from_millis(400));
    assert_eq!(fs::read(&captured_prompts).unwrap(), prompts_before_blocked);

    let prompt_wait = |prompt, timeout| {
        run_cli(
            &socket_path,
            &[
                "agent",
                "prompt",
                "main",
                prompt,
                "--wait",
                "--timeout",
                timeout,
            ],
        )
    };

    assert!(report_agent("idle"));
    let stale_idle = prompt_wait("do not transition", "500");
    assert_eq!(stale_idle.status.code(), Some(1));
    let stale_idle: serde_json::Value = serde_json::from_slice(&stale_idle.stderr).unwrap();
    assert_eq!(stale_idle["error"]["code"], "timeout");

    let stalled = prompt_wait("do not transition", "6000");
    assert_eq!(stalled.status.code(), Some(1));
    let stalled: serde_json::Value = serde_json::from_slice(&stalled.stderr).unwrap();
    assert_eq!(stalled["error"]["code"], "agent_prompt_stalled");
    assert!(stalled["error"]["message"]
        .as_str()
        .is_some_and(|message| message.contains("no observed working or blocked state")));

    let settled_only = prompt_wait("done churn", "500");
    assert_eq!(settled_only.status.code(), Some(1));
    let settled_only: serde_json::Value = serde_json::from_slice(&settled_only.stderr).unwrap();
    assert_eq!(settled_only["error"]["code"], "timeout");

    let blocked_after_submit = prompt_wait("block after submit", "2000");
    assert!(blocked_after_submit.status.success());
    let blocked_after_submit: serde_json::Value =
        serde_json::from_slice(&blocked_after_submit.stdout).unwrap();
    assert_eq!(
        blocked_after_submit["result"]["agent"]["agent_status"],
        "blocked"
    );
    assert!(report_agent("idle"));
    assert!(report_agent("working"));
    let already_working = prompt_wait("finish active", "2000");
    assert!(already_working.status.success());

    let prompted = prompt_wait("Review this diff", "2000");
    assert!(
        prompted.status.success(),
        "prompt failed: {}",
        String::from_utf8_lossy(&prompted.stderr)
    );
    let prompted: serde_json::Value = serde_json::from_slice(&prompted.stdout).unwrap();
    assert_eq!(prompted["result"]["type"], "agent_prompted");

    let duplicate = run_cli(
        &socket_path,
        &["agent", "start", "main", "--kind", "pi", "--pane", &pane_id],
    );
    assert!(!duplicate.status.success());
    let duplicate_json: serde_json::Value = serde_json::from_slice(&duplicate.stderr).unwrap();
    assert_eq!(duplicate_json["error"]["code"], "agent_name_taken");

    let busy = run_cli(
        &socket_path,
        &[
            "agent", "start", "second", "--kind", "pi", "--pane", &pane_id,
        ],
    );
    assert!(!busy.status.success());
    let busy_json: serde_json::Value = serde_json::from_slice(&busy.stderr).unwrap();
    assert_eq!(busy_json["error"]["code"], "agent_pane_busy");

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn agent_start_rejects_a_shell_replaced_by_a_foreground_program() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let topology = pane_topology_snapshot(&run_cli_json(&socket_path, &["pane", "list"]));
    assert!(
        run_cli(&socket_path, &["pane", "run", &pane_id, "exec sleep 5"])
            .status
            .success()
    );
    wait_for_pane_foreground(&socket_path, &pane_id, "sleep");

    let started = run_cli(
        &socket_path,
        &[
            "agent",
            "start",
            "worker",
            "--kind",
            "pi",
            "--pane",
            &pane_id,
            "--timeout",
            "4000",
        ],
    );
    assert_eq!(started.status.code(), Some(1));
    let error: serde_json::Value = serde_json::from_slice(&started.stderr).unwrap();
    assert_eq!(error["error"]["code"], "agent_pane_busy");
    assert_eq!(
        pane_topology_snapshot(&run_cli_json(&socket_path, &["pane", "list"])),
        topology
    );

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn agent_start_timeout_releases_the_name_for_reuse() {
    use std::os::unix::fs::PermissionsExt;

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake_pi = bin.join("pi");
    fs::write(
        &fake_pi,
        "#!/bin/sh\nunset HERDR_AGENT\nexec /bin/sleep 20\n",
    )
    .unwrap();
    fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o755)).unwrap();

    let herdr = spawn_herdr_with_path(&config_home, &runtime_dir, &socket_path, Some(&bin));
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let split = run_cli_json(
        &socket_path,
        &["pane", "split", &pane_id, "--direction", "right"],
    );
    let reuse_pane_id = split["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    crate::support::hc::report_agent(&socket_path, &reuse_pane_id, "pi", "idle");

    let started = run_cli(
        &socket_path,
        &[
            "agent",
            "start",
            "worker",
            "--kind",
            "pi",
            "--pane",
            &pane_id,
            "--timeout",
            "3100",
        ],
    );
    assert_eq!(started.status.code(), Some(1));
    let error: serde_json::Value = serde_json::from_slice(&started.stderr).unwrap();
    assert_eq!(error["error"]["code"], "timeout");

    let reused = run_cli(&socket_path, &["agent", "rename", &reuse_pane_id, "worker"]);
    assert!(
        reused.status.success(),
        "name was not released: {}",
        String::from_utf8_lossy(&reused.stderr)
    );

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn agent_start_reports_detected_kind_mismatch_before_released_name() {
    use std::os::unix::fs::PermissionsExt;

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake_pi = bin.join("pi");
    fs::write(
        &fake_pi,
        "#!/bin/sh\nprintf '\\033]7501;state=idle:app=codex\\007'\nexec /bin/sleep 10\n",
    )
    .unwrap();
    fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o755)).unwrap();

    let herdr = spawn_herdr_with_path(&config_home, &runtime_dir, &socket_path, Some(&bin));
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let split = run_cli_json(
        &socket_path,
        &["pane", "split", &pane_id, "--direction", "right"],
    );
    let reuse_pane_id = split["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();

    let started = run_cli(
        &socket_path,
        &[
            "agent",
            "start",
            "worker",
            "--kind",
            "pi",
            "--pane",
            &pane_id,
            "--timeout",
            "5000",
        ],
    );
    assert_eq!(started.status.code(), Some(1));
    let error: serde_json::Value = serde_json::from_slice(&started.stderr).unwrap();
    assert_eq!(error["error"]["code"], "agent_kind_mismatch");

    crate::support::hc::report_agent(&socket_path, &reuse_pane_id, "pi", "idle");
    let reused = run_cli(&socket_path, &["agent", "rename", &reuse_pane_id, "worker"]);
    assert!(reused.status.success());

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn agent_start_follows_its_named_terminal_when_the_pane_moves() {
    use std::os::unix::fs::PermissionsExt;

    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let bin = base.join("bin");
    fs::create_dir_all(&bin).unwrap();
    let fake_pi = bin.join("pi");
    fs::write(
        &fake_pi,
        "#!/bin/sh\nprintf '\\033]7501;state=idle:app=pi\\007'\nexec /bin/sleep 10\n",
    )
    .unwrap();
    fs::set_permissions(&fake_pi, fs::Permissions::from_mode(0o755)).unwrap();

    let herdr = spawn_herdr_with_path(&config_home, &runtime_dir, &socket_path, Some(&bin));
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let first = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let start_socket = socket_path.clone();
    let start_pane = first.clone();
    let starter = thread::spawn(move || {
        run_cli(
            &start_socket,
            &[
                "agent",
                "start",
                "worker",
                "--kind",
                "pi",
                "--pane",
                &start_pane,
                "--timeout",
                "8000",
            ],
        )
    });
    assert!(wait_until(
        Duration::from_secs(2),
        Duration::from_millis(25),
        || run_cli(&socket_path, &["agent", "get", "worker"])
            .status
            .success()
    ));

    let moved = run_cli(
        &socket_path,
        &[
            "pane",
            "move",
            &first,
            "--new-workspace",
            "--label",
            "moved",
            "--no-focus",
        ],
    );
    assert!(
        moved.status.success(),
        "move failed: {}",
        String::from_utf8_lossy(&moved.stderr)
    );

    let started = starter.join().unwrap();
    assert!(
        started.status.success(),
        "start failed after swap: {}",
        String::from_utf8_lossy(&started.stderr)
    );
    let started: serde_json::Value = serde_json::from_slice(&started.stdout).unwrap();
    assert_ne!(started["result"]["agent"]["pane_id"], first);

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn agent_start_and_rename_reject_invalid_names() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let expected_message = "agent name must start with a lowercase letter and contain only lowercase letters, digits, '-' or '_' (1-32 characters)";

    let started = run_cli(
        &socket_path,
        &[
            "agent",
            "start",
            "reviewer one",
            "--kind",
            "pi",
            "--pane",
            &pane_id,
        ],
    );
    assert_eq!(started.status.code(), Some(1));
    let error: serde_json::Value = serde_json::from_slice(&started.stderr).unwrap();
    assert_eq!(error["error"]["code"], "invalid_agent_name");
    assert_eq!(error["error"]["message"], expected_message);

    crate::support::hc::report_agent(&socket_path, &pane_id, "pi", "idle");
    let renamed = run_cli(&socket_path, &["agent", "rename", &pane_id, "reviewer one"]);
    assert_eq!(renamed.status.code(), Some(1));
    let error: serde_json::Value = serde_json::from_slice(&renamed.stderr).unwrap();
    assert_eq!(error["error"]["code"], "invalid_agent_name");
    assert_eq!(error["error"]["message"], expected_message);

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn agent_commands_work() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");

    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    assert!(created.status.success());
    let created_json: serde_json::Value = serde_json::from_slice(&created.stdout).unwrap();
    let root_pane_id = created_json["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let terminal_id = created_json["result"]["root_pane"]["terminal_id"]
        .as_str()
        .unwrap()
        .to_string();

    crate::support::hc::report_agent(&socket_path, &root_pane_id, "pi", "idle");
    let renamed = run_cli(&socket_path, &["agent", "rename", &root_pane_id, "worker"]);
    assert!(renamed.status.success());

    let listed = run_cli_json(&socket_path, &["agent", "list"]);
    assert_eq!(listed["result"]["type"], "agent_list");
    assert_eq!(listed["result"]["agents"][0]["terminal_id"], terminal_id);
    assert_eq!(listed["result"]["agents"][0]["name"], "worker");

    let fetched = run_cli_json(&socket_path, &["agent", "get", "worker"]);
    assert_eq!(fetched["result"]["agent"]["pane_id"], root_pane_id);
    let waited = run_cli_json(
        &socket_path,
        &["agent", "wait", "worker", "--timeout", "100"],
    );
    assert_eq!(waited["result"]["agent"]["pane_id"], root_pane_id);

    // The source's own idle report is authoritative, so the agent accepts input.
    let prompted = run_cli(&socket_path, &["agent", "prompt", "worker", ":"]);
    assert!(
        prompted.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&prompted.stderr)
    );

    crate::support::hc::report_agent(&socket_path, &root_pane_id, "pi", "working");
    let blocked_socket = socket_path.clone();
    let blocked_pane = root_pane_id.clone();
    let blocked_transition = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        crate::support::hc::report_agent(&blocked_socket, &blocked_pane, "pi", "blocked");
    });
    let waited = run_cli_json(
        &socket_path,
        &["agent", "wait", "worker", "--timeout", "2000"],
    );
    blocked_transition.join().unwrap();
    assert_eq!(waited["result"]["agent"]["agent_status"], "blocked");
    let immediate_blocked =
        run_cli_json(&socket_path, &["agent", "wait", "worker", "--timeout", "1"]);
    assert_eq!(
        immediate_blocked["result"]["agent"]["agent_status"],
        "blocked"
    );

    crate::support::hc::report_agent(&socket_path, &root_pane_id, "pi", "working");
    let idle_socket = socket_path.clone();
    let idle_pane = root_pane_id.clone();
    let idle_transition = thread::spawn(move || {
        thread::sleep(Duration::from_millis(100));
        crate::support::hc::report_agent(&idle_socket, &idle_pane, "pi", "idle");
    });
    let idle_wait = run_cli_json(
        &socket_path,
        &["agent", "wait", "worker", "--timeout", "2000"],
    );
    idle_transition.join().unwrap();
    assert!(matches!(
        idle_wait["result"]["agent"]["agent_status"].as_str(),
        Some("idle" | "done")
    ));

    crate::support::hc::report_agent(&socket_path, &root_pane_id, "pi", "working");
    let timed_out = run_cli(
        &socket_path,
        &["agent", "wait", "worker", "--timeout", "100"],
    );
    assert_eq!(timed_out.status.code(), Some(1));
    let timeout: serde_json::Value = serde_json::from_slice(&timed_out.stderr).unwrap();
    assert_eq!(timeout["error"]["code"], "timeout");

    let pane_read = run_cli(
        &socket_path,
        &["pane", "read", &root_pane_id, "--source", "visible"],
    );
    let agent_read = run_cli(
        &socket_path,
        &["agent", "read", &root_pane_id, "--source", "visible"],
    );
    assert!(pane_read.status.success());
    assert!(agent_read.status.success());
    assert_eq!(agent_read.stdout, pane_read.stdout);

    let missing_read = run_cli(&socket_path, &["agent", "read", "missing"]);
    assert_eq!(missing_read.status.code(), Some(1));
    let missing_read: serde_json::Value = serde_json::from_slice(&missing_read.stderr).unwrap();
    assert_eq!(missing_read["error"]["code"], "agent_not_found");

    // The source still reports a running agent, so it accepts keys.
    let sent = run_cli(&socket_path, &["agent", "send-keys", "worker", "enter"]);
    assert!(
        sent.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&sent.stderr)
    );

    let agent_renamed = run_cli_json(&socket_path, &["agent", "rename", "worker", "reviewer"]);
    assert_eq!(agent_renamed["result"]["agent"]["name"], "reviewer");

    let focused = run_cli_json(&socket_path, &["agent", "focus", "reviewer"]);
    assert_eq!(focused["result"]["agent"]["focused"], true);

    // Withdrawing the source's status record ends the agent; nothing is left to wait on.
    crate::support::hc::clear_agent(&socket_path, &root_pane_id);
    let released = run_cli(
        &socket_path,
        &[
            "agent",
            "wait",
            "reviewer",
            "--until",
            "unknown",
            "--timeout",
            "1000",
        ],
    );
    assert_eq!(released.status.code(), Some(1));
    let released: serde_json::Value = serde_json::from_slice(&released.stderr).unwrap();
    assert_eq!(released["error"]["code"], "agent_not_found");

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn agent_wait_returns_immediately_for_unseen_done_agent() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let first = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let workspace_id = created["result"]["workspace"]["workspace_id"]
        .as_str()
        .unwrap();
    let second_tab = run_cli_json(
        &socket_path,
        &["tab", "create", "--workspace", workspace_id],
    );
    let second_tab_id = second_tab["result"]["tab"]["tab_id"].as_str().unwrap();
    assert_ne!(second_tab_id, "w1:t1");
    assert!(run_cli(&socket_path, &["tab", "focus", second_tab_id])
        .status
        .success());
    crate::support::hc::report_agent(&socket_path, &first, "pi", "working");
    assert!(
        run_cli(&socket_path, &["agent", "rename", &first, "worker"])
            .status
            .success()
    );
    // The source owns completion: Herdr no longer derives done from an unseen idle.
    crate::support::hc::report_agent(&socket_path, &first, "pi", "done");

    let waited = run_cli_json(&socket_path, &["agent", "wait", "worker", "--timeout", "1"]);
    assert_eq!(waited["result"]["agent"]["agent_status"], "done");

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn agent_wait_tolerates_detection_uncertainty_and_pane_target_rename() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));
    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let pane_id = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    crate::support::hc::report_agent(&socket_path, &pane_id, "pi", "working");
    assert!(
        run_cli(&socket_path, &["agent", "rename", &pane_id, "worker"])
            .status
            .success()
    );

    let wait_socket = socket_path.clone();
    let waiter = thread::spawn(move || {
        run_cli(
            &wait_socket,
            &[
                "agent",
                "wait",
                "worker",
                "--until",
                "unknown",
                "--timeout",
                "2000",
            ],
        )
    });
    thread::sleep(Duration::from_millis(150));
    let cleared = send_request(
        &socket_path,
        &format!(
            r#"{{"id":"agent_wait_uncertain","method":"pane.clear_agent_authority","params":{{"pane_id":"{}","source":"custom:uncertain"}}}}"#,
            pane_id
        ),
    );
    assert_eq!(cleared["error"]["code"], "unsupported_runtime");
    crate::support::hc::clear_agent(&socket_path, &pane_id);
    // hc has no "uncertain" state to wait through: withdrawing the source's
    // status record ends the agent, and a pending wait reports that.
    let waited = waiter.join().unwrap();
    assert_eq!(waited.status.code(), Some(1));
    let waited: serde_json::Value = serde_json::from_slice(&waited.stderr).unwrap();
    assert_eq!(waited["error"]["code"], "agent_not_running");

    crate::support::hc::report_agent(&socket_path, &pane_id, "pi", "working");
    let wait_socket = socket_path.clone();
    let wait_pane = pane_id.clone();
    let waiter = thread::spawn(move || {
        run_cli(
            &wait_socket,
            &[
                "agent",
                "wait",
                &wait_pane,
                "--until",
                "idle",
                "--timeout",
                "2000",
            ],
        )
    });
    thread::sleep(Duration::from_millis(150));
    assert!(
        run_cli(&socket_path, &["agent", "rename", "worker", "reviewer"])
            .status
            .success()
    );
    crate::support::hc::report_agent(&socket_path, &pane_id, "pi", "idle");
    let waited = waiter.join().unwrap();
    assert!(
        waited.status.success(),
        "stderr: {}",
        String::from_utf8_lossy(&waited.stderr)
    );
    let waited: serde_json::Value = serde_json::from_slice(&waited.stdout).unwrap();
    assert_eq!(waited["result"]["agent"]["agent_status"], "idle");
    assert_eq!(waited["result"]["agent"]["name"], "reviewer");

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn agent_wait_pins_the_original_terminal_when_name_is_reused() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let first = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let split = run_cli_json(
        &socket_path,
        &["pane", "split", &first, "--direction", "right"],
    );
    let second = split["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    crate::support::hc::report_agent(&socket_path, &first, "pi", "working");
    assert!(
        run_cli(&socket_path, &["agent", "rename", &first, "worker"])
            .status
            .success()
    );

    let wait_socket = socket_path.clone();
    let waiter = thread::spawn(move || {
        run_cli(
            &wait_socket,
            &["agent", "wait", "worker", "--timeout", "2000"],
        )
    });
    thread::sleep(Duration::from_millis(250));
    assert!(
        run_cli(&socket_path, &["agent", "rename", "worker", "--clear"])
            .status
            .success()
    );
    crate::support::hc::report_agent(&socket_path, &second, "pi", "idle");
    assert!(
        run_cli(&socket_path, &["agent", "rename", &second, "worker"])
            .status
            .success()
    );

    let waited = waiter.join().unwrap();
    assert_eq!(waited.status.code(), Some(1));
    let error: serde_json::Value = serde_json::from_slice(&waited.stderr).unwrap();
    assert_eq!(error["error"]["code"], "agent_not_running");

    cleanup_spawned_herdr(herdr, base);
}

#[test]
fn agent_wait_ignores_other_panes_and_errors_when_its_pane_closes() {
    let base = unique_test_dir();
    let config_home = base.join("config");
    let runtime_dir = base.join("runtime");
    let socket_path = runtime_dir.join("herdr.sock");
    let herdr = spawn_herdr(&config_home, &runtime_dir, &socket_path);
    wait_for_socket(&socket_path, Duration::from_secs(5));

    let created = run_cli_json(
        &socket_path,
        &["workspace", "create", "--cwd", base.to_str().unwrap()],
    );
    let first = created["result"]["root_pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    let split = run_cli_json(
        &socket_path,
        &["pane", "split", &first, "--direction", "right"],
    );
    let second = split["result"]["pane"]["pane_id"]
        .as_str()
        .unwrap()
        .to_string();
    crate::support::hc::report_agent(&socket_path, &first, "pi", "working");
    assert!(
        run_cli(&socket_path, &["agent", "rename", &first, "worker"])
            .status
            .success()
    );

    let wait_socket = socket_path.clone();
    let (done_tx, done_rx) = std::sync::mpsc::channel();
    thread::spawn(move || {
        let _ = done_tx.send(run_cli(
            &wait_socket,
            &["agent", "wait", "worker", "--timeout", "3000"],
        ));
    });
    thread::sleep(Duration::from_millis(150));
    crate::support::hc::report_agent(&socket_path, &second, "pi", "idle");
    thread::sleep(Duration::from_millis(150));
    assert!(matches!(
        done_rx.try_recv(),
        Err(std::sync::mpsc::TryRecvError::Empty)
    ));

    assert!(run_cli(&socket_path, &["pane", "close", &first])
        .status
        .success());
    let waited = done_rx.recv_timeout(Duration::from_secs(2)).unwrap();
    assert_eq!(waited.status.code(), Some(1));
    let error: serde_json::Value = serde_json::from_slice(&waited.stderr).unwrap();
    assert_eq!(error["error"]["code"], "agent_not_running");

    cleanup_spawned_herdr(herdr, base);
}
