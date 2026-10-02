//! Elasticsearch エンジン。
//!
//! エディタは Kibana Console 風のリクエストブロックを扱う:
//! `GET /index/_search` のようなメソッド行 + 続く行の JSON body (省略可)。
//! `#` 始まりの行はコメント。body が複数の JSON オブジェクト (NDJSON) の
//! 場合は `_bulk` 用にそのまま送る。複数ブロックの選択実行は順次実行して
//! request / status / result の表形式で返す。
//!
//! - sqlx は使わず reqwest で REST API を直接叩く (公式 elasticsearch crate は
//!   長年 alpha のため使わない)。`EsClient` は base_url と認証情報のみ持つ。
//! - readonly ガードは GET / HEAD 常時許可 + POST は検索系エンドポイントの
//!   ホワイトリスト。PUT / DELETE / PATCH と他の POST は Writable 必須。
//! - 危険ガードはインデックス削除 (DELETE の単一セグメントパス) と
//!   `_delete_by_query` (SQL の WHERE 無し DELETE 相当)。
//! - すべての HTTP リクエストにタイムアウトを掛け、応答はバイト数上限で
//!   読み切る (非有界の応答で webview / メモリを溢れさせない)。
//! - キャンセルはクライアント側で future を打ち切る (`CancelTarget::ClientSide`)。
//!   サーバー側の実行は止まらないが、接続プールを持たないため壊れる状態は無い。

use std::sync::atomic::AtomicBool;
use std::sync::Arc;
use std::time::Instant;

use crate::config::ServerConfig;
use crate::db::{
    dangerous_block_error, readonly_block_error, CancelRegistry, CancelTarget, QueryResult,
    ReadonlyGuard,
};
use crate::error::AppError;
use crate::schema_info::{ColumnInfo, TableInfo};

pub const DEFAULT_PORT: u16 = 9200;

/// 接続確認 (GET /) のタイムアウト。タイムアウト無しの確認リクエストは
/// get_pool (DbManager のロック保持中) を無期限に止めるため必須。
const CONNECT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(15);

/// クエリ実行のリクエストタイムアウト。
const REQUEST_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(120);

/// 応答 body の読み込み上限 (メモリ保護)。超えたらエラーにする。
const MAX_RESPONSE_BYTES: usize = 20 * 1024 * 1024;

/// 1 セルに入れる pretty JSON / テキストの文字数上限。
/// 超えた分は打ち切って truncated を立てる (非有界データを webview に送らない)。
const MAX_RESPONSE_CHARS: usize = 100_000;

/// 表のセルに入れる文字列の文字数上限 (hits の _source 値など)。
const MAX_CELL_CHARS: usize = 10_000;

/// 表のセルに入れるコレクション (配列 / オブジェクト) の要素数上限。
const MAX_CELL_ITEMS: usize = 1_000;

const METHODS: &[&str] = &["GET", "POST", "PUT", "DELETE", "HEAD", "PATCH"];

/// readonly (Writable OFF) でも許可する POST の API セグメント。
/// パス中の最初の `_` 始まりセグメントを API とみなし、後続サブセグメントは
/// `readonly_post_allowed` で API ごとに検証する (`/index/_doc/_search` の
/// ようなドキュメント ID を使ったすり抜けを防ぐ)。
const READONLY_POST_APIS: &[&str] = &[
    "_search", "_msearch", "_count", "_analyze", "_mget", "_field_caps",
    "_validate", "_explain", "_termvectors", "_pit", "_sql", "_render",
];

/// Elasticsearch への接続クライアント。プールは持たず、リクエストごとに
/// reqwest の内部コネクションプールを使う。
#[derive(Clone)]
pub struct EsClient {
    /// 例: `http://127.0.0.1:9200` (末尾スラッシュ無し)
    base_url: String,
    client: reqwest::Client,
    user: Option<String>,
    password: Option<String>,
}

/// エディタ入力からパースした 1 リクエスト。
#[derive(Debug, PartialEq)]
struct EsRequest {
    /// 大文字の HTTP メソッド
    method: String,
    /// `/` 始まりに正規化したパス (クエリ文字列を含み得る)
    path: String,
    /// JSON / NDJSON の body (無ければ None)
    body: Option<String>,
}

impl EsRequest {
    /// 表示用 (複数リクエスト結果の request カラム用)
    fn display(&self) -> String {
        format!("{} {}", self.method, self.path)
    }
}

/// SSH トンネル + TLS の時に URL へ残すホスト名を返す (無ければ None)。
/// DbManager はトンネル確立後に接続先を 127.0.0.1:<local_port> に差し替えるが、
/// URL のホストまで 127.0.0.1 にすると reqwest の SNI / 証明書ホスト名検証が
/// 127.0.0.1 に対して行われ、実ホスト名で発行された証明書の検証に失敗する。
/// URL には設定上のホスト名を残し、実際の接続先だけ resolve() で
/// 127.0.0.1:<local_port> へ向ける。
fn tls_tunnel_url_host(server: &ServerConfig, dial_host: &str) -> Option<String> {
    if !(server.tls && server.ssh_tunnel.is_some() && dial_host == "127.0.0.1") {
        return None;
    }
    server
        .host
        .as_deref()
        .map(str::trim)
        .filter(|h| !h.is_empty())
        .map(str::to_string)
}

/// 接続を確立して疎通確認 (GET /) まで行う。
/// base_url は host / port と `tls: true` (queryfolio 独自拡張) から組み立てる。
pub async fn connect(
    server: &ServerConfig,
    host: &str,
    port: u16,
) -> Result<EsClient, AppError> {
    let scheme = if server.tls { "https" } else { "http" };
    let mut builder = reqwest::Client::builder()
        .timeout(REQUEST_TIMEOUT)
        .connect_timeout(CONNECT_TIMEOUT)
        // redirect は追わない: build_url の origin 検証は最初の URL にしか
        // 効かないため、追うと 302 で別 origin へ飛ばされ得る (SSRF 的挙動)
        .redirect(reqwest::redirect::Policy::none());
    // SSH トンネル + TLS: URL は設定上のホスト名のまま (SNI / 証明書検証と
    // Host ヘッダを実ホストで行う)、接続先だけトンネルのローカルポートへ向ける
    let url_host = match tls_tunnel_url_host(server, host) {
        Some(original) => {
            builder = builder.resolve(
                &original,
                std::net::SocketAddr::from(([127, 0, 0, 1], port)),
            );
            original
        }
        None => host.to_string(),
    };
    let base_url = format!("{scheme}://{url_host}:{port}");
    // base_url が URL として不正 (ホスト名に変な文字等) なら早期にエラー
    reqwest::Url::parse(&base_url)
        .map_err(|e| AppError::Elasticsearch(format!("Invalid server address {base_url}: {e}")))?;
    let client = builder
        .build()
        .map_err(|e| {
            AppError::Elasticsearch(format!("Failed to build the HTTP client: {e}"))
        })?;
    let es = EsClient {
        base_url,
        client,
        user: server
            .user
            .as_deref()
            .filter(|u| !u.trim().is_empty())
            .map(str::to_string),
        password: server.password.clone(),
    };
    // sqlx の connect_with と同様、接続時点で到達性と認証を確認する。
    // 確認リクエストにもタイムアウトを掛ける: TCP は繋がるのに応答しない相手
    // (止まった SSH トンネル等) で get_pool が無期限に停止しないようにする
    let root = EsRequest {
        method: "GET".into(),
        path: "/".into(),
        body: None,
    };
    let confirm = send_request(&es, &root);
    match tokio::time::timeout(CONNECT_TIMEOUT, confirm).await {
        Ok(Ok((status, body))) => {
            if !status.is_success() {
                return Err(http_status_error(status, &body));
            }
        }
        Ok(Err(e)) => return Err(e),
        Err(_) => {
            return Err(AppError::Elasticsearch(format!(
                "The server did not respond within {}s",
                CONNECT_TIMEOUT.as_secs()
            )));
        }
    }
    Ok(es)
}

