#!/usr/bin/env python3
"""Read-only lifecycle statistics. Standard-library only; never emits prompt text."""
import argparse
from collections import Counter, defaultdict
from datetime import datetime, timezone
import json
from pathlib import Path
import re
import sys

EVENTS = (
    "OpenAI websocket request", "OpenAI HTTP request", "OpenAI terminal response",
    "OpenAI continuation invalidated", "Anthropic terminal response",
    "OpenAI websocket connected", "model round summary",
)
EVENT = re.compile(r"(?:^|:\s)(" + "|".join(map(re.escape, EVENTS)) + r")(?:\s|$)")
KEY = re.compile(r"([a-zA-Z_][a-zA-Z_0-9]*)=")
TIMESTAMP = re.compile(r"^\s*(\d{4}-\d\d-\d\dT\S+)")


def timestamp(text):
    value = datetime.fromisoformat(text.replace("Z", "+00:00"))
    if value.tzinfo is None:
        raise ValueError("timestamp requires a timezone")
    return value.astimezone(timezone.utc)


def fields(text):
    """Scan tracing values, including escaped strings and Debug Option values."""
    result = {}
    position = 0
    while match := KEY.search(text, position):
        start = position = match.end()
        depth = 0
        quoted = escaped = False
        while position < len(text):
            char = text[position]
            if escaped:
                escaped = False
            elif quoted and char == "\\":
                escaped = True
            elif char == '"':
                quoted = not quoted
            elif not quoted:
                if char == "(":
                    depth += 1
                elif char == ")":
                    depth -= 1
                elif char.isspace() and depth == 0:
                    break
            position += 1
        if quoted or depth:
            raise ValueError("incomplete tracing value")
        raw = text[start:position]
        if raw.startswith("Some(") and raw.endswith(")"):
            raw = raw[5:-1]
        if raw == "None":
            value = None
        else:
            try:
                value = json.loads(raw)
            except (ValueError, json.JSONDecodeError):
                value = raw
        result[match[1]] = value
    return result


def percentile(values, percent):
    """Linear interpolation at (n - 1) * p, with no rounding of samples."""
    if not values:
        return None
    ordered = sorted(values)
    index = (len(ordered) - 1) * percent / 100
    lower = int(index)
    upper = min(lower + 1, len(ordered) - 1)
    return ordered[lower] + (ordered[upper] - ordered[lower]) * (index - lower)


def distribution(values):
    return {"count": len(values), "sum": sum(values),
            "mean": sum(values) / len(values) if values else None,
            **{f"p{p}": percentile(values, p) for p in (10, 50, 90, 99)},
            "max": max(values) if values else None}


def ratio(numerator, denominator):
    return numerator / denominator if denominator else None


def history_band(count):
    if count < 200:
        return "<200"
    if count < 600:
        return "200-599"
    if count < 1200:
        return "600-1199"
    return ">=1200"


def input_band(count):
    if count < 50000:
        return "<50000"
    if count < 100000:
        return "50000-99999"
    if count < 200000:
        return "100000-199999"
    if count < 300000:
        return "200000-299999"
    return ">=300000"


def output_band(count):
    if count < 50:
        return "<50"
    if count < 150:
        return "50-149"
    if count < 500:
        return "150-499"
    return ">=500"


def duration_table(rows, group, predicate=lambda row: True):
    samples = defaultdict(list)
    for row in rows:
        if row.get("duration_ms") is not None and predicate(row):
            samples[group(row)].append(row["duration_ms"])
    return {key: distribution(values) for key, values in sorted(samples.items())}


def usage_table(rows):
    result = {}
    groups = defaultdict(list)
    for row in rows:
        if row.get("input_tokens_present", True) and isinstance(row.get("input_tokens"), int):
            groups[f'{row.get("transport", "unknown")}/{row.get("request_mode", "unknown")}'].append(row)
            groups["overall"].append(row)
    for key, values in sorted(groups.items()):
        tokens = sum(row["input_tokens"] for row in values)
        cached = sum(row.get("cached_tokens", 0) for row in values)
        result[key] = {"responses": len(values), "input_tokens": tokens, "cached_tokens": cached,
                       "reported_cached_input_fraction": ratio(cached, tokens)}
    return result


def calls_table(counts):
    calls = sum(count * occurrences for count, occurrences in counts.items())
    responses = sum(counts.values())
    tool_responses = sum(n for count, n in counts.items() if count)
    multi = sum(n for count, n in counts.items() if count > 1)
    return {"responses": responses, "tool_calling_responses": tool_responses,
            "calls": calls, "calls_per_response": ratio(calls, responses),
            "calls_per_tool_calling_response": ratio(calls, tool_responses),
            "multi_call_responses": multi, "multi_call_fraction_of_tool_calling": ratio(multi, tool_responses),
            "histogram": {str(key): value for key, value in sorted(counts.items())}}


