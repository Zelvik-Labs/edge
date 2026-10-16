#!/usr/bin/env python3
"""Test phase control without Docker: python3 -I scripts/test-orbstack-orchestration.py."""

from pathlib import Path
import json
import os
import shutil
import subprocess
import tempfile
import unittest


SCRIPT = Path(__file__).with_name("test-orbstack-container.sh")


class OrchestrationTests(unittest.TestCase):
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="zelvik-orchestration-test-")
        self.results = Path(self.directory.name)
        source = SCRIPT.read_text()
        self.functions = source[source.index("run_build() {"):source.index("rustc --version")]
        self.functions = self.functions.replace("/results/", str(self.results) + "/")

    def tearDown(self):
        self.directory.cleanup()

    def run_phases(self, commands, cleanup_status=0):
        script = (
            "set -euo pipefail\nOVERALL_STATUS=0\nLAST_CLEANUP_STATUS=0\n"
            f"cleanup() {{ return {cleanup_status}; }}\n"
            + self.functions + commands
        )
        return subprocess.run(["bash", "-c", script], text=True, capture_output=True)

    def test_successful_phases_return_zero(self):
        result = self.run_phases("run_verification check true\nexit \"$OVERALL_STATUS\"\n")
        self.assertEqual(result.returncode, 0, result.stderr)
        self.assertEqual((self.results / "check-exit-status.txt").read_text(), "0\n")

    def test_failed_phase_does_not_skip_independent_phase(self):
        result = self.run_phases(
            "run_verification unit-tests false\n"
            "run_verification clippy true\nexit \"$OVERALL_STATUS\"\n"
        )
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertEqual((self.results / "unit-tests-exit-status.txt").read_text(), "1\n")
        self.assertEqual((self.results / "clippy-exit-status.txt").read_text(), "0\n")
        self.assertTrue((self.results / "unit-tests-cleanup.log").is_file())

    def test_failed_prerequisite_stops_immediately(self):
        result = self.run_phases("run_build ebpf false\nrun_verification check true\n")
        self.assertEqual(result.returncode, 1, result.stderr)
        self.assertFalse((self.results / "check-exit-status.txt").exists())

    def test_cleanup_failure_stops_independent_phases(self):
        result = self.run_phases(
            "run_verification check true\nrun_verification clippy true\n", cleanup_status=3
        )
        self.assertEqual(result.returncode, 3, result.stderr)
        self.assertFalse((self.results / "clippy-exit-status.txt").exists())

    def test_later_success_cannot_erase_prior_failure(self):
        result = self.run_phases(
            "run_verification tests bash -c 'exit 7'\n"
            "run_verification check true\nexit \"$OVERALL_STATUS\"\n"
        )
        self.assertEqual(result.returncode, 7, result.stderr)


