import { invoke } from "@tauri-apps/api/core";

/// SSH トンネル情報 (機密を除く。バックエンドの config::SshTunnelInfo に対応)
export interface SshTunnelInfo {
  host: string;
  port: number;
  user: string;
  /// ~/.ssh/config の Host エイリアス (system ssh 委譲モード時)。libssh2 モードでは null
  ssh_config: string | null;
}

/// エンジンの能力宣言 (バックエンドの engines::EngineCapabilities に対応)。
/// UI の出し分けはエンジン名ではなくこのフラグで行う。
export interface EngineCapabilities {
  /// エディタのシンタックスハイライト言語
  editor_language: "sql" | "redis" | "es";
  /// クエリファイルの拡張子 (ドット無し)
  file_extension: string;
  /// スキーマ (database) の一覧・切替に対応するか
  supports_schemas: boolean;
  /// スキーマブラウザ (TABLES ペイン) に対応するか
  supports_tables: boolean;
  supports_explain: boolean;
  supports_format: boolean;
  /// 結果グリッドのセル編集 (UPDATE 生成) に対応するか
  supports_editable_cells: boolean;
  /// AI 機能 (SQL 生成 / 解説) に対応するか
  supports_ai: boolean;
}

export interface ConnectionInfo {
  name: string;
  description: string | null;
  engine: string;
  has_ssh_tunnel: boolean;
  /// 接続先ホスト (未設定なら null)
  host: string | null;
  /// 接続先ポート (未設定なら null)
  port: number | null;
  /// 接続ユーザー (未設定なら null)
  user: string | null;
  schema: string | null;
  /// SSH トンネル情報 (機密を除く)。トンネル未使用なら null
  ssh_tunnel: SshTunnelInfo | null;
  readonly: boolean;
  /// 危険な文 (WHERE 無し UPDATE/DELETE、DROP/TRUNCATE) の実行を許可する接続。
  /// true でも実行前に確認を求める
  allow_dangerous_statements: boolean;
  /// 接続一覧での表示グループ名 (グループ未所属なら null)
  group_name: string | null;
  /// パスワードを password_command で取得する接続か
  /// (コマンドの文字列も出力も渡されない)
  password_from_command: boolean;
  /// SQL 系エンジン (mysql / postgres) の実効 TLS モード。
  /// disable / prefer は平文へ降格しうる (prefer は sqlx の既定)。
  /// 他のエンジンでは null
  sql_ssl_mode: string | null;
  /// エンジンの能力宣言 (UI の出し分けに使う)
  capabilities: EngineCapabilities;
}

export interface QueryResult {
  columns: string[];
  rows: unknown[][];
  row_count: number;
  affected_rows: number | null;
  truncated: boolean;
  applied_limit: number | null;
  elapsed_ms: number;
  /// `\c` でアクティブスキーマを切り替えた場合の切替先 (それ以外は null)
  switched_schema: string | null;
}

/// クエリ実行履歴の 1 件分 (バックエンドの history::HistoryEntry に対応)
export interface QueryHistoryEntry {
  /// 実行時刻 (ISO 8601)
  time: string;
  sql: string;
  /// 実行時のアクティブスキーマ (database)
  schema: string | null;
  /// 取得行数または影響行数 (失敗時は null)
  row_count: number | null;
  elapsed_ms: number;
  success: boolean;
}

/// テーブル / ビューの情報 (バックエンドの schema_info::TableInfo に対応)
export interface TableInfo {
  /// テーブル名 (スキーマ修飾なし)
  name: string;
  /// 所属スキーマ名 (PostgreSQL のみ。MySQL / SQLite は null)
  schema: string | null;
  /// "table" または "view"
  kind: string;
  /// SQL に埋め込める修飾名。listColumns の table 引数やエディタへの
  /// 挿入にはこの値を使う
  qualified_name: string;
}

/// カラムの情報 (バックエンドの schema_info::ColumnInfo に対応)
export interface ColumnInfo {
  name: string;
  data_type: string;
  nullable: boolean;
}

/// AI 設定の情報 (バックエンドの ai::AiInfo に対応)。api_key は含まれない
export interface AiInfo {
  configured: boolean;
  /// 使用モデル名 (未設定時は空文字)
  model: string;
}

export interface ConfigInfo {
  config_path: string;
  config_exists: boolean;
  source: string;
  sqlfiles_dir: string;
}

export const getConnections = () =>
  invoke<ConnectionInfo[]>("get_connections");