impl EsClient {
    /// base_url + path から送信先 URL を組み立てる。
    /// パスは常に `/` 始まりなので authority (host:port) は変わらない。
    /// 多重防御として、パース結果の origin が base と一致することも確認する。
    fn build_url(&self, path: &str) -> Result<reqwest::Url, AppError> {
        let url = reqwest::Url::parse(&format!("{}{}", self.base_url, path))
            .map_err(|e| AppError::Elasticsearch(format!("Invalid request path {path}: {e}")))?;
        let base = reqwest::Url::parse(&self.base_url)
            .map_err(|e| AppError::Elasticsearch(format!("Invalid server address: {e}")))?;
        if url.origin() != base.origin() {
            return Err(AppError::Elasticsearch(format!(
                "The request path must stay on the configured server: {path}"
            )));
        }
        Ok(url)
    }
}

/// HTTP エラーステータスを ES のエラー JSON 込みでエラーにする。
fn http_status_error(status: reqwest::StatusCode, body: &serde_json::Value) -> AppError {
    let text = match body {
        serde_json::Value::String(s) => s.clone(),
        serde_json::Value::Null => String::new(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    let (text, _) = truncate_chars(&text, MAX_RESPONSE_CHARS);
    if text.trim().is_empty() {
        AppError::Elasticsearch(format!("HTTP {status}"))
    } else {
        AppError::Elasticsearch(format!("HTTP {status}: {text}"))
    }
}

/// 行の最初のトークンが HTTP メソッドなら (大文字化して) 返す。
/// JSON body の行が `GET` 等で始まることは無い (行頭は `{` / `"` / 空白等) ため、
/// メソッド名で始まる行はリクエスト行とみなしてよい。
fn leading_method(line: &str) -> Option<String> {
    let first = line.trim().split_whitespace().next()?;
    let upper = first.to_ascii_uppercase();
    METHODS.contains(&upper.as_str()).then_some(upper)
}

/// エディタの入力を Kibana Console 風のリクエストブロック列に分解する。
/// - ブロック = メソッド行 (`GET /path`) + 続く行の body (次のメソッド行 /
///   EOF まで)
/// - `#` 始まりの行はコメント。空行は無視 (body 中の空行も区切りにしない)
fn parse_input(input: &str) -> Result<Vec<EsRequest>, AppError> {
    let mut requests: Vec<EsRequest> = Vec::new();
    let mut current: Option<(String, String, Vec<String>)> = None;
    for line in input.lines() {
        let trimmed = line.trim();
        if trimmed.starts_with('#') {
            continue;
        }
        if let Some(method) = leading_method(line) {
            // 直前のブロックを確定する
            if let Some((m, p, body)) = current.take() {
                requests.push(finish_request(m, p, body)?);
            }
            let mut parts = trimmed.split_whitespace();
            parts.next(); // メソッド
            let Some(path) = parts.next() else {
                return Err(AppError::Elasticsearch(format!(
                    "Missing path after {method} (expected e.g. \"{method} /_search\")"
                )));
            };
            if parts.next().is_some() {
                return Err(AppError::Elasticsearch(format!(
                    "Unexpected token after the path in \"{trimmed}\" \
                     (the request line must be \"METHOD /path\")"
                )));
            }
            let path = if path.starts_with('/') {
                path.to_string()
            } else {
                format!("/{path}")
            };
            current = Some((method, path, Vec::new()));
            continue;
        }
        if trimmed.is_empty() {
            // body の途中の空行は保持しない (NDJSON の行検証を単純にする)
            continue;
        }
        match &mut current {
            Some((_, _, body)) => body.push(line.to_string()),
            None => {
                return Err(AppError::Elasticsearch(format!(
                    "Expected a request line like \"GET /_search\", got: {trimmed}"
                )));
            }
        }
    }
    if let Some((m, p, body)) = current.take() {
        requests.push(finish_request(m, p, body)?);
    }
    Ok(requests)
}

fn finish_request(
    method: String,
    path: String,
    body_lines: Vec<String>,
) -> Result<EsRequest, AppError> {
    let body = body_lines.join("\n");
    let body = body.trim();
    Ok(EsRequest {
        method,
        path,
        body: (!body.is_empty()).then(|| body.to_string()),
    })
}

/// body の送信形式。
enum EsBody {
    /// 単一の JSON ドキュメント (application/json)
    Json(String),
    /// 複数の JSON ドキュメント (NDJSON、_bulk 等。application/x-ndjson)
    Ndjson(String),
}

/// body を JSON / NDJSON として検証・分類する。
/// どちらでもなければ実行前にエラーにする (サーバーに送ってから
/// 分かりにくいエラーを受け取るより早い)。
fn classify_body(body: &str, ndjson_api: bool) -> Result<EsBody, AppError> {
    // NDJSON 系 API (_bulk / _msearch) は body が 1 行でも NDJSON として送る。
    // 「JSON としてパースできたら Json」の推定に任せると、1 アクションだけの
    // _bulk body が application/json + 末尾改行なしで送られ、Bulk API の
    // 「末尾に改行必須」の仕様に反して失敗する
    if !ndjson_api && serde_json::from_str::<serde_json::Value>(body).is_ok() {
        return Ok(EsBody::Json(body.to_string()));
    }
    for (i, line) in body.lines().enumerate() {
        let trimmed = line.trim();
        if trimmed.is_empty() {
            continue;
        }
        if let Err(e) = serde_json::from_str::<serde_json::Value>(trimmed) {
            return Err(AppError::Elasticsearch(format!(
                "The request body is not valid JSON (body line {}: {e})",
                i + 1
            )));
        }
    }
    // NDJSON (_bulk) は末尾の改行が必須
    Ok(EsBody::Ndjson(format!("{}\n", body.trim_end())))
}

/// パスが NDJSON body を要求する API (_bulk / _msearch) か。
/// guard_segments が失敗するパスは実行前の検証で先に弾かれるため false でよい。
fn path_is_ndjson_api(path: &str) -> bool {
    guard_segments(path)
        .map(|segments| {
            segments
                .iter()
                .any(|s| s == "_bulk" || s == "_msearch")
        })
        .unwrap_or(false)
}

/// ガード判定用にパスをセグメント列へ分解する。
/// クエリ文字列 / フラグメントを除き、`/` で区切る。
/// `.` / `..` セグメント (percent エンコード形も含む) は、送信時の URL
/// 正規化で検証したパスと実際のパスがズレるため拒否する
/// (検証パスと実 I/O パスの一致)。
fn guard_segments(path: &str) -> Result<Vec<String>, AppError> {
    let path = path.split(['?', '#']).next().unwrap_or("");
    // バックスラッシュは whatwg URL 正規化 (reqwest::Url) が `/` として扱い、
    // さらに `..` セグメントも解決されるため、「ガードが見たパス」と
    // 「実際に送信されるパス」がズレる (例: /a\..\_delete_by_query が
    // /_delete_by_query として送信される)。一律拒否する。
    if path.contains('\\') {
        return Err(AppError::Elasticsearch(format!(
            "Backslashes are not supported in the request path: {path}"
        )));
    }
    let mut segments = Vec::new();
    for segment in path.split('/') {
        if segment.is_empty() {
            continue;
        }
        let decoded = segment.replace("%2e", ".").replace("%2E", ".");
        if decoded == "." || decoded == ".." {
            return Err(AppError::Elasticsearch(format!(
                "Path segments \".\" and \"..\" are not supported: {path}"
            )));
        }
        // ES 本体のルート解決は raw (未デコード) パスで行われるため
        // percent エンコードでガードはすり抜けられないが (RestController)、
        // デコードする中間プロキシや互換実装 (OpenSearch 派生等) を挟んだ
        // 構成に備え、区切り文字 (%2F %5C) とアンダースコア (%5F) の
        // エンコード形は防御的に拒否する (事故防止ガードの完全性を優先)。
        let lower = segment.to_ascii_lowercase();
        if lower.contains("%2f") || lower.contains("%5c") || lower.contains("%5f") {
            return Err(AppError::Elasticsearch(format!(
                "Percent-encoded separators (%2F, %5C) and underscores (%5F) \
                 are not supported in the request path: {path}"
            )));
        }
        segments.push(segment.to_string());
    }
    Ok(segments)
}

/// readonly (Writable OFF / config readonly) でも実行を許可するリクエストか。
/// GET / HEAD は常時許可。POST は検索系エンドポイントのホワイトリストのみ。
/// PUT / DELETE / PATCH は常に Writable 必須。
/// percent エンコードされた API 名はホワイトリストに一致しない = 拒否側に
/// 倒れる (安全側)。
fn is_readonly_request(method: &str, segments: &[String]) -> bool {
    match method {
        "GET" | "HEAD" => true,
        "POST" => readonly_post_allowed(segments),
        _ => false,
    }
}

/// POST の readonly 判定。最初の `_` 始まりセグメントを API とみなし、
/// 後続のサブセグメントも API ごとに検証する。
/// ES の REST ルートではインデックス名が `_` で始まることは無く、API より後ろの
/// セグメントはサブアクションかドキュメント ID なので、最初の `_` セグメントが
/// API である (`POST /index/_doc/_search` = ID が "_search" のドキュメント作成、
/// のようなすり抜けを防ぐため「どこかに _search がある」では判定しない)。
fn readonly_post_allowed(segments: &[String]) -> bool {
    let Some(api_index) = segments.iter().position(|s| s.starts_with('_')) else {
        return false;
    };
    let api = segments[api_index].as_str();
    if !READONLY_POST_APIS.contains(&api) {
        return false;
    }
    let subs: Vec<&str> = segments[api_index + 1..].iter().map(String::as_str).collect();
    match api {
        // POST /_search/scroll (スクロール継続) と _search/template は読み取り
        "_search" => subs.is_empty() || subs == ["scroll"] || subs == ["template"],
        "_msearch" => subs.is_empty() || subs == ["template"],
        "_validate" => subs.is_empty() || subs == ["query"],
        "_render" => subs.is_empty() || subs == ["template"],
        // ドキュメント ID を 1 つ取れる
        "_explain" | "_termvectors" => subs.len() <= 1,
        "_sql" => subs.is_empty() || subs == ["translate"],
        // _count / _analyze / _mget / _field_caps / _pit はサブセグメント無し
        _ => subs.is_empty(),
    }
}

/// 誤操作でインデックス消失・全ドキュメント削除を招くリクエストの理由を返す。
/// SQL の dangerous_reason と同じ扱い (allow_dangerous_statements が無効なら
/// 拒否、有効ならフロントが実行前に確認を出す)。
fn dangerous_request_reason(method: &str, segments: &[String]) -> Option<String> {
    if segments.iter().any(|s| s == "_delete_by_query") {
        return Some(
            "_delete_by_query would delete every document matching the query.".to_string(),
        );
    }
    // WHERE 無し UPDATE 相当: クエリ次第で全ドキュメントを書き換える
    if segments.iter().any(|s| s == "_update_by_query") {
        return Some(
            "_update_by_query can rewrite every document matching the query.".to_string(),
        );
    }
    // DELETE /<index> (単一セグメント) はインデックス削除。
    // カンマ区切り・ワイルドカード・_all も 1 セグメントに含まれる
    if method == "DELETE" && segments.len() == 1 {
        return Some(format!(
            "DELETE /{} would permanently delete the index (and all of its documents).",
            segments[0]
        ));
    }
    // DELETE /_data_stream/<name> はデータストリームとバッキングインデックスを
    // まとめて削除する (インデックス削除と同等のデータ消失)
    if method == "DELETE" && segments.first().is_some_and(|s| s == "_data_stream") {
        return Some(
            "DELETE /_data_stream would permanently delete the data stream \
             and all of its backing indices."
                .to_string(),
        );
    }
    None
}

/// 入力全体から最初の危険リクエストの理由を返す。
/// フロントの実行前確認ダイアログ用 (db::dangerous_statement_reason から呼ぶ)。
/// パースできない入力は None (実行時にエラーとして返る)。
pub fn dangerous_reason_for_input(input: &str) -> Option<String> {
    let requests = parse_input(input).ok()?;
    requests.iter().find_map(|req| {
        let segments = guard_segments(&req.path).ok()?;
        dangerous_request_reason(&req.method, &segments)
    })
}

/// リクエストブロック列を実行して結果を返す (キャンセル対応版)。
/// db::run_query_cancellable から DbPool::Elasticsearch の場合に委譲される。
pub async fn run_query_cancellable(
    client: &EsClient,
    registry: &CancelRegistry,
    connection_name: &str,
    input: &str,
    max_rows: usize,
    readonly: ReadonlyGuard,
    allow_dangerous: bool,
) -> Result<QueryResult, AppError> {
    let requests = parse_input(input)?;
    if requests.is_empty() {
        return Err(AppError::Elasticsearch("The request is empty".into()));
    }

    // 何も実行する前に全リクエストを検証する (一部だけ実行される事態を防ぐ)
    for req in &requests {
        let segments = guard_segments(&req.path)?;
        if readonly != ReadonlyGuard::Off && !is_readonly_request(&req.method, &segments) {
            return Err(readonly_block_error(readonly));
        }
        if !allow_dangerous {
            if let Some(reason) = dangerous_request_reason(&req.method, &segments) {
                return Err(dangerous_block_error(&reason));
            }
        }
        // URL として不正なパスも実行前に検出する
        client.build_url(&req.path)?;
        if let Some(body) = &req.body {
            classify_body(body, path_is_ndjson_api(&req.path))?;
        }
    }

    let cancelled = Arc::new(AtomicBool::new(false));
    let notify = Arc::new(tokio::sync::Notify::new());
    let guard = registry.register(
        connection_name,
        CancelTarget::ClientSide {
            notify: notify.clone(),
        },
        cancelled,
    );
    let started = Instant::now();
    // キャンセルは実行の future を打ち切る。biased で実行結果側を先に見る:
    // 結果とキャンセル通知が同時に ready なら完了済みの結果を優先する
    let result = tokio::select! {
        biased;
        result = execute_requests(client, &requests, max_rows) => result,
        _ = notify.notified() => Err(AppError::Cancelled),
    };
    let was_cancelled = guard.was_cancelled();
    drop(guard);
    // キャンセルが完了と競合した場合は成功結果をそのまま返す (SQL 側と同じ挙動)
    if was_cancelled && result.is_err() {
        return Err(AppError::Cancelled);
    }
    let mut result = result?;
    result.elapsed_ms = started.elapsed().as_millis() as u64;
    Ok(result)
}

/// 複数リクエストはトランザクションではない: 途中のリクエストが転送エラーで
/// 失敗するとそこで中断し、それまでの実行結果は残る。HTTP エラーステータス
/// (4xx / 5xx) は応答として status カラムに載せて続行する (Kibana Console と
/// 同じ感覚で、失敗したリクエストがどれかを一覧で確認できる)。
async fn execute_requests(
    client: &EsClient,
    requests: &[EsRequest],
    max_rows: usize,
) -> Result<QueryResult, AppError> {
    if requests.len() == 1 {
        let (status, value) = send_request(client, &requests[0]).await?;
        if !status.is_success() {
            return Err(http_status_error(status, &value));
        }
        return Ok(shape_single(value, max_rows));
    }
    let mut rows = Vec::new();
    let mut truncated = false;
    for req in requests {
        let (status, value) = send_request(client, req).await?;
        if rows.len() >= max_rows {
            truncated = true;
            continue;
        }
        rows.push(vec![
            serde_json::Value::String(req.display()),
            serde_json::json!(status.as_u16()),
            limit_cell(&value, &mut truncated),
        ]);
    }
    Ok(shape_result(
        vec![
            "request".to_string(),
            "status".to_string(),
            "result".to_string(),
        ],
        rows,
        truncated,
    ))
}

/// 1 リクエストを送信し、(ステータス, 応答 JSON) を返す。
/// 応答が JSON でなければ文字列値として返す (`_cat` 系のテキスト応答等)。
/// 応答 body は MAX_RESPONSE_BYTES で読み切る (非有界の応答を貯めない)。
async fn send_request(
    client: &EsClient,
    req: &EsRequest,
) -> Result<(reqwest::StatusCode, serde_json::Value), AppError> {
    let url = client.build_url(&req.path)?;
    let method = reqwest::Method::from_bytes(req.method.as_bytes())
        .map_err(|e| AppError::Elasticsearch(format!("Invalid method {}: {e}", req.method)))?;
    let mut builder = client.client.request(method, url);
    if let Some(user) = &client.user {
        builder = builder.basic_auth(user, client.password.as_deref());
    }
    if let Some(body) = &req.body {
        builder = match classify_body(body, path_is_ndjson_api(&req.path))? {
            EsBody::Json(b) => builder
                .header(reqwest::header::CONTENT_TYPE, "application/json")
                .body(b),
            EsBody::Ndjson(b) => builder
                .header(reqwest::header::CONTENT_TYPE, "application/x-ndjson")
                .body(b),
        };
    }
    let mut response = builder.send().await.map_err(request_error)?;
    let status = response.status();
    let mut buf: Vec<u8> = Vec::new();
    while let Some(chunk) = response.chunk().await.map_err(request_error)? {
        if buf.len() + chunk.len() > MAX_RESPONSE_BYTES {
            return Err(AppError::Elasticsearch(format!(
                "The response is too large (over {} MB). \
                 Narrow the request (e.g. with \"size\" or filters).",
                MAX_RESPONSE_BYTES / (1024 * 1024)
            )));
        }
        buf.extend_from_slice(&chunk);
    }
    let value = match serde_json::from_slice::<serde_json::Value>(&buf) {
        Ok(value) => value,
        Err(_) => {
            let text = String::from_utf8_lossy(&buf).into_owned();
            serde_json::Value::String(text)
        }
    };
    Ok((status, value))
}

/// reqwest のエラーをアプリのエラー型へ (タイムアウトを分かりやすく)。
fn request_error(e: reqwest::Error) -> AppError {
    if e.is_timeout() {
        AppError::Elasticsearch(format!(
            "The request timed out after {}s",
            REQUEST_TIMEOUT.as_secs()
        ))
    } else {
        AppError::Elasticsearch(e.to_string())
    }
}

/// 単一リクエストの応答を表形式へ整形する。
/// - `hits.hits` 配列 → _index / _id / _score + _source キーの union の表
/// - オブジェクトの配列 (`_cat/...?format=json` 等) → キーの union の表
/// - それ以外 → 1 カラム "response" の 1 セルに pretty JSON (文字数上限付き)
fn shape_single(value: serde_json::Value, max_rows: usize) -> QueryResult {
    if let Some(hits) = value
        .get("hits")
        .and_then(|h| h.get("hits"))
        .and_then(|v| v.as_array())
    {
        return shape_hits(hits, max_rows);
    }
    if let Some(items) = value.as_array() {
        if !items.is_empty() && items.iter().all(|v| v.is_object()) {
            return shape_object_array(items, max_rows);
        }
    }
    let text = match &value {
        serde_json::Value::String(s) => s.clone(),
        other => serde_json::to_string_pretty(other).unwrap_or_default(),
    };
    let (text, truncated) = truncate_chars(&text, MAX_RESPONSE_CHARS);
    shape_result(
        vec!["response".to_string()],
        vec![vec![serde_json::Value::String(text)]],
        truncated,
    )
}

/// 検索応答の hits.hits を表形式へ整形する。
/// columns は `_index` / `_id` / `_score` + 全ヒットの `_source` キーの union
/// (出現順)。max_rows で打ち切り + truncated。
fn shape_hits(hits: &[serde_json::Value], max_rows: usize) -> QueryResult {
    let mut source_keys: Vec<String> = Vec::new();
    for hit in hits {
        if let Some(source) = hit.get("_source").and_then(|s| s.as_object()) {
            for key in source.keys() {
                if !source_keys.iter().any(|k| k == key) {
                    source_keys.push(key.clone());
                }
            }
        }
    }
    let mut columns = vec![
        "_index".to_string(),
        "_id".to_string(),
        "_score".to_string(),
    ];
    columns.extend(source_keys.iter().cloned());

    let mut truncated = hits.len() > max_rows;
    let mut rows = Vec::with_capacity(hits.len().min(max_rows));
    for hit in hits.iter().take(max_rows) {
        let mut row = Vec::with_capacity(columns.len());
        for meta in ["_index", "_id", "_score"] {
            row.push(limit_cell(
                hit.get(meta).unwrap_or(&serde_json::Value::Null),
                &mut truncated,
            ));
        }
        let source = hit.get("_source").and_then(|s| s.as_object());
        for key in &source_keys {
            let value = source
                .and_then(|s| s.get(key))
                .unwrap_or(&serde_json::Value::Null);
            row.push(limit_cell(value, &mut truncated));
        }
        rows.push(row);
    }
    shape_result(columns, rows, truncated)
}

/// オブジェクトの配列 (`_cat/indices?format=json` 等) を表形式へ整形する。
/// columns は全要素のキーの union (出現順)。
fn shape_object_array(items: &[serde_json::Value], max_rows: usize) -> QueryResult {
    let mut columns: Vec<String> = Vec::new();
    for item in items {
        if let Some(object) = item.as_object() {
            for key in object.keys() {
                if !columns.iter().any(|k| k == key) {
                    columns.push(key.clone());
                }
            }
        }
    }
    let mut truncated = items.len() > max_rows;
    let mut rows = Vec::with_capacity(items.len().min(max_rows));
    for item in items.iter().take(max_rows) {
        let object = item.as_object();
        let row = columns
            .iter()
            .map(|key| {
                limit_cell(
                    object
                        .and_then(|o| o.get(key))
                        .unwrap_or(&serde_json::Value::Null),
                    &mut truncated,
                )
            })
            .collect();
        rows.push(row);
    }
    shape_result(columns, rows, truncated)
}

fn shape_result(
    columns: Vec<String>,
    rows: Vec<Vec<serde_json::Value>>,
    truncated: bool,
) -> QueryResult {
    QueryResult {
        row_count: rows.len(),
        columns,
        rows,
        affected_rows: None,
        truncated,
        elapsed_ms: 0,
        applied_limit: None,
        switched_schema: None,
    }
}

/// セルに入れる JSON 値を再帰的に打ち切る。
/// - 長い文字列は MAX_CELL_CHARS で打ち切り
/// - 配列 / オブジェクトは MAX_CELL_ITEMS 要素で打ち切り (末尾にマーカー)
/// - JS の安全整数範囲を超える整数は文字列化 (Tauri invoke 境界の丸め対策)
fn limit_cell(value: &serde_json::Value, truncated: &mut bool) -> serde_json::Value {
    match value {
        serde_json::Value::String(s) => {
            let (text, cut) = truncate_chars(s, MAX_CELL_CHARS);
            if cut {
                *truncated = true;
            }
            serde_json::Value::String(text)
        }
        serde_json::Value::Number(n) => {
            if let Some(i) = n.as_i64() {
                crate::db::json_i64(i)
            } else if let Some(u) = n.as_u64() {
                // as_i64 に失敗した u64 は i64::MAX 超 = 安全整数範囲外
                serde_json::Value::String(u.to_string())
            } else {
                serde_json::Value::Number(n.clone())
            }
        }
        serde_json::Value::Array(items) => {
            let total = items.len();
            let mut out: Vec<serde_json::Value> = items
                .iter()
                .take(MAX_CELL_ITEMS)
                .map(|item| limit_cell(item, truncated))
                .collect();
            if total > MAX_CELL_ITEMS {
                *truncated = true;
                out.push(serde_json::Value::String(format!(
                    "... ({} more items truncated)",
                    total - MAX_CELL_ITEMS
                )));
            }
            serde_json::Value::Array(out)
        }
        serde_json::Value::Object(map) => {
            let total = map.len();
            let mut out = serde_json::Map::new();
            for (key, value) in map.iter().take(MAX_CELL_ITEMS) {
                out.insert(key.clone(), limit_cell(value, truncated));
            }
            if total > MAX_CELL_ITEMS {
                *truncated = true;
                out.insert(
                    "...".to_string(),
                    serde_json::Value::String(format!(
                        "({} more entries truncated)",
                        total - MAX_CELL_ITEMS
                    )),
                );
            }
            serde_json::Value::Object(out)
        }
        other => other.clone(),
    }
}

/// 文字数上限で打ち切る (char 境界安全)。打ち切ったら末尾にマーカーを付ける。
fn truncate_chars(text: &str, max_chars: usize) -> (String, bool) {
    match text.char_indices().nth(max_chars) {
        Some((byte_index, _)) => {
            let mut out = text[..byte_index].to_string();
            out.push_str("\n... (response truncated)");
            (out, true)
        }
        None => (text.to_string(), false),
    }
}

// ---------------------------------------------------------------------------
// スキーマブラウザ (TABLES ペイン) 用: インデックス一覧と mapping のフィールド
// ---------------------------------------------------------------------------

/// インデックス一覧を返す (TABLES ペイン用)。
/// `.` 始まりのシステムインデックスは除外し、名前昇順で返す。
pub async fn fetch_indices(client: &EsClient) -> Result<Vec<TableInfo>, AppError> {
    let req = EsRequest {
        method: "GET".into(),
        path: "/_cat/indices?format=json&h=index,status".into(),
        body: None,
    };
    let (status, value) = send_request(client, &req).await?;
    if !status.is_success() {
        return Err(http_status_error(status, &value));
    }
    Ok(parse_cat_indices(&value))
}

/// TABLES ペインへ返すインデックス数の上限。日次ローテーションの
/// ログクラスタ等ではインデックスが数万に達するため、他の結果整形パスと
/// 同様に非有界のデータを IPC / UI へ送らない (名前昇順の先頭から切る)。
const MAX_INDICES: usize = 5_000;

/// `_cat/indices?format=json` の応答をインデックス一覧へ変換する。
fn parse_cat_indices(value: &serde_json::Value) -> Vec<TableInfo> {
    let mut indices: Vec<TableInfo> = value
        .as_array()
        .map(|items| {
            items
                .iter()
                .filter_map(|item| item.get("index").and_then(|v| v.as_str()))
                .filter(|name| !name.starts_with('.'))
                .map(|name| TableInfo {
                    name: name.to_string(),
                    schema: None,
                    kind: "index".to_string(),
                    qualified_name: name.to_string(),
                })
                .collect()
        })
        .unwrap_or_default();
    indices.sort_by(|a, b| a.name.cmp(&b.name));
    indices.truncate(MAX_INDICES);
    indices
}

/// インデックス名の検証 (URL パスに埋め込むため)。
/// ES のインデックス名の文字集合 (小文字英数と `.` `_` `-` `+` 等) に合わせた
/// 保守的なホワイトリスト。パス区切りや percent エンコードは拒否する。
fn validate_index_name(name: &str) -> Result<&str, AppError> {
    let valid = !name.is_empty()
        && name != "."
        && name != ".."
        && name
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | '+'));
    if !valid {
        return Err(AppError::Elasticsearch(format!(
            "Invalid index name: {name}"
        )));
    }
    Ok(name)
}

