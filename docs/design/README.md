# Handoff: Parano1d Explorer — Redesign (Plain HTML/CSS/JS)

## Overview
Redesign des Parano1d Blockexplorers: dunkle, ruhige Oberfläche in der Bildsprache von parano1d.org,
mit einer **animierten Blockkette** oben auf dem Dashboard (links der Block im Bau / Mempool, rechts
die bereits gefundenen Blöcke; beim Blockfund wandert die Kette in ~3,4 s nach rechts).
Enthalten sind sechs Views: Dashboard, Block, Transaction, Address, Live Mempool, Rich list.

## About the Design Files
Die ursprüngliche Design-Referenz war ein interaktiver Prototyp (React-artige Laufzeit, Inline-Styles,
Mock-Daten) und ist nicht Teil dieses Repos. Zielumgebung ist **Plain HTML/CSS/JS mit plain CSS** —
Markup, Tokens und Verhalten sind in `frontend/site/` nachgebaut (`css/style.css`, `js/`).
`before-redesign.png` zeigt den Stand davor zum Vergleich.

## Fidelity
**Hi-fi.** Farben, Typografie, Abstände, Radien und Animationsparameter sind final und unten exakt
dokumentiert. Pixelgenau nachbauen. Alle Zahlenwerte im Prototyp sind Platzhalter aus Mock-Daten.

## Design Tokens

### Farben
| Token | Hex | Verwendung |
|---|---|---|
| `--bg` | `#050908` | Seitenhintergrund |
| `--bg-bar` | `#070d0c` | Header + Statusleiste |
| `--panel` | `#0a1211` | Karten, Tabellen-Container |
| `--panel-2` | `#0c1514` | Kennzahlen-Karten innerhalb einer Karte |
| `--block-bg` | `#081211` | Blockfläche (leere Zellen: `#0f1a18`) |
| `--block-bg-hot` | `#081312` | Mempool-Blockfläche |
| `--border` | `#16211f` | Karten-Rahmen |
| `--border-soft` | `#141f1d` | Trenner im Karten-Header |
| `--border-row` | `#101917` | Tabellenzeilen-Trenner |
| `--border-kv` | `#111b19` | Key-Value-Zeilen-Trenner |
| `--border-input` | `#1d2b28` | Inputs, Ghost-Buttons |
| `--border-block` | `#1b2a27` | Rahmen bestätigter Blöcke |
| `--border-block-hot` | `#2b6f60` | Rahmen Mempool-Block |
| `--accent` | `#3ee6b8` | Primär: Links, Aktiv-States, Blockfüllung |
| `--accent-hover` | `#7ff5d6` | Link-/Button-Hover |
| `--accent-2` | `#7be38b` | Positivwerte, coinbase-Tag-Text |
| `--tag-bg` | `#12291f` | coinbase-/Status-Tag-Hintergrund |
| `--text` | `#dfe7e4` | Primärtext |
| `--text-2` | `#b8c6c2` | Tabellen-Zellen |
| `--muted` | `#7d8f8a` | Sekundärtext, Nav inaktiv |
| `--muted-2` | `#6a7c77` | Labels in Karten |
| `--muted-3` | `#5d706b` | Tabellen-Kopf, Metazeilen |
| `--muted-4` | `#4d5f5a` | „next block in …" |
| `--arrow` | `#24423b` | Pfeile zwischen Blöcken |
| `--row-hover` | `#0d1716` | Tabellenzeile Hover |
| `--btn-text` | `#05201a` | Text auf Accent-Button |

Akzentfarbe ist bewusst als eine Variable geführt: `#7be38b` (grüner) ist eine getestete Alternative,
`#5eead4` ebenfalls. Nur `--accent` tauschen.

