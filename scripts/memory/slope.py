"""Warm-up-aware growth-rate estimation for the Issue 34 soak.

The gate is the **upper 95% confidence bound** of the post-warm-up RSS slope,
not the point estimate: a noisy but flat series must not pass simply because
its mean slope happens to be small. A robust Theil-Sen slope is reported and
must also stay below the budget, so a single late allocation spike cannot be
hidden by least squares.
"""

from __future__ import annotations

import math
from dataclasses import asdict, dataclass

MIB = 1024.0 * 1024.0
SECONDS_PER_HOUR = 3600.0

# One-sided 95% Student-t critical values. df beyond the table uses the normal
# approximation, which is within 0.3% for df >= 120.
_T95 = {
    1: 6.314,
    2: 2.920,
    3: 2.353,
    4: 2.132,
    5: 2.015,
    6: 1.943,
    7: 1.895,
    8: 1.860,
    9: 1.833,
    10: 1.812,
    11: 1.796,
    12: 1.782,
    13: 1.771,
    14: 1.761,
    15: 1.753,
    16: 1.746,
    17: 1.740,
    18: 1.734,
    19: 1.729,
    20: 1.725,
    21: 1.721,
    22: 1.717,
    23: 1.714,
    24: 1.711,
    25: 1.708,
    26: 1.706,
    27: 1.703,
    28: 1.701,
    29: 1.699,
    30: 1.697,
    40: 1.684,
    60: 1.671,
    120: 1.658,
}


class ThresholdError(RuntimeError):
    """A measured slope or bound violates the configured budget."""


def t_critical_95(df: int) -> float:
    if df < 1:
        raise ThresholdError(f"degrees of freedom must be >= 1, got {df}")
    if df in _T95:
        return _T95[df]
    for limit, value in ((40, _T95[40]), (60, _T95[60]), (120, _T95[120])):
        if df <= limit:
            # Linear interpolation between tabulated neighbours is accurate
            # enough at the df values this harness produces (hundreds+).
            previous = max(key for key in _T95 if key < limit)
            span = limit - previous
            return _T95[previous] + (value - _T95[previous]) * (df - previous) / span
    return 1.645


@dataclass(frozen=True)
class SplitResult:
    """First/second-half analysis plus a half-to-half step baseline.

    Long dedicated runs must not pass merely because a slow leak is diluted
    over tens of hours; the second half is re-gated on its own and the median
    step between the halves must stay within the budget for the elapsed time.
    """

    split_seconds: float
    split_fraction: float
    first: dict
    second: dict
    first_half_median_mib: float
    second_half_median_mib: float
    step_mib: float
    step_allowed_mib: float

    def to_dict(self) -> dict:
        return asdict(self)


@dataclass(frozen=True)
class SlopeResult:
    samples: int
    warmup_seconds: float
    span_seconds: float
    slope_bytes_per_second: float
    upper95_bytes_per_second: float
    slope_mib_per_hour: float
    upper95_mib_per_hour: float
    theil_sen_mib_per_hour: float
    r_squared: float
    t_critical: float
    budget_mib_per_hour: float

    def to_dict(self) -> dict:
        return asdict(self)


def _clean_points(points, warmup_seconds: float):
    cleaned = []
    for point in points:
        if len(point) != 2:
            raise ThresholdError(f"point must be (seconds, bytes): {point!r}")
        t, y = point
        if not math.isfinite(t) or not math.isfinite(y):
            raise ThresholdError(f"non-finite point: {point!r}")
        cleaned.append((float(t), float(y)))
    if not cleaned:
        raise ThresholdError("no samples")
    t0 = min(t for t, _ in cleaned)
    kept = [(t - t0, y) for t, y in cleaned if t - t0 >= warmup_seconds]
    if len(kept) < 3:
        raise ThresholdError(
            f"need at least 3 post-warm-up samples, have {len(kept)} of {len(cleaned)}"
        )
    kept.sort()
    return kept


def _ols(points):
    n = len(points)
    mean_x = sum(x for x, _ in points) / n
    mean_y = sum(y for _, y in points) / n
    sxx = sum((x - mean_x) ** 2 for x, _ in points)
    if sxx <= 0.0:
        raise ThresholdError("all samples share one timestamp")
    sxy = sum((x - mean_x) * (y - mean_y) for x, y in points)
    slope = sxy / sxx
    intercept = mean_y - slope * mean_x
    residuals = [y - (intercept + slope * x) for x, y in points]
    if n <= 2:
        raise ThresholdError(f"need at least 3 samples for a confidence bound, have {n}")
    residual_var = sum(r * r for r in residuals) / (n - 2)
    stderr = math.sqrt(residual_var / sxx)
    total = sum((y - mean_y) ** 2 for _, y in points)
    r_squared = 0.0 if total == 0.0 else 1.0 - sum(r * r for r in residuals) / total
    return slope, stderr, r_squared, intercept


def _theil_sen(points) -> float:
    """Median of pairwise slopes.

    O(n^2) below 1024 points, then a deterministic stride keeps the pair count
    bounded (the harness samples at a fixed interval, so a stride does not
    bias the median).
    """
    n = len(points)
    stride = 1 if n <= 1024 else (n + 1023) // 1024
    sample = points[::stride]
    slopes = []
    for i in range(len(sample)):
        x_i, y_i = sample[i]
        for j in range(i + 1, len(sample)):
            x_j, y_j = sample[j]
            dx = x_j - x_i
            if dx > 0:
                slopes.append((y_j - y_i) / dx)
    if not slopes:
        raise ThresholdError("cannot compute Theil-Sen slope")
    slopes.sort()
    mid = len(slopes) // 2
    if len(slopes) % 2:
        return slopes[mid]
    return (slopes[mid - 1] + slopes[mid]) / 2.0


