//! The explorer: JSON API over the indexer's database plus the static
//! frontend, which is compiled into the binary so a self-hoster only ever
//! deals with one file. Runs in the same process as the indexer by
//! default (see main.rs), on its own database connection.

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
use tower_http::services::{ServeDir, ServeFile};

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
}

#[derive(Default)]
struct StateHistory {
    step: u64,
    points: Vec<(u64, u64)>,
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
        .route("/api/v1/richlist", get(get_richlist))
        .route("/api/v1/mempool", get(get_mempool))
        .route("/api/v1/halving", get(get_halving));

    let app = match &cfg.site_dir {
        Some(dir) => {
            log::info!("serving frontend from {dir} instead of the built-in copy");
            let index_file = std::path::Path::new(dir).join("index.html");
            api.fallback_service(ServeDir::new(dir).fallback(ServeFile::new(index_file)))
        }
        None => api.fallback(embedded_site),
    }
    .layer(CorsLayer::permissive())
    .with_state(state);

    log::info!("explorer listening on http://{}", cfg.listen);
    let listener = tokio::net::TcpListener::bind(&cfg.listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

/// Serves the frontend compiled into the binary. Unknown paths get
/// index.html so the client-side router can take over (block/tx/address
/// URLs are all handled there), the same fallback ServeDir does in
/// `site_dir` mode.
async fn embedded_site(uri: Uri, headers: axum::http::HeaderMap) -> Response {
    let path = uri.path().trim_start_matches('/');
    let (file, spa) = match SITE.get_file(path) {
        Some(f) => (f, false),
        None => match SITE.get_file("index.html") {
            Some(f) => (f, true),
            None => return (StatusCode::NOT_FOUND, "not found").into_response(),
        },
    };
    let mime = if spa {
        mime_guess::mime::TEXT_HTML_UTF_8
    } else {
        mime_guess::from_path(file.path()).first_or_octet_stream()
    };
    // Fonts and icons practically never change and may sit in caches for
    // a day. Everything else (HTML, scripts, styles) is revalidated on
    // every load via the ETag, so a new build shows up immediately at the
    // cost of a 304 round trip.
    let long_lived = matches!(mime.type_().as_str(), "font" | "image");
    let cache = if long_lived { "public, max-age=86400" } else { "no-cache" };
    let etag = etag_of(file.contents());
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
        file.contents(),
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
    let (active_slots, chain_info, mining_info, state_info, expansions) = tokio::task::spawn_blocking(move || {
        let rpc = &state_for_rpc.rpc;
        let chain_info = rpc.get_chain_info().ok();
        let expansions = chain_info.as_ref().and_then(|c| expansion_heights(&state_for_rpc, c.log_slots, c.height));
        (
            rpc.get_active_slot_count().ok(),
            chain_info,
            rpc.get_mining_info().ok(),
            rpc.get_state_info().ok(),
            expansions,
        )
    })
    .await
    .unwrap_or((None, None, None, None, None));

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

async fn get_blocks(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Vec<queries::BlockSummary>> {
    let conn = state.db();
    let limit = q.limit.unwrap_or(25).clamp(1, 200);
    Ok(Json(queries::recent_blocks(&conn, limit)?))
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
    let conn = state.db();
    match queries::tx_by_txid(&conn, &txid)? {
        Some(t) => {
            let is_final = t.block.canonical && t.confirmations.is_some_and(|c| c >= FINAL_CONFIRMATIONS);
            Ok(cached_json(t, is_final))
        }
        None => Err(ApiErrorOr404::NotFound),
    }
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
    transactions: Vec<queries::TxSummary>,
    /// Computed from this permanode's own recorded history only.
    balance: queries::AddressBalance,
    /// Read live from the node's current Live State
    /// (paranoid_getSlotsByOwner) - correct regardless of when this
    /// permanode started recording. `null` if the node couldn't be
    /// reached for this.
    live_balance_micronoid: Option<String>,
    live_utxo_count: Option<u64>,
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
    let (transactions, total, balance) = {
        let conn = state.db();
        let (transactions, total) = queries::txs_by_address(&conn, &address, page, page_size)?;
        let balance = queries::address_balance(&conn, &address)?;
        (transactions, total, balance)
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
        transactions,
        balance,
        live_balance_micronoid,
        live_utxo_count,
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

async fn get_richlist(State(state): State<Arc<AppState>>) -> ApiResult<Vec<queries::RichListEntry>> {
    let conn = state.db();
    Ok(Json(queries::richlist(&conn, 100)?))
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

/// Everything the halving page needs: where the live state stands
/// against the expansion threshold, the finalized window that decides
/// it, and a sampled history of the live-UTXO count from the permanent
/// headers. Mirrors `noid_chain::consensus::slot_expansion` (v1.1.0):
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
    block_reward_micronoid: u64,
}

#[derive(serde::Serialize)]
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

        // finalized window deciding the child of `tip`
        let mut window = Vec::new();
        if let Some(end) = tip.checked_sub(CONSENSUS_FINALITY_DEPTH) {
            if let Some(start) = end.checked_sub(EXPANSION_WINDOW - 1) {
                for h in start..=end {
                    if let Some(header) = rpc.get_block_header(h)? {
                        let qualifies = header.active_slot_count.saturating_mul(4) >= capacity.saturating_mul(3);
                        window.push(WindowHeader { height: h, active_slot_count: header.active_slot_count, qualifies });
                    }
                }
            }
        }

        // sampled history, extended incrementally
        let (history_step, history) = {
            let mut cache = st.state_history.lock().unwrap_or_else(|e| e.into_inner());
            if cache.step == 0 {
                cache.step = HISTORY_STEP;
            }
            while (tip / cache.step) as usize > HISTORY_MAX_POINTS {
                cache.step *= 2;
                let step = cache.step;
                cache.points.retain(|(h, _)| h % step == 0);
            }
            let step = cache.step;
            let mut next = cache.points.last().map_or(0, |(h, _)| h + step);
            while next <= tip {
                match rpc.get_block_header(next)? {
                    Some(header) => cache.points.push((next, header.active_slot_count)),
                    None => break,
                }
                next += step;
            }
            let mut points: Vec<[u64; 2]> = cache.points.iter().map(|(h, a)| [*h, *a]).collect();
            if points.last().map_or(true, |p| p[0] != tip) {
                points.push([tip, chain.active_slot_count]);
            }
            (step, points)
        };

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
            block_reward_micronoid: permanode_core::emission::block_reward(chain.log_slots),
        })
    })
    .await
    .map_err(anyhow::Error::from)??;
    Ok(Json(resp))
}

enum ApiErrorOr404 {
    Error(ApiError),
    NotFound,
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
        }
    }
}