export const resetConnections = () => invoke<void>("reset_connections");

/// writable は Writable スイッチの状態 (未指定・false は読み取り専用の安全側)。
/// config の readonly: true 接続では、writable に関わらず書き込みは拒否される。
/// applyDefaultLimit を false にすると、設定の default_limit を自動付与しない
/// (Copy / Export で全件を出すため)。省略時は従来どおり付与する。
export const runQuery = (
  connection: string,
  sql: string,
  maxRows?: number,
  writable?: boolean,
  applyDefaultLimit?: boolean,
) =>
  invoke<QueryResult>("run_query", {
    connection,
    sql,
    maxRows,
    writable,
    applyDefaultLimit,
  });

/// 接続で実行中のクエリにキャンセルを要求する。実行中でなければ false。
/// キャンセルされた run_query は CANCELLED_ERROR_MESSAGE のエラーで返る。
export const cancelQuery = (connection: string) =>
  invoke<boolean>("cancel_query", { connection });

/// バックエンドの AppError::Cancelled が返す文字列 (キャンセル判定用)
export const CANCELLED_ERROR_MESSAGE = "Query cancelled";

/// 危険な文 (WHERE 無し UPDATE/DELETE、DROP/TRUNCATE) なら理由を、そうでなければ
/// null を返す。allow_dangerous_statements が有効な接続で、実行前の確認要否を
/// 判断するために使う (無効な接続では runQuery 側が拒否する)。
export const checkDangerousStatement = (connection: string, sql: string) =>
  invoke<string | null>("check_dangerous_statement", { connection, sql });

/// Copy / Export で全件を取り直すために、その SQL をもう一度実行してよいかを返す。
/// 書き込みを伴う文を二度実行しないよう、読み取り専用と判定できたものだけ true。
export const canRerunForOutput = (connection: string, sql: string) =>
  invoke<boolean>("can_rerun_for_output", { connection, sql });

/// クエリ実行履歴を新しい順に返す。search は SQL の部分一致 (大小無視)。
export const listQueryHistory = (
  connection: string,
  search?: string,
  limit?: number,
) => invoke<QueryHistoryEntry[]>("list_query_history", { connection, search, limit });

/// FILES ペインの 1 行 (バックエンドの query_files::QueryFileEntry に対応)
export interface QueryFileEntry {
  /// ファイル名 (拡張子付き)
  file_name: string;
  /// 最終更新日時 (UNIX エポックからのミリ秒)。取れなければ null
  modified_ms: number | null;
  /// ファイルサイズ (バイト)
  size: number;
}

/// 接続のクエリファイル一覧 (更新日時の降順 = 最近編集したものが先頭)
export const listQueryFiles = (connection: string) =>
  invoke<QueryFileEntry[]>("list_query_files", { connection });

/// クエリファイル検索の 1 ヒット (バックエンドの query_files::FileSearchHit に対応)
export interface FileSearchHit {
  /// ヒットしたファイル名 (拡張子付き。.sql / .redis)
  file_name: string;
  /// ファイル名が query に一致したか
  name_match: boolean;
  /// 中身が一致した最初の行 (プレビュー用。名前のみ一致なら null)
  content_preview: string | null;
}

/// 接続のクエリファイルをファイル名・中身で検索する (大文字小文字を区別しない部分一致)。
export const searchQueryFiles = (connection: string, query: string) =>
  invoke<FileSearchHit[]>("search_query_files", { connection, query });

export const readQueryFile = (connection: string, fileName: string) =>
  invoke<string>("read_query_file", { connection, fileName });

/// クエリファイルの絶対パスを返す (FilesPane の「Copy full path」用)。
export const queryFilePath = (connection: string, fileName: string) =>
  invoke<string>("query_file_path", { connection, fileName });

export const writeQueryFile = (
  connection: string,
  fileName: string,
  content: string,
) => invoke<void>("write_query_file", { connection, fileName, content });

/// 楽観的排他つきの書き込み。expectedBase とディスクの現在内容が一致する時だけ書き込む。
/// 書けたら true、アプリ外で変更されていて書かなかったら false を返す。
export const writeQueryFileIfUnchanged = (
  connection: string,
  fileName: string,
  content: string,
  expectedBase: string,
) =>
  invoke<boolean>("write_query_file_if_unchanged", {
    connection,
    fileName,
    content,
    expectedBase,
  });

export const createQueryFile = (connection: string, fileName: string) =>
  invoke<string>("create_query_file", { connection, fileName });

