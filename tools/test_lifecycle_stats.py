"""Synthetic, content-free regression fixtures for lifecycle-stats.py."""
import importlib.util
import json
from pathlib import Path
import tempfile
import unittest

SPEC = importlib.util.spec_from_file_location("lifecycle_stats", Path(__file__).with_name("lifecycle-stats.py"))
stats = importlib.util.module_from_spec(SPEC)
SPEC.loader.exec_module(stats)


class LifecycleStatsTests(unittest.TestCase):
    def test_debug_fields_and_escapes(self):
        self.assertEqual(stats.fields(r'key=Some("sha256:abc") id=None count=Some(2) flag=true name="a \"b\" c"'),
                         {"key": "sha256:abc", "id": None, "count": 2, "flag": True, "name": 'a "b" c'})
        with self.assertRaises(ValueError):
            stats.fields('id=Some("torn')

    def test_percentiles_and_empty_denominators(self):
        self.assertEqual(stats.percentile([10, 0], 90), 9)
        self.assertIsNone(stats.distribution([])["p50"])
        self.assertIsNone(stats.calls_table({})["calls_per_response"])

    def test_log_pairing_is_per_fingerprint_and_cutoff_is_explicit(self):
        rows = [
            '00:00:00 OpenAI websocket request transport="websocket" mode="full" history_message_count=100 prompt_cache_key_fingerprint=Some("a")',
            '00:00:01 OpenAI websocket request transport="websocket" mode="full" history_message_count=200 prompt_cache_key_fingerprint=Some("b")',
            '00:00:02 OpenAI terminal response transport="websocket" request_mode="full" response_status="completed" input_tokens_present=true input_tokens=100 cached_tokens=80 output_tokens=20 prompt_cache_key_fingerprint=Some("a")',
            '00:00:03 OpenAI terminal response transport="websocket" request_mode="full" response_status="completed" input_tokens_present=true input_tokens=200 cached_tokens=190 output_tokens=30 prompt_cache_key_fingerprint=Some("b")',
            '00:00:04 OpenAI websocket request transport="websocket" mode="incremental" history_message_count=1200 prompt_cache_key_fingerprint=Some("a")',
            '00:00:06 OpenAI terminal response transport="websocket" request_mode="incremental" response_status="completed" input_tokens_present=true input_tokens=300 cached_tokens=290 output_tokens=40 function_calls=2 first_event_ms=Some(10) prompt_cache_key_fingerprint=Some("a")',
            '00:06:00 OpenAI websocket request transport="websocket" mode="incremental" history_message_count=1202 prompt_cache_key_fingerprint=Some("a")',
            '00:06:01 OpenAI continuation invalidated reason="reconnect" socket_generation=1',
            '00:06:02 OpenAI websocket connected connect_ms=25',
        ]
        text = "\n".join('2026-11-11T' + row[:8] + 'Z  INFO span{profile=p/m}: zevria_provider::turn: ' + row[9:] for row in rows) + "\n"
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "log"
            path.write_text(text)
            result = stats.analyze_log(path)
            self.assertEqual(result["openai_fingerprints"], 2)
            self.assertEqual(result["openai"]["request_to_terminal_ms"]["count"], 3)
            self.assertEqual(result["openai"]["request_to_terminal_ms"]["p50"], 2000)
            self.assertEqual(result["gaps"]["included_ms"]["p50"], 2000)
            self.assertEqual(result["gaps"]["excluded"], 1)
            self.assertEqual(result["gaps"]["included_ms_by_next_history_items"][">=1200"]["count"], 1)
            self.assertEqual(result["openai"]["incremental_after_300_seconds"], 1)
            self.assertAlmostEqual(result["openai"]["reported_usage"]["overall"]["reported_cached_input_fraction"], 560 / 600)
            self.assertEqual(result["openai"]["calls"]["calls_per_tool_calling_response"], 2)
            self.assertEqual(result["openai"]["responses_without_tool_telemetry"], 2)
            self.assertEqual(result["openai"]["continuation_invalidations_global"], {"reconnect": 1})
            self.assertEqual(result["openai"]["connect_ms"]["p50"], 25)
            self.assertEqual(stats.analyze_log(path, max_gap_seconds=None)["gaps"]["excluded"], 0)
            self.assertEqual(path.read_text(), text)

    def test_error_prose_is_not_a_request_and_missing_fingerprint_is_unattributed(self):
        text = ('2026-11-11T00:00:00Z ERROR zevria_provider::turn: recovery failed error="OpenAI HTTP request failed"\n'
                '2026-11-11T00:00:01Z INFO zevria_provider::turn: OpenAI HTTP request transport="http" prompt_cache_key_fingerprint=None\n')
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "log"
            path.write_text(text)
            result = stats.analyze_log(path)
            self.assertEqual(result["events"]["OpenAI HTTP request"], 1)
            self.assertEqual(result["openai_fingerprints"], 0)
            self.assertEqual(result["warnings"]["unattributed_openai_event"], 1)

    def test_anthropic_cache_denominator_includes_creation_and_read(self):
        text = ('2026-11-11T00:00:00Z INFO run{profile=p/m}: zevria_provider::anthropic: '
                'Anthropic terminal response provider=p model=m input_tokens=10 cache_creation=20 cache_read=70 '
                'output_tokens=30 tool_uses=3 request_ms=120 first_event_ms=Some(15)\n')
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "log"
            path.write_text(text)
            result = stats.analyze_log(path)["anthropic_profiles"]["p/m"]
            self.assertEqual(result["canonical_input_tokens"], 100)
            self.assertEqual(result["reported_cached_input_fraction"], 0.7)
            self.assertEqual(result["calls"]["calls"], 3)

    def test_date_filter_is_inclusive_start_exclusive_end(self):
        text = ('2026-11-11T00:00:00Z INFO target: OpenAI continuation invalidated reason="start"\n'
                '2026-11-11T00:00:01Z INFO target: OpenAI continuation invalidated reason="end"\n')
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "log"
            path.write_text(text)
            result = stats.analyze_log(path, stats.timestamp("2026-11-11T00:00:00Z"), stats.timestamp("2026-11-11T00:00:01Z"))
            self.assertEqual(result["openai"]["continuation_invalidations_global"], {"start": 1})

    def test_native_replays_flatten_once_and_never_scan_tool_arguments(self):
        def replay(protocol, items):
            return {"zevria_provider_replay": {"version": 1, "provider": protocol,
                    "source_profile": {"provider": "synthetic", "model": "fixed"}, "items": items}}
        records = [
            replay("openai.responses", [{"type": "message"}]),
            {"zevria_batch": {"version": 1, "records": [
                replay("openai.responses", [{"type": "function_call", "arguments": {"type": "function_call"}}, {"type": "function_call"}]),
                replay("anthropic.messages", [{"type": "tool_use"}]),
            ]}},
            {"zevria_compaction": {"replacement": replay("openai.responses", [{"type": "function_call"}])}},
        ]
        text = "".join(json.dumps(record) + "\n" for record in records)
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "session.jsonl"
            path.write_text(text)
            result = stats.analyze_transcripts(Path(directory))
            self.assertEqual(result["totals"]["logical_records"], 4)
            self.assertEqual(result["totals"]["batch_envelopes"], 1)
            openai = result["by_protocol"]["openai.responses"]
            self.assertEqual(openai["responses"], 2)
            self.assertEqual(openai["calls"], 2)
            self.assertEqual(openai["histogram"], {"0": 1, "2": 1})
            self.assertEqual(result["by_protocol"]["anthropic.messages"]["calls"], 1)
            self.assertEqual(path.read_text(), text)

    def test_torn_and_nested_batches_contribute_no_children(self):
        nested = {"zevria_batch": {"version": 1, "records": [{}, {"zevria_batch": {"version": 1, "records": [{}]}}]}}
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "session.jsonl"
            text = json.dumps(nested) + '\n{"zevria_batch":{"version":1,"records":[{}'
            path.write_text(text)
            result = stats.analyze_transcripts(Path(directory))
            self.assertEqual(result["totals"].get("logical_records", 0), 0)
            self.assertEqual(result["warnings"], {"malformed_record": 1, "incomplete_final_line": 1})
            self.assertEqual(path.read_text(), text)

    def test_valid_final_record_without_newline_is_counted(self):
        with tempfile.TemporaryDirectory() as directory:
            path = Path(directory) / "session.jsonl"
            path.write_text('{}')
            result = stats.analyze_transcripts(Path(directory))
            self.assertEqual(result["totals"]["logical_records"], 1)
            self.assertEqual(result["warnings"], {"missing_final_newline": 1})

    def test_batch_rejects_headers_and_unknown_versions(self):
        for batch in ({"version": 2, "records": [{}]},
                      {"version": 1, "records": [{"zevria_session_mode": {}}]},
                      {"version": 1, "records": []}):
            with self.assertRaises(ValueError):
                stats.flatten({"zevria_batch": batch})

    def test_root_and_child_discovery_excludes_other_state(self):
        with tempfile.TemporaryDirectory() as directory:
            root = Path(directory)
            for name in ("sessions/root.jsonl", "subsessions/root/child.jsonl", "agent-runs/log.jsonl"):
                path = root / name
                path.parent.mkdir(parents=True, exist_ok=True)
                path.write_text("{}\n")
            self.assertEqual(len(stats.transcript_paths(root)), 2)
            self.assertEqual(stats.transcript_paths(root), stats.transcript_paths(root / "sessions"))


if __name__ == "__main__":
    unittest.main()
