mod ai;
mod cli;
mod config;
mod db;
mod engines;
mod history;
mod meta_commands;
mod error;
mod folder_meta;
mod query_files;
mod router;
mod schema_info;
mod third_party_notices;
mod tunnel;

use std::path::PathBuf;
use std::sync::Arc;

use config::{AppConfig, ConfigInfo, ConnectionInfo, ServerConfig};
use db::{CancelRegistry, DbManager, DbPool, QueryResult, DEFAULT_MAX_ROWS};
use error::AppError;

/// 実行中に届いた「開く対象」のフロントへの受け渡し状態。
/// フロントの listener は onMount (webview 準備後) に登録されるため、それより前に
/// deep link / CLI が届くと `open-query-file` イベントを取りこぼす。ready になるまでは
/// キューに積み、frontend_ready でまとめて渡す (単一 Mutex で ready 判定と push/drain を
/// 直列化し、取りこぼし・二重配送を防ぐ)。
#[derive(Default)]
struct LiveDelivery {
    /// フロントの listener が用意できたか (frontend_ready で true になる)。
    ready: bool,
    /// ready 前に届いた開く対象 (frontend_ready で drain して渡す)。
    pending: Vec<router::OpenTarget>,
    /// ready 前に届いた解決失敗のメッセージ (frontend_ready で drain して渡す)。
    /// 成功対象と同様、listener 準備前は emit しても取りこぼすためキューする。
    pending_errors: Vec<String>,
}

/// アプリ全体の共有状態。
#[derive(Default)]
struct AppState {
    /// マージ済み設定 (config_override_command 適用後) のセッションキャッシュ。
    /// 取得コマンドは外部プロセス実行を伴うため、毎回走らせない
    /// (reset_connections でクリアして再取得する)。
    config: tokio::sync::Mutex<Option<Arc<AppConfig>>>,
    /// 接続設定のキャッシュ。get_connections で更新される。
    /// パスワード等の機密を含むためフロントエンドには渡さない。
    servers: tokio::sync::Mutex<Option<Vec<ServerConfig>>>,
    db: DbManager,
    /// 実行中クエリのキャンセルレジストリ (接続名単位)。
    query_cancels: CancelRegistry,
    /// クエリ実行履歴の記録 (接続ごとの行数キャッシュを保持)。
    history: history::HistoryManager,
    /// スキーマ情報 (テーブル・カラム) のキャッシュ。
    /// スキーマブラウザと SQL 補完 (get_schema_map) で共有する。
    schema_cache: schema_info::SchemaCache,
    /// AI 設定のセッションキャッシュ (reset_connections でクリア)。
    /// 外側の None は未解決を表す。api_key を含むためフロントには渡さず、
    /// get_ai_info で configured / model のみを返す。
    ai: tokio::sync::Mutex<Option<Option<ai::AiConfig>>>,
    /// 起動時に `queryfolio://` deep link / CLI サブコマンドで指定された
    /// 開くべきルート (無ければ None)。フロントが起動後に frontend_ready で
    /// 1 度だけ取り出す (取り出すと消える)。実行中に開かれたルートは live 経由
    /// (イベント / キュー) でフロントへ届けるため、ここには積まない。
    launch_route: std::sync::Mutex<Option<router::Route>>,
    /// 実行中に届いた開く対象の受け渡し (listener 準備前の取りこぼし対策)。
    live: std::sync::Mutex<LiveDelivery>,
    /// AI チャットの中断要求 (接続名単位)。
    chat_cancels: ChatCancels,
}

/// 中断要求を覚えておく ID の上限 (開始しなかった要求の取りこぼし対策で
/// 残るため、古いものから捨てて無制限に増えないようにする)。
const CHAT_CANCEL_HISTORY_MAX: usize = 256;

/// エージェントのクエリ実行中に中断要求を見に行く間隔 (ms)。
/// run_query_cancellable がキャンセルレジストリへ登録するまでの間に
/// 届いた中断はレジストリ経由では効かないため、自前で監視する。
const CHAT_CANCEL_POLL_INTERVAL_MS: u64 = 200;

/// AI チャット (エージェント) の中断要求を**リクエスト単位**で保持する。
///
/// クエリのキャンセル (CancelRegistry) は「実行中の 1 本」を止めるだけで、
/// モデルの応答待ちや次のツール往復は止められないため、往復そのものを
/// 止める仕組みが要る。接続単位のカウンタにすると (1) 同じ接続で 2 本が
/// 同時に走る時にどちらを止めるか区別できず、(2) 開始直後に届いた中断が
/// 「開始時の基準値」に吸収されてしまうため、フロントが採番した
/// リクエスト ID をそのまま使う。ID を控えておけば、コマンドが走り出す
/// 前に届いた中断も入口の判定で拾える。
#[derive(Default)]
struct ChatCancels {
    inner: tokio::sync::Mutex<ChatCancelState>,
}

#[derive(Default)]
struct ChatCancelState {
    cancelled: std::collections::HashSet<String>,
    /// 挿入順 (上限超過時に古いものから捨てる)
    order: std::collections::VecDeque<String>,
}

impl ChatCancels {
    /// リクエストの中断を要求する (まだ開始していなくても記録を残す)。
    async fn request(&self, request_id: &str) {
        let mut state = self.inner.lock().await;
        if state.cancelled.insert(request_id.to_string()) {
            state.order.push_back(request_id.to_string());
            while state.order.len() > CHAT_CANCEL_HISTORY_MAX {
                if let Some(old) = state.order.pop_front() {
                    state.cancelled.remove(&old);
                }
            }
        }
    }

    /// このリクエストに中断が要求されているか。
    async fn is_cancelled(&self, request_id: &str) -> bool {
        self.inner.lock().await.cancelled.contains(request_id)
    }

    /// 終了したリクエストの記録を捨てる。
    async fn finish(&self, request_id: &str) {
        let mut state = self.inner.lock().await;
        if state.cancelled.remove(request_id) {
            state.order.retain(|id| id != request_id);
        }
    }
}

/// AI チャットのツール実行を登録するキャンセルレジストリのキー。
/// ユーザーのクエリ (接続名がキー) と衝突せず、かつ同じ接続で複数の
/// 往復が走っても互いのエントリを上書きしないようリクエスト ID を含める
/// (CancelRegistry は同じキーの登録を置き換えるため)。
fn chat_cancel_key(connection: &str, request_id: &str) -> String {
    format!("{connection}\u{1}ai-chat\u{1}{request_id}")
}

impl AppState {
    /// マージ済み設定を解決する (セッションキャッシュあり)。
    /// config_override_command は 1Password 等の外部コマンドで数秒かかり
    /// Touch ID を要求することもあるため、クエリ実行のたびに走らせない。
    /// reset_connections でクリアされる。
    /// 起動前の CLI 書き出し (`apply_cli_write_route`) で既にマージ済み設定を
    /// 解決している場合、それをキャッシュに載せた状態で作る。
    ///
    /// `config_override_command` は 1Password 等の外部プロセスを起こすため、
    /// 1 回の CLI 起動で 2 度実行させない: 遅い / Touch ID が 2 回出るだけでなく、
    /// 取得結果が変わると**書き込みと読み出しで別のスナップショット**を使うことになり、
    /// 書いたファイルを開けなくなる (sqlfiles_dir や接続一覧が食い違う)。
    fn with_config(config: Option<Arc<AppConfig>>) -> Self {
        Self {
            config: tokio::sync::Mutex::new(config),
            ..Default::default()
        }
    }

    async fn resolve_config(&self) -> Result<Arc<AppConfig>, AppError> {
        let mut cached = self.config.lock().await;
        if let Some(config) = cached.as_ref() {
            return Ok(config.clone());
        }
        let config = Arc::new(AppConfig::load_merged().await?);
        *cached = Some(config.clone());
        Ok(config)
    }

    async fn resolve_default_limit(&self) -> Result<u64, AppError> {
        Ok(self.resolve_config().await?.default_limit())
    }

    /// クエリファイル保存ディレクトリを解決する。
    /// config.yml は手編集されるため、開いているファイルの保存中に
    /// sqlfiles_dir が変わると未保存内容が新ディレクトリへ書かれてしまう。
    /// マージ済み設定のキャッシュが再読込 (reset_connections) まで固定される
    /// ため、dirty ファイルの保存先も読み込み時のディレクトリに固定される。
    async fn resolve_sqlfiles_dir(&self) -> Result<PathBuf, AppError> {
        self.resolve_config().await?.resolve_sqlfiles_dir()
    }

    /// クエリファイルの保存フォルダ名 → 接続名の対応表を作る (設定順)。
    /// `queryfolio://open/<path>` / CLI で指定されたパスから、そのファイルが
    /// どの接続のものかを解決するために使う (router::resolve_open_target)。
    async fn folder_connection_map(&self) -> Result<Vec<(String, String)>, AppError> {
        let servers = self.resolve_config().await?.resolve_servers()?;
        Ok(servers
            .iter()
            .map(|s| (s.sqlfiles_folder_name(), s.name.clone()))
            .collect())
    }

    /// ルート (deep link / CLI) を、開く対象のクエリファイル (接続 + ファイル名) へ
    /// 解決する。保存ディレクトリ配下の接続フォルダにある、接続エンジンの拡張子の
    /// クエリファイルでなければエラー。
    /// 既知の限界: ここでの検証と実際の読み込み (read_query_file) は別呼び出しで、
    /// その間に設定リロードが挟まると folder / 拡張子の解決結果がズレ得る
    /// (検証済み設定と読込時設定の狭い TOCTOU)。リロードはユーザーの明示操作で、
    /// どちらの解決も設定由来の保存領域内に閉じるため許容する。
    /// `cwd` は相対パスを解決する基準ディレクトリ。deep link / CLI を実行中
    /// インスタンスが受け取った時は「起動元ディレクトリ」を渡す (single-instance の
    /// callback cwd)。None の時はこのプロセスのカレントディレクトリを使う。
    async fn resolve_route_target(
        &self,
        route: &router::Route,
        cwd: Option<PathBuf>,
    ) -> Result<router::OpenTarget, AppError> {
        match route {
            router::Route::OpenFile { path } => {
                // resolve_sqlfiles_dir は相対設定を設定ディレクトリ基準で絶対化して
                // 返すため、ここでの絶対化は本来 no-op。それでも残すのは、パス検証の
                // base が相対のままだと strip_prefix の比較が実 I/O と食い違うためで、
                // 「base は必ず絶対」という前提をこの場で担保しておく
                // (std::path::absolute は FS に触れない字句的絶対化)。
                let sqlfiles_dir = self.resolve_sqlfiles_dir().await?;
                let sqlfiles_dir =
                    std::path::absolute(&sqlfiles_dir).unwrap_or(sqlfiles_dir);
                let folders = self.folder_connection_map().await?;
                let home = dirs::home_dir();
                // 生の入力パスの相対解決だけは cwd (実行中インスタンスなら起動元) 基準。
                let raw_cwd = cwd.or_else(|| std::env::current_dir().ok());
                let target = router::resolve_open_target(
                    &sqlfiles_dir,
                    &folders,
                    path,
                    home.as_deref(),
                    raw_cwd.as_deref(),
                )
                .map_err(|e| AppError::QueryFile(e.to_string()))?;
                let server = self.find_server(&target.connection).await?;
                // 拡張子が接続エンジンのものと一致することを検証する。
                // 一致しないと「router / verify_within_dir が検証したパス」と
                // 「query_files が拡張子を付け直して実際に開くパス」がズレて、
                // symlink 防御が実 I/O 対象に効かなくなる (例: SQL 接続に
                // foo.redis を渡すと検証は foo.redis、実 I/O は foo.redis.sql)。
                let ext = engines::capabilities_for_name(&server.engine).file_extension;
                if !target
                    .file_name
                    .to_ascii_lowercase()
                    .ends_with(&format!(".{ext}"))
                {
                    return Err(AppError::QueryFile(format!(
                        "The file extension does not match the connection's engine \
                         (expected .{ext}): {}",
                        target.file_name
                    )));
                }
                // 多重防御: 字句検証 (router) を通っても、接続フォルダやファイルが
                // シンボリックリンクで保存領域外の実体を指していることがある。
                // 実際に開くパス (sqlfiles_dir/<folder>/<file>) を canonicalize して、
                // リンク解決後も保存ディレクトリ配下に留まることを確かめる
                // (「queryfolio のデータ保存パスのみ対象」の要件を実体レベルで担保)。
                let folder = server.sqlfiles_folder_name();
                let concrete = sqlfiles_dir.join(&folder).join(&target.file_name);
                query_files::verify_within_dir(&sqlfiles_dir, &concrete)?;
                Ok(target)
            }
            // 接続名 + ファイル名の指定 (CLI の `write`)。**ここでは書き込まない** —
            // 書き出しは起動側プロセスが Tauri の起動前に済ませている
            // (router::Route::WriteFile のドキュメント参照)。ここは
            // 「その接続のそのファイルを開く」の解決だけを行う。
            router::Route::WriteFile {
                connection,
                file_name,
                ..
            } => {
                let server = self.find_server(connection).await?;
                let ext = engines::capabilities_for_name(&server.engine).file_extension;
                // 起動側の書き出しと同じ正規化を通す (拡張子の補完・名前の検証)。
                // 同じ関数を使うことで「検証した名前」と「実際に書いた名前」が
                // 必ず一致する。
                let file_name = query_files::normalize_file_name(file_name, ext)?;
                let sqlfiles_dir = self.resolve_sqlfiles_dir().await?;
                let sqlfiles_dir =
                    std::path::absolute(&sqlfiles_dir).unwrap_or(sqlfiles_dir);
                let concrete = sqlfiles_dir
                    .join(server.sqlfiles_folder_name())
                    .join(&file_name);
                // 書き出しに失敗している (存在しない) 場合は、canonicalize の
                // 一般的な I/O エラーではなく分かりやすい文言で返す。
                if !concrete.exists() {
                    return Err(AppError::QueryFile(format!(
                        "File not found: {}",
                        concrete.display()
                    )));
                }
                // OpenFile と同じ多重防御 (シンボリックリンクで保存領域外を
                // 指していないか)。
                query_files::verify_within_dir(&sqlfiles_dir, &concrete)?;
                Ok(router::OpenTarget {
                    connection: server.name.clone(),
                    file_name,
                })
            }
        }
    }

