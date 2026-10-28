#!/usr/bin/env python3
"""Repeated native clients against matched loopback EdgeProxy fixtures on Linux.
Clients use CPUs 2-3, proxy 0-1, origin 4-5; requires at least six CPUs.
Timing and software-counter runs are separate from sampled CPU profiles.
"""
import argparse
import json
import os
import pathlib
import signal
import socket
import subprocess
import sys
import time


def stop_profiler(proc):
    proc.send_signal(signal.SIGINT)
    try:
        proc.communicate(timeout=5)
    except subprocess.TimeoutExpired:
        proc.kill()
        proc.communicate()


def run(args):
    out = pathlib.Path(args.output).resolve()
    out.mkdir(parents=True, exist_ok=True)
    origin = subprocess.Popen(
        ["taskset", "-c", "4,5", sys.executable, "-I", args.driver, "--origin-only"],
        stdout=subprocess.DEVNULL, stderr=subprocess.PIPE,
    )
    try:
        for _ in range(100):
            try:
                with socket.create_connection(("127.0.0.1", 19081), 1):
                    break
            except OSError:
                time.sleep(.1)
        else:
            raise RuntimeError("origin did not start")
        with (out / "direct-origin.txt").open("w") as dest:
            subprocess.run(["taskset", "-c", "2,3", "wrk", "-t2", "-c32", "-d8s", "--latency",
                            "http://127.0.0.1:19081/fast"], stdout=dest, stderr=subprocess.STDOUT, check=True, timeout=30)
        for repeat in range(args.rounds):
            labels = ["baseline", "candidate"] if repeat % 2 == 0 else ["candidate", "baseline"]
            for label in labels:
                unit = f"zelvik-native-{repeat}-{label}"
                stem = f"{repeat}-{label}"
                home = out / (stem + "-home")
                home.mkdir(exist_ok=True)
                log = out / (stem + ".server.log")
                subprocess.run([
                    "systemd-run", "--quiet", "--collect", "--unit", unit,
                    "-p", "MemoryMax=1G", "-p", "MemorySwapMax=0", "-p", "LimitNOFILE=65536",
                    "-p", "CPUQuota=200%", "-p", "AllowedCPUs=0,1",
                    "-p", f"StandardOutput=append:{log}", "-p", f"StandardError=append:{log}",
                    "--setenv", f"ZELVIK_EDGE_HOME={home}",
                    "--setenv", f"PRESSURE_TLS_DIR={pathlib.Path(args.tls).resolve()}",
                    str(pathlib.Path(getattr(args, label)).resolve()),
                ], check=True)
                try:
                    for _ in range(150):
                        try:
                            with socket.create_connection(("127.0.0.1", 19090), 1) as sock:
                                sock.sendall(b"stats\n")
                                state = json.loads(sock.recv(65536))
                            break
                        except OSError:
                            time.sleep(.1)
                    else:
                        raise RuntimeError(log.read_text())
                    commands = [
                        ("h1", ["wrk", "-t2", "-c32", "-d12s", "--latency", "http://127.0.0.1:19080/fast"]),
                        # Each connection serves 150 requests, below the production
                        # H2 churn close at 200 streams per 30 seconds. Duration
                        # tests on a fixed connection would measure that rejection.
                        ("h2", ["h2load", "-n", "4800", "-c", "32", "-m", "1",
                                "--log-file", str(out / (stem + ".h2-latency.tsv")), "https://localhost:19443/fast"]),
                        ("h3", [args.h3_client, "--host", "127.0.0.1", "--port", "19443", "--path", "/fast",
                                "--conns", "8", "--streams", "4", "--duration", "12s", "--body",
                                "--out", str(out / (stem + ".h3.json"))]),
                    ]
                    for proto, command in commands:
                        before_cpu = (pathlib.Path(state["cgroup"]) / "cpu.stat").read_text()
                        profiler = subprocess.Popen([
                            "perf", "stat", "-x", ",", "-e",
                            "task-clock,context-switches,page-faults,syscalls:sys_enter_futex",
                            "-p", str(state["pid"]), "-o", str(out / f"{stem}-{proto}.perf.csv"),
                        ], stderr=subprocess.PIPE)
                        try:
                            with (out / f"{stem}-{proto}.txt").open("w") as dest:
                                measured = subprocess.run(["taskset", "-c", "2,3", *command],
                                                          stdout=dest, stderr=subprocess.STDOUT, timeout=45,
                                                          env={**os.environ, "TOKIO_WORKER_THREADS": "2"})
                        finally:
                            stop_profiler(profiler)
                        if measured.returncode:
                            raise RuntimeError(f"{proto} failed: {stem}")
                        cpu_stat = pathlib.Path(state["cgroup"]) / "cpu.stat"
                        (out / f"{stem}-{proto}.cpu.json").write_text(json.dumps({"before": before_cpu, "after": cpu_stat.read_text()}))
                        print(json.dumps({"round": repeat, "label": label, "protocol": proto,
                                          "exit": measured.returncode}), flush=True)
                    # One separate CPU sample per variant, after the timing rounds.
                    # A fresh H1 wrk client avoids the H2 churn lifecycle above.
                    if repeat == 0:
                        profile = out / f"{stem}-cpu.perf.data"
                        recorder = subprocess.Popen(["perf", "record", "-q", "-e", "cpu-clock", "-F", "99",
                                                     "-g", "-p", str(state["pid"]), "-o", str(profile)], stderr=subprocess.PIPE)
                        try:
                            subprocess.run(["taskset", "-c", "2,3", "wrk", "-t2", "-c32", "-d8s",
                                            "http://127.0.0.1:19080/fast"], stdout=subprocess.DEVNULL, check=True, timeout=30)
                        finally:
                            stop_profiler(recorder)
                        report = subprocess.run(["perf", "report", "--stdio", "--no-children", "-i", str(profile),
                                                 "--percent-limit", "0.2"], capture_output=True, text=True, check=True)
                        (out / f"{stem}-cpu.profile.txt").write_text(report.stdout + report.stderr)
                finally:
                    subprocess.run(["systemctl", "stop", unit], check=False)
    finally:
        origin.terminate()
        origin.communicate(timeout=5)


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("--output", required=True)
    parser.add_argument("--baseline", required=True)
    parser.add_argument("--candidate", required=True)
    parser.add_argument("--driver", required=True)
    parser.add_argument("--tls", required=True)
    parser.add_argument("--h3-client", required=True)
    parser.add_argument("--rounds", type=int, default=5)
    run(parser.parse_args())