### Typografie
- UI/Headlines: **Space Grotesk** 400/500/700 (Google Fonts)
- Alle Daten, Hashes, Adressen, Zahlen, Zeiten: **JetBrains Mono** 400/500/700
- Größen: H1 Seiten 22px/700, Block-H1 26px/700, Logo 19px/700, Karten-Zahl 19px (Address 17px, Mempool 18px),
  Tabellenzellen 12.5px, Tabellenkopf 10.5px, Karten-Label 10.5px (Address/Mempool 10px),
  Statusleiste 11.5px, Fließtext 13px/1.55
- Letter-Spacing: Headlines `-0.02em`, Logo `-0.01em`; Labels/Tabellenköpfe **uppercase** mit `.13em`
  (Sektionsüberschriften `.16em`)

### Maße
- Radien: Karten 14px, innere Karten/Blockfläche 11–12px (Blockdetail 14px), Inputs/Buttons 7–8px, Zelle 2px, Tag 4–5px
- Seitenpadding: 28px horizontal; Content-Spalte `max-width: 1180px; margin: 0 auto` —
  **Ausnahme: Header, Statusleiste und Blockkette gehen über die volle Breite**
- Karten-Padding: 24px; Tabellenzeile 13px/22px; Tabellenkopf 12px/22px
- Grid-Gaps: Karten-Raster 14px (Address/Mempool 12px), Tabellenspalten 12px
- Schatten: nur am Mempool-Block `0 0 0 1px rgba(62,230,184,.08), 0 14px 40px -22px rgba(62,230,184,.7)`
  und kurz am frisch gefundenen Block `0 0 0 1px rgba(62,230,184,.2), 0 16px 44px -26px var(--accent)`

## Screens / Views

Navigation ist clientseitig; Header bleibt in allen Views identisch (`position: sticky; top: 0`).

### Gemeinsamer Header
- Zeile 1 (Padding 18px/28px, Border-bottom `--border`, `flex-wrap: wrap`, gap 24px):
  Logo = 11×11px Quadrat in `--accent`, `transform: rotate(45deg)`, radius 2px + Text
  „Parano1d Explorer" (zweites Wort in `--accent`), `white-space: nowrap`, klickbar → Dashboard.
  Danach Nav (6 Einträge, 13px, gap 22px): Dashboard · Block · Transaction · Address · Mempool · Rich list.
  Aktiv = `--text` + 1px Border-bottom in `--accent`, 2px Padding-bottom; inaktiv `--muted`, transparente Border.
  Rechts Suchfeld (`flex: 1 1 320px`, max 620px, mono 12.5px, Border `--border-input`, Radius 8px,
  Padding 11px/14px, Focus-Border `--accent`) + Button „Search" (Accent-Fläche, `--btn-text`, 700, Radius 8px, Padding 0 20px).
- Zeile 2 Statusleiste (Padding 10px/28px, mono 11.5px, `--muted`, gap 26px, wrap):
  Tip · Blocks · Transactions · Live UTXOs · Gaps · Avg block time · „live" mit 6px-Punkt in `--accent`,
  Pulse-Animation `noidPulse` 1.6s infinite (opacity .55→1→.55).

### 1. Dashboard
1. **Chain-Kopfzeile** (Padding 0 28px 18px): „CHAIN" (uppercase 13px, `.16em`, `--muted`),
   daneben mono 11.5px „next block in ~{n}s" in `--muted-4`, rechts Ghost-Button „mine now →"
   (nur Demo — im Produktivbetrieb entfällt er).
