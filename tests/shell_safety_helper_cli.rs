use std::io::Write;
use std::process::{Command, Stdio};

const STARTUP_ENV_MARKER: &str = "CODING_BRAIN_SHELL_SAFETY_STARTUP_ENV_UNCERTAIN";
const LASTPIPE_MARKER: &str = "CODING_BRAIN_SHELL_SAFETY_LASTPIPE_ENABLED";
const POSIX_MODE_ENABLED_MARKER: &str = "CODING_BRAIN_SHELL_SAFETY_POSIX_MODE_ENABLED";
const POSIX_MODE_UNCERTAIN_MARKER: &str = "CODING_BRAIN_SHELL_SAFETY_POSIX_MODE_UNCERTAIN";
const POSIX_MODE_PROPAGATES_MARKER: &str = "CODING_BRAIN_SHELL_SAFETY_POSIX_MODE_PROPAGATES";

fn run_shipped_helper_with_environment(
    command: &str,
    name: &str,
    value: &str,
) -> serde_json::Value {
    run_shipped_helper(command, Some((name, value)))
}

fn run_shipped_helper(command: &str, environment: Option<(&str, &str)>) -> serde_json::Value {
    let mut helper = Command::new(env!("CARGO_BIN_EXE_cbrain"));
    helper
        .arg("--shell-safety-helper")
        .env_clear()
        .env("HOME", "/tmp/cbrain-shell-safety-home")
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    if let Some((name, value)) = environment {
        helper.env(name, value);
    }
    let mut child = helper.spawn().unwrap();
    child
        .stdin
        .take()
        .unwrap()
        .write_all(command.as_bytes())
        .unwrap();
    let output = child.wait_with_output().unwrap();

    assert!(
        output.status.success(),
        "{command}: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    serde_json::from_slice(&output.stdout).unwrap()
}

#[test]
fn shipped_helper_denies_literal_nested_root_deletion() {
    for command in [
        "sh -c 'rm --no-preserve-root -rf /'",
        "eval -- 'rm --no-preserve-root -rf /'",
        "builtin -- eval 'rm --no-preserve-root -rf /'",
        "builtin exec sh -c 'rm --no-preserve-root -rf /'",
        "builtin command sh -c 'rm --no-preserve-root -rf /'",
        "builtin builtin eval 'rm --no-preserve-root -rf /'",
        "busybox env sh -c 'rm --no-preserve-root -rf /'",
        "busybox time sh -c 'rm --no-preserve-root -rf /'",
        "busybox time -f '' sh -c 'rm --no-preserve-root -rf /'",
        "busybox time -f \"$FORMAT\" sh -c 'rm --no-preserve-root -rf /'",
        "toybox env sh -c 'rm --no-preserve-root -rf /'",
        "toybox time sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -p sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -i sh -c 'rm --no-preserve-root -rf /'",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "deny", "{command}");
        assert_eq!(response["rule_id"], "irreversible-root-delete", "{command}");
    }
}

#[test]
fn shipped_helper_denies_expanding_wrapper_option_values() {
    for command in [
        "VALUE='log sh'; /usr/bin/time -o $VALUE 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u {HOME,sh} 'rm --no-preserve-root -rf /'",
        "busybox time -o * 'rm --no-preserve-root -rf /'",
        "VALUE='HOME sh'; busybox env -u $VALUE 'rm --no-preserve-root -rf /'",
        "env -S 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -o \"$@\" 'rm --no-preserve-root -rf /'",
        "builtin exec /usr/bin/env -u \"${VALUES[@]}\" 'rm --no-preserve-root -rf /'",
        "builtin command busybox time -f \"$@\" 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -o \"${VALUE:-$@}\" 'rm --no-preserve-root -rf /'",
        "builtin exec busybox env -u \"${VALUE:-${VALUES[@]}}\" 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -o ${VALUE:-$LOG} 'rm --no-preserve-root -rf /'",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "deny", "{command}");
        assert_eq!(
            response["rule_id"], "unsafe-recursive-delete-expansion",
            "{command}"
        );
    }
}