    async fn find_server(&self, connection: &str) -> Result<ServerConfig, AppError> {
        let mut servers = self.servers.lock().await;
        if servers.is_none() {
            *servers = Some(self.resolve_config().await?.resolve_servers()?);
        }
        servers
            .as_ref()
            .unwrap()
            .iter()
            .find(|s| s.name == connection)
            .cloned()
            .ok_or_else(|| {
                AppError::Config(format!("Connection '{connection}' is not defined in the config"))
            })
    }

    /// クエリファイル操作に必要なコンテキストを解決する:
    /// 保存ディレクトリ・接続フォルダ名・エンジン別のファイル拡張子。
    /// フォルダ名は folder_name → <host>_<engine>_<schema>_<user> の順で決まる
    /// (接続 name はフォルダ名には使わない)。
    async fn resolve_files_ctx(
        &self,
        connection: &str,
    ) -> Result<(PathBuf, String, &'static str), AppError> {
        let server = self.find_server(connection).await?;
        Ok((
            self.resolve_sqlfiles_dir().await?,
            server.sqlfiles_folder_name(),
            engines::capabilities_for_name(&server.engine).file_extension,
        ))
    }

    /// 接続のクエリファイルフォルダに、接続を説明するメタファイルを書き出す。
    /// フォルダが未作成なら何もしない (メタだけのために空フォルダを作らない)。
    /// クエリファイルの作成・保存・一覧時のリフレッシュに使う。
    async fn refresh_folder_meta(&self, server: &ServerConfig) -> Result<(), AppError> {
        let dir = query_files::connection_dir(
            &self.resolve_sqlfiles_dir().await?,
            &server.sqlfiles_folder_name(),
        )?;
        folder_meta::write_folder_meta(&dir, server)
    }

    /// スキーマキャッシュのキーになるアクティブスキーマ名を返す
    /// (オーバーライド > 設定のデフォルト > 空文字)。
    async fn active_schema_key(&self, server: &ServerConfig) -> String {
        match self.db.schema_override(&server.name).await {
            Some(schema) => schema,
            None => server.schema.clone().unwrap_or_default(),
        }
    }

    /// AI 設定を解決する (キャッシュあり)。未設定なら Ok(None)。
    /// マージ済み設定のトップレベル `ai:` を見る (config_override_command で
    /// 取得した YAML 側の ai がローカルより優先されるのはマージの結果)。
    /// 解決エラー (不明 provider 等) はキャッシュせず毎回返す
    /// (設定修正 + リロードで直せるように)。
    async fn resolve_ai_config(&self) -> Result<Option<ai::AiConfig>, AppError> {
        let mut cached = self.ai.lock().await;
        if let Some(ai_config) = cached.as_ref() {
            return Ok(ai_config.clone());
        }
        let ai_config = ai::resolve_ai_config(self.resolve_config().await?.ai().as_ref())?;
        *cached = Some(ai_config.clone());
        Ok(ai_config)
    }

    /// テーブル → カラム名リストのマップを解決する (キャッシュあり)。
    /// SQL 補完 (get_schema_map) と AI の SQL 生成コンテキストで共有する。
    async fn resolve_schema_map(
        &self,
        server: &ServerConfig,
        schema_key: &str,
    ) -> Result<std::collections::BTreeMap<String, Vec<String>>, AppError> {
        if let Some(map) = self
            .schema_cache
            .get_schema_map(&server.name, schema_key)
            .await
        {
            return Ok(map);
        }
        let all = self
            .db
            .with_pool(server, |pool| async move {
                schema_info::fetch_all_columns(&pool).await
            })
            .await?;
        let map = all
            .iter()
            .map(|(table, columns)| {
                (
                    table.clone(),
                    columns.iter().map(|c| c.name.clone()).collect(),
                )
            })
            .collect();
        self.schema_cache
            .put_all_columns(&server.name, schema_key, all)
            .await;
        Ok(map)
    }

    /// AI コマンド (SQL 生成 / エラー修正) 共通のコンテキストを解決する:
    /// AI 設定・接続設定・プロンプト用アクティブスキーマ名・スキーママップ。
    /// AI 未設定時は案内メッセージのエラーを返す。
    async fn resolve_ai_context(
        &self,
        connection: &str,
    ) -> Result<
        (
            ai::AiConfig,
            ServerConfig,
            Option<String>,
            std::collections::BTreeMap<String, Vec<String>>,
        ),
        AppError,
    > {
        let ai_config = self.resolve_ai_config().await?.ok_or_else(|| {
            AppError::Ai(
                "AI is not configured. Add an 'ai:' section (provider / api_key) \
                 to config.yml or the YAML fetched by config_override_command"
                    .into(),
            )
        })?;
        let server = self.find_server(connection).await?;
        let schema_key = self.active_schema_key(&server).await;
        let schema_map = self.resolve_schema_map(&server, &schema_key).await?;
        // sqlite の schema はローカル DB ファイルパスなので、プロンプトには含めない
        let is_sqlite = matches!(
            server.engine.to_ascii_lowercase().as_str(),
            "sqlite" | "sqlite3"
        );
        let active_schema =
            (!is_sqlite && !schema_key.trim().is_empty()).then_some(schema_key);
        Ok((ai_config, server, active_schema, schema_map))
    }
}

#[tauri::command]
async fn get_connections(
    state: tauri::State<'_, AppState>,
) -> Result<Vec<ConnectionInfo>, AppError> {
    let config = state.resolve_config().await?;
    let servers = config.resolve_servers()?;
    let infos = servers.iter().map(ConnectionInfo::from).collect();
    // 同じマージ済み設定から AI 設定も解決してキャッシュする。解決エラーは
    // ここでは接続一覧を壊さず、get_ai_info / ai_generate_sql 側の再解決で返す。
    match ai::resolve_ai_config(config.ai().as_ref()) {
        Ok(ai_config) => *state.ai.lock().await = Some(ai_config),
        Err(_) => *state.ai.lock().await = None,
    }
    *state.servers.lock().await = Some(servers);
    Ok(infos)
}

/// 接続設定のキャッシュ・プール・SSH トンネルを破棄する。
/// 設定を変更した後のリロード時に呼ぶ。
#[tauri::command]
async fn reset_connections(
    app: tauri::AppHandle,
    state: tauri::State<'_, AppState>,
) -> Result<(), AppError> {
    *state.config.lock().await = None;
    *state.servers.lock().await = None;
    *state.ai.lock().await = None;
    state.db.reset().await;
    state.schema_cache.clear().await;
    // 設定を編集して config_override_command の有無が変わることがあるため、
    // コピー用ビュー (保存不可) のメニュー項目の要否を再判定する
    rebuild_menu(&app);
    Ok(())
}

#[tauri::command]
async fn run_query(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
    max_rows: Option<usize>,
    // ツールバーの Writable スイッチの状態。省略・false は読み取り専用
    // (安全側の既定)。config の readonly: true はこれより優先される。
    writable: Option<bool>,
    // 設定の default_limit を自動付与するか。省略時は付与する (従来どおり)。
    // Copy / Export は結果テーブルの表示用ではなく全件を出したいので false で呼ぶ。
    apply_default_limit: Option<bool>,
) -> Result<QueryResult, AppError> {
    let server = state.find_server(&connection).await?;
    // config の readonly が最優先のハードロック。次にスイッチ。
    let readonly_guard = if server.readonly {
        db::ReadonlyGuard::Config
    } else if writable.unwrap_or(false) {
        db::ReadonlyGuard::Off
    } else {
        db::ReadonlyGuard::Switch
    };
    // 履歴記録用に実行時点のアクティブスキーマを控えておく
    let schema = match state.db.schema_override(&connection).await {
        Some(schema) => Some(schema),
        None => server.schema.clone(),
    };
    // 結果テーブルの表示用 (apply_default_limit) と、Copy / Export 用の
    // 全件取得 (apply_default_limit = false) で行数の扱いを分ける。
    let apply_default_limit = apply_default_limit.unwrap_or(true);
    let default_limit = if apply_default_limit {
        state.resolve_default_limit().await?
    } else {
        0
    };
    let auto_limit = match default_limit {
        0 => None,
        limit => Some(limit),
    };
    // 表示用の実行では、SQL 自身が LIMIT を持っていて auto_limit を付けられない
    // 場合 (LIMIT 10000 等) でも、結果テーブルには default_limit 行までしか出さない
    // (打ち切りは truncated で UI に出る)。
    //
    // auto_limit が付く文 (LIMIT 無しの SELECT) はここで絞らない。絞ると
    // 「LIMIT 500 で 500 行返ってきた」時に必ず truncated が立ち、
    // 通常のクエリすべてに打ち切り表示が出てしまうため。
    let max_rows = max_rows.unwrap_or(DEFAULT_MAX_ROWS);
    // auto_limit で SQL 側が絞られる文はクライアント側の上限を触らない。
    // engine 名が不正な場合はここでエラーにせず false に倒す (実行時に
    // 下の async ブロックが同じエラーを返し、失敗として履歴に残るため)。
    let sql_gets_auto_limit = auto_limit.is_some()
        && db::parse_engine(&server.engine)
            .map(|engine| db::should_auto_limit(&sql, engine))
            .unwrap_or(false);
    let max_rows = if default_limit > 0 && !sql_gets_auto_limit {
        max_rows.min(default_limit as usize)
    } else {
        max_rows
    };
    let started = std::time::Instant::now();

    let result = async {
        // \c <database> と USE <database> は SQL の実行ではなく接続状態の
        // 変更なので、プールを取得する前にここで処理する。
        // (メタコマンドの解釈エラーもここで出すことで、失敗として履歴に残る)
        let engine = db::parse_engine(&server.engine)?;
        if let Some(meta_commands::MetaCommand::Connect(schema)) =
            meta_commands::translate(engine, &sql)?
        {
            return switch_active_schema(&state, &server, schema, started).await;
        }
        let (cancels, connection, sql) = (&state.query_cancels, &connection, &sql);
        state
            .db
            .with_pool(&server, |pool: DbPool| async move {
                db::run_query_cancellable(
                    &pool,
                    cancels,
                    connection,
                    sql,
                    max_rows,
                    auto_limit,
                    readonly_guard,
                    server.allow_dangerous_statements,
                )
                .await
            })
            .await
    }
    .await;

    // 成功・失敗にかかわらず実行履歴を記録する。
    // 記録の失敗でクエリ結果を損なわないよう、エラーはログに留める。
    // (追記は小さな同期 I/O なので async コンテキストのまま行う。
    //  ローテーション時のみ全読み・書き直しが走るが、上限 1 万行 =
    //  高々数 MB のため許容する)
    let entry = history::HistoryEntry {
        time: chrono::Local::now().to_rfc3339_opts(chrono::SecondsFormat::Millis, false),
        sql,
        schema,
        row_count: result.as_ref().ok().map(|r| match r.affected_rows {
            Some(affected) => affected,
            None => r.row_count as u64,
        }),
        elapsed_ms: started.elapsed().as_millis() as u64,
        success: result.is_ok(),
    };
    match history::default_history_dir() {
        Ok(dir) => {
            if let Err(e) = state.history.append(&dir, &connection, &entry) {
                eprintln!("[history] failed to record the query history: {e}");
            }
        }
        Err(e) => eprintln!("[history] {e}"),
    }

    result
}

