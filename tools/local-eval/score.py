#!/usr/bin/env python3
"""Aggregate auditable local-evaluation trial records.

A completed ACP conversation is not a task pass.  Only an explicit verifier
status of ``pass`` or ``fail`` contributes to the pass-rate denominator.
"""

from __future__ import annotations

import argparse
import json
import math
import statistics
from pathlib import Path
from typing import Any, Iterable, Sequence

OUTCOMES = ("pass", "fail", "timeout", "infra", "incomplete")


def _outcome(trial: dict[str, Any]) -> str:
    runner_status = trial.get("status")
    verifier = trial.get("verifier")
    if runner_status == "timeout":
        return "timeout"
    if runner_status != "completed":
        return "infra"
    if isinstance(verifier, dict):
        status = verifier.get("status")
        if status in OUTCOMES:
            return status
    return "incomplete"


def _wall_time(trial: dict[str, Any]) -> float | None:
    value = trial.get("wall_time_seconds")
    if not isinstance(value, (int, float)) or isinstance(value, bool):
        return None
    try:
        wall_time = float(value)
    except (OverflowError, ValueError):
        return None
    if math.isfinite(wall_time) and wall_time >= 0:
        return wall_time
    return None


def aggregate_trials(trials: Iterable[dict[str, Any]]) -> dict[str, Any]:
    """Return counts, coverage, pass rate, and wall-time summary for trials."""
    counts = {outcome: 0 for outcome in OUTCOMES}
    wall_times: list[float] = []
    for trial in trials:
        counts[_outcome(trial)] += 1
        wall_time = _wall_time(trial)
        if wall_time is not None:
            wall_times.append(wall_time)

    total = sum(counts.values())
    valid = counts["pass"] + counts["fail"]
    wall_summary: dict[str, Any] = {"count": len(wall_times)}
    if wall_times:
        wall_summary.update(
            {
                "min": min(wall_times),
                "max": max(wall_times),
                "mean": statistics.fmean(wall_times),
                "median": statistics.median(wall_times),
            }
        )
    else:
        wall_summary.update({"min": None, "max": None, "mean": None, "median": None})

    return {
        "schema_version": 1,
        "total_trials": total,
        "counts": counts,
        "valid_trials": valid,
        "valid_coverage": valid / total if total else 0.0,
        "pass_rate": counts["pass"] / valid if valid else None,
        "wall_time_seconds": wall_summary,
    }


def _trial_files(inputs: Sequence[str]) -> list[Path]:
    files: set[Path] = set()
    for raw_input in inputs:
        path = Path(raw_input)
        if path.is_dir():
            files.update(candidate.resolve() for candidate in path.rglob("trial.json"))
        elif path.is_file():
            files.add(path.resolve())
        else:
            raise FileNotFoundError(f"input path does not exist: {path}")
    return sorted(files)


def load_trials(inputs: Sequence[str]) -> list[dict[str, Any]]:
    """Load one trial object from each JSON file or directory supplied."""
    trials: list[dict[str, Any]] = []
    for path in _trial_files(inputs):
        try:
            value = json.loads(path.read_text(encoding="utf-8"))
        except (OSError, UnicodeError, json.JSONDecodeError) as error:
            raise ValueError(f"failed to load trial JSON {path}: {error}") from error
        if not isinstance(value, dict):
            raise ValueError(f"trial file must contain a JSON object: {path}")
        trials.append(value)
    return trials


def _readable(summary: dict[str, Any]) -> str:
    counts = summary["counts"]
    wall = summary["wall_time_seconds"]
    pass_rate = summary["pass_rate"]
    pass_text = "n/a" if pass_rate is None else f"{pass_rate:.1%}"
    wall_text = "n/a" if wall["count"] == 0 else f"{wall['mean']:.3f}s mean"
    lines = [
        "Local evaluation summary",
        f"trials: {summary['total_trials']} (valid: {summary['valid_trials']}, coverage: {summary['valid_coverage']:.1%})",
        "outcomes: " + ", ".join(f"{name}={counts[name]}" for name in OUTCOMES),
        f"pass rate (pass / (pass + fail)): {pass_text}",
        f"wall time: {wall_text}",
    ]
    return "\n".join(lines)


def main(argv: Sequence[str] | None = None) -> int:
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("inputs", nargs="+", help="trial.json files or directories containing them")
    parser.add_argument("--output", default="summary.json", help="summary JSON output path")
    args = parser.parse_args(argv)

    try:
        summary = aggregate_trials(load_trials(args.inputs))
    except (OSError, ValueError, json.JSONDecodeError) as error:
        parser.error(str(error))
    output = Path(args.output)
    output.parent.mkdir(parents=True, exist_ok=True)
    output.write_text(json.dumps(summary, indent=2, sort_keys=True) + "\n", encoding="utf-8")
    print(_readable(summary))
    return 0


if __name__ == "__main__":
    raise SystemExit(main())