#[test]
fn shipped_helper_preserves_quoted_exact_one_fallback_option_value_uncertainty() {
    for command in [
        "/usr/bin/time -o \"${VALUE:-$LOG}\" printf ok",
        "/usr/bin/env -u \"${VALUE:+$*}\" printf ok",
        "builtin command /usr/bin/time -o \"${VALUE:-$LOG}\" printf ok",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "indeterminate", "{command}");
    }
}

#[test]
fn shipped_helper_accepts_busybox_time_quoted_exact_one_format_values() {
    for command in [
        "busybox time -f \"${VALUE:-${FORMAT[*]}}\" printf ok",
        "builtin exec busybox time -f \"${VALUE:+$*}\" printf ok",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "no_deterministic_decision", "{command}");
    }
}

#[test]
fn shipped_helper_accepts_busybox_env_quoted_exact_one_fallback_option_value() {
    let command = "busybox env -u \"${VALUE:-${NAME:-HOME}}\" printf ok";
    let response = run_shipped_helper(command, None);
    assert_eq!(response["result"], "no_deterministic_decision", "{command}");
}

#[test]
fn shipped_helper_preserves_multicall_terminating_option_uncertainty() {
    for command in [
        "busybox time -h sh -c 'rm --no-preserve-root -rf /'",
        "busybox env --help sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -v sh -c 'rm --no-preserve-root -rf /'",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "indeterminate", "{command}");
    }
}

#[test]
fn shipped_helper_preserves_direct_wrapper_command_position_uncertainty() {
    for command in [
        "/usr/bin/time -h sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -q sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -vf FORMAT sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env --help sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env --version sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -0 sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env --argv0=displayed sh -c 'rm --no-preserve-root -rf /'",
        "env FOO=bar -i sh -c 'rm --no-preserve-root -rf /'",
        "env FOO=bar -- sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time \"$OPTION\" sh -c 'rm --no-preserve-root -rf /'",
        "env \"$OPTION\" sh -c 'rm --no-preserve-root -rf /'",
        "env FOO=bar \"$COMMAND\" sh -c 'rm --no-preserve-root -rf /'",
        "env --split-string 'rm -rf /'",
        "env --split='rm -rf /'",
        "env $'-\\x53' 'rm -rf /'",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "indeterminate", "{command}");
    }
}

#[test]
fn shipped_helper_preserves_wrapper_value_uncertainty() {
    for command in [
        "/usr/bin/time -o '' sh -c 'rm --no-preserve-root -rf /'",
        "busybox time -o '' sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u '' sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u '=HOME' sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u=HOME sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -uA=B sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -o \"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -u \"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "indeterminate", "{command}");
    }
}

#[test]
fn shipped_helper_preserves_attached_dynamic_wrapper_value_semantics() {
    for command in [
        "busybox time -o\"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
        "busybox time -f\"$FORMAT\" sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/time -oX\"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
        "/usr/bin/env -uX\"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
        "busybox time -oX\"$LOG\" sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -u\"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -iu\"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "indeterminate", "{command}");
    }

    for command in [
        "busybox time -vfX\"$FORMAT\" sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -uX\"$NAME\" sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -u\"$NAME\"X sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -iu\"$NAME\"X sh -c 'rm --no-preserve-root -rf /'",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "deny", "{command}");
        assert_eq!(response["rule_id"], "irreversible-root-delete", "{command}");
    }

    for command in [
        "busybox env -u\"$@\" sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -uX\"$@\" sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -u\"$@\"X sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -uX\"${VALUES[@]}\" sh -c 'rm --no-preserve-root -rf /'",
        "busybox env -u\"${VALUES[@]}\"X sh -c 'rm --no-preserve-root -rf /'",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "deny", "{command}");
        assert_eq!(
            response["rule_id"], "unsafe-recursive-delete-expansion",
            "{command}"
        );
    }
}

