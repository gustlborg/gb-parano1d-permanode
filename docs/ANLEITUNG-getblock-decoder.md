# Anleitung: `getBlock`-Fallback-Decoder für den Permanode-Indexer

Stand 17.09.2026 18:35. Geschrieben für eine andere Claude-Session, die das
umsetzt. Vorarbeit (Ursachenanalyse + Proof-of-Concept) ist erledigt und
verifiziert, siehe Abschnitt 2 — nicht neu untersuchen, sondern umsetzen.

## 1. Ziel und Hintergrund (3 Sätze)

Der Node (v1.1.0) speichert jeden Block-Body, aber `paranoid_getBlockDetails`
liefert für jeden **Zwischenblock eines Mehrblock-Commits** (Reorg mit ≥2
neuen Blöcken oder zwei Blöcke wenige Sekunden auseinander) `retained: null`,
weil der RPC intern einen Accessor benutzt, der Blöcke mit
„recursive suffix marker"-Terminal bewusst ausblendet. Betroffen sind 2,2 %
aller Höhen (jeder ~45. Block), permanent, im Normalbetrieb. Vollständige
Analyse mit Code-Zeilen, Log-Belegen und Repro:
`~/Claude/Parano1d/node-issue-17-09/REPORT.md` (englisch, für den
Node-Entwickler) — bei Bedarf lesen, nicht nacherzählen lassen.

**Was wir bauen:** Wenn `getBlockDetails` innerhalb des 42-Block-Serving-
Fensters `retained: null` liefert, holt der Indexer den rohen Block über
`paranoid_getBlock` (Hex, liefert den Body sehr wohl) und dekodiert ihn
selbst in exakt dieselbe `RetainedBlockInfo`-Struktur, die `store_block`
heute aus dem JSON bekommt. Damit verschwinden die `ingest_gaps` für
Marker-Blöcke vollständig; `store_block` bleibt bis auf den Sonderfall
„Body zu bereits bekannter Blockzeile nachtragen" (Abschnitt 3.5) unverändert.

## 2. Was schon verifiziert ist (nicht wiederholen)

* **Decoder-Ansatz = die Node-Crates selbst einbinden** (`noid_chain` und
  transitiv `noid_tx`, `noid_poseidon2b`, `noid_core`), und die Ableitung
  der Transaktionsfelder **1:1 aus `noid_rpc/src/server.rs:1838-2010`
  (v1.1.0) spiegeln**. Eigenes Nachbauen von Poseidon2b-Txids, bech32m und
  der PagedSpend-Gruppierung ist verworfen — zu fehleranfällig.
* **Proof-of-Concept liegt fertig und geprüft in `docs/decoder-poc/`**
  (`Cargo.toml` + `src/main.rs`, baubar als eigenes Cargo-Projekt). Er hat
  gegen den laufenden Node **40 von 40 Blöcken** mit vorhandenen Details
  **byteidentisch** (tiefer JSON-Vergleich des kompletten
  `transactions`-Arrays inkl. txid, creation_id, page_hashes, owner sowie
  aller Kopffelder) reproduziert und 2 Marker-Höhen gefüllt. Abdeckung im
  Vergleich: 30 User-TX, mehrseitige TX (3 Seiten), Coinbase-only-Blöcke.
  **Nicht live abgedeckt:** `development_payout` (chainweit bisher 1×
  gesehen) — der Codepfad ist identisch zum RPC gespiegelt, muss aber durch
  die Selbstprüfung aus Abschnitt 4.7 abgesichert werden.
* **Fixtures für Unit-Tests liegen in `indexer/tests/fixtures/`**:
  - `block_108552.*` (3 TX, je 1 Seite), `block_108569.*` (3 TX, eine mit
    3 Seiten), `block_108574.*` (nur Coinbase): jeweils
    `getBlock.json` (Hex-String) + `getBlockDetails.json` (Soll).
  - `block_108537_marker.*`: ein echter Marker-Block —
    `getBlock.json`, `getBlockHeader.json`, `getBlockDetails.json`
    (zeigt `retained: null`) und `decoded.expected.json` (Soll-Ausgabe des
    PoC-Decoders für genau diesen Block).
