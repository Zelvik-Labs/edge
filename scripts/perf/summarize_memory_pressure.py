#!/usr/bin/env python3
"""Summarize retained pressure/paired benchmark evidence without hiding failures."""
import argparse
import json
import math
import pathlib
import re
import statistics


def counters(text):
    return {k: int(v) for k, v in (line.split() for line in text.splitlines())}


def distribution(values):
    xs = sorted(v for v in values if v is not None)
    return {"n": len(xs), "min": min(xs), "median": statistics.median(xs), "max": max(xs)} if xs else None


def summarize(root):
    output = {"pressure": [], "paired": {}, "native": {}}
    for path in sorted(root.rglob("*.json")):
        if path.name.endswith((".terminal.json", ".start.json")) or path.name in ["metadata.json", "summary.json"]:
            continue
        obj = json.loads(path.read_text())
        if not isinstance(obj, dict):
            continue
        if obj.get("stages"):
            terminal_path = path.with_name(path.stem + ".terminal.json")
            terminal = json.loads(terminal_path.read_text()) if terminal_path.exists() else {}
            rows = []
            for row in obj["stages"]:
                physical = row["physical"]
                rows.append({"target_percent": row["target_percent"], "current": int(physical["memory.current"]),
                             "peak": int(physical["memory.peak"]), "events": counters(physical["memory.events"]),
                             "psi": physical["memory.pressure"], "host_psi": physical.get("host_memory_pressure"),
                             "rss_pss": physical.get("smaps_rollup"),
                             "pressure": row["state"]["pressure"], "accepted_progress": row["accepted_progress"],
                             "new_requests": {p: {k: v.get(k) for k in ["requests", "attempted", "success", "p99_ms", "errors"]}
                                              for p, v in row["new_requests"].items()}})
            output["pressure"].append({"file": str(path.relative_to(root)), "failure": obj.get("failure"),
                                       "stages": rows, "terminal": terminal})
        if obj.get("abuse"):
            output.setdefault("abuse", []).append({"file": str(path.relative_to(root)), "results": obj["abuse"]})
        for row in obj.get("benchmark", []):
            label = obj["label"]
            key = ("alloc" if path.stem.startswith("alloc-") else "timing") + ":" + row["protocol"]
            entry = {k: row.get(k) for k in ["requests", "attempted", "success", "rps", "p50_ms", "p99_ms", "errors"]}
            entry["file"] = str(path.relative_to(root))
            for field in ["allocations", "allocatedBytes", "reallocations"]:
                entry[field + "_per_success"] = row.get(field, 0) / row["success"] if row["success"] else None
            output["paired"].setdefault(key, {}).setdefault(label, []).append(entry)
    for key, labels in output["paired"].items():
        for label, rows in list(labels.items()):
            labels[label] = {"samples": rows, "rps": distribution([r["rps"] for r in rows]),
                             "p99_ms": distribution([r["p99_ms"] for r in rows]),
                             "all_requests_succeeded": all(r["success"] == r["requests"] for r in rows)}
    for path in sorted(root.rglob("*-h1.txt")):
        text = path.read_text()
        label = "candidate" if "candidate" in path.name else "baseline"
        rate = re.search(r"Requests/sec:\s+([\d.]+)", text)
        latency = re.search(r"99%\s+([\d.]+)(us|ms|s)", text)
        row = {"file": str(path.relative_to(root)), "rps": float(rate[1]) if rate else None,
               "p99_ms": float(latency[1]) * {"us": .001, "ms": 1, "s": 1000}[latency[2]] if latency else None,
               "errors": [line.strip() for line in text.splitlines() if "error" in line.lower() or "non-2xx" in line.lower()]}
        output["native"].setdefault("h1", {}).setdefault(label, []).append(row)
    for path in sorted(root.rglob("*.h2-latency.tsv")):
        lines = [line.split() for line in path.read_text().splitlines() if line.strip() and not line.startswith("#")]
        good = sorted(float(line[2]) / 1000 for line in lines if line[1] == "200")
        label = "candidate" if "candidate" in path.name else "baseline"
        text_path = path.with_name(path.name.replace(".h2-latency.tsv", "-h2.txt"))
        text = text_path.read_text()
        rate = re.search(r"([\d.]+) req/s", text)
        output["native"].setdefault("h2", {}).setdefault(label, []).append({
            "file": str(path.relative_to(root)), "requests": len(lines), "success": len(good),
            "p99_ms": good[max(0, math.ceil(len(good)*.99)-1)] if good else None,
            "rps": float(rate[1]) if rate else None})
    for path in sorted(root.rglob("*.h3.json")):
        label = "candidate" if "candidate" in path.name else "baseline"
        obj = json.loads(path.read_text())
        output["native"].setdefault("h3", {}).setdefault(label, []).append({
            "file": str(path.relative_to(root)), "rps": obj["summary"]["requestsPerSec"],
            "p99_ms": obj["latencyPercentiles"]["p99"] * 1000,
            "success": obj["statusCodeDistribution"].get("200", 0),
            "errors": obj["errorDistribution"], "sizePerRequest": obj["summary"]["sizePerRequest"]})
    return output


if __name__ == "__main__":
    parser = argparse.ArgumentParser()
    parser.add_argument("root", type=pathlib.Path)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    args.output.write_text(json.dumps(summarize(args.root), indent=2))