class ProtocolFixture:
    def setUp(self):
        self.directory = tempfile.TemporaryDirectory(prefix="zelvik-protocol-test-")
        self.root = Path(self.directory.name)
        self.home = self.root / "home"
        self.home.mkdir()
        self.env = os.environ.copy()
        for key in list(self.env):
            if key.startswith(("GIT_", "CARGO_")) or key in ("RUSTC_WRAPPER", "RUSTC_WORKSPACE_WRAPPER"):
                del self.env[key]
        self.env.update(
            HOME=str(self.home), GIT_CONFIG_NOSYSTEM="1", GIT_TERMINAL_PROMPT="0",
            GIT_CONFIG_GLOBAL=str(self.home / ".gitconfig"),
            GIT_AUTHOR_NAME="Fixture", GIT_AUTHOR_EMAIL="fixture@example.invalid",
            GIT_COMMITTER_NAME="Fixture", GIT_COMMITTER_EMAIL="fixture@example.invalid",
            CARGO_HOME=str(self.root / "cargo"), CARGO_TARGET_DIR=str(self.root / "target"),
        )
        self.url = "https://github.com/Zelvik-Labs/protocol"
        self.tag = "rust/v0.0.1"
        self.upstream = self.root / "upstream"
        self.upstream.mkdir()
        self.git("init", "--template=", str(self.upstream))
        (self.upstream / "src").mkdir()
        (self.upstream / "src/lib.rs").write_text("pub fn pinned() {}\n")
        (self.upstream / "Cargo.toml").write_text(
            '[package]\nname = "zelvik-suite"\nversion = "0.0.1"\nedition = "2021"\n'
        )
        self.git("-C", str(self.upstream), "add", ".")
        self.git("-C", str(self.upstream), "commit", "-qm", "locked revision")
        self.rev = self.git("-C", str(self.upstream), "rev-parse", "HEAD").stdout.strip()
        self.git("-C", str(self.upstream), "tag", self.tag)
        # A moved tag must never change the dependency supplied to the container.
        (self.upstream / "src/lib.rs").write_text("pub fn moved_tag() {}\n")
        self.git("-C", str(self.upstream), "commit", "-qam", "move tag")
        self.git("-C", str(self.upstream), "tag", "-f", self.tag)
        self.git("config", "--global", "url." + self.upstream.as_uri() + ".insteadOf", self.url)
        self.git("config", "--global", "credential.helper", "host-only-sentinel")
        self.app = self.root / "app"
        (self.app / "src").mkdir(parents=True)
        (self.app / "src/lib.rs").write_text("pub fn app() {}\n")
        (self.app / "Cargo.toml").write_text(
            '[package]\nname = "fixture-app"\nversion = "0.0.1"\nedition = "2021"\n'
            '[dependencies]\nzelvik-suite = { git = "' + self.url + '", tag = "' + self.tag + '" }\n'
        )
        self.lock = self.app / "Cargo.lock"
        self.lock_text = (
            'version = 4\n\n[[package]]\nname = "fixture-app"\nversion = "0.0.1"\n'
            'dependencies = ["zelvik-suite"]\n\n[[package]]\nname = "zelvik-suite"\n'
            'version = "0.0.1"\nsource = "git+' + self.url + '?tag=rust%2Fv0.0.1#' + self.rev + '"\n'
        )
        self.lock.write_text(self.lock_text)
        self.mirror = self.root / "protocol.git"
        self.helper = SCRIPT.with_name("orbstack-protocol.sh")

    def tearDown(self):
        self.directory.cleanup()

    def git(self, *args):
        return subprocess.run(["git", *args], env=self.env, cwd=self.root,
                              text=True, capture_output=True, check=True)

    def shell(self, commands, *args, env=None):
        return subprocess.run(["bash", "-c", 'set -euo pipefail\nsource "$1"\n' + commands,
                               "fixture", str(self.helper), *map(str, args)],
                              env=env or self.env, cwd=self.root, text=True, capture_output=True)

    def prepare(self):
        result = self.shell('prepare_protocol_mirror "$2" "$3"', self.lock, self.mirror)
        self.assertEqual(result.returncode, 0, result.stderr)