* **Hash-Bindung ist Pflicht und funktioniert:**
  `noid_chain::block_header::block_id(&block.header)` (Poseidon2b) hex ==
  `header.hash` aus `getBlockDetails`/`getBlockHeader`. `getBlock` selbst
  prüft nämlich nicht, ob der Body kanonisch ist (`mdbx_store.rs:2040`).
* **Build-Hürde bekannt und gelöst:** `noid_chain` zieht `libmdbx` →
  `mdbx-sys` → bindgen (libclang). Auf dieser Maschine fehlen die Clang-
  Resource-Header (`stdarg.h`), Workaround ohne sudo:
  `BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/gcc/x86_64-linux-gnu/13/include"`
  beim `cargo build`. Sauber wäre `apt install clang` (Nutzer fragen, nicht
  selbst sudo). Kompletter Build aller noid-Crates: ~16 s Wall / ~1,5 min
  CPU. Toolchain: Node verlangt Rust 1.96.0, hier ist 1.96.0 Standard.
* Lizenz: Node-Crates sind Apache-2.0, Permanode (vorläufig) AGPL-3.0 —
  kompatibel, kein Problem.

## 3. Architekturentscheidungen (so umsetzen)

1. **Abhängigkeit:** in `indexer/Cargo.toml` (nicht in `core`, die API
   braucht den Decoder nicht):
   ```toml
   noid_chain = { git = "https://git.parano1d.org/ignotusnemo/parano1d.git", tag = "v1.1.0" }
   hex = "0.4"
   ```
   Für die Entwicklung darf vorübergehend der Pfad
   `/home/gustavo/Claude/Parano1d/src/parano1d-v1.1.0/noid_chain` verwendet
   werden (so baut der PoC), **aber vor Abschluss muss die Git-Variante
   nachweislich gebaut worden sein** (Remote ist anonym erreichbar, Tag
   `v1.1.0` = Commit `8f3195e`). Cargo löst das Paket `noid_chain` im
   Workspace des Git-Repos selbst auf. `Cargo.lock` mit einchecken.
2. **Decoder als eigenes Modul** `indexer/src/decode.rs` mit genau einer
   öffentlichen Funktion:
   ```rust
   pub fn decode_retained_block(bytes: &[u8], expected_height: u64, expected_hash_hex: &str)
       -> anyhow::Result<RetainedBlockInfo>
   ```
   Rückgabe ist die **bestehende** `RetainedBlockInfo` aus `indexer/src/rpc.rs`
   (Feldtypen dort sind `u32`/`u64` — Werte aus den Crates (`u16`, `u32`
   `slot_index`) hochcasten). Der Code des PoC ist die Vorlage; Struktur
   und Reihenfolge der Ableitung (Coinbase → optional Dev-Payout → User-
   Gruppen, `alloc_cursor`-Logik, Abschlussprüfung
   `alloc_cursor == header.alloc_counter`) **exakt** so übernehmen.
3. **Fallback an beiden Stellen im Indexer**, an denen heute
   `retained: None` zu `record_gap` führt: `ingest_height` (neue Höhe) und
   der Reorg-Zweig in `recheck_height` (dort ist es seit der Analyse sogar
   der Regelfall). Ablauf:
   1. `details = getBlockDetails(h)`; falls `retained.is_some()` → wie bisher.
   2. sonst `raw = getBlock(h)`; falls `null` → `record_gap` mit neuer Notiz
      `"no body via getBlockDetails nor getBlock (outside serving window)"`.
   3. sonst `decode_retained_block(raw, h, &details.header.hash)`; bei
      `Err` → **nicht abstürzen**, `error!`-Log + `record_gap` mit Notiz
      `"getBlock body could not be decoded: <err>"` (fail-safe, nächster
      Zyklus versucht es erneut, weil die Höhe ohne Body bleibt).
   4. bei `Ok(retained)` → `BlockDetailsInfo { header, retained: Some(..) }`
      an `store_block` geben, `body_source = 'getblock'`.
