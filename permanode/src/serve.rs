//! The JSON API over the indexer's database and the node, plus the
//! static page in front of it: the built-in API index, or a frontend
//! from `site_dir`. Runs in the same process as the indexer by default
//! (see main.rs), on its own database connection.

use crate::config::Config;
use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::{header, StatusCode, Uri},
    response::{IntoResponse, Json, Response},
    routing::get,
    Router,
};
use include_dir::{include_dir, Dir};
use permanode_core::{db, live_rpc, queries};
use rusqlite::Connection;
use serde::Deserialize;
use std::sync::{Arc, Mutex};
use tower_http::cors::CorsLayer;

static SITE: Dir<'_> = include_dir!("$CARGO_MANIFEST_DIR/../frontend/site");

/// FNV-1a over the file contents, used as a strong ETag so browsers can
/// revalidate cheaply (304) and still pick up a new build immediately.
fn etag_of(bytes: &[u8]) -> String {
    let mut h: u64 = 0xcbf2_9ce4_8422_2325;
    for b in bytes {
        h ^= u64::from(*b);
        h = h.wrapping_mul(0x0100_0000_01b3);
    }
    format!("\"{h:016x}\"")
}

struct AppState {
    conn: Mutex<Connection>,
    rpc: live_rpc::RpcClient,
    /// How often the indexer refreshes every address's live balance
    /// (the UTXO sweep), so the rich list can say how fresh it is.
    balance_sweep_interval_seconds: Option<u64>,
    /// How often addresses with recorded activity are additionally
    /// refreshed one by one.
    address_refresh_interval_seconds: u64,
    donation_address: Option<String>,
    db_path: String,
    /// Heights at which log_slots first reached 25, 26, ... - found by
    /// binary search over the permanent headers and cached per current
    /// log_slots, so the emission total needs no RPC calls in the common
    /// case where the state has never expanded.
    expansions: Mutex<Option<(u32, Vec<u64>)>>,
    /// Sampled `(height, active_slot_count)` from the permanent headers for
    /// the halving page's growth curve; extended incrementally as the tip
    /// advances, so only the first request pays for the full walk.
    state_history: Mutex<StateHistory>,
    /// The finalized expansion window for the tip it was computed at;
    /// headers are permanent, so it only changes when the tip moves.
    window_cache: Mutex<Option<(u64, Vec<WindowHeader>)>>,
    /// The economics page's 24h/7d/30d aggregates, which scan every
    /// recorded block and get slower as the history grows. Recomputed
    /// when the tip moves on or the entry is older than a minute.
    period_cache: Mutex<Option<(i64, i64, Vec<PeriodActivity>)>>,
    /// `/miners` per period, keyed by the indexed tip and the lowest
    /// block on record (which moves while the header backfill runs); a
    /// scan of the whole chain takes a noticeable fraction of a second.
    miners_cache: Mutex<std::collections::HashMap<&'static str, MinersCacheEntry>>,
}

struct MinersCacheEntry {
    tip: i64,
    lowest: Option<u64>,
    computed_at: i64,
    report: queries::MinersReport,
}

#[derive(Default)]
struct StateHistory {
    step: u64,
    points: Vec<HeaderSample>,
}

/// The two permanent header fields the charts are built from.
#[derive(Clone, Copy)]
struct HeaderSample {
    height: u64,
    active_slot_count: u64,
    /// Mints so far (every live output ever created, coinbases included).
    alloc_counter: u64,
}

