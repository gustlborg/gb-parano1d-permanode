#!/usr/bin/env python3
"""Watchdog for a parano1d-permanode host: checks the services, the node,
the indexer's lag, the public site and the disk, and reports changes over
Telegram (or just the journal if no bot is configured). Python 3 stdlib only.

Configuration comes from the environment (see permanode-watchdog.conf.example,
loaded by the systemd unit via EnvironmentFile)."""
import json, os, shutil, subprocess, sys, time, urllib.request, urllib.parse
from datetime import datetime, timezone

def env(name, default):
    v = os.environ.get(name, "").strip()
    return v if v else default

NODE_RPC = env("NODE_RPC", "http://127.0.0.1:9601")
PERMANODE_URL = env("PERMANODE_URL", "http://127.0.0.1:8420")
PUBLIC_URL = env("PUBLIC_URL", "")
SERVICES = [s.strip() for s in env("SERVICES", "parano1d,parano1d-permanode,caddy").split(",") if s.strip()]
DISK_PATH = env("DISK_PATH", "/var/lib/permanode")
DISK_WARN_PCT = int(env("DISK_WARN_PCT", "80"))
LAG_WARN_BLOCKS = int(env("LAG_WARN_BLOCKS", "3"))
STALE_MINUTES = int(env("STALE_MINUTES", "6"))
MEM_WARN_MB = int(env("MEM_WARN_MB", "200"))
HEARTBEAT_HOUR = int(env("HEARTBEAT_HOUR", "9"))
STATE_FILE = env("STATE_FILE", os.path.join(os.environ.get("STATE_DIRECTORY", "/var/lib/permanode-watchdog"), "state.json"))
TOKEN = env("TELEGRAM_BOT_TOKEN", "")
CHAT = env("TELEGRAM_CHAT_ID", "")
HOST = os.uname().nodename

def http_json(url, data=None, timeout=15):
    # A descriptive User-Agent: proxies with bot protection (Cloudflare's
    # Bot Fight Mode, for one) block Python's default one outright.
    headers = {"User-Agent": "permanode-watchdog/1.0"}
    if data:
        headers["Content-Type"] = "application/json"
    req = urllib.request.Request(url, data=data, headers=headers)
    with urllib.request.urlopen(req, timeout=timeout) as r:
        return json.load(r)

def rpc(method):
    body = json.dumps({"jsonrpc": "2.0", "id": 1, "method": method, "params": []}).encode()
    return http_json(NODE_RPC, body)["result"]

def check():
    problems, info = [], {}
    for s in SERVICES:
        state = subprocess.run(["systemctl", "is-active", s], capture_output=True, text=True).stdout.strip()
        if state != "active":
            problems.append(f"service {s} is {state}")
    try:
        node_tip = rpc("paranoid_blockCount"); info["node_tip"] = node_tip
    except Exception as e:
        node_tip = None; problems.append(f"node RPC unreachable ({type(e).__name__})")
    try:
        stats = http_json(f"{PERMANODE_URL}/api/v1/stats"); tip = stats.get("last_processed_height")
        info["indexed_tip"] = tip; info["gaps"] = stats.get("gaps")
        if node_tip is not None and tip is not None and node_tip - tip > LAG_WARN_BLOCKS:
            problems.append(f"indexer lags {node_tip - tip} blocks behind the node (indexed {tip}, node {node_tip})")
        blocks = http_json(f"{PERMANODE_URL}/api/v1/blocks?limit=1")
        if blocks:
            age = time.time() - blocks[0]["timestamp"]; info["last_block_age_s"] = int(age)
            if age > STALE_MINUTES * 60:
                problems.append(f"no new block for {int(age // 60)} min (node stalled or network quiet)")
    except Exception as e:
        problems.append(f"permanode API unreachable ({type(e).__name__})")
    if PUBLIC_URL:
        try:
            pub = http_json(f"{PUBLIC_URL}/api/v1/stats", timeout=20)
            if info.get("indexed_tip") is not None and abs(pub.get("last_processed_height", 0) - info["indexed_tip"]) > 2:
                problems.append("public site serves a different tip than the local API")
        except Exception as e:
            problems.append(f"public site unreachable: {PUBLIC_URL} ({type(e).__name__})")
    try:
        du = shutil.disk_usage(DISK_PATH); pct = round(du.used * 100 / du.total); info["disk_pct"] = pct
        if pct > DISK_WARN_PCT:
            problems.append(f"disk {pct}% full at {DISK_PATH}")
    except Exception as e:
        problems.append(f"disk check failed ({type(e).__name__})")
    try:
        avail_kb = next(int(l.split()[1]) for l in open("/proc/meminfo") if l.startswith("MemAvailable"))
        info["mem_avail_mb"] = avail_kb // 1024
        if avail_kb // 1024 < MEM_WARN_MB:
            problems.append(f"only {avail_kb // 1024} MB memory available")
    except Exception:
        pass
    return problems, info

def notify(text):
    print(text)
    if not TOKEN or not CHAT:
        return
    data = urllib.parse.urlencode({"chat_id": CHAT, "text": text, "disable_web_page_preview": "true"}).encode()
    try:
        urllib.request.urlopen(f"https://api.telegram.org/bot{TOKEN}/sendMessage", data=data, timeout=20).read()
    except Exception as e:
        print(f"telegram send failed: {e}", file=sys.stderr)

def main():
    try:
        state = json.load(open(STATE_FILE))
    except Exception:
        state = {"problems": [], "heartbeat_date": ""}
    problems, info = check()
    summary = ", ".join(f"{k}={v}" for k, v in info.items())
    new = [p for p in problems if p not in state["problems"]]
    gone = [p for p in state["problems"] if p not in problems]
    if new:
        notify(f"\U0001F534 {HOST}: " + "; ".join(new) + (f"\n({summary})" if summary else ""))
    if gone:
        notify(f"\U0001F7E2 {HOST}: resolved: " + "; ".join(gone) + (f"\n({summary})" if summary else ""))
    today = datetime.now(timezone.utc).strftime("%Y-%m-%d")
    if datetime.now().hour == HEARTBEAT_HOUR and state.get("heartbeat_date") != today:
        notify(("✅" if not problems else "⚠️") + f" {HOST} daily: " + (f"{len(problems)} open problem(s)" if problems else "all checks OK") + f"\n({summary})")
        state["heartbeat_date"] = today
    if not new and not gone:
        print(("OK" if not problems else f"{len(problems)} known problem(s)") + f" ({summary})")
    state["problems"] = problems
    os.makedirs(os.path.dirname(STATE_FILE), exist_ok=True)
    json.dump(state, open(STATE_FILE, "w"))

if __name__ == "__main__":
    main()
