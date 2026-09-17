# Watchdog

Checks every two minutes: services active, node RPC reachable, indexer
lag behind the node, time since the last block, public site reachable and
in sync, disk and memory. Reports every change (new problem, resolved
problem) and one daily heartbeat over Telegram; without a bot it logs to
the journal only.

```sh
sudo install -m 0755 permanode-watchdog.py /usr/local/sbin/permanode-watchdog
sudo install -m 0640 permanode-watchdog.conf.example /etc/permanode-watchdog.conf   # then edit
sudo install -m 0644 permanode-watchdog.service permanode-watchdog.timer /etc/systemd/system/
sudo systemctl daemon-reload
sudo systemctl enable --now permanode-watchdog.timer
sudo systemctl start permanode-watchdog.service && journalctl -u permanode-watchdog -n 3
```