class ProtocolTests(ProtocolFixture, unittest.TestCase):
    def test_exact_locked_commit_and_no_host_config_in_mirror(self):
        self.prepare()
        actual = self.git("--git-dir=" + str(self.mirror), "rev-parse", "refs/tags/" + self.tag)
        self.assertEqual(actual.stdout.strip(), self.rev)
        config = self.git("--git-dir=" + str(self.mirror), "config", "--local", "--list").stdout
        self.assertNotIn("host-only-sentinel", config)
        self.assertNotIn("remote.", config)
        self.assertNotIn("insteadOf", config)
        self.assertEqual(list((self.mirror / "hooks").glob("*")), [])

    def test_missing_revision_fails_without_falling_back_to_tag(self):
        self.lock.write_text(self.lock_text.replace(self.rev, "0" * 40))
        result = self.shell('prepare_protocol_mirror "$2" "$3"', self.lock, self.mirror)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("Cannot fetch locked protocol commit", result.stderr)
        self.assertFalse((self.mirror / "refs/tags" / self.tag).exists())

    def test_invalid_lock_identity_rejected_before_fetch(self):
        for bad in (self.lock_text.replace(self.url, self.url + "-other"),
                    self.lock_text.replace(self.rev, self.rev[:8]),
                    self.lock_text.replace("rust%2Fv0.0.1", "..%2Fevil"),
                    self.lock_text + self.lock_text[self.lock_text.index('[[package]]\nname = "zelvik-suite"'):]):
            with self.subTest(lock=bad):
                self.lock.write_text(bad)
                result = self.shell('prepare_protocol_mirror "$2" "$3"', self.lock, self.mirror)
                self.assertNotEqual(result.returncode, 0)
                self.assertFalse(self.mirror.exists())

    def test_metadata_resolves_locked_identity_using_only_mirror(self):
        self.prepare()
        # Remove upstream access. Only the mirror can satisfy this metadata request.
        self.upstream.rename(self.root / "unavailable-upstream")
        cargo = shutil.which("cargo")
        if shutil.which("rustup"):
            cargo = subprocess.run(["rustup", "which", "cargo"], cwd=self.root,
                                   capture_output=True, text=True, check=True).stdout.strip()
        if not cargo:
            self.fail("cargo is required for the offline dependency-resolution fixture")
        metadata_env = self.env.copy()
        if shutil.which("rustup"):
            metadata_env["RUSTC"] = subprocess.run(
                ["rustup", "which", "rustc"], cwd=self.root, capture_output=True,
                text=True, check=True).stdout.strip()
        # Model the container's host-owned read-only bind: trust must come from the
        # process-scoped safe.directory, not the (unmounted) global config.
        metadata_env["GIT_TEST_ASSUME_DIFFERENT_OWNER"] = "1"
        result = self.shell(
            'configure_protocol_git "$2" "$3"\ncd "$4"\n'
            '"$5" metadata --locked --format-version 1',
            self.lock, self.mirror, self.app, cargo, env=metadata_env,
        )
        self.assertEqual(result.returncode, 0, result.stderr)
        package = next(p for p in json.loads(result.stdout)["packages"] if p["name"] == "zelvik-suite")
        self.assertEqual(package["source"], "git+" + self.url + "?tag=" + self.tag + "#" + self.rev)
        self.assertEqual(Path(package["manifest_path"]).with_name("src").joinpath("lib.rs").read_text(),
                         "pub fn pinned() {}\n")
        self.assertEqual(self.lock.read_text(), self.lock_text)
        self.assertFalse((self.root / "target").exists())

    def test_container_rejects_wrong_mirror_tag(self):
        self.prepare()
        other = self.git("-C", str(self.upstream), "rev-parse", "HEAD").stdout.strip()
        self.git("--git-dir=" + str(self.mirror), "fetch", str(self.upstream), other)
        self.git("--git-dir=" + str(self.mirror), "update-ref", "refs/tags/" + self.tag, other)
        result = self.shell('configure_protocol_git "$2" "$3"', self.lock, self.mirror)
        self.assertNotEqual(result.returncode, 0)
        self.assertIn("does not match Cargo.lock", result.stderr)

    def test_dependency_failure_stops_before_build_stages(self):
        self.prepare()
        source = SCRIPT.read_text()
        functions = source[source.index("run_build() {"):source.index("rustc --version")]
        functions = (functions.replace("/workspace/scripts/orbstack-protocol.sh", str(self.helper))
                     .replace("/workspace/Cargo.lock", str(self.lock))
                     .replace("/protocol.git", str(self.mirror))
                     .replace("/results/", str(self.root) + "/"))
        # Use the actual dependency-stage invocation, followed by a build sentinel.
        gate = source[source.index('# The independent pingora workspace'):source.index("# Resolve the existing dated nightly")]
        script = ('set -euo pipefail\nPHASE=check\nOVERALL_STATUS=0\nLAST_CLEANUP_STATUS=0\n'
                  'cleanup() { return 0; }\ncargo() { return 37; }\n' + functions + gate +
                  '\nprintf build-started > "$1"\n')
        marker = self.root / "build-started"
        result = subprocess.run(["bash", "-c", script, "fixture", str(marker)],
                                env=self.env, cwd=self.root, text=True, capture_output=True)
        self.assertNotEqual(result.returncode, 0)
        self.assertFalse(marker.exists())
        self.assertEqual((self.root / "dependencies-exit-status.txt").read_text(), "37\n")
        self.assertIn("Dependency preflight failed", result.stderr)