4. **Herkunft festhalten (Feldliste-Prinzip „Ingest-Metadaten"):** neue
   Spalte `blocks.body_source TEXT` mit Werten `'details'` | `'getblock'` |
   `NULL` (kein Body). Migration in `core/src/db.rs::init_schema` als
   idempotentes `ALTER TABLE blocks ADD COLUMN body_source TEXT`, geschützt
   durch `PRAGMA table_info(blocks)`-Check — die Live-DB
   (`run/permanode.sqlite3`) läuft unter systemd und darf nicht neu angelegt
   werden. API/Frontend dürfen das Feld anzeigen (optional, klein).
5. **Nachholen offener Lücken:** `ingest_gaps`-Zeilen mit
   `height > tip − 42` sind noch reparierbar. Beim Start und in jedem
   Poll-Zyklus (billig, es sind wenige) für jede solche Höhe den Fallback
   fahren. Dazu muss `store_block` einen Sonderfall bekommen: Blockzeile
   mit gleichem Hash existiert bereits, aber `body_captured = 0` und jetzt
   liegt ein Body vor → Transaktionsdetails einfügen, `body_captured = 1`,
   `body_source` setzen. Heute bricht `store_block` bei bekanntem Hash
   sofort ab — das ist der einzige Punkt, an dem `store_block` angefasst
   wird. Lücken **nicht löschen**, sondern append-only auflösen: neue
   Spalten `ingest_gaps.resolved_at TEXT` und `ingest_gaps.resolution TEXT`
   (z. B. `"recovered via getBlock"`), Migration wie unter 4. Der
   `/api/v1/gaps`-Endpunkt und die Anzeige sollten offene von aufgelösten
   Lücken unterscheiden.
6. **Wichtig für die Bewertung alter Lücken:** alles älter als 42 Blöcke
   ist endgültig weg (auch der Node prunt Marker-Bodies nach dem Fenster).
   Die 20 Lücken aus der Analyse (107885 … 108338) bleiben offen — das ist
   erwartet, nicht nachbessern.

## 4. Schritte in dieser Reihenfolge

### 4.1 Vorbereitung
- `REVISION`-/Memory-Regeln des Projekts gelten (kein `git push` ohne
  Zuruf, lokal committen frei; Live-Zustand ist die Wahrheit).
- Beide Dienste laufen: `parano1d-permanode.service` (Indexer) und
  `parano1d-permanode-api.service`. Für den Umbau reicht es, den Indexer
  nach dem Build neu zu starten (`systemctl --user restart parano1d-permanode`).
- PoC einmal selbst bauen und laufen lassen, um die Umgebung zu bestätigen:
  ```
  cd docs/decoder-poc
  BINDGEN_EXTRA_CLANG_ARGS="-I/usr/lib/gcc/x86_64-linux-gnu/13/include" cargo build --release
  ./target/release/decoder-poc 42          # Vergleich über die letzten 42 Höhen
  ./target/release/decoder-poc 1 <höhe>    # eine Höhe als JSON ausgeben
  ```
  Erwartung: „compared N … → N identical", plus 0–5 gefüllte Marker-Höhen.
  (`docs/decoder-poc/target/` danach löschen oder in `.gitignore` — nicht
  einchecken.)

### 4.2 Abhängigkeit + Decoder-Modul
- `indexer/Cargo.toml` erweitern (Abschnitt 3.1), `indexer/src/decode.rs`
  aus dem PoC ableiten. Nur `noid_chain` und `hex` direkt referenzieren;
  `to_bech32()`/`live_outputs()` sind Methoden auf transitiv gezogenen
  Typen und brauchen keine direkte Abhängigkeit.
- `rpc.rs`: `get_block_raw(height) -> Result<Option<Vec<u8>>>`
  (`paranoid_getBlock`, `null` → `None`, sonst `hex::decode`).
- Den RPC-Strukturen in `rpc.rs` zusätzlich `Serialize` ableiten — das
  erlaubt im Test den JSON-Vergleich gegen die Fixtures.

### 4.3 Unit-Tests (`indexer/tests/decode_fixtures.rs`)
- Für `108552`, `108569`, `108574`: `decode_retained_block(getBlock-Bytes,
  h, details.header.hash)` als `serde_json::Value` serialisieren und mit
  `getBlockDetails.json["retained"]` **vollständig** vergleichen (nicht nur
  Txids). Die Fixture enthält drei Felder, die `RetainedBlockInfo` nicht
  hat: `reward_noid` (f64-Anzeigewert), `history_step_bytes`,
  `bundle_bytes` — vor dem Vergleich aus dem Soll entfernen, alles andere
  (Kopffelder + komplettes `transactions`-Array) muss gleich sein.
- Für `108537_marker`: gegen `decoded.expected.json` vergleichen; zusätzlich
  prüfen, dass ein falscher `expected_hash` einen `Err` ergibt und dass
  eine falsche Höhe einen `Err` ergibt.
- `cargo test -p parano1d-permanode-indexer` muss grün sein, bevor es
  live geht.

### 4.4 Indexer-Logik
- Fallback in `ingest_height` und `recheck_height` (Abschnitt 3.3),
  `store_block`-Sonderfall und Gap-Nachholen (3.5), `body_source` (3.4).
- Log-Zeilen, die man im Journal wiederfinden kann:
  `info!("height {h}: body recovered via getBlock ({n} tx)")`,
  `warn!("height {h}: gap recovered …")`, `error!` bei Decode-Fehlern.

### 4.5 Schema-Migrationen
- `blocks.body_source`, `ingest_gaps.resolved_at`, `ingest_gaps.resolution`
  — idempotent, beim Öffnen der DB. Vorher auf einer **Kopie** der
  Live-DB testen (`sqlite3 run/permanode.sqlite3 ".backup /home/gustavo/Claude/temp/permanode-migtest.sqlite3"`),
  nicht direkt an der laufenden Datei probieren.

### 4.6 Konfiguration
- Neuer Schalter in `permanode.toml` / `config.rs`:
  `getblock_fallback = true` (Default an) — damit ein Betreiber den
  Fallback abschalten kann, falls eine künftige Node-Version das
  Wire-Format ändert und der Decoder Fehler wirft.
- `indexer/permanode.example.toml` und README mitziehen. README bekommt
  zusätzlich den Build-Hinweis: „benötigt einen C-Compiler und libclang
  (`clang`), weil die Node-Crates eingebunden werden".

### 4.7 Selbstprüfung (empfohlen, klein)
- Schalter `decoder_selfcheck = true` (Default an): bei jedem Block, für
  den `getBlockDetails` einen Body liefert, zusätzlich `getBlock` holen,
  dekodieren und mit dem RPC-Ergebnis vergleichen (ein Loopback-Call mit
  ~0,5–2 KB, vernachlässigbar). Abweichung → `error!`-Log + Zähler
  `decoder_mismatches` in `indexer_state` hochzählen; `/api/v1/stats` kann
  ihn ausgeben. Das ist die dauerhafte Absicherung des nicht live
  getesteten Dev-Payout-Pfads und der Frühwarner für Wire-Format-Änderungen
  bei Node-Updates. Nach ein paar Wochen Null-Abweichung darf der Default
  auf `false` gehen.

### 4.8 Live-Verifikation (Pflicht, nicht nur Tests)
1. `cargo build --release` (mit `BINDGEN_EXTRA_CLANG_ARGS`, falls nötig),
   Indexer-Dienst neu starten, Journal beobachten:
   `journalctl --user -u parano1d-permanode -f`.
2. Warten, bis der Node den nächsten Marker-Block erzeugt (statistisch
   alle ~45 Blöcke ≈ 15 min; erkennbar im Node-Log
   `~/.parano1d/data/parano1d-node.log` an `blocks=2` bzw. `applied=2`,
   oder mit `python3 ~/Claude/Parano1d/node-issue-17-09/repro_retained_null.py 42`).
3. Für diese Höhe prüfen: `ingest_gaps` hat **keinen** neuen Eintrag,
   `blocks.body_source = 'getblock'`, `/api/v1/block/height/<h>` zeigt die
   Transaktionen, Frontend-Blockseite ebenfalls.
4. Nachholen: die beim Start noch im Fenster liegenden Lücken müssen
   `resolved_at` bekommen; Lücken älter als 42 Blöcke bleiben offen.
5. Selbstprüfung: nach ≥1 h Betrieb `decoder_mismatches = 0`.
6. Abschluss: Ergebnis (Höhen, Zeiten, Journal-Auszug) in die Projekt-
   Memory `project_parano1d_permanode.md` (Abschnitt „Gaps: Ursache
   geklärt") als „umgesetzt am …" nachtragen; Commit lokal.

## 5. Fallstricke

- **`getBlock` ohne Hash-Prüfung nie verwenden.** Während eines Reorgs
  können `getBlockDetails`-Header und `getBlock`-Body aus zwei
  verschiedenen Kettenständen stammen (zwei getrennte RPC-Aufrufe). Bei
  Hash-Mismatch: nicht speichern, kein Gap eintragen, nächster Zyklus.
- **Höhe 0 (Genesis)** hat keine Transaktionen und keinen Coinbase — der
  Decoder würde `missing coinbase` melden. Kommt praktisch nicht vor
  (unser Start-Height liegt bei 107822), aber `height == 0` explizit
  abfangen statt als Decode-Fehler zu loggen.
- **`slot_index` ist in den Crates `u32`**, im RPC-JSON und in unserer
  Struktur `u64` — `u64::from(...)`; nicht die Struktur ändern.
- **`creation_id` bleibt `TEXT` in der DB** (High-Bit-Sentinel-Problem,
  siehe Memory) — der Decoder liefert `u64`, `store_block` macht schon
  `.to_string()`; nichts umbauen.
- **`proof_class`-String** muss wörtlich `"B25 / m22"` / `"B255 / m24"`
  sein (so formatiert es der RPC, so vergleicht der Test).
- **Bindgen-Fehler `'stdarg.h' file not found`** → Abschnitt 2, Env-Var.
  Auf einer anderen GCC-Version das Verzeichnis anpassen
  (`ls /usr/lib/gcc/x86_64-linux-gnu/*/include/stdarg.h`).
- **Compile-Zeit/Target-Größe:** `target/` wächst um einige hundert MB;
  nicht in `docs/decoder-poc/` bauen und liegen lassen.
- **Kein Versuch, Lücken > 42 Blöcke zu füllen** — es gibt keinen RPC-Weg
  mehr; Zeit nicht damit verbrennen.
- **Nicht** die `noid_chain`-Storage-Schicht (MDBX) direkt öffnen, um an
  Bodies zu kommen — der Node hält die DB exklusiv, und es würde die
  Architektur „nur RPC, läuft neben jeder Node" brechen.

## 6. Referenzen

- Ursachenanalyse + Repro: `~/Claude/Parano1d/node-issue-17-09/REPORT.md`,
  `repro_retained_null.py`.
- Vorlage für die Feldableitung: `~/Claude/Parano1d/src/parano1d-v1.1.0/noid_rpc/src/server.rs:1838-2010`
  (`get_block_details`) — der PoC spiegelt genau diesen Abschnitt.
- Wire-Format (nur zum Verständnis, nicht nachbauen):
  `noid_chain/src/block.rs` (`Block::decode`, `BLOCK_WIRE_MARKER = 0xB3`),
  `noid_chain/src/wire.rs` (`BlockHeader`, little-endian),
  `noid_chain/src/consensus/paged_spend.rs` (Gruppierung),
  `noid_tx/src/paged_spend.rs` (`TxPage`, `PagedSpendFacts`).
- Bestehender Ingest-Pfad: `indexer/src/indexer.rs` (`ingest_height`,
  `recheck_height`, `store_block`), RPC-Typen `indexer/src/rpc.rs`,
  Schema `core/src/db.rs`.