#[test]
fn shipped_helper_projects_execution_bearing_builtins() {
    for command in [
        "trap 'rm --no-preserve-root -rf /' EXIT",
        "builtin -- trap 'rm --no-preserve-root -rf /' EXIT",
        "TARGET=/; TARGET=/tmp/safe trap 'rm --no-preserve-root -rf \"$TARGET\"' EXIT",
        "mapfile -c1 -C 'rm --no-preserve-root -rf /'",
        "mapfile -c +1 -C 'rm --no-preserve-root -rf /'",
        "builtin readarray -C'rm --no-preserve-root -rf /' -c1",
        "bash --posix -c \"mapfile -c1 -C 'rm --no-preserve-root -rf /'\"",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "deny", "{command}");
        assert!(
            matches!(
                response["rule_id"].as_str(),
                Some("irreversible-root-delete" | "unsafe-recursive-delete-expansion")
            ),
            "{command}: {response}"
        );
    }

    for command in [
        "source /definitely/not-read-by-safety",
        ". /dev/stdin",
        "builtin -- source \"$FILE\"",
        "trap ':' EXIT",
        "TARGET=/tmp/safe; TARGET=/ trap 'rm --no-preserve-root -rf \"$TARGET\"' EXIT",
        "trap \"$ACTION\" EXIT",
        "sh -c 'trap -p EXIT'",
        "mapfile -c1 -C ':'",
        "readarray -C \"$CALLBACK\" -c1",
        "mapfile -c",
        "mapfile -C 'rm --no-preserve-root -rf /' -c 0",
        "mapfile -C 'rm --no-preserve-root -rf /' -n 4294967296",
        "mapfile -C 'rm --no-preserve-root -rf /' -c '1\n'",
        "sh -c \"mapfile -c1 -C 'rm --no-preserve-root -rf /'\"",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "indeterminate", "{command}");
    }

    for command in [
        "source",
        ".",
        "trap -p EXIT",
        "trap - EXIT",
        "trap '' EXIT",
        "mapfile",
        "readarray -c1 -- '-Cprintf DANGER'",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "no_deterministic_decision", "{command}");
    }
}

#[test]
fn shipped_helper_consumes_the_startup_environment_marker() {
    let response = run_shipped_helper_with_environment("printf ok", STARTUP_ENV_MARKER, "1");

    assert_eq!(response["result"], "indeterminate");
}

#[test]
fn shipped_helper_startup_uncertainty_keeps_proven_deny_precedence() {
    let response =
        run_shipped_helper_with_environment("rm --no-preserve-root -rf /", STARTUP_ENV_MARKER, "1");

    assert_eq!(response["result"], "deny");
    assert_eq!(response["rule_id"], "irreversible-root-delete");
}

#[test]
fn shipped_helper_consumes_the_inherited_lastpipe_marker() {
    let response = run_shipped_helper_with_environment(
        "TARGET=/tmp/safe; printf x | eval \"TARGET=/\"; rm --no-preserve-root -rf \"$TARGET\"",
        LASTPIPE_MARKER,
        "1",
    );

    assert_eq!(response["result"], "deny");
    assert_eq!(response["rule_id"], "unsafe-recursive-delete-expansion");
}

#[test]
fn shipped_helper_markers_accept_only_the_boolean_true_value() {
    let startup = run_shipped_helper_with_environment(
        "bash -c 'printf ok'",
        STARTUP_ENV_MARKER,
        "/tmp/attacker-startup",
    );
    let lastpipe = run_shipped_helper_with_environment(
        "TARGET=/tmp/safe; printf x | eval \"TARGET=/\"; rm --no-preserve-root -rf \"$TARGET\"",
        LASTPIPE_MARKER,
        "lastpipe",
    );
    let posix_enabled = run_shipped_helper_with_environment(
        "TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
        POSIX_MODE_ENABLED_MARKER,
        "posix",
    );
    let posix_uncertain =
        run_shipped_helper_with_environment("printf ok", POSIX_MODE_UNCERTAIN_MARKER, "uncertain");
    let posix_propagates = run_shipped_helper_with_environment(
        "bash -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'",
        POSIX_MODE_PROPAGATES_MARKER,
        "propagates",
    );

    assert_eq!(startup["result"], "no_deterministic_decision");
    assert_eq!(lastpipe["result"], "no_deterministic_decision");
    assert_eq!(posix_enabled["result"], "no_deterministic_decision");
    assert_eq!(posix_uncertain["result"], "no_deterministic_decision");
    assert_eq!(posix_propagates["result"], "no_deterministic_decision");
}