class HostLifecycleTests(ProtocolFixture, unittest.TestCase):
    def run_host(self, build_status=0, run_status=0, signal=False, list_status=0,
                 remove_status=0, phase="check", volume_status=0):
        fixture = self.root / "edge fixture"
        scripts = fixture / "scripts"
        scripts.mkdir(parents=True)
        shutil.copy2(SCRIPT.with_name("test-orbstack.sh"), scripts)
        shutil.copy2(self.helper, scripts)
        shutil.copy2(self.lock, fixture)
        self.git("init", "--template=", str(fixture))
        self.git("-C", str(fixture), "add", ".")
        self.git("-C", str(fixture), "commit", "-qm", "host fixture")
        bin_dir = self.root / "bin"
        bin_dir.mkdir()
        docker = bin_dir / "docker"
        docker.write_text(
            '#!/usr/bin/env python3\n'
            'import json, os, pathlib, signal, sys\n'
            'args = sys.argv[1:]\n'
            'with open(os.environ["DOCKER_CALLS"], "a") as log:\n'
            '    log.write(json.dumps(args) + "\\n")\n'
            'if args[2] == "container":\n'
            '    print("fixture-cid")\n'
            '    sys.exit(int(os.environ["LIST_STATUS"]))\n'
            'if args[2] == "rm":\n'
            '    sys.exit(int(os.environ["REMOVE_STATUS"]))\n'
            'if args[2:4] == ["image", "ls"]:\n'
            '    print("fixture-image")\n'
            'if args[2:4] == ["volume", "rm"]:\n'
            '    sys.exit(int(os.environ["VOLUME_STATUS"]))\n'
            'if args[2] == "build":\n'
            '    sys.exit(int(os.environ["BUILD_STATUS"]))\n'
            'if args[2] == "run":\n'
            '    pathlib.Path(args[args.index("--cidfile") + 1]).write_text("fixture-cid")\n'
            '    if os.environ["SEND_SIGNAL"] == "1":\n'
            '        os.kill(os.getppid(), signal.SIGTERM)\n'
            '    sys.exit(int(os.environ["RUN_STATUS"]))\n'
        )
        docker.chmod(0o755)
        calls = self.root / "docker-calls.jsonl"
        temp = self.root / "temp"
        temp.mkdir()
        env = dict(self.env, PATH=str(bin_dir) + os.pathsep + self.env["PATH"], TMPDIR=str(temp),
                   ZELVIK_EDGE_TEST_RESULTS=str(self.root / "results"), DOCKER_CALLS=str(calls),
                   BUILD_STATUS=str(build_status), RUN_STATUS=str(run_status),
                   SEND_SIGNAL=str(int(signal)), LIST_STATUS=str(list_status),
                   REMOVE_STATUS=str(remove_status), VOLUME_STATUS=str(volume_status))
        result = subprocess.run(["bash", str(scripts / "test-orbstack.sh"), "arm64", phase],
                                cwd=self.root, env=env, text=True, capture_output=True)
        if list_status or remove_status:
            self.assertEqual(len(list(temp.iterdir())), 1, result.stderr)
            self.assertIn("retaining its bind", result.stderr)
            self.assertTrue(next(temp.iterdir()).joinpath("protocol.git").is_dir())
        else:
            self.assertEqual(list(temp.iterdir()), [], result.stderr)
        calls = [json.loads(line) for line in calls.read_text().splitlines()] if calls.exists() else []
        calls = [c[2:] if c[:2] == ["--context", "orbstack"] else c for c in calls]
        return result, calls

    def test_host_mount_and_cleanup_success(self):
        result, calls = self.run_host()
        self.assertEqual(result.returncode, 0, result.stderr)
        run = next(c for c in calls if c[0] == "run")
        self.assertEqual(run[run.index("--cpus") + 1], "2")
        self.assertEqual(run[run.index("--memory") + 1], "4g")
        mounts = [run[i + 1] for i, value in enumerate(run) if value == "--mount"]
        self.assertEqual(sum("target=/protocol.git,readonly" in m for m in mounts), 1)
        self.assertFalse(any(str(self.home) in m for m in mounts))
        self.assertFalse(any("credential" in a.lower() or "token" in a.lower() for a in run))
        self.assertIn(["rm", "-fv", "fixture-cid"], calls)
        volumes = [c[2] for c in calls if c[:2] == ["volume", "rm"]]
        self.assertEqual(len(volumes), 5)
        run_id = next(c[2] for c in calls if c[:2] == ["image", "rm"]).removeprefix(
            "zelvik-edge-orb-test:1.99.0-arm64-")
        self.assertTrue(all(v.startswith("zelvik-edge-test-") and v.endswith("-arm64-" + run_id)
                            for v in volumes))

    def test_host_fetch_failure_skips_docker_and_cleans_temp(self):
        self.lock.write_text(self.lock_text.replace(self.rev, "0" * 40))
        result, calls = self.run_host()
        self.assertNotEqual(result.returncode, 0)
        self.assertEqual(calls, [])
        self.assertIn("Private dependency preflight failed", result.stderr)

    def test_host_image_failure_cleans_temp(self):
        result, calls = self.run_host(build_status=17)
        self.assertEqual(result.returncode, 17, result.stderr)
        self.assertEqual([c[0] for c in calls], ["build", "image", "image"])
        self.assertFalse(any(c[0] == "volume" for c in calls))

    def test_host_pingora_phase_skips_private_dependency(self):
        result, calls = self.run_host(phase="pingora")
        self.assertEqual(result.returncode, 0, result.stderr)
        run = next(c for c in calls if c[0] == "run")
        mounts = [run[i + 1] for i, value in enumerate(run) if value == "--mount"]
        self.assertFalse(any("/protocol.git" in m for m in mounts))
        self.assertFalse((self.root / "results" / "protocol-fetch.log").exists())

    def test_host_container_failure_cleans_exact_container(self):
        result, calls = self.run_host(run_status=23)
        self.assertEqual(result.returncode, 23, result.stderr)
        self.assertIn(["rm", "-fv", "fixture-cid"], calls)

    def test_host_cleanup_query_failure_returns_nonzero(self):
        result, calls = self.run_host(list_status=29)
        self.assertEqual(result.returncode, 29, result.stderr)
        self.assertEqual(calls[-1][:3], ["container", "ls", "-aq"])

    def test_host_container_removal_failure_retains_bind_and_reports(self):
        result, calls = self.run_host(remove_status=31)
        self.assertEqual(result.returncode, 31, result.stderr)
        self.assertIn(["rm", "-fv", "fixture-cid"], calls)
        self.assertFalse(any(c[:2] == ["volume", "rm"] for c in calls))
        self.assertFalse(any(c[:2] == ["image", "rm"] for c in calls))

    def test_host_signal_cleans_exact_container(self):
        result, calls = self.run_host(signal=True)
        self.assertEqual(result.returncode, 143, result.stderr)
        self.assertIn(["rm", "-fv", "fixture-cid"], calls)


if __name__ == "__main__":
    unittest.main()
