"""Test-only declarations embedded before fixture bodies; no reader starts on import."""
import json
import os
import queue
import sys
import threading


def expand_fixture_template(template, artifact, generation):
    """Copy JSON values, substituting paths before serialization and IDs by field."""
    def expand(value, field=None):
        if isinstance(value, dict):
            return {key: expand(item, key) for key, item in value.items()}
        if isinstance(value, list):
            return [expand(item) for item in value]
        if isinstance(value, str):
            if value == "ARTIFACT":
                # Do not interpret anything in the inserted path as a placeholder.
                return artifact
            if field == "toolCallId":
                return value.replace("GENERATION", str(generation))
        return value

    return expand(template)


class JsonLineReader:
    """One stdin owner, FIFO delivery, and bounded waits (including negative checks).

    The daemon only uses raw reads, never Python's buffered stdin locks, so a
    fixture can exit while the host still holds its pipe open. Its lifetime is
    confined to this process; construct exactly one reader and do not also read
    sys.stdin in the fixture body.
    """
    def __init__(self):
        self._messages = queue.Queue()
        self._failure = None
        threading.Thread(target=self._read_stdin, name="fixture-stdin", daemon=True).start()

    def _read_stdin(self):
        pending = b""
        try:
            descriptor = sys.stdin.fileno()
            while True:
                chunk = os.read(descriptor, 4096)
                if not chunk:
                    if pending:
                        raise ValueError(f"truncated JSON line at EOF: {pending!r}")
                    raise EOFError("host closed connection")
                pending += chunk
                while b"\n" in pending:
                    line, pending = pending.split(b"\n", 1)
                    self._messages.put(("message", json.loads(line.decode("utf-8"))))
        except Exception as error:
            self._messages.put(("error", error))

    def _next(self, timeout):
        if self._failure is not None:
            raise self._failure
        try:
            kind, value = self._messages.get(timeout=timeout)
        except queue.Empty:
            return "timeout", None
        if kind == "error":
            # EOF and failures are sticky: a subsequent negative check cannot
            # mistake a dead reader for successful silence.
            self._failure = value
            raise value
        return kind, value

    def receive(self, timeout):
        kind, value = self._next(timeout)
        if kind == "timeout":
            raise TimeoutError(f"no JSON response within {timeout} seconds")
        return value

    def assert_no_response(self, timeout, context="unexpected response"):
        kind, value = self._next(timeout)
        if kind != "timeout":
            raise AssertionError(f"{context}: {value!r}")