#[test]
fn shipped_helper_preserves_direct_bash_env_parity() {
    let response =
        run_shipped_helper_with_environment("printf ok", "BASH_ENV", "/tmp/attacker-startup");

    assert_eq!(response["result"], "indeterminate");
}

#[test]
fn shipped_helper_preserves_direct_exported_bashopts_parity() {
    let response = run_shipped_helper_with_environment(
        "TARGET=/tmp/safe; printf x | eval \"TARGET=/\"; rm --no-preserve-root -rf \"$TARGET\"",
        "BASHOPTS",
        "braceexpand:lastpipe",
    );

    assert_eq!(response["result"], "deny");
    assert_eq!(response["rule_id"], "unsafe-recursive-delete-expansion");
}

#[test]
fn shipped_helper_preserves_inherited_posix_mode() {
    for (name, value) in [("POSIXLY_CORRECT", ""), ("SHELLOPTS", "braceexpand:posix")] {
        let response = run_shipped_helper_with_environment(
            "TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
            name,
            value,
        );

        assert_eq!(response["result"], "deny", "{name}={value:?}");
        assert_eq!(
            response["rule_id"], "unsafe-recursive-delete-expansion",
            "{name}={value:?}"
        );
    }
}

#[test]
fn shipped_helper_propagates_inherited_posix_mode_to_direct_bash_children() {
    let nested =
        "bash -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'";
    for (name, value) in [("POSIXLY_CORRECT", ""), ("SHELLOPTS", "braceexpand:posix")] {
        let response = run_shipped_helper_with_environment(nested, name, value);
        assert_eq!(response["result"], "deny", "{name}={value:?}");
        assert_eq!(
            response["rule_id"], "unsafe-recursive-delete-expansion",
            "{name}={value:?}"
        );
    }

    for command in [
        "command bash -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'",
        "exec bash -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'",
        "/usr/bin/time bash -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'",
    ] {
        let response =
            run_shipped_helper_with_environment(command, "SHELLOPTS", "braceexpand:posix");
        assert_eq!(response["result"], "deny", "{command}");
        assert_eq!(
            response["rule_id"], "unsafe-recursive-delete-expansion",
            "{command}"
        );
    }

    let disabled = run_shipped_helper_with_environment(
        "set +o posix; bash -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'",
        "SHELLOPTS",
        "braceexpand:posix",
    );
    assert_eq!(disabled["result"], "no_deterministic_decision");

    let runtime_only = run_shipped_helper(
        "set -o posix; bash -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'",
        None,
    );
    assert_eq!(runtime_only["result"], "no_deterministic_decision");

    for (name, value) in [("POSIXLY_CORRECT", ""), ("SHELLOPTS", "braceexpand:posix")] {
        let response = run_shipped_helper_with_environment(
            "set +o posix; set -o posix; bash -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'",
            name,
            value,
        );
        assert_eq!(response["result"], "indeterminate", "{name}={value:?}");
    }

    for command in [
        "env -i HOME=/tmp bash -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'",
        "env -u SHELLOPTS bash -c 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'",
        "SHELLOPTS=braceexpand bash -c 'printf ok'",
        "sudo bash -c 'printf ok'",
    ] {
        let response =
            run_shipped_helper_with_environment(command, "SHELLOPTS", "braceexpand:posix");
        assert_eq!(response["result"], "indeterminate", "{command}");
    }
}