export const deleteQueryFile = (connection: string, fileName: string) =>
  invoke<void>("delete_query_file", { connection, fileName });

export const renameQueryFile = (
  connection: string,
  oldName: string,
  newName: string,
) => invoke<string>("rename_query_file", { connection, oldName, newName });

/// クエリファイルを別の接続のフォルダへ移動する (FILES から CONNECTIONS への
/// ドラッグ & ドロップ)。移動後のファイル名を返す。
/// クエリファイルの拡張子が違うエンジン同士の移動はバックエンドが拒否する。
export const moveQueryFile = (
  fromConnection: string,
  toConnection: string,
  fileName: string,
) =>
  invoke<string>("move_query_file", { fromConnection, toConnection, fileName });

export const listSchemas = (connection: string) =>
  invoke<string[]>("list_schemas", { connection });

export const setActiveSchema = (connection: string, schema: string) =>
  invoke<void>("set_active_schema", { connection, schema });

export const getActiveSchema = (connection: string) =>
  invoke<string | null>("get_active_schema", { connection });

/// 指定接続のプール / SSH トンネルを破棄する (エディタタブを全て閉じた時に呼ぶ)。
/// 接続設定は残り、次に必要になった時 (ファイルを開く / スキーマブラウザ / クエリ)
/// に自動で張り直される。
export const disconnect = (connection: string) =>
  invoke<void>("disconnect", { connection });

/// テーブル / ビューの一覧を返す。refresh = true でキャッシュを破棄して再取得。
export const listTables = (connection: string, refresh?: boolean) =>
  invoke<TableInfo[]>("list_tables", { connection, refresh });

/// テーブルのカラム一覧を返す。table には TableInfo.qualified_name を渡す。
export const listColumns = (connection: string, table: string) =>
  invoke<ColumnInfo[]>("list_columns", { connection, table });

/// テーブル名 → カラム名リストのマップを返す (SQL 補完用)。
export const getSchemaMap = (connection: string) =>
  invoke<Record<string, string[]>>("get_schema_map", { connection });

/// テーブルの主キーを構成するカラム名を返す (結果グリッドのセル編集用)。
/// 主キーが無いテーブルでは空配列。
export const getPrimaryKeys = (connection: string, table: string) =>
  invoke<string[]>("get_primary_keys", { connection, table });

/// 結果グリッドのセル編集を UPDATE 群として 1 トランザクションで適用する。
/// writable の意味は runQuery と同じ (未指定・false は読み取り専用)。
/// 合計の影響行数を返す。
export const runStatements = (
  connection: string,
  statements: string[],
  writable?: boolean,
) => invoke<number>("run_statements", { connection, statements, writable });

/// AI 設定の情報を返す。`ai:` セクションが無い場合は configured: false。
/// セクションはあるが不正 (不明 provider 等) な場合は reject される。
export const getAiInfo = () => invoke<AiInfo>("get_ai_info");

/// 自然言語の指示から SQL を生成して返す (実行はしない)。
export const aiGenerateSql = (connection: string, instruction: string) =>
  invoke<string>("ai_generate_sql", { connection, instruction });

/// 失敗した SQL とエラーメッセージから修正案の SQL を返す (実行はしない)。
export const aiFixSql = (
  connection: string,
  sql: string,
  errorMessage: string,
) => invoke<string>("ai_fix_sql", { connection, sql, errorMessage });

/// エンジン別の EXPLAIN プレフィックスを付けた SQL を組み立てて返す
/// (実行はしない)。SELECT / WITH 以外の文は reject される。
export const buildExplainSql = (connection: string, sql: string) =>
  invoke<string>("build_explain_sql", { connection, sql });

/// EXPLAIN の実行計画を AI に解説させ、Markdown テキストを返す。
export const aiExplainPlan = (
  connection: string,
  sql: string,
  planText: string,
) => invoke<string>("ai_explain_plan", { connection, sql, planText });

/// カーソル位置の SQL 文を AI に平易に解説させ、Markdown テキストを返す
/// (実行はしない)。
export const aiExplainSql = (connection: string, sql: string) =>
  invoke<string>("ai_explain_sql", { connection, sql });

/// AI チャットの 1 ターン (バックエンドの ai::ChatTurn に対応)。
/// role は "user" / "assistant" のみ (system はバックエンドが組み立てる)。
export interface ChatTurn {
  role: "user" | "assistant";
  content: string;
}