/// `\c <database>` / `USE <database>` の実処理。アクティブスキーマを切り替え、切替後の接続で
/// 確認用のクエリを実行して結果として返す (空の結果だと成功が分かりにくいため)。
///
/// 切替に失敗した場合 (存在しない database 等) は元のスキーマへ戻す。
/// 戻さないと、以降すべてのクエリが接続できない状態で残ってしまう。
async fn switch_active_schema(
    state: &tauri::State<'_, AppState>,
    server: &ServerConfig,
    schema: String,
    started: std::time::Instant,
) -> Result<QueryResult, AppError> {
    let previous = state.db.schema_override(&server.name).await;
    state.db.set_schema_override(&server.name, schema.clone()).await;

    // 切替後の接続で実際に繋がることを確かめる。ここで失敗したら巻き戻す
    let confirm = async {
        let pool: DbPool = state.db.get_pool(server).await?;
        let sql = match db::parse_engine(&server.engine)? {
            db::Engine::MySql => "SELECT DATABASE() AS `database`",
            db::Engine::Postgres => "SELECT current_database() AS database",
            // sqlite / duckdb / redis は meta_commands 側で弾いているのでここには来ない
            db::Engine::Sqlite => {
                return Err(AppError::Config(
                    "\\c is not supported for SQLite".into(),
                ));
            }
            db::Engine::DuckDb => {
                return Err(AppError::Config(
                    "\\c is not supported for DuckDB".into(),
                ));
            }
            db::Engine::Redis | db::Engine::Elasticsearch | db::Engine::DynamoDb => {
                return Err(AppError::Config(
                    "\\c is not supported for this engine".into(),
                ));
            }
        };
        let cancels = &state.query_cancels;
        state
            .db
            .retry_on_expired_password(server, pool, |pool| async move {
                db::run_query_cancellable(
                    &pool,
                    cancels,
                    &server.name,
                    sql,
                    DEFAULT_MAX_ROWS,
                    None,
                    // 確認用の SELECT なので readonly 接続でも通る
                    db::ReadonlyGuard::Config,
                    false,
                )
                .await
            })
            .await
    }
    .await;

    match confirm {
        Ok(mut result) => {
            // 確認クエリの実行中にユーザーがスキーマ選択で別の database へ
            // 変えていた場合、こちらの切替先をフロントへ報告すると
            // 実際の接続先と表示が食い違うため報告しない
            // (そちらの切替が自前でキャッシュ破棄と表示更新を済ませている)
            let still_ours =
                state.db.schema_override(&server.name).await.as_deref() == Some(schema.as_str());
            if still_ours {
                // 切替後は古いスキーマのテーブル一覧・カラムを返さないようにする
                state.schema_cache.invalidate_connection(&server.name).await;
                result.switched_schema = Some(schema);
            }
            result.elapsed_ms = started.elapsed().as_millis() as u64;
            Ok(result)
        }
        Err(e) => {
            // 切替中にユーザーがスキーマ選択で別の database へ変えていた場合は
            // 巻き戻さない (そちらの選択を尊重する)
            state
                .db
                .rollback_schema_override(&server.name, &schema, previous)
                .await;
            // キャンセルはフロントが「Query cancelled」の完全一致で判定して
            // 専用表示にするため、理由を包まずそのまま返す
            if matches!(e, AppError::Cancelled) {
                return Err(e);
            }
            Err(AppError::Config(format!(
                "Failed to switch to {schema}: {e}"
            )))
        }
    }
}