impl AppState {
    /// A poisoned mutex (a handler panicked while holding it) must not
    /// take every later request down with it - the connection itself is
    /// still fine, so just keep using it.
    fn db(&self) -> std::sync::MutexGuard<'_, Connection> {
        self.conn.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

type ApiResult<T> = Result<Json<T>, ApiError>;

struct ApiError(anyhow::Error);
impl IntoResponse for ApiError {
    fn into_response(self) -> axum::response::Response {
        log::warn!("request failed: {:#}", self.0);
        (StatusCode::INTERNAL_SERVER_ERROR, "internal error").into_response()
    }
}
impl<E: Into<anyhow::Error>> From<E> for ApiError {
    fn from(e: E) -> Self {
        ApiError(e.into())
    }
}

struct NotFound;
impl IntoResponse for NotFound {
    fn into_response(self) -> axum::response::Response {
        (StatusCode::NOT_FOUND, "not found").into_response()
    }
}

pub async fn run(cfg: &Config) -> Result<()> {
    let conn = db::open(&cfg.db_path)?;
    let state = Arc::new(AppState {
        conn: Mutex::new(conn),
        rpc: live_rpc::RpcClient::new(cfg.rpc_url.clone()),
        balance_sweep_interval_seconds: (cfg.scan_slots_every_cycles > 0)
            .then(|| cfg.scan_slots_every_cycles.saturating_mul(cfg.poll_interval_seconds)),
        address_refresh_interval_seconds: cfg.refresh_addresses_every_cycles.saturating_mul(cfg.poll_interval_seconds),
        donation_address: cfg.donation_address.as_deref().map(str::trim).filter(|a| !a.is_empty()).map(String::from),
        db_path: cfg.db_path.clone(),
        expansions: Mutex::new(None),
        state_history: Mutex::new(StateHistory::default()),
        window_cache: Mutex::new(None),
        period_cache: Mutex::new(None),
        miners_cache: Mutex::new(std::collections::HashMap::new()),
    });

    let api = Router::new()
        .route("/api/v1/stats", get(get_stats))
        .route("/api/v1/blocks", get(get_blocks))
        .route("/api/v1/block/height/{height}", get(get_block_by_height))
        .route("/api/v1/block/hash/{hash}", get(get_block_by_hash))
        .route("/api/v1/tx/{txid}", get(get_tx))
        .route("/api/v1/address/{address}", get(get_address))
        .route("/api/v1/address/{address}/utxos", get(get_address_utxos))
        .route("/api/v1/gaps", get(get_gaps))
        .route("/api/v1/orphans", get(get_orphans))
        .route("/api/v1/richlist", get(get_richlist))
        .route("/api/v1/miners", get(get_miners))
        .route("/api/v1/mempool", get(get_mempool))
        .route("/api/v1/halving", get(get_halving))
        .route("/api/v1/economics", get(get_economics));

    let app = match &cfg.site_dir {
        Some(dir) => {
            let dir = std::path::PathBuf::from(dir);
            anyhow::ensure!(dir.join("index.html").is_file(), "site_dir {} has no index.html", dir.display());
            log::info!("serving the frontend from {}", dir.display());
            let dir = Arc::new(dir);
            api.fallback(move |uri: Uri, headers: axum::http::HeaderMap| site_dir_file(Arc::clone(&dir), uri, headers))
        }
        None => api.fallback(embedded_site),
    }
    .layer(CorsLayer::permissive())
    .with_state(state);

    if let Some(peer_listen) = cfg.peer_listen.clone() {
        tokio::spawn(run_peer(peer_listen, cfg.db_path.clone()));
    }

    log::info!("api listening on http://{}", cfg.listen);
    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

// ---------------------------------------------------------------- peer endpoint

/// The peer endpoint (`peer_listen`): this permanode's block bodies for
/// other permanodes that fill their gaps from it, and a short status. Its
/// own read-only connection. A failed bind (the private network not up
/// yet) is retried and never takes the API down.
async fn run_peer(listen: String, db_path: String) {
    let conn = loop {
        match Connection::open_with_flags(&db_path, rusqlite::OpenFlags::SQLITE_OPEN_READ_ONLY | rusqlite::OpenFlags::SQLITE_OPEN_NO_MUTEX) {
            Ok(c) => break c,
            Err(e) => {
                log::warn!("peer endpoint: cannot open {db_path} ({e}), retrying in 30 s");
                tokio::time::sleep(std::time::Duration::from_secs(30)).await;
            }
        }
    };
    let app = Router::new()
        .route("/peer/v1/body/{height}/{hash}", get(peer_body))
        .route("/peer/v1/status", get(peer_status))
        .with_state(Arc::new(Mutex::new(conn)));
    loop {
        match tokio::net::TcpListener::bind(&listen).await {
            Ok(listener) => {
                log::info!("peer endpoint listening on http://{listen}");
                if let Err(e) = axum::serve(listener, app.clone()).await {
                    log::warn!("peer endpoint stopped: {e}");
                }
            }
            Err(e) => log::warn!("peer endpoint: cannot listen on {listen} ({e}), retrying in 30 s"),
        }
        tokio::time::sleep(std::time::Duration::from_secs(30)).await;
    }
}

type PeerDb = Arc<Mutex<Connection>>;

fn peer_error(status: StatusCode, msg: &str) -> Response {
    (status, Json(serde_json::json!({ "error": msg }))).into_response()
}

/// The recorded body of block `(height, hash)` in the format of the node's
/// `getBlockDetails`, or 404.
async fn peer_body(State(db): State<PeerDb>, Path((height, hash)): Path<(u64, String)>) -> Response {
    if hash.len() != 64 || !hash.bytes().all(|b| b.is_ascii_hexdigit()) {
        return peer_error(StatusCode::BAD_REQUEST, "hash must be 64 hex digits");
    }
    let hash = hash.to_ascii_lowercase();
    let found = tokio::task::spawn_blocking(move || {
        let conn = db.lock().unwrap_or_else(|p| p.into_inner());
        crate::import::body_from_db(&conn, height, &hash)
    })
    .await;
    match found {
        Ok(Ok(Some(body))) => Json(body).into_response(),
        Ok(Ok(None)) => peer_error(StatusCode::NOT_FOUND, "no body on record for this block"),
        Ok(Err(e)) => {
            log::warn!("peer endpoint: body of #{height}: {e:#}");
            peer_error(StatusCode::INTERNAL_SERVER_ERROR, "recorded body unreadable")
        }
        Err(_) => peer_error(StatusCode::INTERNAL_SERVER_ERROR, "internal error"),
    }
}

/// Highest block with a body on record, the archive's first one and the
/// open gaps - enough for the other side to see this permanode is alive.
async fn peer_status(State(db): State<PeerDb>) -> Response {
    let status = tokio::task::spawn_blocking(move || -> rusqlite::Result<serde_json::Value> {
        let conn = db.lock().unwrap_or_else(|p| p.into_inner());
        let (tip, first): (Option<i64>, Option<i64>) =
            conn.query_row("SELECT MAX(height), MIN(height) FROM blocks WHERE body_captured = 1", [], |r| Ok((r.get(0)?, r.get(1)?)))?;
        let gaps: i64 = conn.query_row("SELECT COUNT(*) FROM ingest_gaps WHERE resolved_at IS NULL", [], |r| r.get(0))?;
        Ok(serde_json::json!({ "tip": tip, "archive_from": first, "open_gaps": gaps }))
    })
    .await;
    match status {
        Ok(Ok(v)) => Json(v).into_response(),
        _ => peer_error(StatusCode::INTERNAL_SERVER_ERROR, "status unreadable"),
    }
}

/// An unknown path under /api/ is a client asking for an endpoint this
/// permanode does not have: a JSON 404, not the frontend's index page.
fn unknown_api(uri: &Uri) -> Option<Response> {
    let p = uri.path();
    (p == "/api" || p.starts_with("/api/")).then(|| (StatusCode::NOT_FOUND, Json(serde_json::json!({ "error": "unknown API endpoint" }))).into_response())
}

/// Serves the page compiled into the binary (an index of the API unless
/// a frontend was placed in `frontend/site` at build time). Unknown
/// paths get index.html so a client-side router can take over.
async fn embedded_site(uri: Uri, headers: axum::http::HeaderMap) -> Response {
    if let Some(r) = unknown_api(&uri) {
        return r;
    }
    let path = uri.path().trim_start_matches('/');
    let (file, spa) = match SITE.get_file(path) {
        Some(f) => (f, false),
        None => match SITE.get_file("index.html") {
            Some(f) => (f, true),
            None => return (StatusCode::NOT_FOUND, "not found").into_response(),
        },
    };
    let mime = if spa { mime_guess::mime::TEXT_HTML_UTF_8 } else { mime_guess::from_path(file.path()).first_or_octet_stream() };
    static_response(mime, file.contents().to_vec(), &headers)
}

/// Serves a frontend from `site_dir` with the same caching rules as the
/// built-in page. Files are read per request (they are small and the
/// directory may be updated while running); anything that would leave
/// the directory is refused, and unknown paths fall back to index.html
/// for the client-side router.
async fn site_dir_file(dir: Arc<std::path::PathBuf>, uri: Uri, headers: axum::http::HeaderMap) -> Response {
    use std::path::Component;
    if let Some(r) = unknown_api(&uri) {
        return r;
    }
    let rel = std::path::Path::new(uri.path().trim_start_matches('/'));
    if rel.components().any(|c| !matches!(c, Component::Normal(_))) {
        return (StatusCode::NOT_FOUND, "not found").into_response();
    }
    let candidate = dir.join(rel);
    let (path, spa) = if !rel.as_os_str().is_empty() && candidate.is_file() { (candidate, false) } else { (dir.join("index.html"), true) };
    match tokio::fs::read(&path).await {
        Ok(bytes) => {
            let mime = if spa { mime_guess::mime::TEXT_HTML_UTF_8 } else { mime_guess::from_path(&path).first_or_octet_stream() };
            static_response(mime, bytes, &headers)
        }
        Err(_) => (StatusCode::NOT_FOUND, "not found").into_response(),
    }
}

/// Fonts and icons practically never change and may sit in caches for a
/// day. Everything else (HTML, scripts, styles) is revalidated on every
/// load via the ETag, so an update shows up immediately at the cost of a
/// 304 round trip - also through a CDN, which would otherwise cache
/// scripts for hours on its own.
fn static_response(mime: mime_guess::Mime, bytes: Vec<u8>, headers: &axum::http::HeaderMap) -> Response {
    let long_lived = matches!(mime.type_().as_str(), "font" | "image");
    let cache = if long_lived { "public, max-age=86400" } else { "no-cache" };
    let etag = etag_of(&bytes);
    if headers
        .get(header::IF_NONE_MATCH)
        .and_then(|v| v.to_str().ok())
        .is_some_and(|v| v.split(',').any(|t| t.trim() == etag))
    {
        return (StatusCode::NOT_MODIFIED, [(header::ETAG, etag), (header::CACHE_CONTROL, cache.to_string())]).into_response();
    }
    (
        [
            (header::CONTENT_TYPE, mime.as_ref().to_string()),
            (header::CACHE_CONTROL, cache.to_string()),
            (header::ETAG, etag),
        ],
        bytes,
    )
        .into_response()
}

#[derive(serde::Serialize)]
struct StatsResponse {
    #[serde(flatten)]
    chain: queries::ChainStats,
    network: NetworkMetrics,
    /// Interval of the live-balance sweep behind the rich list, `null` if
    /// the sweep is disabled.
    balance_sweep_interval_seconds: Option<u64>,
    address_refresh_interval_seconds: u64,
    /// The operator's donation address from the config, if any.
    donation_address: Option<String>,
    /// Size of the database on disk (main file plus write-ahead log).
    db_bytes: Option<u64>,
    /// Rules of the next block and the v2 activation; `None` if the node
    /// is unreachable.
    protocol: Option<ProtocolInfo>,
}

#[derive(serde::Serialize, Default)]
struct NetworkMetrics {
    /// The node's own live UTXO count across its whole history, for
    /// comparison against `chain.live_utxos` (which only reflects what
    /// this permanode has itself recorded since it started).
    active_slots: Option<u64>,
    circulating_supply_micronoid: Option<String>,
    block_reward_micronoid: Option<u64>,
    difficulty_bits: Option<u32>,
    difficulty_target: Option<String>,
    /// Rough estimate derived from the current PoW target, not a measured
    /// figure - see live_rpc::estimate_hashrate.
    estimated_hashrate_hs: Option<f64>,
    /// From this permanode's own recorded block timestamps, so a fresh
    /// install won't have a 24h figure yet - not from the node.
    avg_block_time_10m_seconds: Option<f64>,
    avg_block_time_1h_seconds: Option<f64>,
    avg_block_time_24h_seconds: Option<f64>,
    /// Live UTXOs the state can hold at the current log_slots, and how
    /// many more live UTXOs until it expands. The block reward halves
    /// with every expansion (50 -> 25 -> 12.5 ... NOID, floor 1 NOID),
    /// triggered once a majority of the last 18 finalized blocks report
    /// at least `expand_trigger_pct` percent occupancy.
    state_capacity: Option<u64>,
    slots_until_halving: Option<u64>,
    halving_trigger_pct: Option<u64>,
    /// Everything the protocol has minted up to the node's tip (mirrored
    /// emission schedule, see core::emission) and the part of it that fees
    /// have burned since genesis: minted minus circulating supply.
    emitted_total_micronoid: Option<String>,
    burned_total_micronoid: Option<String>,
}

async fn get_stats(State(state): State<Arc<AppState>>) -> ApiResult<StatsResponse> {
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);

    let (chain, avg_10m, avg_1h, avg_24h) = {
        let conn = state.db();
        (
            queries::chain_stats(&conn)?,
            queries::avg_block_time_seconds(&conn, 600, now_unix)?,
            queries::avg_block_time_seconds(&conn, 3600, now_unix)?,
            queries::avg_block_time_seconds(&conn, 86400, now_unix)?,
        )
    };

    let state_for_rpc = Arc::clone(&state);
    let (active_slots, chain_info, mining_info, state_info, expansions, protocol) = tokio::task::spawn_blocking(move || {
        let rpc = &state_for_rpc.rpc;
        let chain_info = rpc.get_chain_info().ok();
        let expansions = chain_info.as_ref().and_then(|c| expansion_heights(&state_for_rpc, c.log_slots, c.height));
        let protocol = chain_info.as_ref().and_then(|c| {
            let ts = rpc.get_block_header(c.height).ok().flatten()?.timestamp;
            Some(protocol_info(c.height, ts))
        });
        (
            rpc.get_active_slot_count().ok(),
            chain_info,
            rpc.get_mining_info().ok(),
            rpc.get_state_info().ok(),
            expansions,
            protocol,
        )
    })
    .await
    .unwrap_or((None, None, None, None, None, None));

    let (emitted_total_micronoid, burned_total_micronoid) = match (&chain_info, &expansions) {
        (Some(c), Some(exp)) => {
            let emitted = permanode_core::emission::emitted_up_to(c.height, exp);
            let supply: u128 = c.circulating_supply_micronoid.parse().unwrap_or(0);
            (Some(emitted.to_string()), Some(emitted.saturating_sub(supply).to_string()))
        }
        _ => (None, None),
    };
    let db_bytes = db_size_bytes(&state.db_path);

    let estimated_hashrate_hs = mining_info
        .as_ref()
        .and_then(|m| live_rpc::estimate_hashrate(&m.difficulty_target));

    let network = NetworkMetrics {
        active_slots,
        circulating_supply_micronoid: chain_info.map(|c| c.circulating_supply_micronoid),
        block_reward_micronoid: mining_info.as_ref().map(|m| m.block_reward_micronoid),
        difficulty_bits: mining_info.as_ref().map(|m| m.difficulty_bits),
        difficulty_target: mining_info.map(|m| m.difficulty_target),
        estimated_hashrate_hs,
        avg_block_time_10m_seconds: avg_10m,
        avg_block_time_1h_seconds: avg_1h,
        avg_block_time_24h_seconds: avg_24h,
        state_capacity: state_info.as_ref().map(|i| i.capacity),
        slots_until_halving: state_info.as_ref().map(|i| i.slots_until_expand),
        halving_trigger_pct: state_info.as_ref().map(|i| i.expand_trigger_pct),
        emitted_total_micronoid,
        burned_total_micronoid,
    };

    Ok(Json(StatsResponse {
        chain,
        network,
        balance_sweep_interval_seconds: state.balance_sweep_interval_seconds,
        address_refresh_interval_seconds: state.address_refresh_interval_seconds,
        donation_address: state.donation_address.clone(),
        db_bytes,
        protocol,
    }))
}

/// Confirmations at which a block can no longer be reorganized away (the
/// protocol's maximum rollback is 17 blocks).
const FINAL_CONFIRMATIONS: i64 = 18;

/// Input shapes are checked before touching the database or the node:
/// every query is parameterized anyway, but a 64-hex txid or a bech32m
/// `o1…` address is the only thing worth a lookup - anything else is a
/// fast 404 instead of a wasted query and RPC call.
fn is_hex64(s: &str) -> bool {
    s.len() == 64 && s.bytes().all(|b| b.is_ascii_hexdigit() && !b.is_ascii_uppercase())
}
fn is_address(s: &str) -> bool {
    s.starts_with("o1")
        && (50..=100).contains(&s.len())
        && s.bytes().all(|b| b.is_ascii_lowercase() || b.is_ascii_digit())
}

/// JSON with a cache policy: data that can still change (young blocks,
/// live state) must always be re-fetched, final history may be cached for
/// an hour by browsers and proxies.
fn cached_json<T: serde::Serialize>(value: T, is_final: bool) -> Response {
    let policy = if is_final { "public, max-age=3600" } else { "no-cache" };
    ([(header::CACHE_CONTROL, policy)], Json(value)).into_response()
}

fn db_size_bytes(db_path: &str) -> Option<u64> {
    let main = std::fs::metadata(db_path).ok()?.len();
    let wal = std::fs::metadata(format!("{db_path}-wal")).map(|m| m.len()).unwrap_or(0);
    Some(main + wal)
}

/// First heights at which the state reached log_slots 25, 26, ..., up to
/// the current value. log_slots only ever grows, so each step is a binary
/// search over the permanent headers; the result is cached until the
/// state expands again.
fn expansion_heights(state: &AppState, current_log_slots: u32, tip: u64) -> Option<Vec<u64>> {
    if let Some((cached_for, heights)) = state.expansions.lock().ok()?.as_ref() {
        if *cached_for == current_log_slots {
            return Some(heights.clone());
        }
    }
    let mut heights = Vec::new();
    let mut lo = 1u64;
    for level in (permanode_core::emission::LOG_SLOTS_GENESIS + 1)..=current_log_slots {
        // smallest h in [lo, tip] with log_slots >= level
        let (mut a, mut b) = (lo, tip);
        while a < b {
            let mid = a + (b - a) / 2;
            let h = state.rpc.get_block_header(mid).ok()??;
            if h.log_slots >= level {
                b = mid;
            } else {
                a = mid + 1;
            }
        }
        heights.push(a);
        lo = a;
    }
    *state.expansions.lock().ok()? = Some((current_log_slots, heights.clone()));
    Some(heights)
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
}

#[derive(Deserialize)]
struct BlocksQuery {
    limit: Option<i64>,
    /// Page on: only blocks below this height.
    before: Option<i64>,
}

/// Newest blocks, or with `before` the next page further down - across
/// the archive's first block into the header-only blocks below it. A full
/// page of final blocks never changes and is cached like a final block.
async fn get_blocks(State(state): State<Arc<AppState>>, Query(q): Query<BlocksQuery>) -> Result<Response, ApiErrorOr404> {
    let conn = state.db();
    let limit = q.limit.unwrap_or(25).clamp(1, 200);
    let blocks = queries::recent_blocks(&conn, limit, q.before)?;
    let tip = queries::indexed_tip(&conn)?;
    let complete_page = blocks.len() as i64 == limit || blocks.last().is_some_and(|b| b.height == 0);
    let newest_final = blocks.first().is_some_and(|b| tip.is_some_and(|t| t - b.height + 1 >= FINAL_CONFIRMATIONS));
    Ok(cached_json(blocks, q.before.is_some() && complete_page && newest_final))
}

async fn get_block_by_height(
    State(state): State<Arc<AppState>>,
    Path(height): Path<i64>,
) -> Result<Response, ApiErrorOr404> {
    if height < 0 {
        return Err(ApiErrorOr404::NotFound);
    }
    let conn = state.db();
    match queries::block_by_height(&conn, height)? {
        Some(b) => {
            let is_final = b.confirmations.is_some_and(|c| c >= FINAL_CONFIRMATIONS);
            Ok(cached_json(b, is_final))
        }
        None => Err(ApiErrorOr404::NotFound),
    }
}

async fn get_block_by_hash(
    State(state): State<Arc<AppState>>,
    Path(hash): Path<String>,
) -> Result<Response, ApiErrorOr404> {
    if !is_hex64(&hash) {
        return Err(ApiErrorOr404::NotFound);
    }
    let conn = state.db();
    match queries::block_by_hash(&conn, &hash)? {
        Some(b) => {
            let is_final = b.confirmations.is_some_and(|c| c >= FINAL_CONFIRMATIONS);
            Ok(cached_json(b, is_final))
        }
        None => Err(ApiErrorOr404::NotFound),
    }
}

async fn get_tx(
    State(state): State<Arc<AppState>>,
    Path(txid): Path<String>,
) -> Result<Response, ApiErrorOr404> {
    if !is_hex64(&txid) {
        return Err(ApiErrorOr404::NotFound);
    }
    let tx = {
        let conn = state.db();
        queries::tx_by_txid(&conn, &txid)?
    };
    match tx {
        Some(t) => {
            let is_final = t.block.canonical && t.confirmations.is_some_and(|c| c >= FINAL_CONFIRMATIONS);
            let fee_breakdown = tx_fee_breakdown(&state, &t).await;
            Ok(cached_json(TxResponse { tx: t, fee_breakdown }, is_final))
        }
        None => Err(ApiErrorOr404::NotFound),
    }
}

#[derive(serde::Serialize)]
struct TxResponse {
    #[serde(flatten)]
    tx: queries::TxDetail,
    /// Miner share vs. consensus burn of the paid fee; absent for coinbase
    /// and development-payout pages or when the parent header can't be
    /// fetched right now.
    fee_breakdown: Option<permanode_core::fees::FeeBreakdown>,
}

/// The burn depends on the occupancy in the parent header (what the node
/// checks the coinbase against), which is permanent, so one RPC call.
async fn tx_fee_breakdown(state: &Arc<AppState>, t: &queries::TxDetail) -> Option<permanode_core::fees::FeeBreakdown> {
    if t.coinbase || t.development_payout {
        return None;
    }
    let parent_height = u64::try_from(t.block.height).ok()?.checked_sub(1)?;
    let (fee, n_in, n_out) = (u64::try_from(t.fee_micronoid).ok()?, t.inputs.len() as u64, t.outputs.len() as u64);
    let st = Arc::clone(state);
    let header = tokio::task::spawn_blocking(move || st.rpc.get_block_header(parent_height)).await.ok()?.ok()??;
    Some(permanode_core::fees::fee_breakdown(fee, n_in, n_out, header.active_slot_count, header.log_slots))
}

#[derive(Deserialize)]
struct AddressQuery {
    page: Option<i64>,
    page_size: Option<i64>,
}

#[derive(serde::Serialize)]
struct AddressPage {
    address: String,
    page: i64,
    page_size: i64,
    total: i64,
    /// Of `total`, transactions imported from payment receipts into blocks
    /// below the archive (marked `source: "receipt"`); they do not count
    /// toward `balance`.
    receipt_transactions: i64,
    transactions: Vec<queries::TxSummary>,
    /// Computed from this permanode's own recorded history only.
    balance: queries::AddressBalance,
    /// Read live from the node's current Live State
    /// (paranoid_getSlotsByOwner) - correct regardless of when this
    /// permanode started recording. `null` if the node couldn't be
    /// reached for this.
    live_balance_micronoid: Option<String>,
    live_utxo_count: Option<u64>,
    /// Canonical blocks this address mined, header-only blocks below the
    /// archive included.
    blocks_mined: queries::BlocksMined,
}

/// Fetches and sorts (largest first) an address's live slots. `None` if the
/// node couldn't be reached - best-effort, callers treat that as "unknown"
/// rather than failing the whole request.
async fn fetch_live_slots(rpc: &live_rpc::RpcClient, address: &str) -> Option<Vec<live_rpc::SlotInfo>> {
    let rpc_client = rpc.clone();
    let addr_for_rpc = address.to_string();
    let slots = tokio::task::spawn_blocking(move || rpc_client.get_slots_by_owner(&addr_for_rpc))
        .await
        .ok()?
        .ok()?;
    let mut live: Vec<_> = slots.into_iter().filter(|s| !s.empty).collect();
    live.sort_by(|a, b| b.value.cmp(&a.value));
    Some(live)
}

async fn get_address(
    State(state): State<Arc<AppState>>,
    Path(address): Path<String>,
    Query(q): Query<AddressQuery>,
) -> Result<Json<AddressPage>, ApiErrorOr404> {
    if !is_address(&address) {
        return Err(ApiErrorOr404::NotFound);
    }
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(25).clamp(1, 200);
    let (transactions, total, receipt_transactions, balance, blocks_mined) = {
        let conn = state.db();
        let (transactions, total) = queries::txs_by_address(&conn, &address, page, page_size)?;
        let receipt_transactions = queries::receipt_txs_by_address(&conn, &address)?;
        let balance = queries::address_balance(&conn, &address)?;
        let blocks_mined = queries::blocks_mined(&conn, &address)?;
        (transactions, total, receipt_transactions, balance, blocks_mined)
    };

    // Just the summary here (balance + count) - the individual UTXOs are a
    // separate, on-demand endpoint (GET .../utxos) so a plain address page
    // view doesn't always pull and ship a potentially long slot list.
    let live_slots = fetch_live_slots(&state.rpc, &address).await;
    let live_balance_micronoid = live_slots.as_ref().map(|s| s.iter().map(|u| u.value).sum::<u64>().to_string());
    let live_utxo_count = live_slots.as_ref().map(|s| s.len() as u64);

    Ok(Json(AddressPage {
        address,
        page,
        page_size,
        total,
        receipt_transactions,
        transactions,
        balance,
        live_balance_micronoid,
        live_utxo_count,
        blocks_mined,
    }))
}

async fn get_address_utxos(
    State(state): State<Arc<AppState>>,
    Path(address): Path<String>,
) -> Result<Json<AddressUtxosPage>, ApiErrorOr404> {
    if !is_address(&address) {
        return Err(ApiErrorOr404::NotFound);
    }
    let live_utxos = fetch_live_slots(&state.rpc, &address).await;
    Ok(Json(AddressUtxosPage { address, live_utxos }))
}

#[derive(serde::Serialize)]
struct AddressUtxosPage {
    address: String,
    /// `null` only if the node couldn't be reached.
    live_utxos: Option<Vec<live_rpc::SlotInfo>>,
}

async fn get_gaps(State(state): State<Arc<AppState>>) -> ApiResult<Vec<queries::GapEntry>> {
    let conn = state.db();
    Ok(Json(queries::recent_gaps(&conn, 100)?))
}

/// Blocks a reorg replaced. Kept on record; normal views show only the
/// canonical chain, this is the trail behind it.
async fn get_orphans(State(state): State<Arc<AppState>>, Query(q): Query<LimitQuery>) -> ApiResult<Vec<queries::OrphanedBlock>> {
    let conn = state.db();
    Ok(Json(queries::orphaned_blocks(&conn, q.limit.unwrap_or(100))?))
}

async fn get_richlist(State(state): State<Arc<AppState>>) -> ApiResult<Vec<queries::RichListEntry>> {
    let conn = state.db();
    Ok(Json(queries::richlist(&conn, 100)?))
}

#[derive(Deserialize)]
struct MinersQuery {
    period: Option<String>,
    limit: Option<usize>,
}

#[derive(serde::Serialize)]
struct MinersResponse {
    /// `all`, `7d` or `24h`.
    period: &'static str,
    #[serde(flatten)]
    report: queries::MinersReport,
}

/// Most miners a cached report keeps; `limit` picks from those.
const MINERS_MAX: usize = 1_000;

/// Miners by blocks found over the whole chain (`period=all`, the default)
/// or the last 7 days / 24 hours, with their share - header-only blocks
/// below the archive included.
async fn get_miners(State(state): State<Arc<AppState>>, Query(q): Query<MinersQuery>) -> Result<Response, ApiErrorOr404> {
    let (period, seconds) = match q.period.as_deref().unwrap_or("all") {
        "all" => ("all", None),
        "7d" => ("7d", Some(7 * 86_400i64)),
        "24h" => ("24h", Some(86_400i64)),
        _ => return Err(ApiErrorOr404::BadRequest("period must be all, 7d or 24h")),
    };
    let limit = q.limit.unwrap_or(100).clamp(1, MINERS_MAX);
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let mut report = {
        let conn = state.db();
        let tip = queries::indexed_tip(&conn)?.unwrap_or(0);
        let lowest = db::lowest_recorded_height(&conn)?;
        let cached = {
            let cache = state.miners_cache.lock().unwrap_or_else(|e| e.into_inner());
            cache
                .get(period)
                .filter(|e| e.tip == tip && e.lowest == lowest && now_unix - e.computed_at < 60)
                .map(|e| e.report.clone())
        };
        match cached {
            Some(report) => report,
            None => {
                let report = queries::miners(&conn, seconds.map(|s| now_unix - s), MINERS_MAX)?;
                let entry = MinersCacheEntry { tip, lowest, computed_at: now_unix, report: report.clone() };
                state.miners_cache.lock().unwrap_or_else(|e| e.into_inner()).insert(period, entry);
                report
            }
        }
    };
    report.miners.truncate(limit);
    Ok(cached_json(MinersResponse { period, report }, false))
}

async fn get_mempool(State(state): State<Arc<AppState>>) -> ApiResult<live_rpc::MempoolInfo> {
    // Runs the blocking RPC call on a blocking-safe thread so it can't
    // stall the async runtime's other requests.
    let rpc_client = state.rpc.clone();
    let info = tokio::task::spawn_blocking(move || rpc_client.get_mempool_info())
        .await
        .map_err(anyhow::Error::from)??;
    Ok(Json(info))
}

/// Which rules the chain follows and when v2 starts. The explorer switches
/// its halving and economics views once `active` is true.
#[derive(serde::Serialize, Clone)]
struct ProtocolInfo {
    /// `"v1.1"` until block 210,537 exists, `"v2"` from then on.
    version: &'static str,
    active: bool,
    activation_height: u64,
    /// Blocks still to be mined up to and including the activation block.
    blocks_until_activation: u64,
    /// Estimated time of the activation block at the target block time,
    /// counted from the tip's timestamp; `None` once active.
    activation_eta_unix: Option<u64>,
    /// Target interval of the next block, in seconds.
    target_block_time_seconds: u64,
}

fn protocol_info(tip: u64, tip_timestamp: u64) -> ProtocolInfo {
    use permanode_core::emission::{block_time_at, target_seconds_between, v2_active, V2_ACTIVATION_HEIGHT};
    let active = v2_active(tip);
    ProtocolInfo {
        version: if active { "v2" } else { "v1.1" },
        active,
        activation_height: V2_ACTIVATION_HEIGHT,
        blocks_until_activation: V2_ACTIVATION_HEIGHT.saturating_sub(tip),
        activation_eta_unix: (!active).then(|| tip_timestamp + target_seconds_between(tip, V2_ACTIVATION_HEIGHT)),
        target_block_time_seconds: block_time_at(tip + 1),
    }
}

/// One interval of the v2 reward schedule.
#[derive(serde::Serialize)]
struct RewardTier {
    tier: usize,
    first_height: u64,
    /// `None` for the tail, which never ends.
    last_height: Option<u64>,
    reward_micronoid: u64,
    /// Gross subsidy of the whole interval; `None` for the tail.
    interval_total_micronoid: Option<String>,
    /// Gross issuance from genesis to the interval's last block, before
    /// burns (a projection for intervals still ahead); `None` for the tail.
    issued_at_end_micronoid: Option<String>,
    /// `"done"`, `"current"` or `"upcoming"`.
    status: &'static str,
    /// Estimated start at the target block time; `None` once started.
    eta_unix: Option<u64>,
}

/// The v2 reward schedule as seen from `tip` - also before the fork, so
/// the explorer can preview it.
#[derive(serde::Serialize)]
struct V2Schedule {
    interval_blocks: u64,
    tiers: Vec<RewardTier>,
    current_tier: Option<usize>,
    /// The next block with a lower reward (the activation block itself
    /// while the fork lies ahead); `None` in the tail.
    next_reduction_height: Option<u64>,
    blocks_to_next_reduction: Option<u64>,
    next_reduction_eta_unix: Option<u64>,
    next_reward_micronoid: Option<u64>,
    /// Share of the current interval already mined, in basis points.
    interval_progress_bps: Option<u64>,
    /// Gross issuance of all legacy blocks (projected while they lie ahead).
    legacy_issued_micronoid: String,
    /// Reward of the last legacy block (50 NOID unless the state expanded).
    legacy_reward_micronoid: u64,
    issued_since_activation_micronoid: String,
    eight_interval_total_micronoid: String,
    tail_height: u64,
    tail_reward_micronoid: u64,
    /// Development share of the incomplete legacy day before the fork,
    /// which is never paid out.
    unpaid_legacy_share_micronoid: String,
}

fn v2_schedule(tip: u64, tip_timestamp: u64, expansions: &[u64]) -> V2Schedule {
    use permanode_core::emission::*;
    let eta = |height: u64| (height > tip).then(|| tip_timestamp + target_seconds_between(tip, height));
    let n = V2_REWARDS_MICRONOID.len();
    let current_tier = v2_tier(tip);
    let tiers = (0..n)
        .map(|tier| {
            let first = v2_tier_first_height(tier);
            let last = (tier + 1 < n).then(|| v2_tier_first_height(tier + 1) - 1);
            let reward = V2_REWARDS_MICRONOID[tier];
            RewardTier {
                tier,
                first_height: first,
                last_height: last,
                reward_micronoid: reward,
                interval_total_micronoid: last.map(|_| (reward as u128 * V2_REWARD_INTERVAL_BLOCKS as u128).to_string()),
                issued_at_end_micronoid: last.map(|l| emitted_up_to(l, expansions).to_string()),
                status: match current_tier {
                    Some(c) if tier < c => "done",
                    Some(c) if tier == c => "current",
                    _ => "upcoming",
                },
                eta_unix: eta(first),
            }
        })
        .collect();
    let next_reduction_height = match current_tier {
        None => Some(V2_ACTIVATION_HEIGHT),
        Some(c) if c + 1 < n => Some(v2_tier_first_height(c + 1)),
        Some(_) => None,
    };
    let last_legacy = V2_ACTIVATION_HEIGHT - 1;
    let legacy_log_slots = LOG_SLOTS_GENESIS + expansions.iter().filter(|h| **h <= last_legacy).count() as u32;
    let unpaid_blocks = last_legacy - (last_legacy / BLOCKS_PER_DAY) * BLOCKS_PER_DAY;
    let legacy_issued = emitted_up_to(last_legacy, expansions);
    V2Schedule {
        interval_blocks: V2_REWARD_INTERVAL_BLOCKS,
        tiers,
        current_tier,
        next_reduction_height,
        blocks_to_next_reduction: next_reduction_height.map(|h| h.saturating_sub(tip)),
        next_reduction_eta_unix: next_reduction_height.and_then(eta),
        next_reward_micronoid: next_reduction_height.map(|h| block_reward_at(h, legacy_log_slots)),
        interval_progress_bps: current_tier.filter(|c| c + 1 < n).map(|c| {
            (tip + 1 - v2_tier_first_height(c)) * 10_000 / V2_REWARD_INTERVAL_BLOCKS
        }),
        legacy_issued_micronoid: legacy_issued.to_string(),
        legacy_reward_micronoid: block_reward(legacy_log_slots),
        issued_since_activation_micronoid: if v2_active(tip) {
            (emitted_up_to(tip, expansions) - legacy_issued).to_string()
        } else {
            "0".to_string()
        },
        eight_interval_total_micronoid: V2_REWARDS_MICRONOID[..n - 1]
            .iter()
            .map(|r| *r as u128 * V2_REWARD_INTERVAL_BLOCKS as u128)
            .sum::<u128>()
            .to_string(),
        tail_height: v2_tier_first_height(n - 1),
        tail_reward_micronoid: V2_REWARDS_MICRONOID[n - 1],
        unpaid_legacy_share_micronoid: (2 * (block_reward(legacy_log_slots) / 20) as u128 * unpaid_blocks as u128).to_string(),
    }
}

/// Everything the halving page needs: where the live state stands
/// against the expansion threshold, the finalized window that decides
/// it, and a sampled history of the live-UTXO count from the permanent
/// headers. Mirrors `noid_chain::consensus::slot_expansion` (unchanged in v2.0.0):
/// the child of `tip` expands when a strict majority of the 18
/// hard-finalized headers `tip-35 ..= tip-18` report at least 75%
/// occupancy.
#[derive(serde::Serialize)]
struct HalvingResponse {
    tip: u64,
    log_slots: u32,
    capacity: u64,
    active_slots: u64,
    threshold: u64,
    trigger_pct: u64,
    window_size: u64,
    window_required: u64,
    window: Vec<WindowHeader>,
    history_step: u64,
    history: Vec<[u64; 2]>,
    /// Gross reward of the next block (legacy: by state size, v2: by height).
    block_reward_micronoid: u64,
    multiplier: u64,
    burn_per_new_slot_micronoid: u64,
    pressure_thresholds: Vec<PressureThreshold>,
    protocol: ProtocolInfo,
    v2: V2Schedule,
}

#[derive(serde::Serialize, Clone)]
struct WindowHeader {
    height: u64,
    active_slot_count: u64,
    qualifies: bool,
}

const EXPANSION_WINDOW: u64 = 18;
const CONSENSUS_FINALITY_DEPTH: u64 = 18;
const HISTORY_STEP: u64 = 512;
const HISTORY_MAX_POINTS: usize = 2400;

async fn get_halving(State(state): State<Arc<AppState>>) -> ApiResult<HalvingResponse> {
    let st = Arc::clone(&state);
    let resp = tokio::task::spawn_blocking(move || -> anyhow::Result<HalvingResponse> {
        let rpc = &st.rpc;
        let chain = rpc.get_chain_info()?;
        let info = rpc.get_state_info()?;
        let tip = chain.height;
        let capacity = 1u64 << chain.log_slots;
        let threshold = capacity / 4 * 3;
        let tip_timestamp = rpc.get_block_header(tip)?.map_or(0, |h| h.timestamp);
        let expansions = expansion_heights(&st, chain.log_slots, tip).unwrap_or_default();

        let window = finalized_window(&st, tip, capacity)?;
        let (history_step, samples) = sampled_state_history(&st, tip)?;
        let mut history: Vec<[u64; 2]> = samples.iter().map(|p| [p.height, p.active_slot_count]).collect();
        if history.last().map_or(true, |p| p[0] != tip) {
            history.push([tip, chain.active_slot_count]);
        }

        Ok(HalvingResponse {
            tip,
            log_slots: chain.log_slots,
            capacity,
            active_slots: chain.active_slot_count,
            threshold,
            trigger_pct: info.expand_trigger_pct,
            window_size: EXPANSION_WINDOW,
            window_required: EXPANSION_WINDOW / 2 + 1,
            window,
            history_step,
            history,
            block_reward_micronoid: permanode_core::emission::block_reward_at(tip + 1, chain.log_slots),
            multiplier: permanode_core::fees::pressure_multiplier(chain.active_slot_count, chain.log_slots),
            burn_per_new_slot_micronoid: permanode_core::fees::state_growth_fee_per_slot(chain.active_slot_count, chain.log_slots),
            pressure_thresholds: pressure_thresholds(capacity),
            protocol: protocol_info(tip, tip_timestamp),
            v2: v2_schedule(tip, tip_timestamp, &expansions),
        })
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(resp))
}

/// The 18 hard-finalized headers `tip-35 ..= tip-18` that decide whether
/// the child of `tip` expands, cached per tip.
fn finalized_window(st: &AppState, tip: u64, capacity: u64) -> anyhow::Result<Vec<WindowHeader>> {
    if let Some((cached_tip, window)) = st.window_cache.lock().unwrap_or_else(|e| e.into_inner()).as_ref() {
        if *cached_tip == tip {
            return Ok(window.clone());
        }
    }
    let mut window = Vec::new();
    if let Some(end) = tip.checked_sub(CONSENSUS_FINALITY_DEPTH) {
        if let Some(start) = end.checked_sub(EXPANSION_WINDOW - 1) {
            for h in start..=end {
                if let Some(header) = st.rpc.get_block_header(h)? {
                    let qualifies = header.active_slot_count.saturating_mul(4) >= capacity.saturating_mul(3);
                    window.push(WindowHeader { height: h, active_slot_count: header.active_slot_count, qualifies });
                }
            }
        }
    }
    *st.window_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((tip, window.clone()));
    Ok(window)
}

/// Header samples every `step` blocks from genesis up to `tip` (the tip
/// itself only when it falls on the step), extended incrementally from
/// the cache so only the first request pays for the full walk.
fn sampled_state_history(st: &AppState, tip: u64) -> anyhow::Result<(u64, Vec<HeaderSample>)> {
    let mut cache = st.state_history.lock().unwrap_or_else(|e| e.into_inner());
    if cache.step == 0 {
        cache.step = HISTORY_STEP;
    }
    while (tip / cache.step) as usize > HISTORY_MAX_POINTS {
        cache.step *= 2;
        let step = cache.step;
        cache.points.retain(|p| p.height % step == 0);
    }
    let step = cache.step;
    let mut next = cache.points.last().map_or(0, |p| p.height + step);
    while next <= tip {
        match st.rpc.get_block_header(next)? {
            Some(header) => cache.points.push(HeaderSample {
                height: next,
                active_slot_count: header.active_slot_count,
                alloc_counter: header.alloc_counter,
            }),
            None => break,
        }
        next += step;
    }
    Ok((step, cache.points.clone()))
}

/// Live outputs created by transactions (not by coinbases or development
/// payouts) up to `height`, from the header's mint counter: every block
/// mints one coinbase output, every payout block two more (legacy every
/// 4,320th block, v2 on its own 2,880-block rhythm).
fn user_mints(sample: &HeaderSample) -> u64 {
    let minted_by_consensus = sample.height + 2 * permanode_core::emission::development_payout_blocks_up_to(sample.height);
    sample.alloc_counter.saturating_sub(minted_by_consensus)
}

#[derive(serde::Serialize, Clone)]
struct PressureThreshold {
    pct: u64,
    /// Live UTXOs at which the tier starts (integer occupancy in basis
    /// points, as the node computes it).
    slots: u64,
    multiplier: u64,
}

fn pressure_thresholds(capacity: u64) -> Vec<PressureThreshold> {
    use permanode_core::fees::{PRESSURE_EXTREME_BPS, PRESSURE_HIGH_BPS, PRESSURE_LOW_BPS};
    [(PRESSURE_LOW_BPS, 2), (PRESSURE_HIGH_BPS, 4), (PRESSURE_EXTREME_BPS, 8)]
        .into_iter()
        .map(|(bps, multiplier)| PressureThreshold {
            pct: bps / 100,
            slots: ((capacity as u128 * bps as u128).div_ceil(10_000)) as u64,
            multiplier,
        })
        .collect()
}

/// The economics page: supply, burn, state pressure, development
/// allocation and the recorded state activity.
#[derive(serde::Serialize)]
struct EconomicsResponse {
    tip: u64,
    log_slots: u32,
    capacity: u64,
    active_slots: u64,
    occupancy_bps: u64,
    multiplier: u64,
    burn_per_new_slot_micronoid: u64,
    block_reward_micronoid: u64,
    next_block_reward_micronoid: u64,
    threshold: u64,
    trigger_pct: u64,
    pressure_thresholds: Vec<PressureThreshold>,
    window_size: u64,
    window_required: u64,
    window_qualifying: u64,
    /// Mirrored emission schedule up to the tip, and what consensus has
    /// burned of it (issued minus the node's supply).
    total_issued_micronoid: String,
    total_burned_micronoid: String,
    net_supply_micronoid: String,
    /// Target blocks per year at the next block's interval (20 s: 1,576,800,
    /// v2 30 s: 1,051,200).
    blocks_per_year: u64,
    /// Current reward times the target blocks per year - a projection at
    /// target block time, not a guaranteed figure.
    annualized_issuance_micronoid: String,
    development: DevelopmentAllocation,
    protocol: ProtocolInfo,
    v2: V2Schedule,
    history_step: u64,
    /// Issued from genesis at every sample; burned exact where the
    /// records allow it (the total at the tip walked backwards through
    /// the recorded blocks) and estimated from the headers' mint counter
    /// before that, see `EconomicsPoint`.
    history: Vec<EconomicsPoint>,
    /// Recorded state activity over the last 24 hours, 7 days, 30 days.
    periods: Vec<PeriodActivity>,
    /// The least that reaching the expansion threshold can burn: every
    /// missing slot created exactly once, charged at the multiplier of
    /// the occupancy band it falls into (the multiplier rises on the way).
    min_burn_to_expansion_micronoid: String,
    min_burn_bands: Vec<BurnBand>,
}

#[derive(serde::Serialize)]
struct BurnBand {
    from_pct: u64,
    to_pct: u64,
    multiplier: u64,
    /// Slots still to be filled inside this band from the current
    /// occupancy; zero once the band has been passed.
    slots_to_fill: u64,
    per_slot_micronoid: u64,
    burn_micronoid: String,
}

/// Splits the slots between the current occupancy and the expansion
/// threshold into the pressure bands they fall into.
fn min_burn_bands(active: u64, log_slots: u32, thresholds: &[PressureThreshold], expansion_threshold: u64) -> Vec<BurnBand> {
    let mut bounds: Vec<(u64, u64)> = vec![(0, 0)];
    bounds.extend(thresholds.iter().filter(|t| t.slots < expansion_threshold).map(|t| (t.pct, t.slots)));
    let mut bands = Vec::new();
    for (i, &(from_pct, from_slots)) in bounds.iter().enumerate() {
        let (to_pct, to_slots) = bounds.get(i + 1).copied().unwrap_or((permanode_core::fees::PRESSURE_HIGH_BPS / 100, expansion_threshold));
        let lo = active.max(from_slots);
        let slots_to_fill = to_slots.saturating_sub(lo);
        // the multiplier in force while filling this band: that of its lower edge
        let per_slot = permanode_core::fees::state_growth_fee_per_slot(from_slots, log_slots);
        bands.push(BurnBand {
            from_pct,
            to_pct,
            multiplier: permanode_core::fees::pressure_multiplier(from_slots, log_slots),
            slots_to_fill,
            per_slot_micronoid: per_slot,
            burn_micronoid: (slots_to_fill as u128 * per_slot as u128).to_string(),
        });
    }
    bands
}

#[derive(serde::Serialize)]
struct DevelopmentAllocation {
    /// Last block of the allocation under the rules in force for the next
    /// block: the pre-v2 end until the fork, the converted v2 end from then
    /// on (like every other field here, which follows the next payout).
    end_height: u64,
    /// Last block under v2, which converts the remaining three years into
    /// 30 s blocks.
    v2_end_height: u64,
    /// Where it ends under the pre-v2 rule (never reached once v2 is live).
    legacy_end_height: u64,
    /// Blocks per payout at the next payout (4,320 legacy, 2,880 v2).
    payout_interval: u64,
    active: bool,
    next_payout_height: Option<u64>,
    /// Per fund, at the next payout.
    payout_per_fund_micronoid: u64,
    last_legacy_payout_height: u64,
    first_v2_payout_height: u64,
    /// Blocks covered by the final, partial v2 payout at `v2_end_height`.
    final_partial_blocks: u64,
    cumulative_miner_micronoid: String,
    /// Each of the two funds has received this much so far.
    cumulative_per_fund_micronoid: String,
}

#[derive(serde::Serialize)]
struct EconomicsPoint {
    height: u64,
    issued_micronoid: String,
    /// Exact, where this permanode's records reach (and at the tip).
    burned_micronoid: Option<String>,
    /// Before the records begin: scaled from the headers' mint counter so
    /// the curve runs from zero at genesis to the first measured value.
    burned_estimate_micronoid: Option<String>,
}

#[derive(serde::Serialize, Clone)]
struct PeriodActivity {
    label: &'static str,
    seconds: i64,
    #[serde(flatten)]
    activity: queries::StateActivity,
}

async fn get_economics(State(state): State<Arc<AppState>>) -> ApiResult<EconomicsResponse> {
    let now_unix = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let (burn_by_block, periods) = {
        let conn = state.db();
        let tip = queries::indexed_tip(&conn)?.unwrap_or(0);
        let cached = {
            let cache = state.period_cache.lock().unwrap_or_else(|e| e.into_inner());
            cache.as_ref().filter(|(h, at, _)| *h == tip && now_unix - *at < 60).map(|(_, _, p)| p.clone())
        };
        let periods = match cached {
            Some(p) => p,
            None => {
                let mut periods = Vec::new();
                for (label, seconds) in [("24h", 86_400i64), ("7d", 7 * 86_400), ("30d", 30 * 86_400)] {
                    periods.push(PeriodActivity { label, seconds, activity: queries::state_activity(&conn, now_unix - seconds)? });
                }
                *state.period_cache.lock().unwrap_or_else(|e| e.into_inner()) = Some((tip, now_unix, periods.clone()));
                periods
            }
        };
        (queries::burn_by_block(&conn)?, periods)
    };

    let st = Arc::clone(&state);
    let resp = tokio::task::spawn_blocking(move || -> anyhow::Result<EconomicsResponse> {
        use permanode_core::{emission, fees};
        let rpc = &st.rpc;
        let chain = rpc.get_chain_info()?;
        let info = rpc.get_state_info()?;
        let tip = chain.height;
        let capacity = 1u64 << chain.log_slots;
        let expansions = expansion_heights(&st, chain.log_slots, tip).unwrap_or_default();
        let split = emission::emitted_split_up_to(tip, &expansions);
        let issued = split.miner + split.development;
        let supply: u128 = chain.circulating_supply_micronoid.parse().unwrap_or(0);
        let burned = issued.saturating_sub(supply);
        let reward = emission::block_reward_at(tip + 1, chain.log_slots);
        let blocks_per_year = 365 * 86_400 / emission::block_time_at(tip + 1);
        let tip_timestamp = rpc.get_block_header(tip)?.map_or(0, |h| h.timestamp);
        let next_payout = emission::next_development_payout_height(tip);
        let window = finalized_window(&st, tip, capacity)?;
        let (history_step, mut samples) = sampled_state_history(&st, tip)?;
        if samples.last().map_or(true, |p| p.height != tip) {
            // the tip only needs its exact burn; its mint counter is unused
            samples.push(HeaderSample { height: tip, active_slot_count: chain.active_slot_count, alloc_counter: 0 });
        }

        // Measured: burned(h) = burned(tip) - burn of every recorded block
        // above h, exact wherever the records reach.
        let first_recorded = burn_by_block.first().map(|(h, _)| *h);
        let mut history = Vec::with_capacity(samples.len());
        let mut remaining = burned;
        let mut idx = burn_by_block.len();
        for sample in samples.iter().rev() {
            let height = sample.height;
            while idx > 0 && burn_by_block[idx - 1].0 > height {
                idx -= 1;
                remaining = remaining.saturating_sub(burn_by_block[idx].1);
            }
            let known = first_recorded.is_some_and(|f| height >= f) || height == tip;
            history.push(EconomicsPoint {
                height,
                issued_micronoid: emission::emitted_up_to(height, &expansions).to_string(),
                burned_micronoid: known.then(|| remaining.to_string()),
                burned_estimate_micronoid: None,
            });
        }
        history.reverse();
        // Estimated before the records begin: the burn is charged per
        // net-new slot, so it grows with the outputs transactions create -
        // a count the headers carry exactly. Scale that curve so it meets
        // the first measured value; genesis minted nothing, so it starts
        // at zero.
        if let Some(anchor) = history.iter().position(|p| p.burned_micronoid.is_some()) {
            let anchor_burn: u128 = history[anchor].burned_micronoid.as_deref().and_then(|b| b.parse().ok()).unwrap_or(0);
            let anchor_mints = user_mints(&samples[anchor]);
            for i in 0..anchor {
                let est = if anchor_mints > 0 { anchor_burn * user_mints(&samples[i]) as u128 / anchor_mints as u128 } else { 0 };
                history[i].burned_estimate_micronoid = Some(est.to_string());
            }
            history[anchor].burned_estimate_micronoid = Some(anchor_burn.to_string());
        }

        let dev_active = emission::development_allocation_active(tip + 1);
        let thresholds = pressure_thresholds(capacity);
        let expansion_threshold = capacity / 4 * 3;
        let min_burn_bands = min_burn_bands(chain.active_slot_count, chain.log_slots, &thresholds, expansion_threshold);
        let min_burn_total: u128 = min_burn_bands.iter().map(|b| b.burn_micronoid.parse::<u128>().unwrap_or(0)).sum();
        Ok(EconomicsResponse {
            tip,
            log_slots: chain.log_slots,
            capacity,
            active_slots: chain.active_slot_count,
            occupancy_bps: fees::occupancy_bps(chain.active_slot_count, chain.log_slots),
            multiplier: fees::pressure_multiplier(chain.active_slot_count, chain.log_slots),
            burn_per_new_slot_micronoid: fees::state_growth_fee_per_slot(chain.active_slot_count, chain.log_slots),
            block_reward_micronoid: reward,
            next_block_reward_micronoid: emission::block_reward(chain.log_slots + 1),
            threshold: expansion_threshold,
            trigger_pct: info.expand_trigger_pct,
            pressure_thresholds: thresholds,
            window_size: EXPANSION_WINDOW,
            window_required: EXPANSION_WINDOW / 2 + 1,
            window_qualifying: window.iter().filter(|w| w.qualifies).count() as u64,
            min_burn_to_expansion_micronoid: min_burn_total.to_string(),
            min_burn_bands,
            total_issued_micronoid: issued.to_string(),
            total_burned_micronoid: burned.to_string(),
            net_supply_micronoid: supply.to_string(),
            blocks_per_year,
            annualized_issuance_micronoid: (reward as u128 * blocks_per_year as u128).to_string(),
            protocol: protocol_info(tip, tip_timestamp),
            v2: v2_schedule(tip, tip_timestamp, &expansions),
            development: DevelopmentAllocation {
                end_height: if emission::v2_active(tip + 1) {
                    emission::DEVELOPMENT_ALLOCATION_END_HEIGHT
                } else {
                    emission::LEGACY_DEVELOPMENT_ALLOCATION_END_HEIGHT
                },
                v2_end_height: emission::DEVELOPMENT_ALLOCATION_END_HEIGHT,
                legacy_end_height: emission::LEGACY_DEVELOPMENT_ALLOCATION_END_HEIGHT,
                payout_interval: if next_payout.is_some_and(emission::v2_active) {
                    emission::V2_BLOCKS_PER_DAY
                } else {
                    emission::BLOCKS_PER_DAY
                },
                active: dev_active,
                next_payout_height: next_payout,
                payout_per_fund_micronoid: next_payout.map_or(0, |h| emission::development_payout(h, chain.log_slots) / 2),
                last_legacy_payout_height: (emission::V2_ACTIVATION_HEIGHT - 1) / emission::BLOCKS_PER_DAY * emission::BLOCKS_PER_DAY,
                first_v2_payout_height: emission::V2_ACTIVATION_HEIGHT - 1 + emission::V2_BLOCKS_PER_DAY,
                final_partial_blocks: (emission::DEVELOPMENT_ALLOCATION_END_HEIGHT - (emission::V2_ACTIVATION_HEIGHT - 1)) % emission::V2_BLOCKS_PER_DAY,
                cumulative_miner_micronoid: split.miner.to_string(),
                cumulative_per_fund_micronoid: (split.development / 2).to_string(),
            },
            history_step,
            history,
            periods,
        })
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(resp))
}

enum ApiErrorOr404 {
    Error(ApiError),
    NotFound,
    /// A query parameter outside what the endpoint accepts.
    BadRequest(&'static str),
}
impl<E: Into<anyhow::Error>> From<E> for ApiErrorOr404 {
    fn from(e: E) -> Self {
        ApiErrorOr404::Error(ApiError(e.into()))
    }
}
impl IntoResponse for ApiErrorOr404 {
    fn into_response(self) -> axum::response::Response {
        match self {
            ApiErrorOr404::Error(e) => e.into_response(),
            ApiErrorOr404::NotFound => NotFound.into_response(),
            ApiErrorOr404::BadRequest(msg) => (StatusCode::BAD_REQUEST, Json(serde_json::json!({ "error": msg }))).into_response(),
        }
    }
}
