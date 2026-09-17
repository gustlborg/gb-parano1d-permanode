use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use clap::Parser;
use permanode_core::{db, live_rpc, queries};
use rusqlite::Connection;
use serde::Deserialize;
use std::path::PathBuf;
use std::sync::{Arc, Mutex};
use tower_http::cors::CorsLayer;
use tower_http::services::{ServeDir, ServeFile};

#[derive(Parser, Debug)]
#[command(version)]
struct Args {
    /// SQLite database written by the indexer.
    #[arg(long, default_value = "permanode.sqlite3")]
    db_path: PathBuf,

    /// Directory of static frontend files to serve.
    #[arg(long, default_value = "site")]
    site_dir: PathBuf,

    /// Address to listen on.
    #[arg(long, default_value = "127.0.0.1:8420")]
    listen: String,

    /// Node JSON-RPC endpoint, used only for the live mempool view (block
    /// data always comes from the indexer's database, never from here).
    #[arg(long, default_value = "http://127.0.0.1:9601")]
    rpc_url: String,
}

struct AppState {
    conn: Mutex<Connection>,
    rpc: live_rpc::RpcClient,
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

#[tokio::main]
async fn main() -> Result<()> {
    env_logger::Builder::from_env(env_logger::Env::default().default_filter_or("info")).init();
    let args = Args::parse();

    let conn = db::open(args.db_path.to_str().expect("db_path must be valid UTF-8"))?;
    let state = Arc::new(AppState {
        conn: Mutex::new(conn),
        rpc: live_rpc::RpcClient::new(args.rpc_url.clone()),
    });

    let index_file = args.site_dir.join("index.html");
    let static_service = ServeDir::new(&args.site_dir).fallback(ServeFile::new(index_file));

    let app = Router::new()
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
        .fallback_service(static_service)
        .layer(CorsLayer::permissive())
        .with_state(state);

    log::info!("listening on http://{}", args.listen);
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

#[derive(serde::Serialize)]
struct StatsResponse {
    #[serde(flatten)]
    chain: queries::ChainStats,
    network: NetworkMetrics,
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

    let rpc_client = state.rpc.clone();
    let (active_slots, chain_info, mining_info) = tokio::task::spawn_blocking(move || {
        (
            rpc_client.get_active_slot_count().ok(),
            rpc_client.get_chain_info().ok(),
            rpc_client.get_mining_info().ok(),
        )
    })
    .await
    .unwrap_or((None, None, None));

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
    };

    Ok(Json(StatsResponse { chain, network }))
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
) -> Result<Json<queries::BlockDetail>, ApiErrorOr404> {
    let conn = state.db();
    match queries::block_by_height(&conn, height)? {
        Some(b) => Ok(Json(b)),
        None => Err(ApiErrorOr404::NotFound),
    }
}

async fn get_block_by_hash(
    State(state): State<Arc<AppState>>,
    Path(hash): Path<String>,
) -> Result<Json<queries::BlockDetail>, ApiErrorOr404> {
    let conn = state.db();
    match queries::block_by_hash(&conn, &hash)? {
        Some(b) => Ok(Json(b)),
        None => Err(ApiErrorOr404::NotFound),
    }
}

async fn get_tx(
    State(state): State<Arc<AppState>>,
    Path(txid): Path<String>,
) -> Result<Json<queries::TxDetail>, ApiErrorOr404> {
    let conn = state.db();
    match queries::tx_by_txid(&conn, &txid)? {
        Some(t) => Ok(Json(t)),
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
) -> ApiResult<AddressPage> {
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(25).clamp(1, 100);
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
) -> ApiResult<AddressUtxosPage> {
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
