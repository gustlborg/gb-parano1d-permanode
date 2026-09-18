# Daily database backup

A consistent online copy of the permanode database via SQLite's backup
API (Python stdlib, no `sqlite3` CLI needed), integrity-checked, kept for
seven days under `/var/lib/permanode/backup/`. Copy the backups off the
machine yourself - a backup on the same disk does not survive that disk.

```sh
sudo install -m 0755 permanode-backup.py /usr/local/sbin/permanode-backup
sudo install -m 0644 permanode-backup.service permanode-backup.timer /etc/systemd/system/
sudo install -d -o permanode -g permanode -m 0750 /var/lib/permanode/backup
sudo systemctl daemon-reload
sudo systemctl enable --now permanode-backup.timer
sudo systemctl start permanode-backup.service && sudo journalctl -u permanode-backup -n 2
```

Restore: stop the service, replace `permanode.sqlite3` with a copy, remove
any stale `permanode.sqlite3-wal`/`-shm`, start the service.
