#!/usr/bin/env python3
"""Summarize the native paired runner's per-round files (no hidden failures)."""
import argparse
import json
import pathlib
import re
import statistics


def h1(path):
    text = path.read_text()
    rate = re.search(r"Requests/sec:\s+([\d.]+)", text)
    p99 = re.search(r"99%\s+([\d.]+)(us|ms|s)", text)
    requests = int(re.search(r"(\d+) requests in", text)[1]) if re.search(r"(\d+) requests in", text) else None
    return {"rps": float(rate[1]) if rate else None,
            "p99_ms": float(p99[1]) * {"us": .001, "ms": 1, "s": 1000}[p99[2]] if p99 else None,
            "requests": requests, "success": requests,
            "errors": [line.strip() for line in text.splitlines()
                       if "Non-2xx" in line or "Socket errors" in line]}


def h2(path, tsv):
    text = path.read_text()
    rate = re.search(r"([\d.]+) req/s", text)
    stats = re.search(r"requests:\s+(\d+) total.*?(\d+) succeeded.*?(\d+) failed.*?(\d+) errored", text)
    rows = [line.split() for line in tsv.read_text().splitlines() if line.strip() and not line.startswith("#")]
    good = sorted(float(row[2]) / 1000 for row in rows if row[1] == "200")
    return {"rps": float(rate[1]) if rate else None,
            "p99_ms": good[max(0, -(-len(good) * 99 // 100) - 1)] if good else None,
            "requests": int(stats[1]) if stats else len(rows), "success": int(stats[2]) if stats else len(good),
            "failed": int(stats[3]) if stats else None, "errored": int(stats[4]) if stats else None}


def h3(path):
    obj = json.loads(path.read_text())
    dist = obj["statusCodeDistribution"]
    return {"rps": obj["summary"]["requestsPerSec"], "p99_ms": obj["latencyPercentiles"]["p99"] * 1000,
            "requests": sum(dist.values()), "success": dist.get("200", 0),
            "errors": obj["errorDistribution"]}


def counters(path):
    result = {}
    for line in path.read_text().splitlines():
        parts = line.split(",")
        if len(parts) >= 3 and parts[2] in ("task-clock", "context-switches", "syscalls:sys_enter_futex"):
            result[parts[2]] = float(parts[0])
    return result


def main(root):
    root = pathlib.Path(root)
    rounds = {}
    for path in sorted(root.glob("*-h1.txt")):
        stem = path.name[:-len("-h1.txt")]
        label = "candidate" if "candidate" in stem else "baseline"
        rows = rounds.setdefault(stem.split("-")[0], {}).setdefault(label, {})
        h2txt = root / f"{stem}-h2.txt"
        h3json = root / f"{stem}.h3.json"
        if not (h2txt.exists() and h3json.exists()):
            continue
        rows["h1"] = h1(path)
        rows["h2"] = h2(h2txt, root / f"{stem}.h2-latency.tsv")
        rows["h3"] = h3(h3json)
        for proto in ["h1", "h2", "h3"]:
            perf = root / f"{stem}-{proto}.perf.csv"
            rows[proto]["counters"] = counters(perf) if perf.exists() else {}
    summary = {}
    for repeat, variants in sorted(rounds.items()):
        for label, protocols in variants.items():
            if "h3" not in protocols:
                continue
            summary.setdefault(label, {}).setdefault("rounds", []).append({"repeat": repeat, **protocols})
    for label, data in summary.items():
        data["medians"] = {}
        for proto in ["h1", "h2", "h3"]:
            values = [r[proto]["rps"] for r in data["rounds"]]
            p99 = [r[proto]["p99_ms"] for r in data["rounds"]]
            data["medians"][proto] = {"rps": statistics.median(values), "rps_min": min(values),
                                      "rps_max": max(values), "p99_ms": statistics.median(p99),
                                      "p99_min": min(p99), "p99_max": max(p99),
                                      "success": sum(r[proto]["success"] for r in data["rounds"]),
                                      "requests": sum(r[proto]["requests"] for r in data["rounds"])}
            for counter, key, scale in [("task-clock", "task_clock_ms_per_request", 1.0),
                                        ("context-switches", "context_switches_per_kreq", 1000.0),
                                        ("syscalls:sys_enter_futex", "futex_per_kreq", 1000.0)]:
                ratios = []
                for r in data["rounds"]:
                    value = r[proto].get("counters", {}).get(counter)
                    if value is not None and r[proto]["requests"]:
                        ratios.append(value * scale / r[proto]["requests"])
                if ratios:
                    data["medians"][proto][key] = statistics.median(ratios)
    return summary


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("root")
    parser.add_argument("--output", required=True)
    args = parser.parse_args()
    pathlib.Path(args.output).write_text(json.dumps(main(args.root), indent=2))