/// インデックスの mapping からフィールド一覧を返す (TABLES ペイン用)。
/// ネストしたフィールドは `a.b` 形式に平坦化し、data_type は mapping の
/// type フィールド、nullable は常に true (ES に NOT NULL の概念が無い)。
pub async fn fetch_index_columns(
    client: &EsClient,
    index: &str,
) -> Result<Vec<ColumnInfo>, AppError> {
    let index = validate_index_name(index)?;
    let req = EsRequest {
        method: "GET".into(),
        path: format!("/{index}/_mapping"),
        body: None,
    };
    let (status, value) = send_request(client, &req).await?;
    if !status.is_success() {
        return Err(http_status_error(status, &value));
    }
    Ok(parse_mapping_response(&value))
}

/// `GET /<index>/_mapping` の応答 (`{ "<index>": { "mappings": { "properties":
/// {...} } } }`) をフィールド一覧へ変換する。
fn parse_mapping_response(value: &serde_json::Value) -> Vec<ColumnInfo> {
    let mut out = Vec::new();
    // 応答のキーは実インデックス名 (エイリアス解決後) なので最初の値を使う
    let properties = value
        .as_object()
        .and_then(|o| o.values().next())
        .and_then(|v| v.get("mappings"))
        .and_then(|m| m.get("properties"))
        .and_then(|p| p.as_object());
    if let Some(properties) = properties {
        flatten_properties(properties, "", &mut out);
    }
    out
}

