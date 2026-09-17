use anyhow::Result;
use axum::{
    extract::{Path, Query, State},
    http::StatusCode,
    response::{IntoResponse, Json},
    routing::get,
    Router,
};
use clap::Parser;
use permanode_core::{db, queries};
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
}

struct AppState {
    conn: Mutex<Connection>,
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
        .route("/api/v1/gaps", get(get_gaps))
        .fallback_service(static_service)
        .layer(CorsLayer::permissive())
        .with_state(state);

    log::info!("listening on http://{}", args.listen);
    let listener = tokio::net::TcpListener::bind(&args.listen).await?;
    axum::serve(listener, app).await?;
    Ok(())
}

async fn get_stats(State(state): State<Arc<AppState>>) -> ApiResult<queries::ChainStats> {
    let conn = state.conn.lock().unwrap();
    Ok(Json(queries::chain_stats(&conn)?))
}

#[derive(Deserialize)]
struct LimitQuery {
    limit: Option<i64>,
}

async fn get_blocks(
    State(state): State<Arc<AppState>>,
    Query(q): Query<LimitQuery>,
) -> ApiResult<Vec<queries::BlockSummary>> {
    let conn = state.conn.lock().unwrap();
    let limit = q.limit.unwrap_or(25).clamp(1, 200);
    Ok(Json(queries::recent_blocks(&conn, limit)?))
}

async fn get_block_by_height(
    State(state): State<Arc<AppState>>,
    Path(height): Path<i64>,
) -> Result<Json<queries::BlockDetail>, ApiErrorOr404> {
    let conn = state.conn.lock().unwrap();
    match queries::block_by_height(&conn, height)? {
        Some(b) => Ok(Json(b)),
        None => Err(ApiErrorOr404::NotFound),
    }
}

async fn get_block_by_hash(
    State(state): State<Arc<AppState>>,
    Path(hash): Path<String>,
) -> Result<Json<queries::BlockDetail>, ApiErrorOr404> {
    let conn = state.conn.lock().unwrap();
    match queries::block_by_hash(&conn, &hash)? {
        Some(b) => Ok(Json(b)),
        None => Err(ApiErrorOr404::NotFound),
    }
}

async fn get_tx(
    State(state): State<Arc<AppState>>,
    Path(txid): Path<String>,
) -> Result<Json<queries::TxDetail>, ApiErrorOr404> {
    let conn = state.conn.lock().unwrap();
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
}

async fn get_address(
    State(state): State<Arc<AppState>>,
    Path(address): Path<String>,
    Query(q): Query<AddressQuery>,
) -> ApiResult<AddressPage> {
    let conn = state.conn.lock().unwrap();
    let page = q.page.unwrap_or(1).max(1);
    let page_size = q.page_size.unwrap_or(25).clamp(1, 100);
    let (transactions, total) = queries::txs_by_address(&conn, &address, page, page_size)?;
    Ok(Json(AddressPage {
        address,
        page,
        page_size,
        total,
        transactions,
    }))
}

async fn get_gaps(State(state): State<Arc<AppState>>) -> ApiResult<Vec<queries::GapEntry>> {
    let conn = state.conn.lock().unwrap();
    Ok(Json(queries::recent_gaps(&conn, 100)?))
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