def analyze_log(path, start=None, end=None, max_gap_seconds=300):
    pending = {}
    last_terminal = {}
    rows, gaps, idle_full, idle_incremental = [], [], [], []
    invalidations, events, warnings = Counter(), Counter(), Counter()
    request_modes = Counter()
    fingerprints = set()
    seen_requests = set()
    mid_session_full = 0
    anthropic = defaultdict(list)
    connects, first_events = [], []
    round_samples = defaultdict(list)
    first = last = None
    # Snapshot the input size: an active log may grow while it is being read.
    with path.open("rb") as source:
        size = path.stat().st_size
        while source.tell() < size:
            line = source.readline(size - source.tell())
            if not line.endswith(b"\n"):
                warnings["incomplete_final_line"] += 1
                break
            try:
                line = line.decode("utf-8")
                match = EVENT.search(line)
                if not match:
                    continue
                values = fields(line[match.end():])
                # Require fields after the exact message, not an error mentioning it.
                if not values:
                    continue
                time_match = TIMESTAMP.match(line)
                when = timestamp(time_match[1]) if time_match else None
            except (UnicodeError, ValueError):
                warnings["malformed_log_event"] += 1
                continue
            if (start or end) and when is None:
                warnings["untimed_filtered_event"] += 1
                continue
            if when is not None and ((start and when < start) or (end and when >= end)):
                continue
            event = match[1]
            events[event] += 1
            if when is not None:
                first = min(first, when) if first else when
                last = max(last, when) if last else when
            if event == "OpenAI continuation invalidated":
                invalidations[str(values.get("reason", "unknown"))] += 1
                continue
            if event == "OpenAI websocket connected":
                if isinstance(values.get("connect_ms"), (int, float)):
                    connects.append(values["connect_ms"])
                continue
            if event == "model round summary":
                for key in ("prepare_us", "model_ms", "commit_response_us", "tools_ms", "commit_results_us"):
                    if isinstance(values.get(key), (int, float)):
                        round_samples[key].append(values[key])
                if values.get("tool_calls") == 1 and isinstance(values.get("tools_ms"), (int, float)):
                    round_samples["single_tool_tools_ms"].append(values["tools_ms"])
                continue
            if event == "Anthropic terminal response":
                anthropic[(str(values.get("provider")), str(values.get("model")))].append(values)
                continue
            fingerprint = values.get("prompt_cache_key_fingerprint")
            if not fingerprint:
                warnings["unattributed_openai_event"] += 1
                continue
            fingerprints.add(fingerprint)
            if event in ("OpenAI websocket request", "OpenAI HTTP request"):
                request_modes[f'{values.get("transport", "unknown")}/{values.get("mode", "unknown")}'] += 1
                if fingerprint in seen_requests and values.get("mode") == "full":
                    mid_session_full += 1
                seen_requests.add(fingerprint)
                if fingerprint in pending:
                    warnings["request_without_terminal"] += 1
                pending[fingerprint] = (when, values)
                previous = last_terminal.pop(fingerprint, None)
                if when and previous:
                    gap = (when - previous).total_seconds() * 1000
                    if gap < 0:
                        warnings["negative_gap"] += 1
                    else:
                        gaps.append((gap, values.get("history_message_count", 0)))
                        (idle_incremental if values.get("mode") == "incremental" else idle_full).append(gap / 1000)
                continue
            request = pending.pop(fingerprint, None)
            row = dict(values)
            row["duration_ms"] = None
            if request and when and request[0]:
                elapsed = (when - request[0]).total_seconds() * 1000
                if elapsed >= 0:
                    row["duration_ms"] = elapsed
                else:
                    warnings["negative_duration"] += 1
            else:
                warnings["terminal_without_timed_request"] += 1
            if when and values.get("response_status") == "completed":
                last_terminal[fingerprint] = when
            if isinstance(values.get("first_event_ms"), (int, float)):
                first_events.append(values["first_event_ms"])
            rows.append(row)
    warnings["request_without_terminal"] += len(pending)
    active_gaps = [(gap, count) for gap, count in gaps if max_gap_seconds is None or gap <= max_gap_seconds * 1000]
    gap_groups = defaultdict(list)
    for gap, count in active_gaps:
        gap_groups[history_band(count)].append(gap)
    successful = [row for row in rows if row.get("response_status") == "completed"]
    model_ms = sum(row["duration_ms"] for row in successful if row["duration_ms"] is not None)
    gap_ms = sum(gap for gap, _ in active_gaps)
    anthropic_report = {}
    for (provider, model), values in sorted(anthropic.items()):
        inputs = sum(row.get("input_tokens", 0) + row.get("cache_creation", 0) + row.get("cache_read", 0) for row in values)
        cached = sum(row.get("cache_read", 0) for row in values)
        measured = [row["request_ms"] for row in values if isinstance(row.get("request_ms"), (int, float))]
        calls = Counter(row["tool_uses"] for row in values if isinstance(row.get("tool_uses"), int))
        anthropic_report[f"{provider}/{model}"] = {
            "responses": len(values), "canonical_input_tokens": inputs,
            "reported_cached_input_fraction": ratio(cached, inputs),
            "request_ms": distribution(measured),
            "first_event_ms": distribution([row["first_event_ms"] for row in values if isinstance(row.get("first_event_ms"), (int, float))]),
            "calls": calls_table(calls),
            "responses_without_tool_telemetry": len(values) - sum(calls.values()),
        }
    return {
        "snapshot_bytes": size, "first_event": first.isoformat() if first else None,
        "last_event": last.isoformat() if last else None, "events": dict(events),
        "openai_fingerprints": len(fingerprints), "warnings": dict(warnings),
        "openai": {
            "successful_responses": len(successful),
            "requests_by_mode": dict(request_modes),
            "mid_session_full_requests": mid_session_full,
            "transport_request_ms": distribution([row["request_ms"] for row in successful if isinstance(row.get("request_ms"), (int, float))]),
            "full_requests_with_prior_terminal": len(idle_full),
            "request_to_terminal_ms": distribution([row["duration_ms"] for row in successful if row["duration_ms"] is not None]),
            "duration_ms_by_output_tokens": duration_table(successful, lambda row: output_band(row.get("output_tokens", 0))),
            "duration_ms_by_input_for_output_below_150": duration_table(successful, lambda row: input_band(row.get("input_tokens", 0)), lambda row: row.get("output_tokens", 0) < 150 and row.get("input_tokens_present", True)),
            "duration_ms_by_mode": duration_table(successful, lambda row: f'{row.get("transport")}/{row.get("request_mode")}'),
            "uncached_input_tokens_by_mode": {
                mode: distribution([row.get("input_tokens", 0) - row.get("cached_tokens", 0) for row in successful if row.get("request_mode") == mode and row.get("input_tokens_present", True)])
                for mode in ("full", "incremental")
            },
            "first_event_ms": distribution(first_events), "connect_ms": distribution(connects),
            "reported_usage": usage_table(successful),
            "calls": calls_table(Counter(row["function_calls"] for row in successful if isinstance(row.get("function_calls"), int))),
            "responses_without_tool_telemetry": sum("function_calls" not in row for row in successful),
            "continuation_invalidations_global": dict(invalidations),
            "reconnect_invalidations_per_1000_requests": 1000 * ratio(invalidations["reconnect"], events["OpenAI websocket request"] + events["OpenAI HTTP request"]) if events["OpenAI websocket request"] + events["OpenAI HTTP request"] else None,
            "idle_seconds_before_full": distribution(idle_full),
            "full_replays_after_60_seconds": sum(gap >= 60 for gap in idle_full),
            "idle_seconds_before_incremental": distribution(idle_incremental),
            "incremental_after_300_seconds": sum(gap > 300 for gap in idle_incremental),
        },
        "gaps": {"max_included_seconds": max_gap_seconds, "excluded": len(gaps) - len(active_gaps),
                 "all_ms": distribution([gap for gap, _ in gaps]),
                 "included_ms": distribution([gap for gap, _ in active_gaps]),
                 "included_ms_by_next_history_items": {key: distribution(values) for key, values in sorted(gap_groups.items())}},
        "aggregate_accounted_time": {"model_hours": model_ms / 3600000, "gap_hours": gap_ms / 3600000,
                                     "model_fraction": ratio(model_ms, model_ms + gap_ms)},
        "anthropic_profiles": anthropic_report,
        "rounds": {key: distribution(values) for key, values in sorted(round_samples.items())},
    }


