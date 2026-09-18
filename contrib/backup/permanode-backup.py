#!/usr/bin/env python3
"""Consistent online copy of the permanode database (SQLite backup API), daily, keep 7."""
import sqlite3, datetime, pathlib
src = pathlib.Path("/var/lib/permanode/permanode.sqlite3")
dst_dir = pathlib.Path("/var/lib/permanode/backup"); dst_dir.mkdir(exist_ok=True)
dst = dst_dir / f"permanode-{datetime.datetime.now():%Y%m%d-%H%M}.sqlite3"
with sqlite3.connect(f"file:{src}?mode=ro", uri=True) as s, sqlite3.connect(dst) as d:
    s.backup(d)
    d.execute("PRAGMA journal_mode=DELETE")  # single self-contained file, no -wal sidecar
ok = sqlite3.connect(f"file:{dst}?mode=ro", uri=True).execute("PRAGMA integrity_check").fetchone()[0]
if ok != "ok":
    dst.unlink(); raise SystemExit(f"backup integrity check failed: {ok}")
for old in sorted(dst_dir.glob("permanode-*.sqlite3"))[:-7]:
    old.unlink()
print(f"backup written: {dst} ({dst.stat().st_size // 1024} KB)")
