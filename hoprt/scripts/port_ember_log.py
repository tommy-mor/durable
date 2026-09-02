#!/usr/bin/env python3
"""Absorb a bare ember being file into a durable hop tape.

Old lines are the event body:

    {"type":"perception","id":"...","content":"...","timestamp":...}

Hop's log is the envelope the reducer replays:

    {"seq":0,"ts_ms":...,"event":{...}}

The being types (init, thought, perception, response, declaration, vote,
compaction) become the tape. Secrets on init are dropped. Re-running on an
already-enveloped file unwraps and re-stamps seq.

    python3 hoprt/scripts/port_ember_log.py \\
        ~/programming/family/ember.jsonl \\
        --data ~/programming/ember2/hop-data
"""

from __future__ import annotations

import argparse
import json
import shutil
import sys
from pathlib import Path


BEING = {
    "init",
    "thought",
    "perception",
    "response",
    "declaration",
    "vote",
    "compaction",
}


def unwrap(obj: dict) -> dict | None:
    if not isinstance(obj, dict):
        return None
    if "event" in obj and "seq" in obj and "type" not in obj:
        inner = obj["event"]
        return inner if isinstance(inner, dict) else None
    return obj


def clean(event: dict) -> dict | None:
    t = event.get("type")
    if t not in BEING:
        return None
    out = dict(event)
    if t == "init":
        out.pop("api_key", None)
    return out


def ts_ms(event: dict, seq: int) -> int:
    raw = event.get("timestamp")
    if isinstance(raw, bool):
        return seq
    if isinstance(raw, (int, float)) and raw >= 0:
        return int(raw)
    return seq


def port_lines(src: Path) -> list[dict]:
    records = []
    with src.open() as f:
        for lineno, line in enumerate(f, 1):
            line = line.strip()
            if not line:
                continue
            try:
                obj = json.loads(line)
            except json.JSONDecodeError as e:
                raise SystemExit(f"{src}:{lineno}: {e}") from e
            event = unwrap(obj)
            if event is None:
                continue
            event = clean(event)
            if event is None:
                continue
            seq = len(records)
            records.append(
                {"seq": seq, "ts_ms": ts_ms(event, seq), "event": event}
            )
    return records


def write_jsonl(path: Path, records: list[dict]) -> None:
    path.parent.mkdir(parents=True, exist_ok=True)
    tmp = path.with_suffix(path.suffix + ".tmp")
    with tmp.open("w") as f:
        for rec in records:
            f.write(json.dumps(rec, ensure_ascii=False, separators=(",", ":")) + "\n")
    tmp.replace(path)


def main() -> None:
    p = argparse.ArgumentParser(description=__doc__)
    p.add_argument("source", type=Path, help="bare (or already enveloped) ember jsonl")
    p.add_argument(
        "--data",
        type=Path,
        required=True,
        help="hopd --data dir; writes log.jsonl and clears proj/ so hop replays",
    )
    p.add_argument(
        "--rewrite-source",
        action="store_true",
        help="replace the source file with the enveloped tape (keeps a .bare copy)",
    )
    args = p.parse_args()
    src = args.source.expanduser().resolve()
    if not src.is_file():
        raise SystemExit(f"no such file: {src}")

    records = port_lines(src)
    if not records:
        raise SystemExit(f"{src}: no being events")

    data = args.data.expanduser().resolve()
    log = data / "log.jsonl"
    write_jsonl(log, records)
    proj = data / "proj"
    if proj.exists():
        shutil.rmtree(proj)

    if args.rewrite_source:
        bare = src.with_name(src.name + ".bare")
        if not bare.exists():
            shutil.copy2(src, bare)
        write_jsonl(src, records)

    types = {}
    for rec in records:
        t = rec["event"].get("type", "?")
        types[t] = types.get(t, 0) + 1
    summary = " ".join(f"{k}={v}" for k, v in sorted(types.items()))
    print(f"wrote {len(records)} records → {log}", file=sys.stderr)
    print(summary, file=sys.stderr)


if __name__ == "__main__":
    main()
