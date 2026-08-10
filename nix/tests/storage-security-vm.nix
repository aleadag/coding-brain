{
  doctorFixtures,
  package,
}:

{
  name = "coding-brain-storage-security";
  globalTimeout = 15 * 60;
  requiredFeatures.kvm = false;
  qemu.forceAccel = false;

  nodes.machine =
    { pkgs, ... }:
    {
      users.groups."cbrain-test" = { };
      users.groups."cbrain-attacker" = { };
      users.users."cbrain-test" = {
        isNormalUser = true;
        group = "cbrain-test";
        home = "/home/cbrain-test";
        createHome = true;
      };
      users.users."cbrain-attacker" = {
        isNormalUser = true;
        group = "cbrain-attacker";
        home = "/home/cbrain-attacker";
        createHome = true;
      };
      environment.systemPackages = [
        package
        pkgs.coreutils
        pkgs.util-linux
      ];
    };

  testScript = ''
    import json

    machine.start()
    machine.wait_for_unit("multi-user.target")

    binary = "${package}/bin/cbrain"
    home = "/home/cbrain-test"
    config = f"{home}/.config"
    provider_files = "${doctorFixtures.providerHomeManagerFiles}"
    invalid_provider_files = "${doctorFixtures.invalidProviderHomeManagerFiles}"
    provider_path = "${doctorFixtures.fakeProviders}/bin:/run/current-system/sw/bin"

    def run_cbrain_at(label, cwd, command_home, command_config, state, arguments):
        stdout = f"/tmp/{label}.stdout"
        stderr = f"/tmp/{label}.stderr"
        command = (
            f"cd {cwd} && runuser -u cbrain-test -- env "
            f"PATH={provider_path} HOME={command_home} "
            f"XDG_CONFIG_HOME={command_config} XDG_STATE_HOME={state} "
            "CODING_BRAIN_SKIP_FIRST_RUN=1 "
            f"{binary} {arguments} >{stdout} 2>{stderr}"
        )
        status, _ = machine.execute(command)
        return (
            status,
            machine.succeed(f"tail -c 16384 {stdout}"),
            machine.succeed(f"tail -c 16384 {stderr}"),
        )

    def run_cbrain(label, state, arguments):
        return run_cbrain_at(label, home, home, config, state, arguments)

    def install_provider_files(target_home, source):
        machine.succeed(
            "install -d -o cbrain-test -g cbrain-test -m 0700 "
            f"{target_home}/.codex {target_home}/.claude "
            f"{target_home}/.gemini/config"
        )
        for relative in [
            ".codex/hooks.json",
            ".claude/settings.json",
            ".gemini/config/hooks.json",
        ]:
            machine.succeed(
                f"runuser -u cbrain-test -- ln -s {source}/{relative} "
                f"{target_home}/{relative}"
            )

    def named_check(checks, name):
        return next(item for item in checks if item["name"] == name)

    def metadata(path):
        return machine.succeed(f"stat -c '%U:%G:%a' {path}").strip()

    def hierarchy(path):
        return machine.succeed(f"namei -l {path}")

    with subtest("private absolute XDG storage succeeds"):
        state = f"{home}/.local/state"
        machine.succeed(
            "install -d -o cbrain-test -g cbrain-test -m 0700 "
            f"{home} {config} {state}"
        )
        machine.succeed(
            f"test \"$(readlink -f $(command -v cbrain))\" = {binary}"
        )
        status, stdout, stderr = run_cbrain("positive-init", state, "--distill-once")
        assert status == 0, f"init failed: stdout={stdout!r} stderr={stderr!r}"
        status, stdout, stderr = run_cbrain("positive-review", state, "--brain-review list")
        assert status == 0, f"review init failed: stdout={stdout!r} stderr={stderr!r}"
        status, stdout, stderr = run_cbrain("positive-doctor", state, "doctor --json")
        assert status == 0, f"doctor failed: stdout={stdout!r} stderr={stderr!r}"
        json.loads(stdout)
        root = f"{state}/coding-brain"
        assert metadata(root) == "cbrain-test:cbrain-test:700"
        assert metadata(f"{root}/db") == "cbrain-test:cbrain-test:700"
        assert metadata(f"{root}/db/brain.sqlite3") == "cbrain-test:cbrain-test:600"
        assert metadata(f"{root}/db/review.sqlite3") == "cbrain-test:cbrain-test:600"
        machine.succeed(f"test ! -e {root}/brain/decisions.jsonl")
        machine.succeed(f"test ! -e {root}/activity.jsonl")
        machine.succeed(f"test ! -e {root}/lifecycle.jsonl")

    with subtest("Home Manager provider hooks pass doctor"):
        state = f"{home}/.local/state"
        install_provider_files(home, provider_files)
        status, stdout, stderr = run_cbrain("doctor-home-manager", state, "doctor --json")
        assert status in [0, 1], f"doctor failed: stdout={stdout!r} stderr={stderr!r}"
        checks = json.loads(stdout)
        for provider in ["Codex", "Claude", "Antigravity"]:
            setup = named_check(checks, f"{provider} setup")
            assert setup["status"] == "pass", setup
            assert setup["fix_hint"] is None, setup
            assert "evidence" not in setup, setup
        trust = named_check(checks, "Codex hook trust")
        assert trust["status"] == "advisory", trust
        assert "trust unverified" in trust["message"], trust
        assert "/hooks" in trust["fix_hint"], trust

        mixed_project = f"{home}/mixed-project"
        mixed_config = f"{mixed_project}/.config"
        mixed_state = f"{mixed_project}/.local/state"
        machine.succeed(
            "install -d -o cbrain-test -g cbrain-test -m 0700 "
            f"{mixed_project} {mixed_project}/.git {mixed_config} {mixed_state}"
        )
        status, stdout, stderr = run_cbrain_at(
            "mixed-init",
            home,
            mixed_project,
            mixed_config,
            mixed_state,
            "init codex claude --non-interactive --skip-brain --skip-skills",
        )
        assert status == 0, f"mixed init failed: stdout={stdout!r} stderr={stderr!r}"
        status, stdout, stderr = run_cbrain_at(
            "doctor-mixed",
            mixed_project,
            home,
            config,
            state,
            "doctor --json",
        )
        assert status in [0, 1], f"mixed doctor failed: stdout={stdout!r} stderr={stderr!r}"
        checks = json.loads(stdout)
        for provider, relative in [
            ("Codex", ".codex/hooks.json"),
            ("Claude", ".claude/settings.json"),
        ]:
            setup = named_check(checks, f"{provider} setup")
            assert setup["status"] == "advisory", setup
            assert setup["evidence"]["provider_files"] == [
                {
                    "path": f"{home}/{relative}",
                    "path_lossy": False,
                    "scope": "global",
                    "ownership": "home_manager",
                    "state": "current",
                },
                {
                    "path": f"{mixed_project}/{relative}",
                    "path_lossy": False,
                    "scope": "project",
                    "ownership": "imperative",
                    "state": "current",
                },
            ], setup

    with subtest("invalid Home Manager provider content fails doctor"):
        invalid_home = f"{home}/invalid-home"
        invalid_config = f"{invalid_home}/.config"
        invalid_state = f"{invalid_home}/.local/state"
        machine.succeed(
            "install -d -o cbrain-test -g cbrain-test -m 0700 "
            f"{invalid_home} {invalid_config} {invalid_state}"
        )
        install_provider_files(invalid_home, invalid_provider_files)
        status, stdout, stderr = run_cbrain_at(
            "doctor-invalid-home-manager",
            invalid_home,
            invalid_home,
            invalid_config,
            invalid_state,
            "doctor --json",
        )
        assert status == 1, f"invalid doctor status={status}: stdout={stdout!r} stderr={stderr!r}"
        checks = json.loads(stdout)
        setup = named_check(checks, "Antigravity setup")
        assert setup["status"] == "fail", setup
        assert setup["evidence"]["provider_files"] == [{
            "path": f"{invalid_home}/.gemini/config/hooks.json",
            "path_lossy": False,
            "scope": "global",
            "ownership": "home_manager",
            "state": "invalid",
            "reason": "malformed_content",
        }], setup
        assert "SECRET_PROVIDER_CONTENT" not in stdout
        assert "SECRET_PROVIDER_CONTENT" not in stderr

    with subtest("foreign-owned ancestor fails closed"):
        machine.succeed("install -d -o cbrain-attacker -g cbrain-attacker -m 0755 /srv/foreign")
        machine.succeed("install -d -o cbrain-test -g cbrain-test -m 0700 /srv/foreign/state")
        machine.succeed("runuser -u cbrain-test -- touch /srv/foreign/state/write-probe")
        machine.succeed("rm /srv/foreign/state/write-probe")
        status, stdout, stderr = run_cbrain("foreign", "/srv/foreign/state", "--distill-once")
        diagnostic = hierarchy("/srv/foreign/state")
        assert status != 0, f"foreign ancestor unexpectedly succeeded: {stdout!r}\n{diagnostic}"
        assert "state directory ancestor is foreign-owned" in stderr, f"{stderr}\n{diagnostic}"
        machine.succeed("test ! -e /srv/foreign/state/coding-brain")

    with subtest("replaceable ancestor fails closed"):
        machine.succeed("install -d -o cbrain-test -g cbrain-test -m 0777 /srv/replaceable")
        assert metadata("/srv/replaceable") == "cbrain-test:cbrain-test:777"
        machine.succeed("runuser -u cbrain-test -- touch /srv/replaceable/write-probe")
        machine.succeed("rm /srv/replaceable/write-probe")
        status, stdout, stderr = run_cbrain("replaceable", "/srv/replaceable", "--distill-once")
        diagnostic = hierarchy("/srv/replaceable")
        assert status != 0, f"replaceable ancestor unexpectedly succeeded: {stdout!r}\n{diagnostic}"
        assert "state directory ancestor is replaceable by another user" in stderr, f"{stderr}\n{diagnostic}"
        machine.succeed("test ! -e /srv/replaceable/coding-brain")
  '';
}