/// mapping の properties を `a.b` 形式へ再帰的に平坦化する。
fn flatten_properties(
    properties: &serde_json::Map<String, serde_json::Value>,
    prefix: &str,
    out: &mut Vec<ColumnInfo>,
) {
    for (name, definition) in properties {
        let full = if prefix.is_empty() {
            name.clone()
        } else {
            format!("{prefix}.{name}")
        };
        if let Some(children) = definition.get("properties").and_then(|p| p.as_object()) {
            // object / nested は子フィールドを展開する
            flatten_properties(children, &full, out);
            continue;
        }
        let data_type = definition
            .get("type")
            .and_then(|t| t.as_str())
            .unwrap_or("object")
            .to_string();
        out.push(ColumnInfo {
            name: full,
            data_type,
            nullable: true,
        });
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn segs(path: &str) -> Vec<String> {
        guard_segments(path).unwrap()
    }

    #[test]
    fn test_parse_input_single_block() {
        let requests = parse_input("GET /_cat/indices?format=json").unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "GET");
        assert_eq!(requests[0].path, "/_cat/indices?format=json");
        assert_eq!(requests[0].body, None);
    }

    #[test]
    fn test_parse_input_with_body() {
        let input = "POST /books/_search\n{\n  \"query\": { \"match_all\": {} }\n}\n";
        let requests = parse_input(input).unwrap();
        assert_eq!(requests.len(), 1);
        assert_eq!(requests[0].method, "POST");
        assert_eq!(requests[0].path, "/books/_search");
        assert_eq!(
            requests[0].body.as_deref(),
            Some("{\n  \"query\": { \"match_all\": {} }\n}")
        );
    }

    #[test]
    fn test_parse_input_multiple_blocks_and_comments() {
        let input = "# comment\nGET /\n\nPUT books/_doc/1\n{\"title\": \"a\"}\n# tail comment\nHEAD /books\n";
        let requests = parse_input(input).unwrap();
        assert_eq!(requests.len(), 3);
        assert_eq!(requests[0].display(), "GET /");
        // パスの先頭 / は補完される
        assert_eq!(requests[1].path, "/books/_doc/1");
        assert_eq!(requests[1].body.as_deref(), Some("{\"title\": \"a\"}"));
        assert_eq!(requests[2].method, "HEAD");
        assert_eq!(requests[2].body, None);
    }

    #[test]
    fn test_parse_input_ndjson_body() {
        let input = "POST /_bulk\n{\"index\":{\"_id\":\"1\"}}\n{\"title\":\"a\"}\n{\"index\":{\"_id\":\"2\"}}\n{\"title\":\"b\"}";
        let requests = parse_input(input).unwrap();
        assert_eq!(requests.len(), 1);
        let body = requests[0].body.as_deref().unwrap();
        assert_eq!(body.lines().count(), 4);
        // NDJSON として分類され、末尾に改行が付く
        match classify_body(body, true).unwrap() {
            EsBody::Ndjson(b) => assert!(b.ends_with('\n')),
            EsBody::Json(_) => panic!("expected NDJSON"),
        }
    }

    #[test]
    fn test_parse_input_lowercase_method() {
        let requests = parse_input("get /_cluster/health").unwrap();
        assert_eq!(requests[0].method, "GET");
    }

    #[test]
    fn test_parse_input_errors() {
        // メソッド行の前に本文がある
        assert!(parse_input("{\"a\": 1}").is_err());
        // パスが無い
        assert!(parse_input("GET").is_err());
        // パスの後に余分なトークン
        assert!(parse_input("GET /a extra").is_err());
        // 空入力はエラーではなく空 (run_query_cancellable 側でエラーにする)
        assert!(parse_input("").unwrap().is_empty());
        assert!(parse_input("# only comment\n").unwrap().is_empty());
    }

    #[test]
    fn test_classify_body() {
        assert!(matches!(
            classify_body("{\"a\": 1}", false).unwrap(),
            EsBody::Json(_)
        ));
        // pretty JSON も単一ドキュメント
        assert!(matches!(
            classify_body("{\n  \"a\": 1\n}", false).unwrap(),
            EsBody::Json(_)
        ));
        assert!(matches!(
            classify_body("{\"a\":1}\n{\"b\":2}", false).unwrap(),
            EsBody::Ndjson(_)
        ));
        assert!(classify_body("not json", false).is_err());
        assert!(classify_body("{\"a\":1}\nbroken", false).is_err());
        // NDJSON 系 API (_bulk / _msearch) は 1 行の body でも NDJSON として
        // 分類し、末尾に改行を付ける (Bulk API の末尾改行必須の仕様)
        match classify_body("{\"delete\":{\"_id\":\"1\"}}", true).unwrap() {
            EsBody::Ndjson(b) => assert_eq!(b, "{\"delete\":{\"_id\":\"1\"}}\n"),
            EsBody::Json(_) => panic!("expected NDJSON for a bulk body"),
        }
        // pretty JSON の複数行 body は NDJSON API では行単位で不正 → エラー
        assert!(classify_body("{\n  \"a\": 1\n}", true).is_err());
    }

    #[test]
    fn test_tls_tunnel_url_host() {
        use crate::config::SshTunnelConfig;
        let mut server = ServerConfig {
            name: "es".into(),
            description: None,
            folder_name: None,
            engine: "elasticsearch".into(),
            host: Some("es.example.com".into()),
            port: Some(9200),
            schema: None,
            user: None,
            password: None,
            password_command: None,
            ssh_tunnel: None,
            readonly: false,
            allow_dangerous_statements: false,
            group_name: None,
            tls: true,
            ssl_mode: None,
            ssl_root_cert: None,
            aws_profile: None,
        };
        // トンネル無しなら URL ホストは差し替えない
        assert_eq!(tls_tunnel_url_host(&server, "es.example.com"), None);
        // トンネルあり + TLS + 接続先がローカルポートなら元ホスト名を返す
        server.ssh_tunnel = Some(SshTunnelConfig {
            host: "bastion".into(),
            port: 22,
            user: "u".into(),
            ssh_config: None,
            password: None,
            private_key_path: None,
            private_key_passphrase: None,
            identity_agent: None,
        });
        assert_eq!(
            tls_tunnel_url_host(&server, "127.0.0.1"),
            Some("es.example.com".to_string())
        );
        // TLS 無しなら差し替え不要 (証明書検証が無い)
        server.tls = false;
        assert_eq!(tls_tunnel_url_host(&server, "127.0.0.1"), None);
    }

    #[test]
    fn test_path_is_ndjson_api() {
        assert!(path_is_ndjson_api("/_bulk"));
        assert!(path_is_ndjson_api("/books/_bulk"));
        assert!(path_is_ndjson_api("/_msearch"));
        assert!(path_is_ndjson_api("/books/_msearch?typed_keys=true"));
        assert!(!path_is_ndjson_api("/books/_search"));
        assert!(!path_is_ndjson_api("/"));
    }

    #[test]
    fn test_guard_segments() {
        assert_eq!(segs("/books/_search?size=1"), vec!["books", "_search"]);
        assert_eq!(segs("/"), Vec::<String>::new());
        assert_eq!(segs("//a//b"), vec!["a", "b"]);
        // ドットセグメント (エンコード形含む) は拒否
        assert!(guard_segments("/a/../_bulk").is_err());
        assert!(guard_segments("/a/./b").is_err());
        assert!(guard_segments("/a/%2e%2e/_bulk").is_err());
        assert!(guard_segments("/a/.%2E/b").is_err());
        // バックスラッシュは URL 正規化で / になり検証パスとズレるため拒否
        assert!(guard_segments("/a\\..\\_delete_by_query").is_err());
        assert!(guard_segments("/books\\x").is_err());
        // エンコードされた区切り / アンダースコアは防御的に拒否
        assert!(guard_segments("/books%2F_delete_by_query").is_err());
        assert!(guard_segments("/books/%5Fdelete_by_query").is_err());
        assert!(guard_segments("/books/%5c..").is_err());
    }

    #[test]
    fn test_is_readonly_request() {
        let ro = |method: &str, path: &str| is_readonly_request(method, &segs(path));
        // GET / HEAD は常時許可
        assert!(ro("GET", "/books/_search"));
        assert!(ro("GET", "/"));
        assert!(ro("HEAD", "/books"));
        // POST は検索系のみ
        assert!(ro("POST", "/_search"));
        assert!(ro("POST", "/books/_search?size=1"));
        assert!(ro("POST", "/_search/scroll"));
        assert!(ro("POST", "/books/_msearch"));
        assert!(ro("POST", "/books/_count"));
        assert!(ro("POST", "/books/_analyze"));
        assert!(ro("POST", "/_mget"));
        assert!(ro("POST", "/books/_field_caps"));
        assert!(ro("POST", "/books/_validate/query"));
        assert!(ro("POST", "/books/_explain/1"));
        assert!(ro("POST", "/books/_termvectors/1"));
        assert!(ro("POST", "/books/_pit"));
        assert!(ro("POST", "/_sql"));
        assert!(ro("POST", "/_sql/translate"));
        assert!(ro("POST", "/_render/template"));
        // 書き込み系 POST は拒否
        assert!(!ro("POST", "/books/_doc"));
        assert!(!ro("POST", "/_bulk"));
        assert!(!ro("POST", "/books/_update/1"));
        assert!(!ro("POST", "/books/_delete_by_query"));
        assert!(!ro("POST", "/books"));
        // ドキュメント ID にホワイトリスト名を使ったすり抜けは拒否
        assert!(!ro("POST", "/books/_doc/_search"));
        assert!(!ro("POST", "/books/_update/_count"));
        // PUT / DELETE / PATCH は常に Writable 必須
        assert!(!ro("PUT", "/books/_doc/1"));
        assert!(!ro("DELETE", "/books/_doc/1"));
        assert!(!ro("PATCH", "/books"));
    }

    #[test]
    fn test_dangerous_request_reason() {
        let danger =
            |method: &str, path: &str| dangerous_request_reason(method, &segs(path));
        // インデックス削除 (単一セグメント。ワイルドカード / カンマ / _all 含む)
        assert!(danger("DELETE", "/books").is_some());
        assert!(danger("DELETE", "/logs-*,metrics-*").is_some());
        assert!(danger("DELETE", "/_all").is_some());
        // ドキュメント削除・スクロール解放は対象外 (readonly ガード側で Writable 必須)
        assert!(danger("DELETE", "/books/_doc/1").is_none());
        assert!(danger("DELETE", "/_search/scroll").is_none());
        // _delete_by_query はメソッドによらず危険
        assert!(danger("POST", "/books/_delete_by_query").is_some());
        assert!(danger("POST", "/books/_delete_by_query?conflicts=proceed").is_some());
        // WHERE 無し UPDATE 相当の _update_by_query も危険
        assert!(danger("POST", "/books/_update_by_query").is_some());
        // データストリーム削除 (バッキングインデックスごと消える) も危険
        assert!(danger("DELETE", "/_data_stream/logs").is_some());
        assert!(danger("DELETE", "/_data_stream/logs-*").is_some());
        // GET /_data_stream (一覧) は危険でない
        assert!(danger("GET", "/_data_stream/logs").is_none());
        // 読み取りは対象外
        assert!(danger("GET", "/books/_search").is_none());
        assert!(danger("PUT", "/books/_doc/1").is_none());
    }

    #[test]
    fn test_dangerous_reason_for_input() {
        assert!(dangerous_reason_for_input("GET /books/_search").is_none());
        assert!(dangerous_reason_for_input("DELETE /books").is_some());
        assert!(dangerous_reason_for_input(
            "GET /\nPOST /books/_delete_by_query\n{\"query\":{\"match_all\":{}}}"
        )
        .is_some());
        // パース不能な入力は None (実行時にエラーとして返る)
        assert!(dangerous_reason_for_input("{\"a\": 1}").is_none());
    }

    #[test]
    fn test_shape_single_hits() {
        let value = serde_json::json!({
            "took": 3,
            "hits": {
                "total": {"value": 3},
                "hits": [
                    {"_index": "books", "_id": "1", "_score": 1.0,
                     "_source": {"title": "a", "year": 2001}},
                    {"_index": "books", "_id": "2", "_score": 0.5,
                     "_source": {"title": "b", "author": "x"}},
                    {"_index": "books", "_id": "3", "_score": null,
                     "_source": {"title": "c"}}
                ]
            }
        });
        let result = shape_single(value, 10);
        assert_eq!(
            result.columns,
            vec!["_index", "_id", "_score", "title", "year", "author"]
        );
        assert_eq!(result.row_count, 3);
        assert!(!result.truncated);
        assert_eq!(result.rows[0][0], serde_json::json!("books"));
        assert_eq!(result.rows[0][3], serde_json::json!("a"));
        assert_eq!(result.rows[0][4], serde_json::json!(2001));
        // union にあるが自分の _source に無いキーは null
        assert_eq!(result.rows[0][5], serde_json::Value::Null);
        assert_eq!(result.rows[2][2], serde_json::Value::Null);
    }

    #[test]
    fn test_shape_single_hits_truncation() {
        let hits: Vec<serde_json::Value> = (0..5)
            .map(|i| serde_json::json!({"_id": i.to_string(), "_source": {"n": i}}))
            .collect();
        let value = serde_json::json!({"hits": {"hits": hits}});
        let result = shape_single(value, 2);
        assert_eq!(result.row_count, 2);
        assert!(result.truncated);
    }

    #[test]
    fn test_shape_single_object_array() {
        // _cat/indices?format=json 相当
        let value = serde_json::json!([
            {"index": "books", "status": "open"},
            {"index": "logs", "status": "open", "extra": 1}
        ]);
        let result = shape_single(value, 10);
        assert_eq!(result.columns, vec!["index", "status", "extra"]);
        assert_eq!(result.row_count, 2);
        assert_eq!(result.rows[0][2], serde_json::Value::Null);

        // 打ち切り
        let value = serde_json::json!([{"a": 1}, {"a": 2}, {"a": 3}]);
        let result = shape_single(value, 2);
        assert_eq!(result.row_count, 2);
        assert!(result.truncated);
    }

    #[test]
    fn test_shape_single_scalar_response() {
        let value = serde_json::json!({"acknowledged": true});
        let result = shape_single(value, 10);
        assert_eq!(result.columns, vec!["response"]);
        assert_eq!(result.row_count, 1);
        assert!(!result.truncated);
        let cell = result.rows[0][0].as_str().unwrap();
        assert!(cell.contains("\"acknowledged\": true"));

        // テキスト応答 (JSON でない _cat 応答等) はそのまま 1 セル
        let value = serde_json::Value::String("green open books".into());
        let result = shape_single(value, 10);
        assert_eq!(result.rows[0][0], serde_json::json!("green open books"));

        // 混在配列 (オブジェクトでない要素を含む) は表にしない
        let value = serde_json::json!([{"a": 1}, 2]);
        let result = shape_single(value, 10);
        assert_eq!(result.columns, vec!["response"]);
    }

    #[test]
    fn test_truncate_chars() {
        let (text, cut) = truncate_chars("hello", 10);
        assert_eq!(text, "hello");
        assert!(!cut);
        let (text, cut) = truncate_chars(&"x".repeat(20), 5);
        assert!(cut);
        assert!(text.starts_with("xxxxx"));
        assert!(text.ends_with("(response truncated)"));
        // マルチバイト境界でもパニックしない
        let (text, cut) = truncate_chars("あいうえお", 2);
        assert!(cut);
        assert!(text.starts_with("あい"));
    }

    #[test]
    fn test_limit_cell() {
        let mut truncated = false;
        // 安全整数範囲を超える整数は文字列化
        let v = limit_cell(&serde_json::json!(9007199254740993_i64), &mut truncated);
        assert_eq!(v, serde_json::json!("9007199254740993"));
        assert!(!truncated);
        let v = limit_cell(&serde_json::json!(18446744073709551615_u64), &mut truncated);
        assert_eq!(v, serde_json::json!("18446744073709551615"));
        // 範囲内の整数・浮動小数点はそのまま
        assert_eq!(
            limit_cell(&serde_json::json!(42), &mut truncated),
            serde_json::json!(42)
        );
        assert_eq!(
            limit_cell(&serde_json::json!(1.5), &mut truncated),
            serde_json::json!(1.5)
        );
        assert!(!truncated);

        // 長い文字列は打ち切り
        let mut truncated = false;
        let long = "y".repeat(MAX_CELL_CHARS + 10);
        let v = limit_cell(&serde_json::json!(long), &mut truncated);
        assert!(truncated);
        assert!(v.as_str().unwrap().len() < MAX_CELL_CHARS + 100);

        // 大きい配列は要素数打ち切り + マーカー
        let mut truncated = false;
        let big: Vec<u32> = (0..(MAX_CELL_ITEMS as u32 + 5)).collect();
        let v = limit_cell(&serde_json::json!(big), &mut truncated);
        assert!(truncated);
        let items = v.as_array().unwrap();
        assert_eq!(items.len(), MAX_CELL_ITEMS + 1);
        assert!(items[MAX_CELL_ITEMS]
            .as_str()
            .unwrap()
            .contains("truncated"));

        // ネストしたオブジェクトも再帰的に処理される
        let mut truncated = false;
        let v = limit_cell(
            &serde_json::json!({"nested": {"big": 9007199254740993_i64}}),
            &mut truncated,
        );
        assert_eq!(v["nested"]["big"], serde_json::json!("9007199254740993"));
    }

    #[test]
    fn test_parse_cat_indices() {
        let value = serde_json::json!([
            {"index": "logs", "status": "open"},
            {"index": ".internal-system", "status": "open"},
            {"index": "books", "status": "open"}
        ]);
        let indices = parse_cat_indices(&value);
        // システムインデックス (.始まり) は除外、名前昇順
        let names: Vec<&str> = indices.iter().map(|t| t.name.as_str()).collect();
        assert_eq!(names, vec!["books", "logs"]);
        assert!(indices.iter().all(|t| t.kind == "index"));
        assert!(indices.iter().all(|t| t.schema.is_none()));
        assert!(indices.iter().all(|t| t.name == t.qualified_name));
        // 配列でない応答は空
        assert!(parse_cat_indices(&serde_json::json!({"error": "x"})).is_empty());
    }

    #[test]
    fn test_validate_index_name() {
        assert!(validate_index_name("books").is_ok());
        assert!(validate_index_name("logs-2026.07.24").is_ok());
        assert!(validate_index_name("my_index+v2").is_ok());
        for bad in ["", ".", "..", "a/b", "a b", "a?b", "a%2Fb", "a#b", "a*"] {
            assert!(validate_index_name(bad).is_err(), "should reject {bad:?}");
        }
    }

    #[test]
    fn test_parse_mapping_response() {
        let value = serde_json::json!({
            "books": {
                "mappings": {
                    "properties": {
                        "title": {"type": "text", "fields": {"keyword": {"type": "keyword"}}},
                        "year": {"type": "integer"},
                        "author": {
                            "properties": {
                                "name": {"type": "text"},
                                "age": {"type": "integer"}
                            }
                        },
                        "misc": {}
                    }
                }
            }
        });
        let columns = parse_mapping_response(&value);
        let summary: Vec<(&str, &str)> = columns
            .iter()
            .map(|c| (c.name.as_str(), c.data_type.as_str()))
            .collect();
        // serde_json の Map はキー昇順。ネストは a.b 形式に平坦化される
        assert_eq!(
            summary,
            vec![
                ("author.age", "integer"),
                ("author.name", "text"),
                ("misc", "object"),
                ("title", "text"),
                ("year", "integer"),
            ]
        );
        assert!(columns.iter().all(|c| c.nullable));
        // mappings が無い応答は空
        assert!(parse_mapping_response(&serde_json::json!({})).is_empty());
        assert!(
            parse_mapping_response(&serde_json::json!({"books": {"mappings": {}}}))
                .is_empty()
        );
    }
}