def transcript_paths(directory):
    """Accept .zevria, its sessions/ directory, or a supplied transcript directory."""
    if (directory / "sessions").is_dir():
        return sorted(set((directory / "sessions").glob("*.jsonl")) | set((directory / "subsessions").glob("*/*.jsonl")))
    paths = set(directory.glob("*.jsonl"))
    if directory.name == "sessions":
        paths.update((directory.parent / "subsessions").glob("*/*.jsonl"))
    else:
        paths.update(directory.glob("*/*.jsonl"))
    return sorted(paths)


def flatten(record):
    if not isinstance(record, dict):
        raise ValueError("record is not an object")
    if "zevria_batch" not in record:
        return [record]
    batch = record["zevria_batch"]
    if not isinstance(batch, dict) or batch.get("version") != 1:
        raise ValueError("unsupported batch")
    items = batch.get("records")
    forbidden = {"zevria_batch", "zevria_session_models", "zevria_session_mode"}
    if not isinstance(items, list) or not items or any(not isinstance(item, dict) or forbidden.intersection(item) for item in items):
        raise ValueError("invalid batch children")
    return items


def analyze_transcripts(directory):
    counts = defaultdict(Counter)
    tool_names = defaultdict(Counter)
    multi_call_names = defaultdict(Counter)
    totals, warnings = Counter(), Counter()
    paths = transcript_paths(directory)
    for path in paths:
        totals["files"] += 1
        with path.open("rb") as source:
            size = path.stat().st_size
            totals["bytes"] += size
            while source.tell() < size:
                line = source.readline(size - source.tell())
                totals["physical_lines"] += 1
                final_without_newline = not line.endswith(b"\n")
                try:
                    record = json.loads(line)
                    items = flatten(record)
                    if final_without_newline:
                        warnings["missing_final_newline"] += 1
                    if "zevria_batch" in record:
                        totals["batch_envelopes"] += 1
                    totals["logical_records"] += len(items)
                    for item in items:
                        replay = item.get("zevria_provider_replay")
                        if replay is None:
                            continue
                        if not isinstance(replay, dict) or replay.get("version") != 1 or not isinstance(replay.get("items"), list):
                            warnings["unsupported_replay"] += 1
                            continue
                        protocol = replay.get("provider")
                        kind = {"openai.responses": "function_call", "anthropic.messages": "tool_use"}.get(protocol)
                        if kind is None:
                            warnings["unsupported_replay_provider"] += 1
                            continue
                        profile = replay.get("source_profile", {})
                        key = (protocol, profile.get("provider"), profile.get("model"))
                        calls = [native for native in replay["items"] if isinstance(native, dict) and native.get("type") == kind]
                        counts[key][len(calls)] += 1
                        names = [native.get("name", "<unknown>") for native in calls]
                        tool_names[protocol].update(name if isinstance(name, str) else "<invalid>" for name in names)
                        if len(calls) > 1:
                            multi_call_names[protocol][tuple(sorted({name if isinstance(name, str) else "<invalid>" for name in names}))] += 1
                except (UnicodeError, ValueError, TypeError, AttributeError, RecursionError):
                    warnings["incomplete_final_line" if final_without_newline else "malformed_record"] += 1
    protocols = defaultdict(Counter)
    for (protocol, _, _), values in counts.items():
        protocols[protocol].update(values)
    return {"totals": dict(totals), "warnings": dict(warnings),
            "by_protocol": {key: {**calls_table(values), "tool_names": dict(tool_names[key]),
                                  "multi_call_tool_sets": [{"tools": names, "responses": count} for names, count in sorted(multi_call_names[key].items())]}
                            for key, values in sorted(protocols.items())},
            "by_profile": [{"protocol": protocol, "provider": provider, "model": model, **calls_table(values)}
                           for (protocol, provider, model), values in sorted(counts.items(), key=lambda pair: str(pair[0]))]}