#[test]
fn shipped_helper_fails_closed_for_privileged_bash_posix_carrier_ambiguity() {
    let command =
        "bash -pc 'TARGET=/tmp/safe; TARGET=/ eval \":\"; rm --no-preserve-root -rf \"$TARGET\"'";
    for (name, value) in [("SHELLOPTS", "braceexpand:posix"), ("POSIXLY_CORRECT", "")] {
        let response = run_shipped_helper_with_environment(command, name, value);
        assert_eq!(response["result"], "indeterminate", "{name}={value:?}");
    }

    let direct = run_shipped_helper("bash -pc 'rm --no-preserve-root -rf /'", None);
    assert_eq!(direct["result"], "deny");
    assert_eq!(direct["rule_id"], "irreversible-root-delete");
}

#[test]
fn shipped_helper_preserves_posix_set_prefix_environment_uncertainty() {
    for dispatch in ["set", "builtin set", "command set"] {
        let command =
            format!("BASH_ENV=/tmp/attacker-startup {dispatch} -o posix; bash -c 'printf ok'");
        let response =
            run_shipped_helper_with_environment(&command, "SHELLOPTS", "braceexpand:posix");
        assert_eq!(response["result"], "indeterminate", "{command}");
    }

    let response = run_shipped_helper_with_environment(
        "BASH_ENV=/tmp/attacker-startup set -o posix; bash -c 'printf ok'; rm --no-preserve-root -rf /",
        "SHELLOPTS",
        "braceexpand:posix",
    );
    assert_eq!(response["result"], "deny");
    assert_eq!(response["rule_id"], "irreversible-root-delete");
}

#[test]
fn shipped_helper_tracks_runtime_posix_mode() {
    for command in [
        "set -o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
        "set -o posix; set +o posix; set -o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
        "builtin set -o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
        "command set -o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "deny", "{command}");
        assert_eq!(
            response["rule_id"], "unsafe-recursive-delete-expansion",
            "{command}"
        );
    }

    let response = run_shipped_helper(
        "set -o posix; set +o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
        None,
    );
    assert_eq!(response["result"], "no_deterministic_decision");

    for command in [
        "set -o posix +o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
        "set -- -o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "no_deterministic_decision", "{command}");
    }

    let response = run_shipped_helper(
        "set +o posix -o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
        None,
    );
    assert_eq!(response["result"], "deny");

    for command in [
        "set -xo posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
        "set -euxo posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
        "set -o posix foo +o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
        "set + -o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "deny", "{command}");
    }

    for command in [
        "set +xo posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
        "set foo -o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
        "set - -o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "no_deterministic_decision", "{command}");
    }

    for command in [
        "(set -o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\")",
        "{ set -o posix; TARGET=/tmp/safe; TARGET=/ eval ':'; rm --no-preserve-root -rf \"$TARGET\"; }",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "deny", "{command}");
        assert_eq!(
            response["rule_id"], "unsafe-recursive-delete-expansion",
            "{command}"
        );
    }
}

#[test]
fn shipped_helper_fails_closed_after_alias_dispatch_mutations() {
    for command in [
        "bash --posix -c 'alias wipe=\"rm --no-preserve-root -rf /\"\nwipe'",
        "eval 'shopt -s expand_aliases\nalias wipe=\"rm --no-preserve-root -rf /\"\nwipe'",
        "bash --posix -c 'alias load=source\nload /tmp/attacker-controlled-script'",
        "eval 'shopt -s expand_aliases\nalias load=source\nload /tmp/attacker-controlled-script'",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "indeterminate", "{command}");
    }

    let response = run_shipped_helper("alias load=source; rm --no-preserve-root -rf /", None);
    assert_eq!(response["result"], "deny");
    assert_eq!(response["rule_id"], "irreversible-root-delete");
}

#[test]
fn shipped_helper_fails_closed_after_literal_command_remapping() {
    for command in [
        "hash -p /tmp/attacker-controlled-executable wipe; wipe",
        "enable -f /tmp/attacker-controlled-builtin.so wipe; wipe",
    ] {
        let response = run_shipped_helper(command, None);
        assert_eq!(response["result"], "indeterminate", "{command}");
    }
}
