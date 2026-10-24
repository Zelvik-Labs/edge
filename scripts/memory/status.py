"""Strict ingestion of the node-status ``resourceGovernor`` snapshot.

At Edge 40fda4f the node uploads ``status_json`` over gRPC
(``NodeService.UpdateNodeStatus``) and exposes no HTTP/file endpoint. A fake
control-plane backend can capture the *actual* controller-reported JSON; this
module reads that artifact from a file or URL and validates the new Issue 34
observability fields.

Only the ``resourceGovernor`` subset is retained in the soak report, so host
identifiers and credentials are never echoed into CI logs. Missing fields are
hard errors, never zero. ``mi_process_info``/mimalloc RSS is never substituted
for the kernel ``/proc`` RSS used by the slope gate.
"""

from __future__ import annotations

import json
import os
import urllib.error
import urllib.request

DEFAULT_REQUIRED_FIELDS = (
    "workerReclaim.generation",
    "workerReclaim.requests",
    "workerReclaim.started",
    "workerReclaim.live",
    "workerReclaim.stopped",
    "workerReclaim.parkCollects",
    "workerReclaim.rateLimited",
    "workerReclaim.stopCollects",
    "workerReclaim.unhookedPingoraWorkers",
    "workerReclaim.coversAllWorkers",
    "cacheJanitor.cycles",
    "cacheJanitor.maintenanceCycles",
    "cacheJanitor.admissionSkips",
    "cacheJanitor.budgetPublishes",
    "cacheJanitor.rebuildsDeferred",
    "cacheJanitor.rebuildsApplied",
    "cacheJanitor.negativeEntriesRemoved",
    "cacheJanitor.workerCollectRequests",
    "metricValuesCache.entries",
    "metricValuesCache.estimatedBytes",
    "metricValuesCache.maxEntries",
    "metricValuesCache.maxEstimatedBytes",
    "metricValuesCache.evictedEntries",
    "metricsAggregatorBytes",
)


class StatusError(RuntimeError):
    """The node-status snapshot is missing, unreadable or incomplete."""


def _lookup(document, dotted: str):
    node = document
    for part in dotted.split("."):
        if not isinstance(node, dict) or part not in node:
            raise StatusError(f"node status missing required field {dotted!r}")
        node = node[part]
    return node


def _governor(document) -> dict:
    if not isinstance(document, dict):
        raise StatusError("node status root must be an object")
    if isinstance(document.get("resourceGovernor"), dict):
        return document["resourceGovernor"]
    for wrapper in ("status", "node", "status_json", "statusJson"):
        if wrapper in document:
            inner = document[wrapper]
            if isinstance(inner, str):
                try:
                    inner = json.loads(inner)
                except json.JSONDecodeError as exc:
                    raise StatusError(f"node status {wrapper} is not JSON: {exc}") from exc
            return _governor(inner)
    reports = document.get("reports")
    if isinstance(reports, list) and reports:
        return _governor(reports[-1])
    raise StatusError("node status has no resourceGovernor object")


def normalize(document, required=DEFAULT_REQUIRED_FIELDS) -> dict:
    governor = _governor(document)
    flat: dict[str, int | bool] = {}
    for field in required:
        value = _lookup(governor, field)
        if field.endswith("coversAllWorkers"):
            if not isinstance(value, bool):
                raise StatusError(f"resourceGovernor.{field} must be a boolean")
            flat[field] = value
            continue
        if isinstance(value, bool) or not isinstance(value, (int, float)):
            raise StatusError(f"resourceGovernor.{field} is not numeric: {value!r}")
        if value < 0:
            raise StatusError(f"resourceGovernor.{field} is negative: {value!r}")
        flat[field] = int(value)

    # Invariant wired by the integrator: the combined gauge includes the
    # metric-values cache estimate. Only asserted when both fields exist.
    if "metricsAggregatorBytes" in flat and "metricValuesCache.estimatedBytes" in flat:
        if flat["metricsAggregatorBytes"] < flat["metricValuesCache.estimatedBytes"]:
            raise StatusError(
                "resourceGovernor.metricsAggregatorBytes is smaller than "
                "metricValuesCache.estimatedBytes"
            )
    return flat


def load(source: str, required=DEFAULT_REQUIRED_FIELDS) -> dict:
    if source.startswith(("http://", "https://")):
        try:
            with urllib.request.urlopen(source, timeout=5.0) as response:
                payload = response.read()
        except (urllib.error.URLError, OSError) as exc:
            raise StatusError(f"cannot fetch node status {source}: {exc}") from exc
        try:
            document = json.loads(payload)
        except json.JSONDecodeError as exc:
            raise StatusError(f"node status {source} is not JSON: {exc}") from exc
        return normalize(document, required)

    if not os.path.isfile(source):
        raise StatusError(f"node status source does not exist: {source}")
    try:
        with open(source, "r", encoding="utf-8") as handle:
            lines = [line for line in handle if line.strip()]
    except OSError as exc:
        raise StatusError(f"cannot read node status {source}: {exc}") from exc
    if not lines:
        raise StatusError(f"node status file {source} is empty")
    # Support a JSONL capture (one status report per line) and a single object.
    for line in reversed(lines):
        try:
            document = json.loads(line)
        except json.JSONDecodeError:
            continue
        if isinstance(document, dict):
            return normalize(document, required)
    raise StatusError(f"node status file {source} has no complete JSON object")


def parse_required_fields(value: str) -> tuple[str, ...]:
    fields = tuple(part.strip() for part in value.split(",") if part.strip())
    if not fields:
        raise StatusError("--require-status-fields must name at least one field")
    return fields