/// AI エージェントが実行したツール呼び出しの記録
/// (バックエンドの ai::ChatToolCall に対応)。
export interface ChatToolCall {
  name: string;
  /// 実行した SQL (引数のパースに失敗した場合は生の引数)
  argument: string;
  ok: boolean;
  /// 結果の要約 (行数 / エラーメッセージの 1 行目)
  summary: string;
}

/// AI チャット 1 往復の応答 (バックエンドの ai::ChatReply に対応)。
/// 失敗した往復も reject ではなくこの形で返る (途中まで実行したクエリを
/// 隠さないため)。error が非 null なら失敗で、content は空。
export interface ChatReply {
  content: string;
  tool_calls: ChatToolCall[];
  error: string | null;
}

/// AI チャット (エージェント) の 1 往復を実行する。会話履歴は毎回そのまま
/// 送り、system prompt の組み立てとツール実行ループはバックエンドが行う。
/// エージェントが実行できるのは読み取り専用の SQL のみ。
/// requestId はフロントが採番する往復の識別子 (中断はこの ID を指定する。
/// 同じ接続で複数の往復が走りうるため接続名だけでは区別できない)。
export const aiChat = (
  connection: string,
  history: ChatTurn[],
  requestId: string,
) => invoke<ChatReply>("ai_chat", { connection, history, requestId });

/// AI チャットのエージェントの往復を中断する (実行中のクエリを止め、
/// 次のモデル呼び出し・ツール往復も行わせない)。まだ ai_chat が走り出す
/// 前の ID も指定でき、その場合は開始時に中断される。
/// 実行中のクエリを実際に止めたら true。
export const cancelAiChat = (connection: string, requestIds: string[]) =>
  invoke<boolean>("cancel_ai_chat", { connection, requestIds });

/// `queryfolio://open/<path>` deep link / CLI で指定された「開く対象」
/// (バックエンドの router::OpenTarget に対応)。
export interface OpenTarget {
  /// 対象ファイルが属する接続名
  connection: string;
  /// 開くファイル名 (拡張子付き。.sql / .redis)
  fileName: string;
}

/// frontend_ready の戻り値 (バックエンドの LaunchResult に対応)。
export interface LaunchResult {
  /// 開く対象 (起動時指定 + 起動中に届いた分)
  targets: OpenTarget[];
  /// 起動時指定の解決に失敗した理由 (トーストで表示する)
  errors: string[];
}

/// フロントの listener 登録完了を知らせ、それまでに溜まった開く対象
/// (起動時の deep link / CLI 指定 + 起動中に届いた分) と、起動時指定の解決に
/// 失敗した理由をまとめて受け取る。呼び出し後は以降の指定が open-query-file
/// イベントで直接届く。onMount で listener を登録した直後に 1 度だけ呼ぶ。
export const frontendReady = () => invoke<LaunchResult>("frontend_ready");

export const getConfigInfo = () => invoke<ConfigInfo>("get_config_info");

/// 配布物に同梱している依存ライブラリのライセンス一覧 (THIRD-PARTY-NOTICES.txt の本文)。
export const getThirdPartyNotices = () => invoke<string>("third_party_notices");

/// config.yml が無ければテンプレートを作成する。作成した場合はそのパスを返す。
export const ensureConfigFile = () =>
  invoke<string | null>("ensure_config_file");

/// 設定エディタ用に config.yml の中身を読む (無ければテンプレートを作成してから読む)。
export const readConfigFile = () => invoke<string>("read_config_file");

/// 設定エディタからの保存。書き込んだファイルのパスを返す。
export const writeConfigFile = (content: string) =>
  invoke<string>("write_config_file", { content });

/// config_override_command を実行して取得した生の YAML を返す
/// (コピー用ビュー用。表示先では編集できるが保存はしない)。
export const readOverrideConfigYaml = () =>
  invoke<string>("read_override_config_yaml");

/// エクスポート時の文字コード。既定は UTF-8。
/// Excel など UTF-8 を前提としないツール向けに CP932 / EUC-JP を選べる。
export type ExportEncoding = "utf-8" | "cp932" | "euc-jp";

/// 結果テーブルの Export で、ネイティブ保存ダイアログで選んだパスへ
/// テキスト (CSV/TSV/JSON) を書き出す。パスはユーザーが明示的に選択したもの。
export const writeExportFile = (
  path: string,
  contents: string,
  encoding: ExportEncoding = "utf-8",
) => invoke<void>("write_export_file", { path, contents, encoding });
