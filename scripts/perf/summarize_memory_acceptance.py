#!/usr/bin/env python3
"""Extract the round-2 acceptance fields that the generic summarizer omits."""
import argparse
import json
import pathlib


def event_counters(text):
    result = {}
    for line in (text or "").splitlines():
        parts = line.split()
        if len(parts) == 2 and parts[1].isdigit():
            result[parts[0]] = int(parts[1])
    return result


def physical_summary(physical):
    events = event_counters((physical or {}).get("memory.events", ""))
    return {
        "current_bytes": int((physical or {}).get("memory.current", 0)),
        "peak_bytes": int((physical or {}).get("memory.peak", 0)),
        "events": events,
        "oom_kill": events.get("oom_kill", 0),
        "psi_memory": (physical or {}).get("memory.pressure"),
    }


def proto_row(row):
    return {
        "requests": row.get("requests"),
        "attempted": row.get("attempted"),
        "success": row.get("success"),
        "p99_ms": row.get("p99_ms"),
        "errors": row.get("errors", [])[:3],
    }


def held_row(row):
    if not isinstance(row, dict) or not row:
        return {}
    return {proto: proto_row(value) for proto, value in row.items()}


def stage_row(row):
    state = row.get("state", {})
    return {
        "target_percent": row.get("target_percent"),
        "source": row.get("source"),
        "reached": row.get("reached"),
        "pressure": state.get("pressure"),
        "l4_pressure": state.get("l4Pressure"),
        "tcp_active_per_ip_limit": state.get("tcpActivePerIpLimit"),
        "cache_mb": round((state.get("cache") or {}).get("memoryBytes", 0) / (1 << 20), 1),
        "cache": state.get("cache"),
        "governor": state.get("governor"),
        "new_requests": held_row(row.get("new_requests")),
        "held_new_requests": {
            proto: {
                "success": value.get("success"),
                "attempted": value.get("attempted"),
                "connection_alive": value.get("connection_alive"),
                "held_progress_bytes": value.get("held_progress_bytes"),
                "errors": value.get("errors", [])[:3],
            }
            for proto, value in (row.get("held_new_requests") or {}).items()
        },
        "held": {
            proto: {
                "alive": value.get("alive"),
                "bytes_delta": value.get("bytes_delta"),
                "status": value.get("status"),
                "error": value.get("error"),
            }
            for proto, value in (row.get("accepted_progress") or {}).items()
        },
        "shed": row.get("shed"),
        "admission_rejects": row.get("admission_rejects"),
        "physical": physical_summary(row.get("physical")),
    }


def run_row(path):
    data = json.loads(path.read_text())
    mid = data.get("mid_recovery") or {}
    terminal_path = path.with_name(path.stem + ".terminal.json")
    terminal = json.loads(terminal_path.read_text()) if terminal_path.exists() else {}
    return {
        "file": path.name,
        "label": data.get("label"),
        "source": data.get("acceptance_source"),
        "failure": data.get("failure"),
        "stopped_after_final_resort_at": data.get("stopped_after_final_resort_at"),
        "stages": [stage_row(row) for row in data.get("stages", [])],
        "mid_recovery": {
            "release_seconds": mid.get("release_seconds"),
            "pressure_below_critical": mid.get("pressure_below_critical"),
            "l4_pressure_below_critical": mid.get("l4_pressure_below_critical"),
            "tcp_active_per_ip_limit": mid.get("tcp_active_per_ip_limit"),
            "pressure": (mid.get("state") or {}).get("pressure"),
            "l4_pressure": (mid.get("state") or {}).get("l4Pressure"),
            "new_requests": held_row(mid.get("new_requests")),
            "held_new_requests": {
                proto: {
                    "success": value.get("success"),
                    "attempted": value.get("attempted"),
                    "connection_alive": value.get("connection_alive"),
                    "held_progress_bytes": value.get("held_progress_bytes"),
                    "errors": value.get("errors", [])[:3],
                }
                for proto, value in (mid.get("held_new_requests") or {}).items()
            },
            "held": {
                proto: {
                    "alive": value.get("alive"),
                    "bytes_delta": value.get("bytes_delta"),
                    "status": value.get("status"),
                    "error": value.get("error"),
                }
                for proto, value in (mid.get("accepted_progress") or {}).items()
            },
            "physical": physical_summary(mid.get("physical")),
        },
        "last_resort": data.get("last_resort"),
        "normal_completion": data.get("normal_completion"),
        "held_completion": data.get("held_completion"),
        "cache_reclaim": data.get("cache_reclaim"),
        "preflight": data.get("preflight"),
        "final": {
            "pressure": (data.get("final") or {}).get("pressure"),
            "l4_pressure": (data.get("final") or {}).get("l4Pressure"),
            "shed": (data.get("final") or {}).get("shed"),
            "physical": physical_summary(data.get("final")),
        },
        "terminal": {
            "current_bytes": int(terminal.get("memory.current", 0) or 0),
            "peak_bytes": int(terminal.get("memory.peak", 0) or 0),
            "events": event_counters(terminal.get("memory.events", "")),
            "psi_memory": terminal.get("memory.pressure"),
            "unit": terminal.get("unit"),
        },
        "cleanup_seconds": data.get("cleanup_seconds"),
    }


def main():
    parser = argparse.ArgumentParser()
    parser.add_argument("root", type=pathlib.Path)
    parser.add_argument("--output", type=pathlib.Path, required=True)
    args = parser.parse_args()
    rows = [
        run_row(path)
        for path in sorted(args.root.glob("acceptance-*.json"))
        if not path.name.endswith((".terminal.json", ".start.json"))
        and path.name != "acceptance-summary.json"
    ]
    args.output.write_text(json.dumps({"runs": rows}, indent=2))


if __name__ == "__main__":
    main()