2. **Blockkette** — horizontale Flex-Reihe, volle Breite, `overflow: hidden`, rechts ein Fade-Overlay
   (130px breit, `linear-gradient(90deg, rgba(5,9,8,0), #050908)`).
   Element = Spalte 148px breit, gap 11px:
   - Blockfläche 148×148px, Radius 12px, `overflow: hidden`, Hintergrund `--block-bg`, 1px Border.
     Innen ein 4×4-Grid (gap 2px, padding 2px) aus Zellen mit Radius 2px: gefüllte Zellen = `--accent`
     mit variierender `opacity` (0.36–0.85 bei bestätigten Blöcken, 0.42–1.0 beim Mempool-Block),
     leere Zellen `#0f1a18`. Anteil gefüllter Zellen = Füllgrad des Blocks. Zell-Transition
     `opacity .8s ease, background .8s ease`.
   - Beschriftung mono: Zeile 1 12.5px (`#109030` in `--text`, „Mempool" in `--accent`),
     Zeile 2 11px `--muted-3`: `48s ago · 3 tx` bzw. `12 pending · 0.0081 NOID`.
   - Zwischen zwei Blöcken ein 44px breiter Container mit „←" in `--arrow`, 14px.
   - Blockbreite + Abstand = **192px** (148 + 44) — diese Zahl steuert die Slide-Animation.
   - Mempool-Block zusätzlich: Border `--border-block-hot`, Glow-Schatten und ein Scan-Streifen
     (22px hoch, `linear-gradient(180deg, transparent, rgba(62,230,184,.22), transparent)`,
     Animation `noidScan` 2.6s linear infinite, `translateY(-100%) → translateY(400%)`, `pointer-events: none`).
   - Klick auf Mempool-Block → Mempool-View, Klick auf Block → Block-View.
3. **Kennzahlen-Raster** (max 1180px, `repeat(auto-fit, minmax(170px, 1fr))`, gap 14px):
   Circulating supply · Block reward · Network hashrate · Avg block time (1h) · Mempool pending · Fee floor.
4. **Recent blocks** — Karte mit Kopf („RECENT BLOCKS", rechts „all blocks →"), Spalten
   `1fr 1fr 2fr 0.7fr 1.1fr 1fr`: Height (Link) · Time · Miner (Link) · Txs · Reward · Fees.

### 2. Block
Zurück-Link „← back to chain" (mono 12px, `--muted`, Hover `--accent`) über jeder Detailseite.
- Karte 1, zweispaltig (`flex-wrap: wrap`, gap 34px): links Blockvisual 240×240px (4×4-Grid, gap 3px,
  padding 3px) + Bildunterschrift mono 11px „packed by size, shaded by fee rate";
  rechts H1 „Block #109030" (Nummer in `--accent`) und Key-Value-Liste
  (Grid `160px 1fr`, gap 16px, Padding 9px 0, Border-bottom `--border-kv`; Key 12.5px `--muted-2`,
  Value mono 12.5px, `word-break: break-all`).
  Felder: Hash · Parent (Link) · Timestamp (`YYYY-MM-DD HH:MM:SS UTC (1m 44s ago)`) · Miner (Link) ·
  Proof class · Reward · Total fees · Body captured (`yes` in `--accent-2`) · State root · Tx root ·
  Nonce · Difficulty target (die letzten vier in `#8b9d98`).
- Karte 2 „TRANSACTIONS (n)", Spalten `1.6fr 1.6fr 0.7fr 1.6fr 1.1fr 1fr`:
  Txid (Link, bei Coinbase mit Tag „coinbase": 10px, `--tag-bg`/`--accent-2`, Radius 4px, Padding 2px 6px) ·
  Sender · In → out · Receiver (Link) · Amount · Fee.

### 3. Transaction
- Karte 1: H1 „Transaction" + Status-Tag „CONFIRMED" (10.5px uppercase, `.1em`, `--tag-bg`/`--accent-2`,
  Radius 5px, Padding 4px 9px). Key-Value-Liste wie oben:
  Txid · Block (Link) · Time · Type · Sender (Link) · Receivers (Link) · Fee · Input sum · Output sum · Epoch anchor.
- Danach zwei Karten nebeneinander (`repeat(auto-fit, minmax(320px, 1fr))`, gap 16px):
  „INPUTS (n)" mit Zeilen `slot 9724573` ↔ Betrag, „OUTPUTS (n)" mit Adresse (Link) ↔ Betrag.
  Zeilen: `display: flex; justify-content: space-between`, Padding 11px/20px, mono 12.5px.

### 4. Address
- Karte 1: Label „ADDRESS", volle Adresse mono 15px mit `word-break: break-all`;
  darunter 6 Kennzahlen-Karten (`repeat(auto-fit, minmax(168px, 1fr))`, gap 12px, `--panel-2`, Radius 11px, Padding 16px):
  Current balance (live) — Wert in `--accent` · Current UTXOs (live) · Recorded balance · Recorded UTXOs ·
  Total received · Total sent. Darunter Hinweiszeile 13px `--muted` („302 Transaktionen … erfasst").
- Karte 2, Spalten `1.5fr 1fr 0.9fr 1.5fr 0.7fr 1.1fr 0.9fr`:
  Txid (+coinbase-Tag) · Time · Block (Link) · Counterparty · In → out · Amount (Zugang `--accent-2`,
  Abgang `--text-2`) · Fee.

### 5. Live mempool
- Karte 1 zweispaltig: links 240×240px Mempool-Visual mit Scan-Streifen (30px hoch);
  rechts H1 „Live mempool" + „next block in ~{n}s" und drei Kennzahlen-Karten
  (Pending txs · Fee floor · Fee rate range).
- Karte 2 „PENDING TRANSACTIONS", Spalten `1.6fr 0.8fr 1.1fr 1fr 1fr`:
  Txid (Link) · In → out · Fee · Fee rate (>2000 `--accent-2`, >1200 `--accent`, sonst `#8b9d98`) · Seen.

### 6. Rich list
- Eine Karte: H1 „Rich list" + Erklärtext (13px/1.55, `--muted`, `max-width: 720px`, `text-wrap: pretty`).
- Spalten `0.35fr 2.2fr 1.2fr 0.7fr 0.9fr`: # · Address (Link) · Balance (Wert + horizontaler Balken:
  `height: 4px`, Radius 2px, `background: var(--accent)`, `opacity: .55`, Breite = Anteil am Top-Guthaben,
  max 90px, min 4px) · UTXOs · Updated.

## Interactions & Behavior

### Blockketten-Animation (Kernstück)
Ausgelöst **genau dann**, wenn ein neuer Block an den Anfang der Liste tritt (Polling erkennt neue Tip-Höhe):
1. Neuen Block ganz links **vor** dem Mempool-Block in den DOM einfügen bzw. die Liste neu rendern,
   Mempool-Block auf den neuen (geleerten) Stand setzen.
2. Danach **im nächsten Frame** (`requestAnimationFrame`) auf dem Track animieren:
   ```js
   track.animate(
     [{ transform: 'translateX(-192px)' }, { transform: 'translateX(0)' }],
     { duration: 3400, easing: 'cubic-bezier(.16,.86,.28,1)' }
   );
   ```
   Wichtig: **WAAPI statt CSS-Transition** — die Animation überlebt Re-Renders des Tracks.
   Dauer 3,4 s (Wunsch: 3–4 s), `-192px` = Blockbreite + Pfeilspalte.
3. Der neue Block bekommt für ~2,4 s Border `--accent` + Glow, danach Übergang auf Ruhezustand
   (`transition: border-color 2.4s ease, box-shadow 2.4s ease`).
4. Liste auf 14 Blöcke kürzen; ältester fällt hinten raus.
5. `prefers-reduced-motion: reduce` respektieren: Slide und Scan-Streifen abschalten, Block erscheint direkt.

### Mempool-Block
- Füllgrad = Mempool-Auslastung (gewichtete vBytes / Blockkapazität), auf 0–1 normalisiert.
- Auslastung > 1 ⇒ **mehrere Blöcke links**: pro voller Blockkapazität ein weiterer Kasten,
  Beschriftung „Queued +1", „Queued +2" …; Reihenfolge links→rechts: Mempool, Queued +1, Queued +2, dann bestätigte Blöcke.
- Füllstands-Änderungen laufen über die 0,8s-Transition der Zellen, nicht über Neuaufbau des Grids.

### Weitere Interaktionen
- Hover Tabellenzeile: `background: var(--row-hover)`; Hover Link: `--accent-hover`;
  Hover Ghost-Button: Border + Text `--accent`.
- Klickpfade: Blockkachel/Height → Block; Txid → Transaction; Miner/Receiver/Adresse → Address;
  Mempool-Kachel → Live mempool; Logo → Dashboard. Beim View-Wechsel `window.scrollTo(0, 0)`.
- Zeitangaben („48s ago", „1m 44s ago") sekündlich clientseitig neu berechnen, nicht neu vom Server holen.
- Die Kette scrollt horizontal über (bewusster) Overflow; rechts das Fade-Overlay beibehalten.

### Responsive
- Header, Kennzahlen-Raster und Input-Zeile umbrechen via `flex-wrap` / `auto-fit`-Grids.
- Unter ~900px: Tabellen horizontal scrollbar machen (`overflow-x: auto`, Mindestbreite am Grid),
  Detailseiten-Key-Value-Grid auf eine Spalte umstellen.
- Blockkette bleibt bei 148px Kacheln; auf Mobil horizontal scrollbar (Touch-Scroll erlauben).

## State Management & Datenanbindung
REST unter `/api/v1`, kein WebSocket, Polling per `setInterval` wie vom Team vorgesehen:

| Poll | Intervall | Endpunkt(e) | Aktualisiert |
|---|---|---|---|
| Ticker/Statusleiste | 20 s | `stats`, `gaps` | Tip, Blocks, Transactions, Live UTXOs, Gaps, Avg block time |
| Dashboard-Kette | 1 s | `blocks`, `mempool` | Kettenkacheln, „next block in", Mempool-Füllgrad |
| Mempool-View | 1 s | `mempool` | Visual, 3 Kennzahlen, Pending-Tabelle |
| Block-View | 1 s | `block/height/{h}` bzw. `block/hash/{h}` | Bestätigungen/Alter |
| Transaction-View | einmalig + 1 s solange unbestätigt | `tx/{txid}` | Status-Tag |
| Address-View | einmalig, Refresh-Button | `address/{addr}`, `address/{addr}/utxos` | Kennzahlen, Historie |
| Rich list | einmalig | `richlist` | Tabelle |

Clientseitiger State: `view`, `selectedHeight/txid/address`, `blocks[]` (max 14, absteigend),
`mempool` (Auslastung + Pending-Liste), `tipHeight`, `nextBlockEta`, `flashHeight` (für den Glow).
Trigger für die Slide-Animation: `newTip > tipHeight`. Bei Sprung um mehr als einen Block
(Polling-Lücke) nur **einmal** animieren und die Liste komplett neu setzen.
Fehlerfälle: bei fehlgeschlagenem Poll letzten Stand stehen lassen, „live"-Punkt in `--muted` schalten
und nach 3 Fehlversuchen eine dezente Zeile in der Statusleiste („connection lost — retrying").
Leere Zustände: Mempool ohne Pending-Txs = Blockkachel komplett `#0f1a18`, Beschriftung „0 pending".

## Assets
Keine Bild-Assets. Logo-Raute ist ein gedrehtes `div`. Schriften über Google Fonts
(`Space Grotesk` 400/500/700, `JetBrains Mono` 400/500/700) — bei Self-Hosting bitte
`font-display: swap` und Preload der beiden Mono-/Sans-Regular-Schnitte.

## Files
- `before-redesign.png` — Stand des Explorers vor dem Redesign, zum Vergleich.
- Umsetzung: `frontend/site/css/style.css` (Tokens, Layout), `frontend/site/js/chain.js` (Blockkette +
  Animation), `frontend/site/js/cells.js` (4×4-Zellenraster), `frontend/site/js/views.js` (die sechs Views).