/// 接続で実行中のクエリにキャンセルを要求する。
/// 実行中のクエリが無ければ何もせず false を返す。
/// キャンセルされた実行は run_query 側が AppError::Cancelled
/// ("Query cancelled") で返る。
#[tauri::command]
async fn cancel_query(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<bool, AppError> {
    state.query_cancels.cancel(&connection).await
}

/// AI チャットのエージェントの往復を中断する。
/// request_ids はフロントが採番した実行中リクエストの ID
/// (同じ接続で複数の往復が走りうるため、まとめて渡す)。
///
/// 実行中のクエリを止める (CancelRegistry) だけでなく、ID を控えて
/// 次のモデル呼び出し・ツール実行も行わせない。まだ ai_chat が走り
/// 出していないリクエストの ID も控えられるので、送信直後の中断も効く。
/// 戻り値は「実行中のクエリを実際に止めたか」。
#[tauri::command]
async fn cancel_ai_chat(
    state: tauri::State<'_, AppState>,
    connection: String,
    request_ids: Vec<String>,
) -> Result<bool, AppError> {
    // 先に**全ての ID を記録する**。クエリのキャンセルは DB へ問い合わせる
    // ため失敗しうるが、そこで打ち切ると残りの往復が中断されないまま
    // 切替後のバックエンドで動き続けてしまう
    for request_id in &request_ids {
        state.chat_cancels.request(request_id).await;
    }
    let mut cancelled_query = false;
    let mut first_error: Option<AppError> = None;
    for request_id in &request_ids {
        match state
            .query_cancels
            .cancel(&chat_cancel_key(&connection, request_id))
            .await
        {
            Ok(true) => cancelled_query = true,
            Ok(false) => {}
            // 1 本の失敗で残りのキャンセルを止めない (記録は済んでいるので
            // 往復自体は次の判定で止まる)。エラーは最初の 1 件だけ返す
            Err(e) => {
                first_error.get_or_insert(e);
            }
        }
    }
    match first_error {
        Some(e) => Err(e),
        None => Ok(cancelled_query),
    }
}

/// 接続のクエリ実行履歴を新しい順に返す。
/// search を指定すると SQL の部分一致 (大文字小文字を区別しない) で絞り込む。
#[tauri::command]
fn list_query_history(
    connection: String,
    search: Option<String>,
    limit: Option<usize>,
) -> Result<Vec<history::HistoryEntry>, AppError> {
    history::list_history(
        &history::default_history_dir()?,
        &connection,
        search.as_deref(),
        limit.unwrap_or(history::DEFAULT_LIST_LIMIT),
    )
}

#[tauri::command]
async fn list_query_files(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<Vec<query_files::QueryFileEntry>, AppError> {
    let server = state.find_server(&connection).await?;
    let ext = engines::capabilities_for_name(&server.engine).file_extension;
    let files = query_files::list_query_file_entries(
        &state.resolve_sqlfiles_dir().await?,
        &server.sqlfiles_folder_name(),
        ext,
    )?;
    // フォルダを開いた時に接続の説明メタファイルを最新化する (ベストエフォート:
    // メタ書き込みの失敗で一覧取得を壊さない)。フォルダ未作成時は何もしない。
    let _ = state.refresh_folder_meta(&server).await;
    Ok(files)
}

/// 接続のクエリファイルをファイル名・中身で検索する (大文字小文字を区別しない部分一致)。
#[tauri::command]
async fn search_query_files(
    state: tauri::State<'_, AppState>,
    connection: String,
    query: String,
) -> Result<Vec<query_files::FileSearchHit>, AppError> {
    let (dir, folder, ext) = state.resolve_files_ctx(&connection).await?;
    query_files::search_query_files(&dir, &folder, &query, ext)
}

#[tauri::command]
async fn read_query_file(
    state: tauri::State<'_, AppState>,
    connection: String,
    file_name: String,
) -> Result<String, AppError> {
    let (dir, folder, ext) = state.resolve_files_ctx(&connection).await?;
    query_files::read_query_file(&dir, &folder, &file_name, ext)
}

/// クエリファイルの絶対パスを返す (FilesPane の「Copy full path」用)。
#[tauri::command]
async fn query_file_path(
    state: tauri::State<'_, AppState>,
    connection: String,
    file_name: String,
) -> Result<String, AppError> {
    let (dir, folder, ext) = state.resolve_files_ctx(&connection).await?;
    query_files::query_file_path(&dir, &folder, &file_name, ext)
}

#[tauri::command]
async fn write_query_file(
    state: tauri::State<'_, AppState>,
    connection: String,
    file_name: String,
    content: String,
) -> Result<(), AppError> {
    let server = state.find_server(&connection).await?;
    let ext = engines::capabilities_for_name(&server.engine).file_extension;
    query_files::write_query_file(
        &state.resolve_sqlfiles_dir().await?,
        &server.sqlfiles_folder_name(),
        &file_name,
        &content,
        ext,
    )?;
    // 保存でフォルダが確実に存在するタイミングで説明メタファイルを最新化する
    // (ベストエフォート: メタ書き込みの失敗で保存を壊さない)。
    let _ = state.refresh_folder_meta(&server).await;
    Ok(())
}

/// 楽観的排他つきの保存。expected_base とディスクの現在内容が一致する時だけ書き込む。
/// 書けたら true、アプリ外で変更されていて書かなかったら false を返す
/// (フロントはマージ/衝突処理へ回す)。暗黙の保存 (自動保存・閉じる前保存) で使い、
/// 外部変更を黙って上書きしないための atomic 寄りの CAS。
#[tauri::command]
async fn write_query_file_if_unchanged(
    state: tauri::State<'_, AppState>,
    connection: String,
    file_name: String,
    content: String,
    expected_base: String,
) -> Result<bool, AppError> {
    let server = state.find_server(&connection).await?;
    let ext = engines::capabilities_for_name(&server.engine).file_extension;
    let wrote = query_files::write_query_file_if_unchanged(
        &state.resolve_sqlfiles_dir().await?,
        &server.sqlfiles_folder_name(),
        &file_name,
        &content,
        &expected_base,
        ext,
    )?;
    if wrote {
        let _ = state.refresh_folder_meta(&server).await;
    }
    Ok(wrote)
}

#[tauri::command]
async fn create_query_file(
    state: tauri::State<'_, AppState>,
    connection: String,
    file_name: String,
) -> Result<String, AppError> {
    let server = state.find_server(&connection).await?;
    let ext = engines::capabilities_for_name(&server.engine).file_extension;
    let normalized = query_files::create_query_file(
        &state.resolve_sqlfiles_dir().await?,
        &server.sqlfiles_folder_name(),
        &file_name,
        ext,
    )?;
    // フォルダ新規作成のタイミングで接続の説明メタファイルを書き出す
    // (ベストエフォート: メタ書き込みの失敗で作成を壊さない)。
    let _ = state.refresh_folder_meta(&server).await;
    Ok(normalized)
}

#[tauri::command]
async fn delete_query_file(
    state: tauri::State<'_, AppState>,
    connection: String,
    file_name: String,
) -> Result<(), AppError> {
    let (dir, folder, ext) = state.resolve_files_ctx(&connection).await?;
    query_files::delete_query_file(&dir, &folder, &file_name, ext)
}

#[tauri::command]
async fn rename_query_file(
    state: tauri::State<'_, AppState>,
    connection: String,
    old_name: String,
    new_name: String,
) -> Result<String, AppError> {
    let (dir, folder, ext) = state.resolve_files_ctx(&connection).await?;
    query_files::rename_query_file(&dir, &folder, &old_name, &new_name, ext)
}

/// クエリファイルを別の接続のフォルダへ移動する (FILES から CONNECTIONS への
/// ドラッグ & ドロップ)。正規化された移動後のファイル名を返す。
#[tauri::command]
async fn move_query_file(
    state: tauri::State<'_, AppState>,
    from_connection: String,
    to_connection: String,
    file_name: String,
) -> Result<String, AppError> {
    let from = state.find_server(&from_connection).await?;
    let to = state.find_server(&to_connection).await?;
    let from_ext = engines::capabilities_for_name(&from.engine).file_extension;
    let to_ext = engines::capabilities_for_name(&to.engine).file_extension;
    // クエリファイルの拡張子はエンジンごとに違う (.sql / .redis / .es)。
    // 拡張子が変わる移動は、移動先の一覧に出てこないファイルを作るだけなので
    // 受け付けない (勝手に拡張子を付け替えると中身と食い違う)。
    if from_ext != to_ext {
        return Err(AppError::QueryFile(format!(
            "Cannot move a .{from_ext} file to \"{to_connection}\": it uses .{to_ext} files"
        )));
    }
    // 別の接続でもクエリファイルの保存フォルダは同じことがある
    // (folder_name の明示指定、または host/engine/schema/user が同じ場合)。
    // 移動しても同じ場所なので、成功として返さずここで知らせる。成功にすると
    // フロントがタブを閉じて "Moved" と出すのに、ファイルは移動元の一覧に
    // 残ったままになる。
    if from.sqlfiles_folder_name() == to.sqlfiles_folder_name() {
        return Err(AppError::QueryFile(format!(
            "\"{to_connection}\" shares the same query file folder as \"{from_connection}\": the file is already there"
        )));
    }
    let moved = query_files::move_query_file(
        &state.resolve_sqlfiles_dir().await?,
        &from.sqlfiles_folder_name(),
        &to.sqlfiles_folder_name(),
        &file_name,
        from_ext,
    )?;
    // 移動先フォルダが新規作成された場合があるので、接続の説明メタファイルを
    // 書き出す (ベストエフォート: メタ書き込みの失敗で移動を壊さない)。
    let _ = state.refresh_folder_meta(&to).await;
    Ok(moved)
}

/// 接続先サーバー上の database (スキーマ) 一覧を返す。
#[tauri::command]
async fn list_schemas(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<Vec<String>, AppError> {
    let server = state.find_server(&connection).await?;
    let server = &server;
    state
        .db
        .with_pool(server, |pool| async move { db::list_schemas(&pool, server).await })
        .await
}

/// 接続のアクティブスキーマ (database) を切り替える。
/// プールが再構築され、次のクエリから新しい database に接続される。
#[tauri::command]
async fn set_active_schema(
    state: tauri::State<'_, AppState>,
    connection: String,
    schema: String,
) -> Result<(), AppError> {
    if schema.trim().is_empty() {
        return Err(AppError::Config("The schema name is empty".into()));
    }
    // 接続名の実在確認 (存在しない接続へのオーバーライド蓄積を防ぐ)
    state.find_server(&connection).await?;
    state.db.set_schema_override(&connection, schema).await;
    // 切替後に古いスキーマ情報を返さないよう、接続単位でキャッシュを破棄する
    state.schema_cache.invalidate_connection(&connection).await;
    Ok(())
}

/// 接続のアクティブスキーマを返す (オーバーライトが無ければ設定のデフォルト)。
#[tauri::command]
async fn get_active_schema(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<Option<String>, AppError> {
    if let Some(schema) = state.db.schema_override(&connection).await {
        return Ok(Some(schema));
    }
    let server = state.find_server(&connection).await?;
    Ok(db::resolve_active_schema(
        &server.engine,
        server.schema.as_deref(),
    ))
}

/// 指定接続のプールと SSH トンネルを破棄する。
/// この接続のエディタタブが全て閉じられた時にフロントから呼ぶ。
/// 接続設定・アクティブスキーマの選択は残るため、次に必要になった時
/// (ファイルを開く / スキーマブラウザを開く / クエリ実行) に自動で張り直される。
#[tauri::command]
async fn disconnect(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<(), AppError> {
    state.db.disconnect(&connection).await;
    Ok(())
}

/// 接続先のテーブル / ビューの一覧を返す (キャッシュあり)。
/// refresh = true でキャッシュを破棄して再取得する (リロードボタン用)。
#[tauri::command]
async fn list_tables(
    state: tauri::State<'_, AppState>,
    connection: String,
    refresh: Option<bool>,
) -> Result<Vec<schema_info::TableInfo>, AppError> {
    let server = state.find_server(&connection).await?;
    let schema_key = state.active_schema_key(&server).await;
    if refresh.unwrap_or(false) {
        // カラムのキャッシュも古い可能性があるため、スキーマ単位で丸ごと破棄する
        state
            .schema_cache
            .invalidate_schema(&connection, &schema_key)
            .await;
    } else if let Some(tables) = state.schema_cache.get_tables(&connection, &schema_key).await {
        return Ok(tables);
    }
    let tables = state
        .db
        .with_pool(&server, |pool| async move { schema_info::fetch_tables(&pool).await })
        .await?;
    state
        .schema_cache
        .put_tables(&connection, &schema_key, &tables)
        .await;
    Ok(tables)
}

/// テーブルのカラム一覧を返す (キャッシュあり。ツリー展開時の遅延ロード用)。
/// table は list_tables が返す qualified_name を渡す。
#[tauri::command]
async fn list_columns(
    state: tauri::State<'_, AppState>,
    connection: String,
    table: String,
) -> Result<Vec<schema_info::ColumnInfo>, AppError> {
    let server = state.find_server(&connection).await?;
    let schema_key = state.active_schema_key(&server).await;
    if let Some(columns) = state
        .schema_cache
        .get_columns(&connection, &schema_key, &table)
        .await
    {
        return Ok(columns);
    }
    let table_name = &table;
    let columns = state
        .db
        .with_pool(&server, |pool| async move {
            schema_info::fetch_columns(&pool, table_name).await
        })
        .await?;
    state
        .schema_cache
        .put_columns(&connection, &schema_key, &table, &columns)
        .await;
    Ok(columns)
}

/// テーブル名 → カラム名リストのマップを返す (SQL 補完の強化用)。
/// キャッシュに全テーブル分のカラムが無ければ一括取得してキャッシュする。
#[tauri::command]
async fn get_schema_map(
    state: tauri::State<'_, AppState>,
    connection: String,
) -> Result<std::collections::BTreeMap<String, Vec<String>>, AppError> {
    let server = state.find_server(&connection).await?;
    let schema_key = state.active_schema_key(&server).await;
    state.resolve_schema_map(&server, &schema_key).await
}

/// テーブルの主キーを構成するカラム名を返す (結果グリッドのセル編集用)。
/// 主キーが無いテーブルでは空を返す。
#[tauri::command]
async fn get_primary_keys(
    state: tauri::State<'_, AppState>,
    connection: String,
    table: String,
) -> Result<Vec<String>, AppError> {
    let server = state.find_server(&connection).await?;
    let table = &table;
    state
        .db
        .with_pool(&server, |pool| async move {
            schema_info::fetch_primary_keys(&pool, table).await
        })
        .await
}

/// 結果グリッドのセル編集を UPDATE 群として 1 トランザクションで適用する。
/// writable の解決は run_query と同じ (config readonly が最優先、次にスイッチ)。
/// 合計の影響行数を返す。
#[tauri::command]
async fn run_statements(
    state: tauri::State<'_, AppState>,
    connection: String,
    statements: Vec<String>,
    writable: Option<bool>,
) -> Result<u64, AppError> {
    let server = state.find_server(&connection).await?;
    let readonly_guard = if server.readonly {
        db::ReadonlyGuard::Config
    } else if writable.unwrap_or(false) {
        db::ReadonlyGuard::Off
    } else {
        db::ReadonlyGuard::Switch
    };
    let (statements, allow_dangerous) = (&statements, server.allow_dangerous_statements);
    state
        .db
        .with_pool(&server, |pool| async move {
            db::run_statements(&pool, statements, readonly_guard, allow_dangerous).await
        })
        .await
}

/// AI 設定の情報 (configured / model) を返す。api_key は含めない。
/// `ai:` セクションが無い場合はエラーではなく configured: false。
/// セクションはあるが不正 (不明 provider 等) な場合はエラーを返す。
#[tauri::command]
async fn get_ai_info(state: tauri::State<'_, AppState>) -> Result<ai::AiInfo, AppError> {
    Ok(match state.resolve_ai_config().await? {
        Some(config) => ai::AiInfo {
            configured: true,
            model: config.model().to_string(),
        },
        None => ai::AiInfo {
            configured: false,
            model: String::new(),
        },
    })
}

/// 自然言語の指示から SQL を生成して返す。実行はせず、エディタへの
/// 挿入もフロント側に任せる (ユーザーが確認してから実行する)。
/// LLM に送るのはスキーマ情報 (テーブル・カラム名)・エンジン方言・
/// アクティブスキーマ名・ユーザーの指示のみ。クエリの結果データや
/// 接続情報 (ホスト・認証情報) は送らない。
#[tauri::command]
async fn ai_generate_sql(
    state: tauri::State<'_, AppState>,
    connection: String,
    instruction: String,
) -> Result<String, AppError> {
    if instruction.trim().is_empty() {
        return Err(AppError::Ai("The instruction is empty".into()));
    }
    let (ai_config, server, active_schema, schema_map) =
        state.resolve_ai_context(&connection).await?;
    let system_prompt =
        ai::build_sql_system_prompt(&server.engine, active_schema.as_deref(), &schema_map);
    let response = ai::chat_complete(&ai_config, &system_prompt, &instruction).await?;
    Ok(ai::strip_sql_fences(&response))
}

/// 失敗した SQL と DB のエラーメッセージから修正案の SQL を生成して返す。
/// 実行はせず、エディタへの反映もユーザーの確認 (Apply) に任せる。
/// LLM に送るのは失敗した SQL・エラーメッセージ・スキーマ情報
/// (テーブル・カラム名)・エンジン方言・アクティブスキーマ名のみ。
/// クエリの結果データや接続情報 (ホスト・認証情報) は送らない。
/// 注意: DB のエラーメッセージ自体が値を含むことがある (例: 一意制約違反の
/// DETAIL に衝突したキー値が載る)。修正に必要な情報のため加工せず送る
/// 設計とし、フロントのボタン tooltip で送信内容を明示している。
#[tauri::command]
async fn ai_fix_sql(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
    error_message: String,
) -> Result<String, AppError> {
    if sql.trim().is_empty() {
        return Err(AppError::Ai("The SQL statement is empty".into()));
    }
    if error_message.trim().is_empty() {
        return Err(AppError::Ai("The error message is empty".into()));
    }
    let (ai_config, server, active_schema, schema_map) =
        state.resolve_ai_context(&connection).await?;
    let system_prompt =
        ai::build_fix_sql_system_prompt(&server.engine, active_schema.as_deref(), &schema_map);
    let user_prompt = ai::build_fix_sql_user_prompt(&sql, &error_message);
    let response = ai::chat_complete(&ai_config, &system_prompt, &user_prompt).await?;
    Ok(ai::strip_sql_fences(&response))
}

/// エンジン別の EXPLAIN プレフィックスを付けた SQL を組み立てて返す。
/// 実行はしない (フロントが通常の run_query 経路で実行する)。
/// 対象は SELECT / WITH のみ (Postgres の EXPLAIN ANALYZE は対象文を
/// 実際に実行するため、DML への付与はエラーで拒否する)。
#[tauri::command]
async fn build_explain_sql(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
) -> Result<String, AppError> {
    let server = state.find_server(&connection).await?;
    db::build_explain_sql(&server.engine, &sql)
}

/// 危険な文 (WHERE 無し UPDATE/DELETE、DROP/TRUNCATE) なら理由を返す。
/// 実行はしない。allow_dangerous_statements が有効な接続で、フロントが
/// 実行前に確認ダイアログを出すかどうかを判断するために使う
/// (無効な接続では run_query 側が拒否するため、フロントは呼ぶ必要がない)。
#[tauri::command]
async fn check_dangerous_statement(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
) -> Result<Option<String>, AppError> {
    let server = state.find_server(&connection).await?;
    db::dangerous_statement_reason(&server.engine, &sql)
}

/// Copy / Export で全件を取り直すために、その SQL をもう一度実行してよいかを返す。
///
/// 結果テーブルは default_limit で打ち切られるため、全件を出すには同じ SQL を
/// 実行し直す必要がある。ただし書き込みを伴う文を二度実行してしまうと事故になる
/// ので、AI エージェント経路と同じ厳しい読み取り専用判定 (複文・EXPLAIN ANALYZE・
/// CALL / PRAGMA も拒否) を通ったものだけ許可する。
#[tauri::command]
async fn can_rerun_for_output(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
) -> Result<bool, AppError> {
    let server = state.find_server(&connection).await?;
    let engine = db::parse_engine(&server.engine)?;
    Ok(db::is_safe_to_rerun(&sql, engine))
}

/// EXPLAIN の実行計画を AI に解説させ、ボトルネックの特定・インデックス
/// 提案・書き直し案の Markdown を返す。LLM に送るのはスキーマ情報
/// (テーブル・カラム名)・エンジン方言・アクティブスキーマ名・SQL・
/// 実行計画テキストのみ (実行計画はクエリの結果データではなくプランナー
/// 出力なので許容する)。接続情報 (ホスト・認証情報) は送らない。
#[tauri::command]
async fn ai_explain_plan(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
    plan_text: String,
) -> Result<String, AppError> {
    if sql.trim().is_empty() {
        return Err(AppError::Ai("The SQL statement is empty".into()));
    }
    if plan_text.trim().is_empty() {
        return Err(AppError::Ai("The execution plan is empty".into()));
    }
    let ai_config = state.resolve_ai_config().await?.ok_or_else(|| {
        AppError::Ai(
            "AI is not configured. Add an 'ai:' section (provider / api_key) \
             to config.yml or the YAML fetched by config_override_command"
                .into(),
        )
    })?;
    let server = state.find_server(&connection).await?;
    let schema_key = state.active_schema_key(&server).await;
    let schema_map = state.resolve_schema_map(&server, &schema_key).await?;
    // sqlite の schema はローカル DB ファイルパスなので、プロンプトには含めない
    let is_sqlite = matches!(
        server.engine.to_ascii_lowercase().as_str(),
        "sqlite" | "sqlite3"
    );
    let active_schema =
        (!is_sqlite && !schema_key.trim().is_empty()).then_some(schema_key.as_str());
    let system_prompt =
        ai::build_explain_system_prompt(&server.engine, active_schema, &schema_map);
    let user_message = ai::build_explain_user_message(&sql, &plan_text);
    let response = ai::chat_complete(&ai_config, &system_prompt, &user_message).await?;
    Ok(response.trim().to_string())
}

/// カーソル位置 (選択中) の SQL 文を AI に平易に解説させ、Markdown を返す。
/// 実行はしない。LLM に送るのは SQL・スキーマ情報 (テーブル・カラム名)・
/// エンジン方言・アクティブスキーマ名のみ。クエリの結果データや接続情報
/// (ホスト・認証情報) は送らない。
#[tauri::command]
async fn ai_explain_sql(
    state: tauri::State<'_, AppState>,
    connection: String,
    sql: String,
) -> Result<String, AppError> {
    if sql.trim().is_empty() {
        return Err(AppError::Ai("The SQL statement is empty".into()));
    }
    let (ai_config, server, active_schema, schema_map) =
        state.resolve_ai_context(&connection).await?;
    let system_prompt = ai::build_explain_sql_system_prompt(
        &server.engine,
        active_schema.as_deref(),
        &schema_map,
    );
    let user_message = ai::build_explain_sql_user_message(&sql);
    let response = ai::chat_complete(&ai_config, &system_prompt, &user_message).await?;
    Ok(response.trim().to_string())
}

/// エージェントの run_sql の結果を LLM 向けのテキストに整形する。
/// 行数を明示し、列名 + 各行を 1 行の JSON にして渡す (トークン効率と
/// パースのしやすさの両立)。
fn format_chat_tool_result(result: &QueryResult) -> String {
    let mut text = format!(
        "{} row(s){}",
        result.row_count,
        if result.truncated {
            " (truncated)"
        } else {
            ""
        }
    );
    if let Some(affected) = result.affected_rows {
        text.push_str(&format!(", {affected} affected"));
    }
    text.push_str(&format!("\ncolumns: {}\n", result.columns.join(", ")));
    for row in &result.rows {
        let line = serde_json::to_string(row).unwrap_or_else(|_| "[unserializable row]".into());
        text.push_str(&line);
        text.push('\n');
    }
    ai::truncate_tool_result(&text)
}

/// AI チャット (エージェント) の 1 往復を実行する。
/// フロントは会話履歴を毎回そのまま渡し、バックエンドが system prompt の
/// 組み立てとツール実行ループを担う。
///
/// ツールは読み取り専用の `run_sql` のみで、実行は**常に読み取り専用**
/// (ツールバーの Writable スイッチが ON でも書き込みは許可しない)。
/// エージェントが自分の判断で書き込む事故を構造的に防ぐため。
/// LLM に送るのはスキーマ情報・方言・アクティブスキーマ名・会話履歴と、
/// エージェント自身が実行した読み取りクエリの結果のみ。接続情報
/// (ホスト・認証情報) は送らない。
#[tauri::command]
async fn ai_chat(
    state: tauri::State<'_, AppState>,
    connection: String,
    history: Vec<ai::ChatTurn>,
    request_id: String,
) -> Result<ai::ChatReply, AppError> {
    // 実行したツール呼び出しは失敗時にも返す (途中まで実行したクエリを
    // 隠さない。特に中断・タイムアウトはツール実行の後に起きやすい)
    let mut tool_calls: Vec<ai::ChatToolCall> = Vec::new();
    let result =
        run_ai_chat(&state, &connection, &history, &request_id, &mut tool_calls).await;
    // 中断記録は往復の終了時に掃除する (残っても上限で捨てられるが、
    // 同じ ID が再利用されることはないので溜めておく意味が無い)
    state.chat_cancels.finish(&request_id).await;
    Ok(match result {
        Ok(content) => ai::ChatReply {
            content,
            tool_calls,
            error: None,
        },
        Err(e) => ai::ChatReply {
            content: String::new(),
            tool_calls,
            error: Some(e.to_string()),
        },
    })
}

/// ai_chat の本体。アシスタントの最終メッセージを返し、実行したツール
/// 呼び出しは (失敗時も呼び出し側が拾えるよう) 引数の Vec へ積む。
async fn run_ai_chat(
    state: &AppState,
    connection: &str,
    history: &[ai::ChatTurn],
    request_id: &str,
    tool_calls: &mut Vec<ai::ChatToolCall>,
) -> Result<String, AppError> {
    let connection = connection.to_string();
    let mut messages = ai::chat_history_messages(history);
    if messages.is_empty() {
        return Err(AppError::Ai("The chat history is empty".into()));
    }
    // 中断はリクエスト ID で判定する。接続単位のカウンタだと、同じ接続で
    // 2 本走る時に区別できず、開始直後に届いた中断も「開始時の基準値」に
    // 吸収されてしまう。ID なら、このコマンドが走り出す前に届いた中断も
    // ここで拾える
    let cancelled = || async { state.chat_cancels.is_cancelled(&request_id).await };
    if cancelled().await {
        return Err(AppError::Cancelled);
    }

    let (ai_config, server, active_schema, schema_map) =
        state.resolve_ai_context(&connection).await?;
    // コンテキスト解決の間に中断されていたら、ここで打ち切る
    // (この時点の schema_map / プロンプトは既に古い可能性がある)
    if cancelled().await {
        return Err(AppError::Cancelled);
    }
    // AI 非対応のエンジン (redis / elasticsearch / dynamodb) はフロントでも
    // 入力を塞いでいるが、コマンド側でも拒否する (プロンプトが SQL 前提のため)
    if !engines::capabilities_for_name(&server.engine).supports_ai {
        return Err(AppError::Ai(format!(
            "The AI features are not available for the '{}' engine",
            server.engine
        )));
    }
    let system_prompt =
        ai::build_chat_system_prompt(&server.engine, active_schema.as_deref(), &schema_map);
    messages.insert(
        0,
        serde_json::json!({ "role": "system", "content": system_prompt }),
    );

    // エージェントの実行は Writable スイッチや config に関わらず Agent 固定。
    // 文レベルのガードに加えて DB レベルの読み取り専用 (読み取り専用
    // トランザクション / PRAGMA query_only) も強制される。
    let readonly_guard = db::ReadonlyGuard::Agent;
    // ユーザーのクエリのキャンセル (接続名がキー) と衝突せず、同じ接続の
    // 別の往復とも衝突しないキーを使う
    let cancel_key = chat_cancel_key(&connection, &request_id);

    let engine = db::parse_engine(&server.engine)?;
    // ツール実行の累計。1 応答が複数の tool_calls を並べられるため、
    // 往復回数 (ラウンド) とは別に累計でも上限を課す
    let mut executed_calls = 0usize;
    for _ in 0..ai::CHAT_MAX_TOOL_ROUNDS {
        // 累計上限に達したら、ツールを渡さず最後の回答を書かせる
        // (上限超過をエラーにせず、そこまでに読めた内容で答えさせる)
        let allow_tools = executed_calls < ai::CHAT_MAX_TOOL_CALLS;
        // 会話が破棄された (接続 / スキーマ切替・Clear・Stop) なら、
        // 次のモデル呼び出しもツール実行も行わずに打ち切る
        if cancelled().await {
            return Err(AppError::Cancelled);
        }
        let message = ai::chat_step(&ai_config, &messages, allow_tools, &cancelled).await?;
        // モデルの応答を待つ間に中断された場合、その応答は採用しない
        // (ツール無しの応答で終わる往復が最も多いため、ここを見落とすと
        //  Stop を押しても普通の回答が返ってくる)
        if cancelled().await {
            return Err(AppError::Cancelled);
        }
        let requested = ai::parse_tool_calls(&message);
        if requested.is_empty() {
            let content = ai::message_content(&message);
            if content.is_empty() {
                return Err(AppError::Ai(
                    "The AI returned an empty message".into(),
                ));
            }
            return Ok(content);
        }
        // ツール呼び出しを含むアシスタントメッセージはそのまま履歴へ積む
        // (tool メッセージは直前の tool_calls と対応していなければならない)
        messages.push(message);
        for (id, name, arguments) in requested {
            // 1 応答内で複数の tool_calls を並べられるため、累計でも打ち切る。
            // 打ち切った分にも tool メッセージは返す (tool_calls と対応する
            // tool メッセージが欠けると API がエラーになる)
            if executed_calls >= ai::CHAT_MAX_TOOL_CALLS {
                messages.push(serde_json::json!({
                    "role": "tool",
                    "tool_call_id": id,
                    "content": format!(
                        "Error: the tool call budget ({}) for this reply is exhausted. \
                         Answer with what you have.",
                        ai::CHAT_MAX_TOOL_CALLS
                    ),
                }));
                continue;
            }
            if cancelled().await {
                return Err(AppError::Cancelled);
            }
            executed_calls += 1;
            let (ok, argument, result_text) = if name == "run_sql" {
                match ai::parse_run_sql_argument(&arguments) {
                    Ok(sql) => {
                        // DB へ実際に投げたか (投げていない中断を「実行した」
                        // と記録しないための目印)
                        let mut started = false;
                        let outcome = async {
                            // エージェント経路は通常の readonly ガードより狭い
                            // ホワイトリストを課す (CALL / PRAGMA / 複文を落とす)
                            if let Some(reason) = db::agent_rejection_reason(&sql, engine) {
                                return Err(AppError::Readonly(reason));
                            }
                            // プール取得 (SSH トンネルの確立を含む) は待ちが
                            // 長い。その間に届いた中断は CancelRegistry には
                            // 届かない (run_query_cancellable がまだ登録して
                            // いない) ので、実行の直前にもう一度確認する
                            let pool = state.db.get_pool(&server).await?;
                            if cancelled().await {
                                return Err(AppError::Cancelled);
                            }
                            started = true;
                            // run_query_cancellable は内部で登録するまでの間
                            // (コネクション取得・セッション ID 照会) キャンセル
                            // レジストリに現れないため、そこへ届いた中断は
                            // 空振りする。ポーリングで自前に監視し、中断されたら
                            // クエリの future を drop して待つのをやめる
                            // (サーバー側は登録済みなら停止し、未登録なら
                            //  クライアント側の打ち切りになる)
                            // password_command の期限切れによる認証エラーは
                            // パスワードを取り直して 1 度だけ再実行する
                            // (その間の中断も下のポーリングが拾う)
                            let (cancels, cancel_key, sql) =
                                (&state.query_cancels, &cancel_key, &sql);
                            let query = state.db.retry_on_expired_password(
                                &server,
                                pool,
                                |pool| async move {
                                    db::run_query_cancellable(
                                        &pool,
                                        cancels,
                                        cancel_key,
                                        sql,
                                        ai::CHAT_TOOL_MAX_ROWS,
                                        None,
                                        readonly_guard,
                                        // エージェントには危険な文も許可しない
                                        false,
                                    )
                                    .await
                                },
                            );
                            tokio::pin!(query);
                            loop {
                                tokio::select! {
                                    biased;
                                    result = &mut query => break result,
                                    _ = tokio::time::sleep(
                                        std::time::Duration::from_millis(
                                            CHAT_CANCEL_POLL_INTERVAL_MS,
                                        ),
                                    ) => {
                                        if cancelled().await {
                                            // future を drop するだけでは
                                            // spawn_blocking で走るエンジン
                                            // (DuckDB) は止まらない。この時点
                                            // では登録が済んでいるはずなので、
                                            // エンジン別のキャンセル
                                            // (DuckDB の InterruptHandle 等) を
                                            // 改めて要求してから待つのをやめる
                                            let _ = state
                                                .query_cancels
                                                .cancel(&cancel_key)
                                                .await;
                                            break Err(AppError::Cancelled);
                                        }
                                    }
                                }
                            }
                        }
                        .await;
                        // 中断で終える場合も、DB へ投げた SQL は記録に残す
                        // (結果はモデルにもユーザーにも見せないが、「何を
                        //  実行したか」を隠さない)。中断の狙いは「破棄した /
                        // 切り替えた後のデータを AI プロバイダへ送らないこと」
                        // なので、結果だけを捨てて往復ごと終える
                        let cancelled_now = cancelled().await;
                        if cancelled_now || matches!(outcome, Err(AppError::Cancelled)) {
                            // DB へ投げる前に中断した分は「実行した」と
                            // 記録しない (実行していないクエリを一覧に
                            // 出すと、監査としてかえって誤解を招く)
                            if started {
                                tool_calls.push(ai::ChatToolCall {
                                    name: name.clone(),
                                    argument: sql,
                                    ok: false,
                                    // 実行に入ったことは分かるが、コネクション
                                    // 取得の途中で止まった可能性もあるため
                                    // 「実行した」と断定はしない
                                    summary: "Cancelled (may not have run)".to_string(),
                                });
                            }
                            return Err(AppError::Cancelled);
                        }
                        match outcome {
                            Ok(result) => (true, sql, format_chat_tool_result(&result)),
                            Err(e) => (false, sql, format!("Error: {e}")),
                        }
                    }
                    Err(e) => (false, arguments.clone(), format!("Error: {e}")),
                }
            } else {
                (
                    false,
                    arguments.clone(),
                    format!("Error: unknown tool '{name}'"),
                )
            };
            tool_calls.push(ai::ChatToolCall {
                name: name.clone(),
                argument,
                ok,
                // 要約は 1 行に収める (フロントのツールチップ表示用)
                summary: result_text.lines().next().unwrap_or("").to_string(),
            });
            messages.push(serde_json::json!({
                "role": "tool",
                "tool_call_id": id,
                "content": result_text,
            }));
        }
    }
    // 往復の上限に達した場合も、ツールを渡さない最後の 1 回で回答を書かせる
    // (ここまでのツール結果は履歴に載っているので、調べた内容を無駄にしない)
    if cancelled().await {
        return Err(AppError::Cancelled);
    }
    let message = ai::chat_step(&ai_config, &messages, false, &cancelled).await?;
    if cancelled().await {
        return Err(AppError::Cancelled);
    }
    let content = ai::message_content(&message);
    if content.is_empty() {
        return Err(AppError::Ai(format!(
            "The AI kept calling tools without answering (stopped after {} rounds)",
            ai::CHAT_MAX_TOOL_ROUNDS
        )));
    }
    Ok(content)
}

/// 設定の解決結果を返す (情報表示用。機密を含まない)。
/// マージ済み設定 (キャッシュ) から作るので、config_override_command で
/// 上書きされた sqlfiles_dir 等も実際に使われている値が表示される。
/// キャッシュ経由なので取得コマンドがモーダルを開くたびに走ることはない。
#[tauri::command]
async fn get_config_info(state: tauri::State<'_, AppState>) -> Result<ConfigInfo, AppError> {
    Ok(match state.resolve_config().await {
        Ok(config) => config
            .info()
            .unwrap_or_else(|e| config::config_info_error(&e)),
        Err(e) => config::config_info_error(&e),
    })
}

/// config.yml が無ければテンプレートを作成する。作成した場合はそのパスを返す。
#[tauri::command]
fn ensure_config_file() -> Result<Option<String>, AppError> {
    config::ensure_config_file()
}

/// 設定エディタ用に config.yml の中身を返す (無ければテンプレートを作成してから読む)。
#[tauri::command]
fn read_config_file() -> Result<String, AppError> {
    config::read_config_file()
}

/// 設定エディタからの保存。書き込んだファイルのパスを返す。
#[tauri::command]
fn write_config_file(content: String) -> Result<String, AppError> {
    config::write_config_file(&content)
}

/// config_override_command を実行して取得した生の YAML を返す
/// (コピー用ビュー用。表示先では編集できるが保存はしない)。
#[tauri::command]
async fn read_override_config_yaml() -> Result<String, AppError> {
    config::fetch_override_config_yaml().await
}

/// 結果テーブルの Export で、ネイティブ保存ダイアログ (フロントの
/// plugin-dialog save) でユーザーが選んだパスへテキストを書き出す。
/// パスは実行時にダイアログでユーザーが選んだものが渡ってくる。
///
/// バックエンドではパスを検証しない (任意パスへ書ける) 点に注意。これは
/// このアプリの信頼モデルに沿う: フロントエンドは自前の同梱コードのみで
/// リモートコンテンツを読み込まず、`run_query` で任意 SQL 実行・
/// `write_config_file` で設定書き込みが既に可能なため、フロントが侵害された
/// 場合の被害範囲は元々広い。ここで新たにファイル書き込みが増えることの
/// 追加リスクは限定的と判断している。
/// エクスポート時の文字コード。
///
/// 既定は UTF-8。Excel など UTF-8 を前提としないツール向けに CP932 / EUC-JP を選べる。
/// 文字コード名はフロントから文字列で渡ってくる。
fn encode_export_contents(contents: &str, encoding: &str) -> Result<Vec<u8>, AppError> {
    let encoder = match encoding {
        // 空文字・未指定は既定の UTF-8 として扱う
        "" | "utf-8" | "utf8" => return Ok(contents.as_bytes().to_vec()),
        // encoding_rs の SHIFT_JIS は Encoding Standard の定義により Windows-31J (CP932)
        "cp932" | "shift_jis" | "sjis" => encoding_rs::SHIFT_JIS,
        "euc-jp" | "eucjp" => encoding_rs::EUC_JP,
        other => {
            return Err(AppError::Export(format!(
                "Unsupported export encoding: {other}"
            )));
        }
    };

    let (encoded, _, had_unmappable) = encoder.encode(contents);
    if had_unmappable {
        // 変換できない文字は encoding_rs が数値文字参照 (&#12345;) に置き換える。
        // 黙って壊れた出力を書くとデータの取り違えにつながるため、失敗として返す。
        return Err(AppError::Export(format!(
            "The result contains characters that cannot be represented in {encoding}. \
             Export as UTF-8 instead, or remove those characters."
        )));
    }
    Ok(encoded.into_owned())
}

#[tauri::command]
async fn write_export_file(
    path: String,
    contents: String,
    encoding: Option<String>,
) -> Result<(), AppError> {
    let encoding = encoding.unwrap_or_default();
    let bytes = encode_export_contents(&contents, &encoding)?;
    std::fs::write(&path, bytes)?;
    Ok(())
}

/// frontend_ready の戻り値。開く対象と、起動時指定の解決に失敗したエラーメッセージ。
/// GUI 起動では stderr が見えないため、失敗はフロントへ返してトーストで知らせる。
#[derive(serde::Serialize)]
#[serde(rename_all = "camelCase")]
struct LaunchResult {
    /// 開く対象 (起動時指定 + 起動中にキューされた分)。
    targets: Vec<router::OpenTarget>,
    /// 起動時指定 (launch route) の解決に失敗した理由 (無ければ空)。
    errors: Vec<String>,
}

/// フロントの listener 登録が済んだことを知らせ、それまでに溜まった「開く対象」を
/// まとめて受け取る。フロントは onMount で listener を登録した直後にこれを呼び、
/// 返った各対象について接続を選択してファイルを開き、errors はトーストで知らせる。
/// targets の内訳は次の 2 つ:
/// (1) 起動時に deep link / CLI で指定された launch route (解決してから返す)、
/// (2) 起動中 (ready 前) に届いてキューされた実行中ルートの解決済み対象。
/// 呼び出し後は ready = true になり、以降の実行中ルートは `open-query-file` イベントで
/// 直接届く (listener が既にあるため取りこぼさない)。
#[tauri::command]
async fn frontend_ready(
    state: tauri::State<'_, AppState>,
) -> Result<LaunchResult, AppError> {
    let mut targets = Vec::new();
    let mut errors = Vec::new();
    // (1) 起動時指定 (launch route)。このプロセスの cwd 基準で解決する (None)。
    //     std Mutex は await をまたいで保持しない (take だけして即解放)。
    let launch = state.launch_route.lock().unwrap().take();
    if let Some(route) = launch {
        match state.resolve_route_target(&route, None).await {
            Ok(target) => targets.push(target),
            // 起動時指定が失敗したら、GUI 起動では stderr が見えないためフロントへ
            // 返してトーストで知らせる (握り潰さない)。他の対象・起動は止めない。
            Err(e) => errors.push(e.to_string()),
        }
    }
    // (2) ready にして、それまでにキューされた対象を drain する。
    //     ready 設定と drain を 1 つのロックで行い、dispatch_route の
    //     「ready 判定 → push/emit」と直列化する (取りこぼし・二重配送を防ぐ)。
    let (mut queued, mut queued_errors) = {
        let mut live = state.live.lock().unwrap();
        live.ready = true;
        (
            std::mem::take(&mut live.pending),
            std::mem::take(&mut live.pending_errors),
        )
    };
    targets.append(&mut queued);
    errors.append(&mut queued_errors);
    Ok(LaunchResult { targets, errors })
}

/// 実行中に受け取ったルート (deep link / CLI サブコマンド) を解決し、フロントへ
/// イベントで届ける。解決は設定の読み取りを伴い async なので、別タスクで行う。
/// 成功なら `open-query-file` に OpenTarget を、失敗なら `open-query-file-error` に
/// エラーメッセージを載せる。
fn dispatch_route(app: &tauri::AppHandle, route: router::Route, cwd: Option<PathBuf>) {
    use tauri::{Emitter, Manager};
    let app = app.clone();
    tauri::async_runtime::spawn(async move {
        let state = app.state::<AppState>();
        match state.resolve_route_target(&route, cwd).await {
            Ok(target) => {
                // フロントの listener が未登録 (起動中) なら取りこぼすため、ready で
                // なければキューに積む (frontend_ready が drain する)。ready 判定と
                // push を 1 ロックで行い、frontend_ready の ready 設定 + drain と直列化。
                let emit_now = {
                    let mut live = state.live.lock().unwrap();
                    if live.ready {
                        true
                    } else {
                        live.pending.push(target.clone());
                        false
                    }
                };
                if emit_now {
                    if let Err(e) = app.emit("open-query-file", target) {
                        eprintln!("[router] failed to emit open-query-file: {e}");
                    }
                }
            }
            Err(e) => {
                // 成功対象と同様、listener 未登録 (起動中) なら emit しても取りこぼす。
                // ready でなければエラーもキューに積み、frontend_ready で drain させる。
                let message = e.to_string();
                let emit_now = {
                    let mut live = state.live.lock().unwrap();
                    if live.ready {
                        true
                    } else {
                        live.pending_errors.push(message.clone());
                        false
                    }
                };
                if emit_now {
                    if let Err(emit_err) = app.emit("open-query-file-error", message) {
                        eprintln!("[router] failed to emit open-query-file-error: {emit_err}");
                    }
                }
            }
        }
    });
}

/// 配布物に同梱している依存ライブラリのライセンス一覧 (Third-Party Licenses のモーダル)。
#[tauri::command]
fn third_party_notices() -> &'static str {
    third_party_notices::NOTICES
}

/// About ダイアログに出すメタ情報 (tauri の Menu::default と同じ内容)。
fn about_metadata(app: &tauri::AppHandle) -> tauri::menu::AboutMetadata<'_> {
    let package_info = app.package_info();
    let config = app.config();
    tauri::menu::AboutMetadata {
        name: Some(package_info.name.clone()),
        version: Some(package_info.version.to_string()),
        copyright: config.bundle.copyright.clone(),
        authors: config.bundle.publisher.clone().map(|p| vec![p]),
        ..Default::default()
    }
}

/// アプリのメニューバーを組み立てる。
///
/// macOS のアプリメニュー (Queryfolio) は NSApplication がメインメニュー設置時の
/// 内容で確定させるため、後から項目を insert しても反映されない。そのため
/// tauri のデフォルトメニューを流用せず、アプリメニューを含めて丸ごと自前で組み、
/// Builder::menu で最初の設置時から渡す。設定変更時はこの関数で組み直す。
///
/// 「View override config yaml (Copy only)」は config_override_command が
/// 設定されている時だけ出す。
///
/// 構成は tauri の `Menu::default` を踏襲する (アプリメニュー / View は macOS のみ、
/// File の quit は macOS 以外のみ)。ただし Close Window は置かず、File に
/// Close Tab (CmdOrCtrl+W) を置く (CYBERNEURA-DEV-773)。設定関連の項目は
/// プラットフォームに関わらず Config サブメニューにまとめる
/// (アプリメニューと Config に散らばっていると探しにくいため)。
fn build_menu(app: &tauri::AppHandle) -> tauri::Result<tauri::menu::Menu<tauri::Wry>> {
    use tauri::menu::{MenuBuilder, MenuItemBuilder, SubmenuBuilder};

    let edit_config_item =
        MenuItemBuilder::with_id("edit_config_file", "Edit config.yml").build(app)?;
    let edit_source_item = MenuItemBuilder::with_id(
        "view_override_config",
        "View override config yaml (Copy only)",
    )
    .build(app)?;
    let show_source_item = config::has_config_override_command();
    // About の直下に置く (macOS はアプリメニュー、他は Help メニュー)
    let licenses_item =
        MenuItemBuilder::with_id("show_licenses", "Third-Party Licenses").build(app)?;

    #[cfg(target_os = "macos")]
    let app_menu = {
        use tauri::menu::PredefinedMenuItem;

        let package_info = app.package_info();
        SubmenuBuilder::new(app, package_info.name.clone())
            .item(&PredefinedMenuItem::about(
                app,
                None,
                Some(about_metadata(app)),
            )?)
            .item(&licenses_item)
            .separator()
            .services()
            .separator()
            .hide()
            .hide_others()
            .separator()
            .quit()
            .build()?
    };

    // CmdOrCtrl+W はウインドウではなくアクティブなエディタタブを閉じる。
    // 定義済みの Close Window は macOS で Cmd+W を持ち、ウインドウが 1 枚のこのアプリでは
    // 押すとアプリごと閉じてしまう。そのため File / Window のどちらにも置かない
    // (メニューのキー割り当ては WebView より先に NSApp が処理するので、フロントの
    // keydown で preventDefault しても止められない)。閉じる対象の判断は
    // エディタタブと開いているモーダルを知っているフロントに任せる
    let close_tab_item = MenuItemBuilder::with_id("close_editor_tab", "Close Tab")
        .accelerator("CmdOrCtrl+W")
        .build(app)?;
    let file_menu = {
        let builder = SubmenuBuilder::new(app, "File").item(&close_tab_item);
        #[cfg(not(target_os = "macos"))]
        let builder = builder.quit();
        builder.build()?
    };
    let edit_menu = SubmenuBuilder::new(app, "Edit")
        .undo()
        .redo()
        .separator()
        .cut()
        .copy()
        .paste()
        .select_all()
        .build()?;
    #[cfg(target_os = "macos")]
    let view_menu = SubmenuBuilder::new(app, "View").fullscreen().build()?;
    // Window / Help は tauri と同じ固定 ID で作る。macOS の init_app_menu は
    // この ID でメニューを探して NSApp の windowsMenu / helpMenu に登録するため、
    // ID が無いとウインドウ一覧やヘルプ検索が付かなくなる
    let window_menu = SubmenuBuilder::with_id(app, tauri::menu::WINDOW_SUBMENU_ID, "Window")
        .minimize()
        .maximize()
        .build()?;
    // tauri のデフォルトメニュー同様、macOS では中身を持たない
    // (About はアプリメニュー側にあり、システムがヘルプ検索を足す)
    let help_menu = {
        let builder = SubmenuBuilder::with_id(app, tauri::menu::HELP_SUBMENU_ID, "Help");
        #[cfg(not(target_os = "macos"))]
        let builder = builder
            .about(Some(about_metadata(app)))
            .item(&licenses_item);
        builder.build()?
    };

    // アクセラレータは付けない。以前は CmdOrCtrl+R を割り当てていたが、
    // ブラウザのリロードと同じキーなのに実際に起きるのは reload_config_file =
    // 全エディタタブの破棄・接続の張り直し・チャットの中断で、ページの再読込の
    // つもりで押すとアプリ全体が初期状態へ戻る (CYBERNEURA-DEV-648)。
    // 破壊的なうえ取り消せないので、Config メニューからの明示的な選択だけにする
    let reload_item =
        MenuItemBuilder::with_id("reload_config_file", "Reload config file").build(app)?;
    let reveal_item =
        MenuItemBuilder::with_id("reveal_config_folder", "Reveal config folder").build(app)?;
    let config_menu = {
        let mut builder = SubmenuBuilder::new(app, "Config").item(&edit_config_item);
        if show_source_item {
            builder = builder.item(&edit_source_item);
        }
        builder
            .separator()
            .item(&reload_item)
            .item(&reveal_item)
            .build()?
    };

    #[allow(unused_mut)]
    let mut menu = MenuBuilder::new(app);
    #[cfg(target_os = "macos")]
    {
        menu = menu.item(&app_menu);
    }
    menu = menu.item(&file_menu).item(&edit_menu);
    #[cfg(target_os = "macos")]
    {
        menu = menu.item(&view_menu);
    }
    menu.item(&window_menu)
        .item(&help_menu)
        .item(&config_menu)
        .build()
}

/// 設定を読み直した後にメニューを組み直す。
/// config_override_command の有無が変わるとコピー用ビューの項目の要否も変わるため。
fn rebuild_menu(app: &tauri::AppHandle) {
    match build_menu(app).and_then(|menu| app.set_menu(menu)) {
        Ok(_) => {}
        Err(e) => eprintln!("[menu] failed to rebuild the menu: {e}"),
    }
}

/// config.yml (無ければ設定フォルダ) を Finder 等のファイルマネージャで表示する。
fn reveal_config_folder() -> Result<(), AppError> {
    let target = match config::existing_config_path()? {
        Some(path) => path,
        None => config::app_config_dir()?,
    };
    tauri_plugin_opener::reveal_item_in_dir(&target)
        .map_err(|e| AppError::Config(format!("Failed to reveal {}: {e}", target.display())))
}

/// CLI の `write` サブコマンドで標準入力から受け取る内容の上限 (バイト)。
/// クエリファイルとしては十分に大きく、取り違えたパイプで巨大なファイルを
/// 丸ごとメモリに載せることは防げる大きさ。超過は切り詰めずエラーにする
/// (途中まで書くとクエリが壊れたまま保存される)。
const MAX_STDIN_CONTENT_BYTES: usize = 10 * 1024 * 1024;

/// CLI の `write` サブコマンドの内容を決める。
///
/// 引数で渡っていればそれを使い、無ければ標準入力から読む。次の場合は
/// 「内容の指定なし」= `Ok(None)` として扱う (= 既存ファイルを潰さない):
///
/// - 標準入力が端末 (対話シェルで内容を渡していない。読むと入力待ちで固まる)
/// - 標準入力が空 (GUI 起動 `open -a Queryfolio --args write ...` の stdin は
///   /dev/null で即 EOF になる。これを「空で書け」と解釈すると、既存のクエリ
///   ファイルを黙って空にしてしまう)
///
/// 読み取りの失敗と上限超過は `Err` にする (内容が分からないまま / 途中までで
/// 上書きしない。呼び出し側はここで打ち切って非ゼロ終了する)。
fn resolve_write_content(arg_content: Option<String>) -> Result<Option<String>, AppError> {
    use std::io::{IsTerminal, Read};

    if let Some(content) = arg_content {
        return Ok(Some(content));
    }
    let stdin = std::io::stdin();
    if stdin.is_terminal() {
        return Ok(None);
    }
    // 上限 + 1 バイトまで読み、超えたら「上限超過」と判定する
    // (ちょうど上限で切ると、切り詰めたのか元から上限ぴったりなのか分からない)。
    let mut buf = String::new();
    stdin
        .take(MAX_STDIN_CONTENT_BYTES as u64 + 1)
        .read_to_string(&mut buf)
        .map_err(|e| {
            AppError::QueryFile(format!("Failed to read the content from stdin: {e}"))
        })?;
    if buf.len() > MAX_STDIN_CONTENT_BYTES {
        return Err(AppError::QueryFile(format!(
            "The content from stdin is larger than the {MAX_STDIN_CONTENT_BYTES} byte limit"
        )));
    }
    Ok(if buf.is_empty() { None } else { Some(buf) })
}

/// CLI の `write <connection> <file-name> [content]` を処理し、クエリファイルを
/// 書き出す (内容の指定が無ければ空ファイルを作るだけ。既存は変更しない)。
///
/// **Tauri を起動する前に、起動したプロセス自身が行う**。実行中インスタンスが
/// いる場合、single-instance プラグインが転送するのは argv と cwd だけで
/// 標準入力は渡らないため、書き出しを実行中インスタンス側に任せると
/// パイプで渡した内容が失われる。書き出しを起動側で完結させ、実行中インスタンス
/// (または自分自身) には「開く」だけを任せる。
///
/// 失敗した場合は `Err` を返す。呼び出し側 (run) は起動を続けず**非ゼロで終了する**:
/// 続行して「開く」だけ行うと、書き込みに失敗しているのに古い内容がそのまま開き、
/// 依頼したエージェントには成功したように見えてしまう (終了ステータスからも
/// 失敗を判別できない)。
///
/// 解決したマージ済み設定を返す (書き出しを行わなかった場合は `None`)。
/// 呼び出し側はこれを `AppState` のキャッシュに載せ、`config_override_command` を
/// 1 回の起動で 2 度実行しないようにする (`AppState::with_config` 参照)。
fn apply_cli_write_route(
    route: &router::Route,
) -> Result<Option<Arc<AppConfig>>, AppError> {
    let router::Route::WriteFile {
        connection,
        file_name,
        content,
    } = route
    else {
        return Ok(None);
    };
    let content = resolve_write_content(content.clone())?;
    tauri::async_runtime::block_on(async {
        let config = Arc::new(AppConfig::load_merged().await?);
        let servers = config.resolve_servers()?;
        let server = servers
            .iter()
            .find(|s| &s.name == connection)
            .ok_or_else(|| {
                AppError::Config(format!(
                    "Connection '{connection}' is not defined in the config"
                ))
            })?;
        let sqlfiles_dir = config.resolve_sqlfiles_dir()?;
        let folder = server.sqlfiles_folder_name();
        let ext = engines::capabilities_for_name(&server.engine).file_extension;
        match &content {
            // 内容が指定された時だけ上書きする。
            Some(text) => {
                let name = query_files::normalize_file_name(file_name, ext)?;
                query_files::write_query_file(
                    &sqlfiles_dir,
                    &folder,
                    &name,
                    text,
                    ext,
                )?;
            }
            // 指定が無ければ「無ければ作る」だけ (既存の内容は残す)。
            None => {
                query_files::ensure_query_file(
                    &sqlfiles_dir,
                    &folder,
                    file_name,
                    ext,
                )?;
            }
        }
        // フォルダを新規作成した時に接続の説明メタファイルを置く
        // (アプリ内の作成・保存と同じ扱い。ベストエフォート)。
        let dir = query_files::connection_dir(&sqlfiles_dir, &folder)?;
        let _ = folder_meta::write_folder_meta(&dir, server);
        Ok::<Option<Arc<AppConfig>>, AppError>(Some(config))
    })
}

/// GUI を起動しない CLI オプションを処理し、プロセスの終了コードを返す。
///
/// 表示の組み立ては [`crate::cli`] の純粋な関数に任せ、ここは設定の読み込みと
/// 出力だけを行う。`--list-servers` は設定を読むので失敗しうる。その場合は
/// 標準エラーへ書いて 1 で終わる (呼び出したスクリプトが失敗を判別できるように)。
fn run_info_command(command: cli::InfoCommand) -> i32 {
    // Windows の release ビルドはコンソールを持たないため、書く前に繋ぎ直す
    // (詳細は cli::attach_parent_console)。他の OS では何もしない
    cli::attach_parent_console();

    match command {
        cli::InfoCommand::Help => {
            print!("{}", cli::help_text());
            0
        }
        cli::InfoCommand::Version => {
            println!("{}", cli::version_text());
            0
        }
        cli::InfoCommand::License => {
            print!("{}", third_party_notices::NOTICES);
            0
        }
        cli::InfoCommand::ListServers => {
            let result = tauri::async_runtime::block_on(async {
                let config = AppConfig::load_merged().await?;
                let servers = config.resolve_servers()?;
                let sqlfiles_dir = config.resolve_sqlfiles_dir()?;
                // エンドポイント上書きの環境変数はここで解決する
                // (cli.rs の表の組み立てはプロセスの環境に依存しない純粋な関数に保つ)
                let aws_endpoint = cli::aws_endpoint_override_from_env();
                Ok::<String, AppError>(cli::format_server_list(
                    &servers,
                    &sqlfiles_dir,
                    aws_endpoint.as_deref(),
                ))
            });
            match result {
                Ok(text) => {
                    print!("{text}");
                    0
                }
                Err(e) => {
                    eprintln!("[cli] failed to read the config: {e}");
                    1
                }
            }
        }
    }
}

#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn run() {
    use tauri::Emitter;

    // CLI の `write` は、Tauri (single-instance プラグイン) を起動する前に
    // このプロセスで書き出す (apply_cli_write_route のドキュメント参照)。
    // 起動時引数だけを見る: 実行中インスタンスが転送されてくる argv を
    // 処理する経路 (single-instance のコールバック) では、その argv は
    // 別プロセスのものなので書き出しは既に済んでいる。
    //
    // ここで解決したマージ済み設定は AppState のキャッシュに引き継ぐ
    // (config_override_command を 1 起動で 2 度実行しない)。
    let preflight_config = {
        let argv: Vec<String> = std::env::args().skip(1).collect();
        // --help / --version / --license / --list-servers は表示だけして終わる
        // (GUI もウインドウも起動しない)。write の書き出しより前に見る。
        if let Some(command) = cli::info_command_from_args(&argv) {
            std::process::exit(run_info_command(command));
        }
        match router::route_from_cli_args(&argv) {
            Some(route) => match apply_cli_write_route(&route) {
                Ok(config) => config,
                Err(e) => {
                    // 書けていないものを開きに行かない。CLI から呼んだ側が
                    // 終了ステータスで失敗を判別できるようにする。
                    eprintln!("[cli] failed to write the query file: {e}");
                    std::process::exit(1);
                }
            },
            None => None,
        }
    };

    tauri::Builder::default()
        // single-instance は最初に登録する (プラグインは登録順に走る)。
        // deep-link feature 有効: 2 個目の起動の argv に含まれる queryfolio:// URL は
        // 実行中インスタンスの deep-link プラグインへ転送され on_open_url が発火する。
        // ここでは追加で (1) ウインドウを前面化し (2) CLI サブコマンド
        // (queryfolio open <path>) を処理する (URL 引数は上記で処理済みなので無視)。
        .plugin(tauri_plugin_single_instance::init(|app, argv, cwd| {
            use tauri::Manager;
            if let Some(window) = app.get_webview_window("main") {
                let _ = window.set_focus();
            }
            // cwd は 2 個目の起動元ディレクトリ。CLI の相対パスをこの基準で解決する。
            if let Some(route) = router::route_from_cli_args(&argv) {
                dispatch_route(app, route, Some(PathBuf::from(cwd)));
            }
        }))
        // queryfolio:// スキームの deep link。macOS はネイティブに URL を受け取り、
        // Linux/Windows は上の single-instance (deep-link feature) 経由で受け取る。
        .plugin(tauri_plugin_deep_link::init())
        .plugin(tauri_plugin_opener::init())
        .plugin(tauri_plugin_clipboard_manager::init())
        // 結果テーブルの Export でネイティブ保存ダイアログを開くのに使う
        .plugin(tauri_plugin_dialog::init())
        // 終了時のウインドウサイズ・位置を保存し、起動時に復元する
        .plugin(tauri_plugin_window_state::Builder::default().build())
        // setup で set_menu すると、それより前に設置される tauri のデフォルト
        // メニューで macOS のアプリメニューが確定してしまうため、ここで渡す
        .menu(build_menu)
        .on_menu_event(|app, event| match event.id().as_ref() {
            "reload_config_file" => {
                // 再読込はフロントの状態 (選択・未保存編集) と連動するため、
                // イベントで通知してフロント側の reloadConnections に任せる
                if let Err(e) = app.emit("menu-reload-config", ()) {
                    eprintln!("[menu] failed to emit reload event: {e}");
                }
            }
            "reveal_config_folder" => {
                if let Err(e) = reveal_config_folder() {
                    eprintln!("[menu] {e}");
                }
            }
            "edit_config_file" => {
                if let Err(e) = app.emit("menu-edit-config", ()) {
                    eprintln!("[menu] failed to emit edit config event: {e}");
                }
            }
            "close_editor_tab" => {
                if let Err(e) = app.emit("menu-close-editor-tab", ()) {
                    eprintln!("[menu] failed to emit close editor tab event: {e}");
                }
            }
            "show_licenses" => {
                if let Err(e) = app.emit("menu-show-licenses", ()) {
                    eprintln!("[menu] failed to emit show licenses event: {e}");
                }
            }
            "view_override_config" => {
                if let Err(e) = app.emit("menu-view-override-config", ()) {
                    eprintln!("[menu] failed to emit view override config event: {e}");
                }
            }
            _ => {}
        })
        .manage(AppState::with_config(preflight_config))
        .setup(|app| {
            use tauri::Manager;
            use tauri_plugin_deep_link::DeepLinkExt;
            // dev / Linux 実行向けにスキームを実行時登録する (macOS は bundle 時に
            // Info.plist へ登録される)。ベストエフォート: 失敗しても起動は続ける。
            if let Err(e) = app.deep_link().register_all() {
                eprintln!("[router] failed to register deep link schemes: {e}");
            }
            // 実行中に URL を開かれた時のハンドラ (macOS ネイティブ / Linux 転送)。
            let handle = app.handle().clone();
            app.deep_link().on_open_url(move |event| {
                for url in event.urls() {
                    match router::parse_uri(url.as_str()) {
                        // deep link の URL は絶対パス想定なので cwd は不要 (None)
                        Ok(route) => dispatch_route(&handle, route, None),
                        Err(e) => eprintln!("[router] ignoring URL {url}: {e}"),
                    }
                }
            });
            // 起動時に指定されたルートを控える (フロントが frontend_ready で取り出す)。
            // 優先度: deep link 起動 (macOS: get_current が URL を返す) → CLI サブコマンド。
            let mut launch: Option<router::Route> = None;
            if let Ok(Some(urls)) = app.deep_link().get_current() {
                for url in urls {
                    if let Ok(route) = router::parse_uri(url.as_str()) {
                        launch = Some(route);
                        break;
                    }
                }
            }
            if launch.is_none() {
                let argv: Vec<String> = std::env::args().skip(1).collect();
                launch = router::route_from_cli_args(&argv);
            }
            if launch.is_some() {
                *app.state::<AppState>().launch_route.lock().unwrap() = launch;
            }
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            get_connections,
            reset_connections,
            run_query,
            cancel_query,
            list_query_history,
            list_query_files,
            search_query_files,
            read_query_file,
            query_file_path,
            write_query_file,
            write_query_file_if_unchanged,
            create_query_file,
            delete_query_file,
            rename_query_file,
            move_query_file,
            list_schemas,
            set_active_schema,
            get_active_schema,
            disconnect,
            list_tables,
            list_columns,
            get_schema_map,
            get_primary_keys,
            run_statements,
            get_ai_info,
            ai_generate_sql,
            build_explain_sql,
            check_dangerous_statement,
            can_rerun_for_output,
            ai_explain_plan,
            ai_explain_sql,
            ai_fix_sql,
            ai_chat,
            cancel_ai_chat,
            get_config_info,
            third_party_notices,
            ensure_config_file,
            read_config_file,
            write_config_file,
            read_override_config_yaml,
            write_export_file,
            frontend_ready,
        ])
        .run(tauri::generate_context!())
        .expect("error while running tauri application");
}

#[cfg(test)]
mod export_encoding_tests {
    use super::*;

    #[test]
    fn utf8_is_the_default() {
        assert_eq!(
            encode_export_contents("あa", "").unwrap(),
            "あa".as_bytes()
        );
        assert_eq!(
            encode_export_contents("あa", "utf-8").unwrap(),
            "あa".as_bytes()
        );
    }

    #[test]
    fn encodes_cp932_and_euc_jp() {
        // 「あ」は CP932 で 0x82 0xA0、EUC-JP で 0xA4 0xA2
        assert_eq!(
            encode_export_contents("あa", "cp932").unwrap(),
            vec![0x82, 0xA0, 0x61]
        );
        assert_eq!(
            encode_export_contents("あa", "euc-jp").unwrap(),
            vec![0xA4, 0xA2, 0x61]
        );
    }

    #[test]
    fn rejects_unknown_encoding() {
        assert!(encode_export_contents("a", "utf-16").is_err());
    }

    #[test]
    fn rejects_unmappable_characters() {
        // 変換できない文字は数値文字参照に化けるため、黙って書かずエラーにする
        assert!(encode_export_contents("a🍣b", "cp932").is_err());
        assert!(encode_export_contents("🍣", "euc-jp").is_err());
    }
}