def main():
    parser = argparse.ArgumentParser(description=__doc__)
    parser.add_argument("log", type=Path)
    parser.add_argument("sessions", type=Path)
    parser.add_argument("--start", type=timestamp, help="inclusive RFC3339 timestamp")
    parser.add_argument("--end", type=timestamp, help="exclusive RFC3339 timestamp")
    parser.add_argument("--max-gap-seconds", type=float, default=300, help="accounted gap cutoff; 0 includes all (default: 300)")
    args = parser.parse_args()
    if args.max_gap_seconds < 0 or (args.start and args.end and args.start >= args.end):
        parser.error("invalid time range or gap cutoff")
    if not args.log.is_file() or not args.sessions.is_dir():
        parser.error("log must be a file and sessions must be a directory")
    try:
        report = {"method": {"version": 1, "units": "milliseconds unless labeled", "gap_history": "next request history_message_count",
                             "pairing": "latest request to terminal per non-null fingerprint; failed attempts may have no terminal",
                             "invalidations": "global only; historical rows lack fingerprints",
                             "transcript_time_filter": "none; --start/--end apply only to logs",
                             "aggregate_time": "sum across sessions, not elapsed wall time; gaps include tools, storage and user idle"},
                  "log": analyze_log(args.log, args.start, args.end, args.max_gap_seconds or None),
                  "transcripts": analyze_transcripts(args.sessions)}
    except OSError:
        print("cannot read an input path", file=sys.stderr)
        return 1
    json.dump(report, sys.stdout, indent=2, sort_keys=True)
    print()
    return 0


if __name__ == "__main__":
    sys.exit(main())