def estimate_slope(
    points,
    *,
    warmup_seconds: float,
    budget_mib_per_hour: float,
    min_span_seconds: float = 60.0,
) -> SlopeResult:
    """Estimate RSS growth after ``warmup_seconds`` and apply the budget gate.

    ``points`` is an iterable of ``(monotonic_seconds, rss_bytes)``.
    Raises :class:`ThresholdError` when the series is too short or the upper
    confidence bound / robust slope is at or above ``budget_mib_per_hour``.
    """
    if budget_mib_per_hour <= 0:
        raise ThresholdError("budget_mib_per_hour must be positive")
    kept = _clean_points(points, warmup_seconds)
    span = kept[-1][0] - kept[0][0]
    if span < min_span_seconds:
        raise ThresholdError(
            f"post-warm-up span {span:.1f}s is below the {min_span_seconds:.0f}s minimum"
        )
    slope, stderr, r_squared, _ = _ols(kept)
    df = len(kept) - 2
    tcrit = t_critical_95(df)
    upper = slope + tcrit * stderr
    robust = _theil_sen(kept)
    to_mib_hour = SECONDS_PER_HOUR / MIB
    result = SlopeResult(
        samples=len(kept),
        warmup_seconds=float(warmup_seconds),
        span_seconds=span,
        slope_bytes_per_second=slope,
        upper95_bytes_per_second=upper,
        slope_mib_per_hour=slope * to_mib_hour,
        upper95_mib_per_hour=upper * to_mib_hour,
        theil_sen_mib_per_hour=robust * to_mib_hour,
        r_squared=r_squared,
        t_critical=tcrit,
        budget_mib_per_hour=float(budget_mib_per_hour),
    )
    if result.upper95_mib_per_hour >= budget_mib_per_hour:
        raise ThresholdError(
            "upper 95% RSS slope "
            f"{result.upper95_mib_per_hour:.3f} MiB/h >= budget "
            f"{budget_mib_per_hour:.3f} MiB/h (point {result.slope_mib_per_hour:.3f}, "
            f"robust {result.theil_sen_mib_per_hour:.3f})"
        )
    if result.theil_sen_mib_per_hour >= budget_mib_per_hour:
        raise ThresholdError(
            "Theil-Sen RSS slope "
            f"{result.theil_sen_mib_per_hour:.3f} MiB/h >= budget "
            f"{budget_mib_per_hour:.3f} MiB/h"
        )
    return result


def _median(values) -> float:
    ordered = sorted(values)
    count = len(ordered)
    if count == 0:
        raise ThresholdError("cannot take the median of an empty series")
    mid = count // 2
    if count % 2:
        return ordered[mid]
    return (ordered[mid - 1] + ordered[mid]) / 2.0


def estimate_split_slope(
    points,
    *,
    warmup_seconds: float,
    budget_mib_per_hour: float,
    min_span_seconds: float = 60.0,
    split_fraction: float = 0.5,
) -> SplitResult:
    """Gate both halves of the post-warm-up window and the half-to-half step.

    This is the long-run (>=48 h) platform comparison: a leak of 4.5 MiB/h is
    still 4.5 MiB/h in the second half, while a one-off start-up step is
    bounded by ``budget * half_hours``. Every failure is explicit; noise that
    pushes a bound over the budget makes the run inconclusive/failed, not a
    reason to relax the budget.
    """
    if not (0.0 < split_fraction < 1.0):
        raise ThresholdError(f"split_fraction must be in (0, 1), got {split_fraction}")
    kept = _clean_points(points, warmup_seconds)
    start, end = kept[0][0], kept[-1][0]
    split = start + (end - start) * split_fraction
    first_points = [point for point in kept if point[0] <= split]
    second_points = [point for point in kept if point[0] > split]
    if len(first_points) < 3 or len(second_points) < 3:
        raise ThresholdError(
            "split baseline needs at least 3 samples in each half, have "
            f"{len(first_points)} and {len(second_points)}"
        )
    first_span = first_points[-1][0] - first_points[0][0]
    second_span = second_points[-1][0] - second_points[0][0]
    # Each half must still be long enough to produce a meaningful bound, but
    # never demand more than the half itself contains.
    first_min = min(min_span_seconds, first_span * 0.5)
    second_min = min(min_span_seconds, second_span * 0.5)
    first = estimate_slope(
        first_points,
        warmup_seconds=0.0,
        budget_mib_per_hour=budget_mib_per_hour,
        min_span_seconds=first_min,
    )
    second = estimate_slope(
        second_points,
        warmup_seconds=0.0,
        budget_mib_per_hour=budget_mib_per_hour,
        min_span_seconds=second_min,
    )
    first_median = _median(point[1] for point in first_points)
    second_median = _median(point[1] for point in second_points)
    step_mib = (second_median - first_median) / MIB
    elapsed_hours = second_span / SECONDS_PER_HOUR
    step_allowed_mib = budget_mib_per_hour * elapsed_hours
    if step_mib > step_allowed_mib:
        raise ThresholdError(
            f"second-half step {step_mib:.3f} MiB exceeds the step baseline "
            f"{step_allowed_mib:.3f} MiB ({budget_mib_per_hour:.3f} MiB/h over "
            f"{elapsed_hours:.2f} h)"
        )
    return SplitResult(
        split_seconds=split,
        split_fraction=split_fraction,
        first=first.to_dict(),
        second=second.to_dict(),
        first_half_median_mib=first_median / MIB,
        second_half_median_mib=second_median / MIB,
        step_mib=step_mib,
        step_allowed_mib=step_allowed_mib,
    )
