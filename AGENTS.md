# queryfolio

多目的な SQL GUI クライアントを目指すデスクトップアプリ。

## 技術スタック

- **Tauri 2** (Rust バックエンド + WKWebView)
- **SvelteKit + Svelte 5 (runes)** / TypeScript / Vite 6
- **Tailwind CSS 4** (@tailwindcss/vite プラグイン方式)
- **CodeMirror 6** + @codemirror/lang-sql (SQL エディタ)
- **Bootstrap Icons** (bootstrap-icons パッケージ、app.css で CSS フォントを import)
- **sqlx 0.8** (MySQL / PostgreSQL / SQLite)
- **ssh2** (SSH ローカルポートフォワードトンネル)
- パッケージマネージャ: pnpm

Tauri 2 / Svelte 5 / Tailwind 4 は比較的新しいため、API に迷ったら context7 MCP でドキュメントを参照すること。

## コマンド

```shell
pnpm tauri dev          # 開発起動 (Rust ビルド + vite dev + ネイティブウインドウ)
pnpm check              # svelte-check (型チェック)
cd src-tauri && cargo test   # Rust ユニットテスト
cd src-tauri && cargo check  # Rust 型チェック
pnpm tauri build        # リリースビルド
pnpm release            # version 採番 → main へ push (これがリリースを始める) → watch (patch|minor|major)
fab release             # 同上 (fab release:minor 等)
fab -l                  # fab タスク一覧 (dev / check / unittest / build_local / release / releases)
```

リリースは main の `src-tauri/tauri.conf.json` の version を変えると始まる (`.github/workflows/release.yml` の `on: push`。`plan` ジョブが `scripts/release-decide.sh` で「その version が未公開で、公開中の最新より新しいか」を releases API に訊き、そうである時だけビルドへ進む — 判定が diff でなく状態なので、squash / rebase / 直 push で結果が変わらない)。`draft` ジョブが **draft** Release を 1 つ用意し、macOS universal dmg (Developer ID 署名 + 公証 + staple) と Windows NSIS インストーラ (署名なし) を matrix で並列ビルドしてその draft にアップロード → 全プラットフォーム成功後に `publish` ジョブが公開する。version の採番と push は `scripts/release.sh` (`pnpm release` / `fab release`) が行い、その後は push で始まった run を watch する (起動はしない)。設計の詳細はグローバルスキル `tauri-github-actions-release`、公開までの runbook は `publish-macos-release` スキル (`.claude/skills/publish-macos-release/`。署名 Secrets の初回設定手順を含む)。設計上の要点:

- **draft → publish 分離が必須**: matrix の 2 ジョブが同じ `v<version>` Release に上げるため、公開状態で上げると先に終わった方だけの不完全な Release が即公開される。
- **draft は `draft` ジョブがビルドの前に 1 つだけ作り、tauri-action には `releaseId` で渡す** (`tagName` を渡さない)。tagName 方式だと各ジョブがビルド後に「一覧から探して無ければ作る」ので、両方のビルドがほぼ同時に終わると同じ tag の draft が 2 つでき、publish が片方のプラットフォームを欠いたまま公開する。失敗した run の draft は再利用し、`target_commitish` をその run の commit に直す (最初に draft を作った commit のまま公開すると tag が中身と食い違う)。同じ tag の draft が 2 つ以上あれば、どれに上げるか決められないので止まる (不要な draft を消して再実行)。
- **公開中の最新より古い version は出さない** (`release-decide.sh`)。version を上げた commit の revert や、pending の run が push 順と逆に消化された時に、`--latest` 相当 (`make_latest`) で最新と Homebrew tap を巻き戻さないため。
- **`publish` は公開直前に同じ判定をやり直す**。「Re-run failed jobs」は成功済みの `plan` を再実行せず当時の判定を使い回すので、その間に新しい version が公開されていると最新を巻き戻してしまう。`scripts/release.sh` も run の成功だけで Done とせず、Release が draft でないことを確かめる (plan が release=false を返した run も成功で終わるため)。
- **version は毎回インクリメント必須**: 公開済みの version は `plan` が弾くので、上げ忘れるとリリースされない。だから `scripts/release.sh` が採番を自動化している (bump し忘れ事故を構造的に消す)。
- **`pnpm publish` は使えない** (pnpm 組み込みコマンドで上書き不可)。コマンド名は `release`。
- **`uses:` は全て commit SHA 固定** (行末コメントが元のタグ)。Apple の証明書・認証情報を扱うジョブなので、tauri-action だけ固定しても先行ステップの action が改変されれば同じこと。checkout は `persist-credentials: false` で write 権限の token を `.git/config` に残さない。`APPLE_*` は macOS ジョブにのみ渡す (Windows には空文字)。
- **`tauriScript: pnpm exec tauri`** を必ず指定する。省くと tauri-action は pnpm プロジェクトに対して `pnpm tauri build` を実行し、package.json の `tauri` スクリプト (`APPLE_SIGNING_IDENTITY='...' tauri`) が走る。シェルのインライン代入は継承 env より強いので、**workflow が渡した `secrets.APPLE_SIGNING_IDENTITY` が黙って無視される** (加えて Windows のシェルでは構文エラーになる)。`pnpm exec tauri` はスクリプトを経由しない。
- **`cancel-in-progress: false` + `queue: max`**: 1 push = 1 version なので、キャンセルされた run の version は (bump コミットは main に載ったまま) 公開されないまま残る (`workflow_dispatch` で拾い直せるが、気付かなければ同じこと)。走行中の run を守る (`cancel-in-progress: false`) だけでは足りず、既定の `queue: single` は pending を 1 件しか保持せず新しい push で既存 pending を捨てるため、`queue: max` (最大 100 件) も要る。CI 分数より取りこぼし防止を優先する。
- **Homebrew 配布**: `brew install --cask cyberneura/tap/queryfolio` (tap は cyberneura/homebrew-tap、`Casks/queryfolio.rb`)。**Cask の更新は tap 側が毎時行う** (tap の `scripts/update.py` が各プロジェクトの latest release を見て version / url / sha256 を書き換える)。このリポジトリから tap へ push しない — そうすると tap に書ける PAT を全プロジェクトへ配ることになるため。以前あった `homebrew` ジョブと `HOMEBREW_TAP_TOKEN` secret は不要 (CYBERNEURA-DEV-481)。
- **弱点**: `pnpm release` は main へ直接 push するため、ブランチ保護 (PR 必須) を掛けると破綻する (version を変える PR をマージする形なら、workflow 側はそのままで動く)。

## アーキテクチャ

### Rust (src-tauri/src/)

| ファイル | 役割 |
|---------|------|
| lib.rs | Tauri コマンド定義と AppState (接続設定キャッシュ + DbManager)、メニューバーの組み立て (build_menu / rebuild_menu) |
| config.rs | config.yml のロード・config_override_command による設定の再帰マージ (load_merged / merge_mapping)・テンプレート展開・expand_tilde・設定エディタの読み書き (read_config_file / write_config_file)。設定に書かれたコマンド (config_override_command / password_command) の実行は `run_setting_command` に一本化 (shlex・シェル非経由・PATH 補完・タイムアウト・kill_on_drop。エラーにはコマンドと stderr を載せ、stdout は載せない) |
| db.rs | sqlx プール管理 (DbManager。接続の確立は接続名ごとのロック `connect_locks` で直列化し、マップ全体のロック `inner` は確立中に握らない — 遅い password_command / SSH トンネルが確立済みの別接続のクエリを待たせないため。ロック順は connect_locks → inner。プールを捨てる disconnect / スキーマ切替も connect_locks を取って確立の完了を待ち、reset は `reset_generation` で確立中のプールの登録を防ぐ)、password_command の期限切れ対応 (`with_pool` / `retry_on_expired_password`: 認証エラーなら `Pool::set_connect_options` でパスワードだけ差し替えて 1 度だけ再実行)、クエリ実行・キャンセル (CancelRegistry)、型別 JSON 変換、readonly ガード / 危険な文ガード (dangerous_reason)。sqlx を使わないエンジン (Redis / Elasticsearch / DuckDB / DynamoDB) は `DbPool` の variant として持ち、run_query_cancellable の冒頭で engines/ の各モジュールへ委譲する |
| engines/mod.rs | プラガブルエンジン層の中核。`EngineCapabilities` (エディタ言語・クエリファイル拡張子・schemas/tables/explain/format/セル編集/AI の対応可否) をエンジンごとに宣言し、`ConnectionInfo.capabilities` としてフロントへ渡す。フロントはエンジン名でなく capability で UI を出し分けるため、エンジン追加時は「engines/ にモジュールを足す + capability を宣言 + db.rs の enum に variant を足す」で済む |
| engines/redis.rs | Redis エンジン (engine: redis / valkey)。1 行 = 1 コマンド (redis-cli 互換のクォート・エスケープのトークナイザ)、複数行は同一コネクションで順次実行。readonly ガードは読み取りコマンドのホワイトリスト方式、危険コマンド (FLUSHALL / FLUSHDB / SHUTDOWN / DEBUG) は SQL の危険文ガードと同じ扱い。RESP 値は Map=field/value、ペア返しコマンド (HGETALL / CONFIG GET) の偶数長フラット配列 (RESP2) も field/value、Array=1要素1行、スカラー=1セル、複数コマンド=command/result の表形式へ変換。引数はバイナリ安全のためバイト列 (\xHH エスケープは生バイト)。キャンセルはクライアント側で future を打ち切る (`CancelTarget::ClientSide`)。接続は実行ごとに multiplexed connection を張る (キャンセルで放棄してもプールが壊れない)。schema は database 番号、SSH トンネル可、pub/sub 系とブロッキング系 (BLPOP / XREAD BLOCK 等。キャンセルしても接続が塞がるため) は拒否 |
| engines/duckdb.rs | DuckDB エンジン (engine: duckdb)。SQL エンジンだが sqlx に DuckDB ドライバが無いため `duckdb` crate (bundled、C++ ソースを静的リンク) で結線する。接続は sqlite と同型 (`schema`、無ければ `host` を DB ファイルパスとして開く。ファイルが無ければエラーで新規作成しない。SSH トンネル不可)。duckdb::Connection は Send だが Sync でないため `Arc<Mutex<Connection>>` で 1 本を維持し、同期 API の実行は spawn_blocking で包む。SQL 系の共通ガード (メタコマンド変換 → readonly → dangerous → auto LIMIT) は db.rs の既存ロジック (is_readonly_allowed / dangerous_reason / should_auto_limit / scan_sql。方言は Postgres 相当 = ドル引用対応) を再利用する。キャンセルは duckdb の `InterruptHandle` (`CancelTarget::DuckDb`) によるサーバー側中断 (spawn_blocking は future の drop では止まらないため必須。実行開始前に届いたキャンセルは blocking 側のフラグ確認で拾う)。型変換は 2^53 超の整数 / HUGEINT / DECIMAL を文字列化、TIMESTAMP / DATE / TIME は文字列、LIST / STRUCT / MAP は要素数上限 (1,000) 付きで JSON 化して truncated を立てる。EXPLAIN は prefix `EXPLAIN` (EXPLAIN ANALYZE は対象文を実行するため使わない)。スキーマブラウザ / SQL 補完 / PK は information_schema と duckdb_constraints() を照会。セル編集 (run_statements) は sqlx 経路のため非対応 (capabilities.supports_editable_cells = false) |
| engines/elasticsearch.rs | Elasticsearch エンジン (engine: elasticsearch / es / opensearch)。sqlx でなく reqwest で REST API を直叩き (`EsClient` = base_url + Basic 認証。`tls: true` で https)。エディタは Kibana Console 風のリクエストブロック (`GET /index/_search` のメソッド行 + JSON body。`#` 行はコメント、NDJSON body は `_bulk` 用にそのまま送る)。複数ブロックの選択実行は request / status / result の表 (HTTP エラーステータスも行として載せて続行、転送エラーで中断)。結果整形は hits.hits → `_index`/`_id`/`_score` + `_source` キー union の表、オブジェクト配列 (`_cat/...?format=json`) → キー union の表、その他 → 1 セル pretty JSON (10 万字上限)。全整形パスで max_rows / セル内文字数・要素数上限 + truncated、応答 body は 20MB で読み切り。readonly ガードは GET/HEAD 常時許可 + POST は検索系 API のホワイトリスト (最初の `_` セグメントを API とみなす。`/index/_doc/_search` のような ID すり抜けを防ぐ)。危険ガードは DELETE の単一セグメント (インデックス削除) と `_delete_by_query`。パスの `.`/`..` セグメント (percent エンコード形含む) は検証と実 I/O の URL 正規化ズレを防ぐため拒否。接続時の GET / と全リクエストにタイムアウト。キャンセルは `CancelTarget::ClientSide`。TABLES ペインは `_cat/indices` のインデックス一覧 (`.` 始まりのシステムインデックス除外) + `_mapping` の properties を `a.b` 形式へ平坦化したフィールド |
| engines/dynamodb.rs | DynamoDB エンジン (engine: dynamodb)。PartiQL (SQL 互換サブセット) を AWS SDK (`aws-sdk-dynamodb`) の ExecuteStatement API で実行する。エディタは通常の SQL (editor_language "sql" / 拡張子 .sql) で、readonly / dangerous ガードは db.rs の SQL 系ロジックを再利用 (PartiQL は SELECT / INSERT / UPDATE / DELETE のみ。ダブルクォート識別子は scan_sql が文字列として空白化するが WHERE 等のキーワードはクォートされないため判定に影響しない)。接続は `schema` = AWS リージョン (必須)、`host`/`port` = エンドポイント上書き (dynamodb-local 用、`tls` で http/https)、認証は user/password (静的アクセスキー) → `aws_profile` (queryfolio 独自拡張) → 既定の credentials chain の順。接続確認は ListTables (limit 1) を 15 秒タイムアウト付きで実行。PartiQL に LIMIT 句が無いため auto LIMIT は付与せず、ExecuteStatement の limit パラメータ + NextToken ページネーションで max_rows + 1 件で打ち切り truncated を報告 (ページネーション全体にも 120 秒の締切。フィルタの強い SELECT はスキャンの空ページが続き得るため)。INSERT / UPDATE / DELETE は影響行数が API から取れないため affected_rows = None + 空結果 (`RETURNING ALL OLD *` の Items はそのまま表形式)。結果整形は Items のキー union (ソート順。SDK の Item は HashMap でキー順不定のため) を columns にし、AttributeValue → JSON は N (任意精度) を安全整数のみ数値・他は文字列、B/BS は bytes_to_json、L/M/SS/NS/BS はセル全体の共有予算 (1,000 要素)・深さ上限 32・文字列 10,000 字で打ち切り + truncated。キャンセルは `CancelTarget::ClientSide` (結果側を先に見る biased select)。HTTPS クライアントは既存依存と同じ ring ベース rustls を明示 (SDK 既定の aws-lc はネイティブビルドに cmake / NASM を要し CI リスクのため)。TABLES ペインは ListTables (上限 5,000) + DescribeTable のキースキーマ (partition / sort key) と属性定義 (data_type は S/N/B 表記)。メタコマンド / \c / EXPLAIN / セル編集 / AI / SSH トンネルは非対応 |
| tunnel.rs | SSH ローカルポートフォワード (known_hosts 検証付き)。ssh-agent 認証時は使う agent socket を `ssh_tunnel.identity_agent` → `~/.ssh/config` の IdentityAgent → SSH_AUTH_SOCK の順で解決し libssh2 の `set_identity_path` で指定する (GUI 起動でシェルの SSH_AUTH_SOCK を継承しなくても 1Password 等の agent に届く)。ssh_config パーサは Include の条件付き展開・glob・Host マッチ・エスケープ/コメント除去に対応 (best-effort)。`ssh_tunnel.ssh_config` (Host エイリアス) 指定時は libssh2 経路を使わず system の `ssh` に委譲 (`start_system_ssh`): 空きローカルポートを確保して `ssh -N -L 127.0.0.1:<port>:<db_host>:<db_port> <alias>` を spawn (`ExitOnForwardFailure=yes` `BatchMode=yes` `ConnectTimeout`)、ローカルポートが接続を受けるまでポーリングして認証成功を確認、Drop で kill。ProxyJump / 多段トンネル / HostName / User 解決は OpenSSH と ~/.ssh/config に委譲する。このモードでは host / user / private_key / identity_agent は無視 (認証・ホスト鍵検証も OpenSSH 任せ)。PATH は GUI 起動対策で /opt/homebrew/bin 等を補完 (config.rs の supplement_path 共用) |
| query_files.rs | クエリファイル CRUD (パストラバーサル対策)。`ensure_query_file` は「無ければ空で作る・あればそのまま」(CLI の `write` で内容を省略した時に使う。既存が通常ファイルでない = ディレクトリ・壊れたリンク等ならエラー)。書き込み (`write_query_file` / `write_query_file_if_unchanged`) は同一ディレクトリの一時ファイル + rename (`write_file_atomic`) で行う — CLI は別プロセスなので、truncate してから書くと GUI 側の読み取りや別の書き手と重なった時に壊れた内容が見える。拡張子はエンジン別 (`EngineCapabilities.file_extension`: `.sql` / `.redis` / `.es`)。接続フォルダ間の移動 (`move_query_file`) もここ。FILES ペインの一覧は `list_query_file_entries` (更新日時の降順 + 更新日時・サイズ付き。CYBERNEURA-DEV-774) で、検索は名前の降順の `list_query_file_names` のまま (`list_query_files` はテスト専用)。保存・外部変更の取り込み・CLI からのオープンの後と、ファイルウォッチャの tick で 10 秒ごとに、フロント (`refreshFileEntries`) が一覧を取り直す (開いていないファイルの外部変更や、同じ内容での書き直しで mtime だけ変わった場合はタブの内容比較で検知できないため。変化が無ければ書き換えない) |
| router.rs | `queryfolio://` deep link と CLI サブコマンドを共通の `Route` に落とす。`parse_uri` (URI パース。書き込みを伴う `write` は受け付けない) / `route_from_cli_args` (`open <path>` / `write <connection> <file-name> [content]` サブコマンド) / `resolve_open_target` (生パス → 接続 + ファイル名。保存ディレクトリ配下の接続フォルダにあるクエリファイル拡張子 (`.sql` / `.redis` / `.es`) だけを許可し、`..` トラバーサル・領域外を字句正規化で拒否)。Tauri 非依存の純 std + 単体テストで境界を固める。lib.rs が `Route` を解決してフロントへ `open-query-file` イベント / `frontend_ready` で届ける |
| folder_meta.rs | クエリファイル保存フォルダに接続を説明するメタファイル (`_queryfolio.md`) を生成する (エージェント/人間がフォルダを見て「どの DB 用のクエリか」を理解できるように)。非機密のみ (パスワード・SSH 鍵は含めない)。`create_query_file` / `write_query_file` / `list_query_files` の後に lib.rs (refresh_folder_meta) が書き出す。フォルダ未作成なら何もしない・内容が同じなら書かない (mtime churn 回避)。`.sql` でないため一覧・検索には出ない |
| meta_commands.rs | psql 風メタコマンド (\l \dt \dv \dn \du \d) をエンジン別カタログ SQL に変換 (MetaCommand::Sql)。識別子バリデーションでインジェクション拒否。`\c <database>` と `USE <database>` (MySQL / PostgreSQL のみ) だけは SQL にならずアクティブスキーマ切替 (MetaCommand::Connect) として lib.rs が処理する |
| history.rs | クエリ実行履歴。接続ごとに JSONL (~/.config/queryfolio/history/<connection>.jsonl) へ追記、上限 10,000 行でローテーション。SQL に機密が含まれ得るためディレクトリ 700 / ファイル 600 |
| schema_info.rs | テーブル・カラムのカタログ照会と SchemaCache (接続+スキーマ単位のキャッシュ。スキーマブラウザと SQL 補完用 get_schema_map で共有) |
| ai.rs | AI 基盤 (AiConfig の解決・OpenAI Chat Completions 呼び出し chat_complete・SQL 生成 / EXPLAIN 解説 / 選択 SQL 解説プロンプト整形・フェンス剥がし)。API キーはフロントに渡さない (get_ai_info は configured / model のみ)。チャット (エージェント) 用に function calling 対応の request_chat_completion / chat_step とツール定義 (chat_tools_spec = 読み取り専用の run_sql のみ)・履歴の整形 (chat_history_messages は user / assistant 以外の role を落とし、フロント経由で system を差し込ませない)・ツール引数のパース・結果の切り詰めを持つ |
| error.rs | AppError (フロントには文字列でシリアライズ) |
| third_party_notices.rs | THIRD-PARTY-NOTICES.txt の埋め込み (`NOTICES`) と、notices が lock と食い違っていないかのテスト |

### フロントエンド (src/)

- `lib/api.ts` — invoke の型付きラッパー (バックエンドとの境界)。`ConnectionInfo.capabilities` (EngineCapabilities) でエンジンの能力宣言を受け取る
- `lib/editor/redisLanguage.ts` — Redis コマンドの CodeMirror StreamLanguage (コマンド辞書 + サブコマンド + 文字列 + 数値 + `#` コメント)。SqlEditor が `capabilities.editor_language` で lang-sql と切り替える。行単位実行 (選択があれば選択範囲、カーソル行が空なら直前の非空行へフォールバック) も editor_language で分岐
- `lib/editor/esLanguage.ts` — Elasticsearch (Kibana Console 風) の CodeMirror StreamLanguage (行頭の HTTP メソッド → keyword、同じ行の残り = パス → string、`#` 行 → comment、JSON body は文字列/プロパティ名/数値/true・false・null/括弧を色分け)。実行対象はリクエストブロック単位 (選択があれば選択範囲、無ければカーソル行から上方向の最初のメソッド行 〜 次のメソッド行の手前。上にメソッド行が無ければ実行対象なし)。バックエンド (elasticsearch.rs の parse_input) とメソッド行の判定規則を揃えている
- `lib/stores/app.svelte.ts` — Svelte 5 runes ストア (getter + メソッドを default export)
- `lib/editor/vscodeEditing.ts` — CodeMirror を VSCode 互換のマルチカーソル / 複数選択で使えるようにする共通拡張 (SqlEditor と ConfigEditorModal の両方に入れる)。**要点は `EditorState.allowMultipleSelections`**: この facet が無いと、複数レンジを持つ選択がトランザクションの時点で主レンジ 1 つに畳まれるため、コマンド自体は成功するのにカーソルが増えない。Cmd+D (`selectNextOccurrence`) / Cmd+Shift+L (`selectSelectionMatches`) は `searchKeymap` に、Cmd+Alt+ArrowUp/Down (`addCursorAbove` / `addCursorBelow`) は `defaultKeymap` に元から入っているので、キーバインドを自前で足しているのは Cmd+L (行を選択) だけ。マウス操作は CodeMirror の既定が VSCode と違うので上書きしている — カーソルの追加は 既定の Cmd/Ctrl+click ではなく **Alt+click** (`clickAddsSelectionRange`)、矩形選択は既定の Alt+drag ではなく **Shift+Alt+drag** (`rectangularSelection` の `eventFilter`。Alt+click と修飾が衝突するため)。Alt+drag が「範囲の追加」になるよう `dragMovesSelection` も外している。**Cmd+Enter は VSCode と違って「文の実行」のまま** (アプリの中核操作なので譲らない。SqlEditor は `runKeymap` を `searchKeymap` / `defaultKeymap` より先に置いて確保している) (CYBERNEURA-DEV-647)
- `lib/reloadGuard.ts` + `routes/+layout.svelte` — WebView 既定のリロード (Cmd+R / Ctrl+R / F5、Shift 併用のキャッシュ無視リロードを含む) を無効化する。Queryfolio は SPA で編集中のクエリ・実行結果・接続状態をメモリ上に持つため、リロードされると復元手段なく初期状態へ戻る。**capture フェーズ**で window に付けるのが要点で、バブルフェーズの `+page.svelte` 側 (`handleGlobalKeydown`) に混ぜると、モーダルや CodeMirror が stopPropagation した時に素通りする
- `lib/components/` — Toolbar (グローバルツールバー。Writable スイッチ・検索ボタンを含む) / ConnectionsPane / FilesPane / HistoryPane / TablesPane (スキーマブラウザ) / SqlEditor / EditorToolbar / ResultsPane / CellInspector / ConfigInfoModal (読み取り専用の設定表示) / ConfigEditorModal (config.yml のアプリ内エディタ) / AiAnalysisModal (EXPLAIN / 選択 SQL の AI 解説表示) / ChatPane (右側の AI チャットペイン) / SearchModal (接続・クエリファイル横断検索) / PaneDivider (ドラッグ可能なペイン区切り線)
- AI チャットペイン (ChatPane) — ツールバーの `data-annotate="toggle-chat-pane"` で開閉する右側のペイン (幅と開閉状態は localStorage `queryfolio.layout.chatWidth` / `chatOpen` に永続化)。会話状態はフロント (`app.svelte.ts` の `chatMessages`) が持ち、1 往復ごとに履歴をまるごと `ai_chat` コマンドへ送る。バックエンド (lib.rs) が system prompt (方言 + スキーマ) を組み立て、`run_sql` ツールの実行ループを回す。**エージェントの SQL 実行は常に読み取り専用ガードを通す** (Writable スイッチや config の readonly に関わらず `ReadonlyGuard::Agent` 固定、`allow_dangerous_statements` も渡さない)。`Agent` は由来であると同時に強度でもあり、**文レベルの判定に加えて DB レベルの読み取り専用を強制する**: Postgres は `BEGIN READ ONLY`、MySQL は `START TRANSACTION READ ONLY`、DuckDB は `BEGIN TRANSACTION READ ONLY` で包んで必ず ROLLBACK し、SQLite は `PRAGMA query_only = 1` を張る (db.rs の `readonly_begin_sql` / `set_sqlite_query_only` / `run_query_readonly`)。文レベルの判定だけでは `SELECT nextval(...)` のような副作用のある関数呼び出しを見抜けないため、最終的な拒否は DB 自身にさせる。開始に失敗したらクエリごとエラーにする (fail-closed)。設計上の要点: (1) Postgres / MySQL は sqlx の `begin_with` を使う — 生の `BEGIN` を投げると、中断でクエリの future が drop された時に ROLLBACK が走らず、トランザクションを開いたままのコネクションがプールへ返る。sqlx の `Transaction` は drop 時にも ROLLBACK を積むのでこれを避けられる。(2) SQLite の `PRAGMA query_only` はセッション設定なので drop 時の後始末に頼れない — 「解除する」のではなく **run_query_on / run_statements が実行のたびに 0/1 を明示する**ことで、残留した設定を次の実行が必ず上書きする。(3) DuckDB は Mutex を握ったまま同期実行するので、ROLLBACK は同じ blocking クロージャ内で必ず完了する。残る限界 (実測ベース。PostgreSQL 17 / MySQL 8.4 で確認。ドキュメントにも明記): (a) **一時オブジェクトは両エンジンとも読み取り専用トランザクションの対象外**で書き込みが通る、(b) **MySQL の DDL は暗黙コミットでトランザクションを抜けるため拒否されない** (DDL を止めているのは文レベルのホワイトリストの方。Postgres は DDL も拒否する)、(c) 別システムへの書き込み (dblink / postgres_fdw)、(d) 読み取り内容そのもの。統合テスト (`test_agent_guard_enforces_readonly_transaction`) のプローブに一時オブジェクトや MySQL の DDL を使うと**素通りして緑になる**ので注意。さらに db.rs の `agent_rejection_reason` で通常の readonly ガードより狭いホワイトリストを課す: `CALL` (ストアドが中で DML を実行できる) と `PRAGMA` (DB 設定を変えうる) を落とし、`EXPLAIN ANALYZE` (対象文を実際に実行する。`EXPLAIN (ANALYZE) CREATE TABLE x AS SELECT ...` は DML 語も INTO も含まないため is_readonly_allowed を素通りする) と複文 (`;` 区切り) も拒否する。行数上限 50・ツール往復上限 6・**ツール呼び出しの累計上限 12** (1 応答が複数 tool_calls を並べられるため往復上限だけでは縛れない。累計・往復のどちらの上限に達しても、最後に tools を渡さない chat_step を 1 回だけ回して回答を書かせる — 調べた内容を捨てない)・ツール結果 6,000 字で切り詰め。中断は**リクエスト単位**で扱う (会話を破棄しても古い往復は走り続け、次の送信も許すため、同じ接続で複数本が同時に走りうる)。フロントが往復ごとに ID を採番し、キャンセルレジストリのキーは `<connection>\u{1}ai-chat\u{1}<request_id>` (`chat_cancel_key`。同じキーは登録が上書きされるため ID を含めないと片方しか止められない)。`cancel_ai_chat` コマンドは実行中のクエリを止める (CancelRegistry) だけでなく、`ChatCancels` に ID を控えて**次のモデル呼び出し・ツール実行も行わせない** (クエリ 1 本を止めるだけでは、モデルの応答待ちや次の往復は止まらないため)。ID を控える方式なので、**ai_chat が走り出す前に届いた中断も入口の判定で拾える** (接続ごとのカウンタ方式では、開始時の基準値に吸収されて効かなかった)。フロントは会話を破棄する時 (接続 / スキーマ切替・Clear・設定リロード) と Stop ボタンでこれを呼ぶ。**会話の破棄は必ず `clearChat` を通す** (`chatMessages = []` を直に書かない): 破棄と中断はセットで、片方だけだとバックエンドのエージェントが切替前の接続で走り続ける。バックエンドの状態を書き換える操作 (スキーマ切替 `set_active_schema` / 設定リロード `reset_connections`) の**前**に `clearChatAndWait` で中断要求の到達まで待つ (投げっぱなしだと、中断カウンタが進む前に切替が先行し、古いエージェントが新しいスキーマ / プールでツール実行を続けうる)。遷移中は `chatTransitions` (カウンタ。真偽値だと遷移が重なった時に先に終わった方が後続分まで解除する) で新しい送信を止める (中断要求は「その時点の実行中 ID」を対象にするため、待っている隙に始まった往復は中断対象から漏れる)。例外は `\c` (`applySwitchedSchema`): クエリの実行そのものが切替なので事前に中断できず、要求が届くまでの短い窓は残る。なお `CancelRegistry` への登録は `run_query_cancellable` の内部で行われるため、プール取得 (SSH トンネル確立を含む) の待ちの間に届いた中断はレジストリに刺さらない — その隙をすり抜けても読んだ行をモデルへ送らないよう、**実行の直前と結果の受領後にも中断を判定し、さらに実行中は 200ms ごとに中断を監視して future を drop する** (登録前に届いた中断でも待ち続けない)。監査記録 (tool_calls) には「DB へ実際に投げた」ものだけを載せる (投げる前に中断した分を載せると誤解を招く)。実行したクエリは応答と一緒に表示する (何を見て答えたかを隠さない)。**失敗した往復も reject せず `ChatReply { content: "", tool_calls, error: Some(...) }` で返す** — 中断やタイムアウトはツール実行の後に起きやすく、エラーで返すとその時点までに実行したクエリが表示から消えてしまうため。接続・スキーマを切り替えると会話は破棄する (system prompt のスキーマが変わるため)。会話を破棄するたびに `chatGeneration` を進め、応答待ちの往復は接続名 + 世代の両方で照合して捨てる (設定リロード / スキーマ切替は接続名が変わらないため、名前比較だけでは弾けない)。待機表示 (`chatSending`) も世代一致で判定するので、破棄した往復のスピナーが新しい会話に残らない。応答待ちの接続は `isConnectionRunning` で「実行中」として扱い、エディタタブが無いだけでトンネル / プールが切られないようにする (完了時に `maybeDisconnectIfIdle` で再判定)。会話を破棄すると古い往復が走ったまま次の送信を許すため、同じ接続で複数本が同時に走りうる — 実行中の印は接続ごとの参照カウント (`chatRunningConnections`) で持ち、最後の 1 本が終わった時だけ切断を再判定する
- クエリファイルの接続間移動 (ドラッグ & ドロップ) — FILES ペインのファイル行を CONNECTIONS ペインの接続へドロップすると、そのファイルが移動先接続のフォルダへ移る (`move_query_file` コマンド → `query_files::move_query_file`)。受け渡しは `lib/fileDrag.ts` の**独自 MIME タイプ** (`application/x-queryfolio-query-file`) で行う: `dataTransfer.getData()` は drop でしか読めないが `types` は dragover でも読めるため、**ドロップ可否とハイライトは type で判定し、中身は drop で取り出す** (`text/plain` だけだと外部から流れてきた無関係なテキストまで受け入れてしまう)。バックエンドは (1) クエリファイルの拡張子が違うエンジン間の移動を拒否する (移動先の一覧に出てこないファイルができるだけなので)、(2) 移動先の同名ファイルを case-insensitive で検出して拒否し、さらに **`O_EXCL` (`create_new`) で移動先の名前を atomic に予約してから `fs::rename` する** (Unix の rename は移動先を黙って置き換えるため、確認と rename の間に同名ファイルが作られると失われる)、(3) 別接続でも `folder_name` が同じなら同じフォルダなので no-op にする。フロント側は**移動の前に**未保存内容を確定させてから (移動後に保存すると移動元のパスにファイルが復活する)、そのファイルを開いていたタブを閉じる (タブの接続を差し替えると `maybeDisconnectIfIdle` のプール / トンネル参照と噛み合わなくなるため、開き直しはユーザーに委ねる)
- 検索モーダル (SearchModal) — ツールバーの検索ボタン (`data-annotate="button-open-search"`) または Cmd+K / Ctrl+K (`+page.svelte` の `handleGlobalKeydown` + `<svelte:window>`) で開くコマンドパレット風モーダル。接続は `app.svelte.ts` の一覧を名前・説明で絞り込み (フロント)、クエリファイルは選択中接続のものを `search_query_files` コマンド (query_files.rs) でファイル名 + 中身検索 (大小無視の部分一致、中身は最初の一致行をプレビュー)。検索は純 Rust (rg/grep 等の外部プロセスは使わない。クエリファイルは少数のため堅牢・インジェクション面なし)。↑↓ で候補移動・Enter で開く (接続はその接続へ切替、ファイルは選択中接続で開く)・Esc で閉じる。デバウンス 150ms + 世代番号で古い応答の上書きを防ぐ
- 設定エディタ (ConfigEditorModal) — メニューバー Config の `Edit config.yml` で開く CodeMirror (YAML) のモーダル。`read_config_file` / `write_config_file` コマンド (config.rs) で ~/.config/queryfolio/config.yml を読み書きする。保存時は YAML マッピングとしてパースできることを確認してから一時ファイル + rename で書き、常に 600 で書く (config はパスワード等を平文で含み得るため、既存が 644/640 でも 600 へ絞る。ensure_config_file_in の新規生成・AppConfig::load の読込時是正と同方針)。保存後に reloadConnections まで行う。未保存で閉じようとすると破棄確認を出す。`QUERYFOLIO_CONFIG_YAML` で上書き中は編集対象のファイルが無いためエラーを返す。`config_override_command` が設定されている時だけ `View override config yaml (Copy only)` が同メニューに出て、`read_override_config_yaml` で取得した YAML を表示する。こちらは取得元が外部コマンドで書き戻せないため Save は無いが、**エディタ上では編集できる** (メモリ上だけの変更。整形してから 1Password 等の保管場所へコピーする用途)。両モードとも YAML のシンタックスハイライトに加え、`yaml` パッケージの `parseDocument` を使った lint (`@codemirror/lint`) でパースエラー・警告を行内とガター (lintGutter) に表示する。ファイル内検索は `@codemirror/search` の `search({ top: true })` + `searchKeymap` (Cmd+F / Cmd+G / F3)。Escape は searchKeymap → 自前の Escape バインド → defaultKeymap の順で評価され、検索パネルが開いていればパネルを閉じ、そうでなければモーダルを閉じる (選択中でも simplifySelection に食われない)。window 側の keydown ハンドラは `e.defaultPrevented` で CodeMirror が処理済みのキーを無視する
- `\c <database>` / `USE <database>` — アクティブスキーマ (database) の切替。SQL に変換できないので `lib.rs` の `run_query` がプール取得前に処理する (`switch_active_schema`)。**`USE` を SQL としてそのまま実行しても切り替わらない**: MySQL の `USE` はセッション単位の変更なので、プールの別コネクションに当たる次のクエリには効かない。加えて `USE` は fetch 系の文でないため readonly ガード (is_readonly_allowed) にも弾かれる。そこで `meta_commands::translate` が `USE <database>` を `\c` と同じ `MetaCommand::Connect` に落とし、プールごと張り直すことで両方を解決している (切替は書き込みではないので Writable OFF でも許す)。対象は `\c` が使える MySQL / PostgreSQL のみ (PostgreSQL に `USE` 文は無いが、MySQL の癖で打たれるため受け付ける)。sqlite / duckdb は `\c` 自体が非対応で、DuckDB の `USE` はネイティブに動くため通常の SQL として実行させる。引数は `\c` と同じ識別子検証 + 方言のクォート (MySQL は `` ` ``、PostgreSQL は `"`) 剥がし。末尾のセミコロンと末尾コメント (`USE mydb; -- switch`) は `scan_sql` の `body_end` で落とす (方言ごとのコメント規則を書き直さないため。MySQL の実行コメント `/*! ... */` は scan_sql が code として扱うので本体に残り、下の複文判定と引数判定で拒否される)。複文 (`USE db; DELETE ...`) は 2 文目を黙って捨てることになるため拒否する (`body_end` 内に残る `;` が 2 文目の証拠)。エージェント経路 (`ReadonlyGuard::Agent`) は `run_query` を通らないため、`db.rs` の `run_query_on` が `MetaCommand::Connect` をエラーにする (接続状態の変更はエージェントに許さない)。`set_schema_override` でプールを捨てて張り直し、切替後の接続で確認用の `SELECT current_database()` / `SELECT DATABASE()` を実行してその結果を返す (空の結果だと成功が分かりにくいため)。接続できなければ `replace_schema_override` で元のスキーマへ巻き戻す (巻き戻さないと以降の全クエリが繋がらない状態で残る)。切替先は `QueryResult.switched_schema` でフロントへ返し、`app.svelte.ts` の `applySwitchedSchema` が `activeSchema` を更新する (スキーマブラウザは activeSchema の変化を購読しているため自動追従、補完用スキーママップは取り直す)。sqlite / duckdb は schema が DB ファイルパスのため非対応
- 接続 (SSH トンネル) の遅延確立 — 接続を選択しただけでは DB 接続 / SSH トンネルを開かない (「選択した瞬間にトンネルが開く」のを避ける)。`applyConnectionContext` は接続を張らない `listQueryFiles` / `getActiveSchema` のみ行い、スキーマ一覧 (`listSchemas`) と補完マップ (`getSchemaMap`) は遅延させる。実際に接続を開く契機は (1) ファイルをエディタに読み込んだ時 (`selectFile` → `ensureConnectionResources`) と (2) スキーマブラウザ (TABLES) を開いた時 (`TablesPane` の `listTables`) の 2 つ。`ensureConnectionResources` はその時点でスキーマ一覧・補完マップも取り込む。確立済みの接続は `resourcesLoaded` (Set) で管理し二重取得を避ける — キャッシュ (`schemas` 等) の有無を接続状態の代用にしない (切断後もキャッシュは残るため、代用すると再オープンの契機を取りこぼす)。確立済みの接続を選び直した時は `applyConnectionContext` がリセットしたスキーマ一覧・補完マップをその場で取り直す (トンネルは開いたままなので新規オープンにはならない。リセットしたまま放置するとプルダウン・補完が現在スキーマだけになってしまう)。逆に、ある接続のエディタタブが全て閉じられたら `removeEditorTab` → `maybeDisconnectIfIdle` が `disconnect` コマンド (lib.rs → `DbManager::disconnect`) でその接続のプールと SSH トンネルを破棄する。スキーマブラウザを開いただけでエディタタブを持たない接続は `removeEditorTab` を通らないため、別接続へ切り替える経路 (`selectConnection` および別接続のエディタタブをアクティブ化する `activateEditorTab`) で切替元へ `maybeDisconnectIfIdle` を呼んで閉じる (エディタタブを持つ接続は no-op になり貼りっぱなしのまま残る)。取り込み (`listSchemas`) を待つ間に切断や設定リロードが挟まった場合は、接続個別の世代 (`connectionLifecycleGen`) と全接続共通のリセット世代 (`connectionsResetGen`) を取り込み前後で比較して検知し、確立済み登録を防ぐ (アイドルな接続を `resourcesLoaded` に残さない)。ただし実行中のクエリ/セル編集がある間は破棄しない (トンネルを途中で切ると実行中コネクションが壊れるため。`isConnectionRunning` でガードし、クエリ完了時 `executeTab` からも再判定する)。`schema_override` はバックエンドに保持されるので張り直し後も同じアクティブスキーマで繋がる。クエリ実行 (`run_query` の `get_pool`) は従来どおり必要時に自動で張り直す。CSV エクスポートは実行済みの結果データを使うためトンネルを必要としない。
- メニューバー — macOS のアプリメニュー (Queryfolio) は NSApplication がメインメニュー設置時の内容で確定するため、tauri のデフォルトメニューに後から insert しても反映されない。そのため `Menu::default` を使わず `build_menu` でアプリメニューを含めて丸ごと組み、`Builder::menu` で最初の設置時から渡す。設定リロード時 (reset_connections) は `rebuild_menu` で組み直し、コピー用ビュー (保存不可) の項目を出し入れする。設定関連の項目 (`Edit config.yml` / `View override config yaml (Copy only)` / `Reload config file` / `Reveal config folder`) は**プラットフォームに関わらず Config サブメニューにまとめる** (以前は macOS だけアプリメニュー側に前 2 つを置いていたが、探す場所が 2 箇所に分かれるため統合した)。`Reload config file` には**アクセラレータを付けない** — 以前は `CmdOrCtrl+R` だったが、実際に走るのは reloadConnections (全エディタタブの破棄・接続の張り直し・チャットの中断) なので、ページ再読込のつもりで Cmd+R を押すとアプリ全体が初期状態へ戻る (CYBERNEURA-DEV-648)。**Cmd+W / Ctrl+W はウインドウではなくアクティブなエディタタブを閉じる** — 定義済みの Close Window は macOS で Cmd+W を持ち、ウインドウ 1 枚のこのアプリでは押すとアプリごと閉じるため、File / Window のどちらにも置かず、File の `Close Tab` (`close_editor_tab`、アクセラレータ `CmdOrCtrl+W`) が `menu-close-editor-tab` イベントを出す。フロント (`+page.svelte`) はタブが無い時とモーダルが開いている時は何もしない。モーダルの判定は `isModalOpen` (ページが状態を持つもの) に加えて `document.querySelector("[data-modal]")` を見る — コンポーネントが自前で開くモーダル (ResultsPane のセル編集プレビュー) は `isModalOpen` から見えないため。**モーダルを足す時はルート要素に `data-modal` を付けること**。メニューのキー割り当ては NSApp が WebView より先に処理するので、フロントの keydown では止められない (CYBERNEURA-DEV-773)。`Third-Party Licenses` (`show_licenses`) は About の直下 (macOS はアプリメニュー、他は Help) に置き、`menu-show-licenses` イベントでフロントの `LicensesModal` を開く
- Writable スイッチ — ツールバーの `data-annotate="toggle-writable"` トグル。OFF (既定、セッションごとに OFF から始め永続化しない) の間は SELECT/SHOW 等の副作用の無い文しか実行できない。app.svelte.ts の `writable` state が run_query に `writable` として渡り、バックエンド (lib.rs) が readonly ガードの由来を `db.rs` の `ReadonlyGuard` (Off / Config / Switch / Agent。Agent は AI チャット専用で DB レベルの強制を伴う) で決めて強制する。実効 readonly = `config readonly || !writable`。config で `readonly: true` の接続はスイッチより優先 (ロック表示 = `writable-locked`) で解除できない。ブロック時のメッセージは由来で出し分ける (Config / Switch)
- ペインのサイズ変更 — `+page.svelte` が接続一覧幅 / サイドバー幅 / エディタ縦割合を `$state` で管理し、`PaneDivider` (Pointer Events + setPointerCapture) のドラッグで変更する。ドラッグ終了時に localStorage (`queryfolio.layout.*`) へ保存し起動時に復元。各ペインコンポーネントの root は `w-full` で、幅は `+page.svelte` のラッパー div が inline style で与える
- `lib/export.ts` — CSV/TSV/JSON 変換 (formula injection 対策込み)。テーブル全体用 (`toCsv` / `toTsv` / `toJson`) と選択範囲用 (`toCsvRange` / `toTsvRange` / `toJsonRange`、Cmd+C コピー用) の両系統がある
- 結果ツールバーの出力 UI (ResultsPane) — フォーマット選択プルダウン (TSV / CSV / JSON、既定 TSV、localStorage `queryfolio.results.copyFormat` に永続化) + `Copy` ボタン (テーブル全体をクリップボードへ) + `Export` ボタン (ネイティブ保存ダイアログ `@tauri-apps/plugin-dialog` の `save` で選んだパスへ Rust の `write_export_file` コマンドで書き出す)。Cmd+C の選択範囲コピーも同じ選択フォーマットに従う。`Copy with headers` チェックボックスは Cmd+C 選択コピーのヘッダ有無 (CSV/TSV のみ) に効く
- 結果テーブルの行仮想化 (ResultsPane) — **全行 × 全列を DOM に置いてはいけない**。行数は `max_rows` で抑えているが**列数には上限が無い**ため、500 行 × 61 列で 31,000 セルになり、同じドキュメントにいる CodeMirror エディタの入力が詰まる (CodeMirror はキー入力のたびに強制同期レイアウトを行うため。実測で 1 打鍵 17ms、実使用では**キー入力を取りこぼす**レベル)。さらに全セルが `cellBgClass` / `isSelectedCell` 経由で `selection` を購読するため、ドラッグ選択のたびに全セルのエフェクトが再評価される。対策は 3 点セットで、**どれか 1 つでも欠けると成立しない**: (1) 表示範囲 + オーバースキャン 10 行だけ描画し、上下のスペーサー `<tr>` でスクロール量を保つ。(2) `border-collapse` ではなく **`border-separate` + `border-spacing: 0`** を使う (各セルが `border-b border-r` を自前で描いているので見た目は同じだが、WebKit の border-collapse のレイアウト経路が重く、これだけで 17ms → 5ms)。(3) `table-layout: fixed` + 列幅の明示指定 (仮想化すると「描画中のセル」だけで auto レイアウトの列幅が決まり、スクロール中に幅が動くため)。設計上の要点: **`table-layout: fixed` は table の `width` が `auto` でないときだけ有効**なので `min-w-full` (= `min-width`) では効かず、自動レイアウトに戻る (col の幅は単なるヒント扱いになる)。かといって `width: 100%` にすると、ペインが狭い時 (セルインスペクタを開いた時など) に列が指定幅未満まで圧縮される。そこで **`width: max(100%, 列幅合計)`** を明示指定し、狭い時は横スクロール・広い時は**幅指定の無い余白列** (`<col>` + `<th>` + 各行の `<td>`) が余りを吸う形にしている (余白列が無いと余りが全列へ比例配分され `#` 列が極端に広がる)。列幅は等幅フォントなので `ch` 単位で文字数から直接引ける (DOM 実測が不要) が、**CSS の `ch` は半角基準なので `String.length` をそのまま使うと日本語の列が半分の幅になる** — `displayWidth()` で East Asian Wide / Fullwidth を 2 と数える。この幅計算は**必ず上限 (`MAX_COL_CHARS`) で打ち切ること**: 打ち切らないとコストがセル数でなく**総文字数**に比例し、長い TEXT 列で秒単位のフリーズになる (`db.rs` は sqlx 経路でセルの文字数を切り詰めないため 100KB の TEXT がそのまま届く。実測 500 行 × 3 列 × 100KB で 1,971ms → 打ち切りで 1ms)。仮想化に伴う注意: **フォーカス中の要素が DOM から取り除かれても `blur` は発火しない**ため、編集中セルが描画範囲外に出たら `$effect` で明示的に確定する (`commitCellEdit` は対象タブを `activeTab` ではなく `editingCell.tabId` から引く — 別タブへ切り替えた時点で `activeTab` は切替先になっているため)。行のアンマウントでフォーカスが `body` へ落ちると Cmd+C / Cmd+A が効かなくなるので復帰させるが、**これは scroll ハンドラではなく `rowWindow` を購読する `$effect` で行う** (scroll の時点ではまだ行が DOM にあり、PageDown のような単発スクロールでは次の scroll イベントも来ない)。結果を差し替える時は `scrollTop` と **`scrollLeft` の両方**を戻し、`activeTabId` だけでなく **`executedAt` も購読する** (`prepareTargetTab` はピン留めの無いタブを再利用するため、同一タブでの再実行では `activeTabId` が変わらない)。`rowWindow.start` は**必ず最終行までクランプする** (行数の少ない結果へ差し替えた直後は `scrollTop` が古いままで `start > end` になり 0 行描画になる)
- `lib/sqlFormat.ts` — SQL 整形器 (自前トークナイザ。SELECT / UNION 系のみ整形し、INSERT / UPDATE / WITH 等やパース不能な文は原文維持。整形結果を再トークナイズして入力とトークン列が一致しなければ原文に戻す安全ネット付き)
- Run and Log (`lib/runLog.ts`) — `-- 📝 <label>` を頭に置いた行コメントの直下にある文を実行すると、その文の下へ結果を `/* 🗒️ <label> <実行時刻>` で始まるブロックコメント (TSV) として書き戻す (CYBERNEURA-DEV-447。**runandlog** (CYBERNEURA-DEV-442) の SQL エディタ版)。結果テーブルへの表示は従来どおりで、書き戻しはその複製。マーカーの検出 (`findRunLogLabel`) は「実行対象の直前に連続する行コメント」と「実行対象 [from, to) の先頭に含まれるコメント」だけを見る (実行対象の外では、間に空行や別の文があれば別の文のマーカー)。設計上の要点: (1) **書き戻す本文の `*/` と `/*` は必ず潰す** (`escapeBlockComment`)。`*/` を残すとコメントがそこで閉じ、続くデータ行がそのまま SQL として実行される。`/*` も残せない — PostgreSQL のブロックコメントは**入れ子になる**ため、閉じない `/*` があると以降のファイル全体がコメントに飲まれる。(2) **再実行は既存ブロックを置き換える** (runandlog と同じで、何度実行してもブロックは 1 つ)。`/* 🗒️` の直後の最初の `*/` を終端とみなせるのは (1) で本文から `*/` を消しているから。閉じていない既存ブロックは書かずに知らせる (壊れたコメントの前に足すと入れ子が増えるだけ)。(3) 挿入位置は文末の空白と `;` を越えた後ろ (lang-sql の Statement は `;` を含むが、含まない場合にセミコロンの手前へ挟み込まないため)。前後の空行は 1 行に正規化するので、同じ結果を書けばテキストは変わらない (冪等)。(4) **実行完了は非同期**なので、書き戻し前に「実行元のタブが残っていて同じ接続か」「アクティブスキーマが実行開始時のままか (`\c` / `USE` が切り替えた先は除く)」「対象範囲のテキストが実行した SQL と一致するか」の 3 つを照合し、**ラベルは書き戻す時点の本文から取り直す** (SQL が変わらなくてもマーカー行だけ編集されうるうえ、同じ長さの書き換えは範囲の照合を素通りする。マーカーごと消されていれば取り消しとみなして何も書かない) (`SqlEditor.writeRunLog`。`replaceRangeIfMatches` と同じ考え方)。ズレていたら書かずに toast で知らせる。カーソルとフォーカスは動かさない (待っている間にユーザーが別の場所を編集している)。**実行中に別のタブへ移っていても書く** (CYBERNEURA-DEV-858): そのタブの本文 (`tab.content`) を `app.svelte.ts` の `writeRunLogToInactiveTab` が直接書き換えて即座に CAS 保存する (自動保存の予約は 1 タブ分しか持てず、予約すると編集中の別タブの予約を奪うため)。判定はエディタ経路と共通の `planRunLogWrite` で、加えてタブが閉じられた・別接続へ移った・衝突中 (`conflicted`) の場合は書かない。未編集の CRLF ファイルは CodeMirror と同じく LF に揃えてから照合する (target の位置は LF 基準のため)。別接続を開いている間は `activeSchema` がその接続のものではないので、実行した接続のスキーマは `getActiveSchema` で訊く。その待ちの間にタブへ戻ってきたらエディタ経路に切り替える (表示中の本文を丸ごと差し替えない)。(5) **200 行 (`RUN_LOG_CONFIRM_ROWS`) を超える結果は書き戻す前に確認ダイアログ (`RunLogConfirmModal`) を出し、「全部書く / 先頭 200 行だけ書く / 書かない」を選ばせる** (CYBERNEURA-DEV-518)。ちょうど 200 行なら全部書いても同じ量なので訊かない (判定は `>=` ではなく `>`)。閾値と「一部だけ書く」の行数を同じ定数にしているのは、別の値にするとダイアログで何行になるのか説明できないため。**行数の上限だけでは足りない** — セルの文字数に上限が無いため数行でも長い TEXT / JSON 列があれば数百 MB になる。総文字数とセルあたりの文字数でも打ち切り (`toTsvCapped`)、打ち切ったことを本文に書く。本文末尾の注記は 4 種類あり、**それぞれ別の事実なので文言を読み分けられるようにしてある**: `(limited to N rows)` = ダイアログで一部だけ書くと選んだ / `(the query was limited to N rows)` = auto LIMIT が実際に行を落とした / `(the result itself was truncated)` / `(this log was truncated …)`。**auto LIMIT の注記は「LIMIT が付いた」ではなく「実際に抑制された」時だけ書く** — `LIMIT 500` を付けても 5 行しか返らなければ何も落ちていないので、書くと「まだ続きがある」という誤った警告になる (`result.rows.length >= result.applied_limit` で判定。ちょうど上限の時は続きの有無が分からないので書く側に倒す)。(6) 対象は **SQL 言語のエディタのみ** (redis / es は行コメントもブロックコメントも記法が違う)。lang-sql はブロックコメントを必ず Statement の兄弟ノードにするので、書き戻したブロックが次の文の実行範囲に混ざることはない。(7) **行コメントは Statement の兄弟になるとは限らない**。中身の無い `--` だけの行があると lang-sql はそれを LineComment にせず、**その行から SQL までが 1 つの Statement になる** — 説明のコメント・空行・`-- 📝 ラベル` の行がまとめて実行範囲に入る。そのため `findRunLogLabel` の前方スキャン (実行範囲の先頭のコメントを読み飛ばして SQL 本体を探す処理) は**空行で止めてはいけない** (CYBERNEURA-DEV-516)。代わりに実行範囲の終端 `to` で止める — 範囲の外へ出ると次の文のマーカーを拾ってしまう

## CLI (GUI を起動しないオプション)

`--help` / `--version` / `--license` / `--list-servers` は標準出力に書いて `std::process::exit` で終わる。
`cli.rs` にまとめてあり、`run()` が Tauri を組み立てる前 (write の書き出しよりも前) に
`cli::info_command_from_args` で判定する。

- **サブコマンドが先に来たら見ない**。`queryfolio write conn a.sql "--help"` の第 3 引数は
  書き出す内容であってオプションではない。位置固定ではなく走査にしているのは、macOS が
  `.app` 起動時に `-psn_0_12345` のような引数を先頭へ差し込むことがあるため
  (`route_from_cli_args` と同じ理由)。
- **`--list-servers` はパスワードと SSH の鍵・パスフレーズを出さない。** 出す項目は
  `ConnectionInfo` (フロントへ渡す機密を含まない射影) とフォルダ名だけで、`ServerConfig` を
  直接読むのは TLS の判定だけ。項目を増やす時もこの経路を守ること (テストで担保している)。
- TLS 列は mysql / postgres / redis では**実効モードをそのまま出す**
  (`disable` / `prefer` / `require` / `verify-ca` / `verify-full`)。既定の `prefer` は
  「TLS を試み、張れなければ平文に降格し証明書も検証しない」なので、yes/no に丸めると
  暗号化されていない接続に気付けなくなる。実効モードを持たないエンジンは `tls` を `on` / `off` で出す。
  **`ConnectionInfo::sql_ssl_mode` が `None` でもそのまま `tls` に落とさないこと** —
  `None` には「実効モードの概念が無いエンジン」だけでなく「`engine` / `ssl_mode` の値が
  不正で解決できなかった」場合も含まれ、後者を `on` / `off` で出すと**接続時にエラーになる
  設定を有効な TLS 設定として見せる** (`ssl_mode: requre` の書き間違いが `off` = 平文で
  繋がる、と読める)。決められない時は `invalid` と出す (`ssl_summary`)。
  **ただし `invalid` を出すのは「その値で実際に接続が失敗するエンジン」だけ** —
  `ssl_mode` を読むのは `db::connect` の mysql / postgres の分岐だけで、
  elasticsearch / sqlite / duckdb / dynamodb の経路は見ない。共有テンプレート等で
  不正な `ssl_mode` が紛れ込んでいてもそれらは普通に繋がるので、`invalid` と出すと
  使える接続を壊れているように見せる。`invalid` の意味は「この設定では繋がらない」で
  あって「設定に無効な値が書いてある」ではない。
  **`ssl_mode` が解決できても組み合わせで拒否される設定は `invalid`** — `ssl_root_cert` を
  検証しないモード (`disable` / `prefer` / `require`。`ssl_mode` 省略時の既定 `prefer` を
  含む) と併記すると `sql_ssl_root_cert` がエラーにするため `db::connect` は必ず失敗する。
  実効モードだけ見て `prefer` と出すと繋がらない接続を有効に見せることになる。
  ただし**ルート CA がファイルとして実在するか (`ssl_root_cert_path` の `is_file`) までは
  見ない** — 表示の組み立てはファイルシステムに触らない純粋な関数として単体テストで
  固めてあり、後から置ける不在ファイルは設定の誤りとも違う。
  **`tls` が実際の接続方式を決めていないエンジンでも、そのまま出さないこと** —
  dynamodb の `host` / `port` / `tls` は dynamodb-local 向けのエンドポイント上書き専用で、
  `host` を書かない通常の AWS 接続は SDK が地域エンドポイントを常に https で解決する
  (`build_client` は `host` がある時しか `endpoint_url` を組み立てない)。既定の
  `tls: false` を出すと**暗号化されている接続を平文と読ませる**ので、上書きが無ければ
  `on` を出す (`has_endpoint_override`)。エンジンを足す時は「`tls` がそのエンジンの
  実際の接続方式を決めているか」を先に確認すること。
  **ただし「`host` が無い = 地域エンドポイント」でもない。** `aws_config::defaults` は
  エンドポイント上書きの設定も読むので、`AWS_ENDPOINT_URL=http://localhost:8000` が
  効いていれば平文で繋がる。環境変数は `cli::aws_endpoint_override` が SDK と同じ
  優先順 (`AWS_IGNORE_CONFIGURED_ENDPOINT_URLS` → `AWS_ENDPOINT_URL_DYNAMODB` →
  `AWS_ENDPOINT_URL`) で解決し、URL のスキームから `on` / `off` を出す。解決は
  `lib.rs` の `run_info_command` で行って `format_server_list` へ渡す (表の組み立ては
  プロセスの環境にも依存しない純粋な関数に保つ)。**`~/.aws/config` の `endpoint_url` /
  `services` セクションは見ていない** — ファイルを読まない経路に保ちたいうえ、SDK の
  プロファイル解決を写すと本体とずれた第二の実装になるため。プロファイルで上書きして
  いる環境では `on` と出る (既知の限界)。
- **`--list-servers` は dynamodb の USER を `(hidden)` にする。** この `user` は AWS の
  アクセスキー ID であって DB のユーザー名ではなく (`folder_meta.rs` が同じ理由で
  `(aws access key, hidden)` に差し替え、`sqlfiles_folder_name` はハッシュ化している)、
  端末とシェル履歴に残す値ではない (`user_cell`)。未設定の `-` とは別の語にしてある —
  「静的キーを設定していない」と「設定しているが出さない」は別の事実なので。
  **`ConnectionInfo` は「フロントへ渡してよい」射影であって「端末に出してよい」射影ではない。**
  `user` のようにエンジンで意味が変わるフィールドがあるので、列を足す時は素通しにしない。
- **Windows の release ビルドはコンソールを持たない。** `main.rs` の
  `windows_subsystem = "windows"` により GUI サブシステムでリンクされるため、
  `GetStdHandle(STD_OUTPUT_HANDLE)` が無効ハンドルを返し `print!` が黙って捨てられる。
  表示が目的の情報系オプションでは機能しないので、表示の前に
  `cli::attach_parent_console` (`AttachConsole(ATTACH_PARENT_PROCESS)`) で親プロセスの
  コンソールへ繋ぎ直す。**実機の Windows では未検証** (開発ホストにも CI にも Windows が
  無いため、Windows ターゲットでの型検査までしか行えていない)。
- **上記で直るのは出力先だけで、「シェルが終了を待たない」ことは直らない。** GUI
  サブシステムの exe を cmd.exe / PowerShell から起動すると、シェルは終了を待たずに
  プロンプトへ戻る。そのため対話シェルでは (1) 出力が次のプロンプトの後に現れることがあり、
  (2) `%ERRORLEVEL%` / `$LASTEXITCODE` が情報系オプションの終了コードにならない。
  リダイレクトとパイプは通常どおり動く (ハンドルは継承され、読み手は書き込み側の
  クローズまで待つ) ので、スクリプトから使う分には影響しない。
  **これを直すにはコンソールサブシステムの別 exe を配布物に足すしかなく、GUI 起動時に
  コンソール窓が出る副作用と、この環境では検証できない配布物の変更を伴う。**
  アプリの主目的は GUI なので、その取引はしていない。対話シェルで終了コードまで要るなら
  `Start-Process -Wait queryfolio -ArgumentList '--list-servers'` を使う。
- 表示の組み立て (`help_text` / `format_server_list`) は Tauri にもファイルシステムにも
  依存しない純粋な関数にして単体テストで固めてある。設定の読み込みと出力は `lib.rs` の
  `run_info_command`。`--list-servers` は設定読み込みに失敗したら stderr に書いて 1 で終わる。
- **版番号は `CARGO_PKG_VERSION` ではなく `tauri.conf.json` の `version`。** リリースは
  そちらの version で決まる (`release.yml`) 一方、`src-tauri/Cargo.toml` の version は
  追随していないので、`CARGO_PKG_VERSION` を出すと配布物が 0.1.4 でも `--version` は
  0.1.0 と答えてしまう。`build.rs` が `tauri.conf.json` を読んで `QUERYFOLIO_VERSION` として
  埋め込み (読めなければビルドを失敗させる)、`cli.rs` の `APP_VERSION` がそれを使う。
  ずれの再発は `test_version_comes_from_the_tauri_config` が止める。
- macOS の `.app` からは `open -a Queryfolio --args --list-servers` では標準出力が返らない。
  `Queryfolio.app/Contents/MacOS/queryfolio --list-servers` とバイナリを直接叩く。

## 依存ライブラリのライセンス表示

`THIRD-PARTY-NOTICES.txt` は `scripts/generate-third-party-notices.sh` (`pnpm notices`) の生成物で、
`third_party_notices.rs` が `include_str!` で埋め込み、メニューの Third-Party Licenses
(macOS はアプリメニューの About の直下、Windows は Help の About の直下。フロントの
`LicensesModal` に出す) と `queryfolio --license` が表示する。
**依存を足す・上げる時は流し直してコミットする** (直接依存が lock の version で載っていない・
載っている version が lock に無いと `cargo test` が落ちる)。

- Rust 側は cargo-about (`cargo install cargo-about --locked --features cli`)。
  `src-tauri/about.toml` の `targets` で配布ターゲット (mac / Windows) だけに絞っている。
  依存が多いので 1 回 6 分ほどかかる。`accepted` に無いライセンスが出たら `--fail` で止まる。
  **GPL / LGPL / AGPL 系を accepted に足さないこと** (配布条件が変わる)。
- C / C++ のソースを同梱して静的リンクする crate (openssl-src / libssh2-sys / libz-sys /
  libsqlite3-sys / libduckdb-sys) は、crate のライセンスとは別に同梱ライブラリ側のライセンスを
  "Native libraries" 節に載せている (スクリプトの `NATIVE`)。vendored の C ライブラリを
  足したらここにも足す。
- DuckDB は `third_party/` に 20 以上のライブラリを同梱するが、crate の tarball には
  ライセンスファイルが無い。スクリプトが tarball の `DUCKDB_VERSION` を読み、同じタグの
  DuckDB リポジトリ (GitHub) から取ってくる (ネットワークが要る)。**`DUCKDB_THIRD_PARTY` に
  無いディレクトリが増えたら生成が止まる** — ライセンスを確かめてから SPDX を足すこと
  (mbedtls のように GPL との二択のものは許容側を選んだことを書く。GPL のみのものなら人間に回す)。
- npm 側は `dependencies` を推移的に全部と、devDependencies だが webview に bundle される
  もの (スクリプトの `BUNDLED_DEV_PACKAGES`)。後者は `vite build` の sourcemap に現れる
  パッケージから決めた。devDependencies に bundle されるものを足したらここにも足す。

## `queryfolio://` スキーム / CLI (ファイルを開く)

保存済みのクエリファイルを、URL スキームまたは CLI からパス指定で開ける。どちらも
`router.rs` の共通ルーターを通り、今後アクションを増やす時は `Route` の variant と
パースを足すだけで両方に対応できる。

- **URL スキーム**: `queryfolio://open/<絶対パス>` (例: `queryfolio://open//Users/me/.config/queryfolio/sqlfiles/reporting/monthly.sql`。絶対パスなのでスキーム後に `/` が重なる)。macOS はネイティブに (`tauri.conf.json > plugins > deep-link > desktop > schemes` の `queryfolio` を bundle 時に Info.plist へ登録) URL を受け取る。Linux/Windows は `single-instance` プラグイン (deep-link feature) が 2 個目の起動の URL を実行中インスタンスへ転送する。
- **CLI**: アプリのバイナリを `queryfolio open <パス>` サブコマンドで起動する。実行中インスタンスがあれば single-instance がそのウインドウを前面化してそこで開き、無ければ新規起動後に開く (macOS の `.app` からは `open -a Queryfolio --args open <パス>`、または `open "queryfolio://open/<パス>"` でも同じ経路)。
- **CLI (書き出して開く)**: `queryfolio write <接続名> <ファイル名> [内容]`。パスではなく**接続名 + ファイル名**で指定するので、呼ぶ側が `sqlfiles_dir` やフォルダ名の規則を知らなくてよい (AI エージェント向け)。内容は第 3 引数か**標準入力**で渡す。拡張子は接続エンジンのものが補われ、接続フォルダは無ければ作られる (`_queryfolio.md` も書かれる)。設計上の要点:
  - **書き出しは起動したプロセス自身が Tauri の起動前に行う** (`lib.rs` の `apply_cli_write_route`)。single-instance が実行中インスタンスへ転送するのは argv と cwd だけで**標準入力は転送されない**ため、書き出しを実行中インスタンス側に任せるとパイプで渡した内容が消える。転送される `Route` は「開く」指示としてだけ使い、`resolve_route_target` の `WriteFile` 分岐は**書き込まない** (argv 経由で内容が二重に届いても二重書き込みにならない)。
  - **失敗したら起動を続けず非ゼロ終了する** (`run` が `std::process::exit(1)`)。続行して「開く」だけ行うと、書き込みに失敗しているのに古い内容が開いてしまい、呼んだエージェントには成功に見える。標準入力は 10MiB 上限で、超過は切り詰めずエラー (途中まで書くとクエリが壊れる)。
  - **空の標準入力は「内容の指定なし」として扱う** (既存ファイルを潰さない)。GUI 起動 (`open -a Queryfolio --args write ...`) の stdin は /dev/null で即 EOF になるため、これを「空で書け」と解釈すると既存のクエリファイルが黙って消える。端末が stdin の場合も読まない (入力待ちで固まるため)。内容の指定が無い時は「無ければ空で作る」だけ (`query_files::ensure_query_file`)。
  - **`write` は URI からは受け付けない** (`parse_uri` は UnknownAction にする)。`queryfolio://` は Web ページからでも開かせられるため、URI で書き込みを許すと閲覧中のページが任意の SQL をクエリファイルとして置ける (後で人間が実行する危険がある)。読むだけの `open` と違い、書き込みは CLI 限定にする。
  - 実行中インスタンスが既にその接続を選択済みだと `selectConnection` が no-op になり、外部で作られたファイルが FILES 一覧に出ない。`openFileByTarget` は一覧に無いファイル名なら `listQueryFiles` を取り直す。
- **セキュリティ**: 開けるのはクエリファイル保存ディレクトリ (`sqlfiles_dir`) 直下の接続フォルダにあるクエリファイル (`.sql` / `.redis` / `.es`) だけ。`resolve_open_target` が字句正規化で `..` トラバーサルを潰し、保存領域外・未知のフォルダ・許可外拡張子・ドット始まりを拒否する (ファイルシステムには触れない純粋な検証)。拡張子が接続エンジンのものと一致するかは lib.rs (resolve_route_target) が追加で検証する。
- **配線**: 起動時指定は setup が `AppState.launch_route` に控える。フロントは onMount で listener を登録した直後に `frontend_ready` コマンドを 1 度呼び、起動時指定 + 起動中 (listener 準備前) に届いてキューされた分をまとめて受け取って開く。以降の実行中指定は Rust が解決して `open-query-file` (成功) / `open-query-file-error` (失敗) イベントで直接届き、`app.svelte.ts` の `openFileByTarget` が接続を選択してファイルを開く (listener 準備前の取りこぼしを `AppState.live` の ready フラグ + キューで防ぐ)。

## 設定 (config.yml)

設定は `~/.config/queryfolio/config.yml` (無ければ `config.yaml`) に一本化されている。settings.json は存在しない。

- `servers` はサーバー定義のリスト (各エントリの形式は sql-agent-mcp-server 互換の直書き。あちらの同等キーは `sql_servers`)。マッピングを書くとエラー。旧名 `sql_servers` / `sql_server_templates` は互換のために残さず、パース時 (config.rs の reject_renamed_keys) に改名を案内するエラーにする (黙って無視すると接続 0 件の原因が分からないため)。ネストしたグループエントリ内の旧名も parse_server_entries で拒否する。
- グループ機能 (queryfolio 独自拡張) — `servers` のリスト項目に `group_name:` + ネストした `servers:` リストを書くと、その中のサーバーが接続一覧 (ConnectionsPane) でグループ見出し付きで表示される。パース時にフラット化され各 `ServerConfig.group_name` に記録 → `ConnectionInfo.group_name` でフロントへ (config.rs の parse_server_entries)。直書きサーバーとの混在可・設定順のまま表示。グループのネスト (深さ 2 以上) と、グループエントリの group_name / servers 以外のキーはエラー。グループ内でも `template:` 継承は有効。
- `config_override_command` (任意、queryfolio 独自拡張) — 書いたコマンドを実行し、その stdout (YAML) を設定全体へ**再帰的にマージ**する (取得 YAML 側が優先)。`servers` に限らずどのキーでも上書きできるため、API キーや接続情報を 1Password 等に置いたまま `default_limit` や `sqlfiles_dir` も差し替えられる。マージ規則は config.rs の `merge_mapping`: **マッピング同士は再帰的に混ぜ、スカラーとシーケンス (servers を含む) は丸ごと置き換える** (リストは要素の同一性を決められないため要素単位マージはしない)。取得 YAML 側に `config_override_command` があっても再帰取得はせず、マージ後にキーを落とす。解決は `AppConfig::load_merged` (async)。`AppConfig::load` はローカルファイルのみ読む同期版で、メニュー出し分け (has_config_override_command) 等で使う。**マージ済み設定は AppState に 1 つだけセッションキャッシュされる** (取得コマンドは 1Password 等で数秒 + Touch ID を要するため、クエリ実行のたびに走らせない)。default_limit / sqlfiles_dir / 接続一覧 / ai はすべてこのキャッシュから導出するので個別キャッシュは持たない (クリア漏れ防止)。reset_connections でクリア。キーが存在するのに文字列でない・空文字ならエラー (黙って「未設定」に倒すと、オーバーライドが効かないままローカル設定で動いていることに気付けないため)。旧方式 (旧キー `sql_servers` にソース宣言) の設定はエラーになり、メッセージで `config_override_command` へ移行するよう案内する (reject_renamed_keys が改名案内と一緒に出す)。なお `View override config yaml (Copy only)` はキャッシュを経由せず毎回コマンドを実行する (保管場所の現在値を確認・コピーする用途のため意図的)。
- `config_override_command` はシェル非経由 (shlex 分解) で実行。GUI 起動の最小 PATH 対策として /opt/homebrew/bin と /usr/local/bin を補完する。60 秒タイムアウト + kill_on_drop。
- `password_command` (任意、queryfolio 独自拡張) — 接続ごとに、パスワードを取得するコマンドを書く (RDS IAM 認証トークン等の期限付き資格情報、Terraform state から読むスクリプト等)。stdout から末尾の `\r` / `\n` だけを除いた文字列がパスワード (トークンは `&` `=` `%` を含むので他は触らない)。空出力・非 UTF-8・非ゼロ終了はエラー。実行器は config_override_command と共通 (`run_setting_command`、60 秒)。**実行するのは `DbManager::get_pool` が新しいプールを作る時だけ** — 設定の読み込み時・接続の選択時・他の接続の分は実行しない (遅延接続の方針を崩さない)。結果は get_pool が複製した `ServerConfig.password` へ入れて `connect()` に渡すので、password を読む全エンジンがエンジン側の変更なしに動き、プールの接続オプション以外には残らない。検証 (`ServerConfig::password_command`) は ssl_root_cert と同じく接続時に行う (読み込み時に弾くと 1 接続の誤りで一覧ごと失われる): password との併記 (テンプレート由来を含む。打ち消すなら `password: null`)・sqlite / duckdb・空文字はエラー。**期限切れへの対応**: sqlx のプールは後から張るコネクションにも作成時のパスワードを使うため、15 分で失効する IAM トークンのプールは既存コネクションが生きていても新しいコネクションから失敗する。そこで DB を使う Tauri コマンドは `get_pool` ではなく `DbManager::with_pool` (プール取得済みなら `retry_on_expired_password`) を通し、認証エラー (Postgres 28P01 / 28000、MySQL 1045。`is_auth_failure_code`) なら password_command を取り直して 1 度だけ再実行する。判定は純関数 `should_retry_with_fresh_password` (password_command の接続か・再試行済みか・エラーコード)。静的な password の接続は再試行しない。**再実行はプールを作り直さず `Pool::set_connect_options` でパスワードだけ差し替える** — 作り直すと配布済みのプールの複製 (CancelTarget がキャンセル発行用に持つもの等) が古いパスワードのまま残り、認証済みの既存コネクションも捨てることになる。差し替えならトンネルにも触れず、接続オプションもプール自身のものを複製するので TLS / ローカルポートの解決をやり直さない。認証エラーは接続の確立時にしか起きず文は実行されていないので、再実行で文が二重に走ることはない。redis / elasticsearch / dynamodb は自動再試行の対象外 (プールを捨てるまで同じパスワードを使う)。**秘匿**: 出力はフロント (`ConnectionInfo` は `password_from_command: bool` だけ。ホバー表示は `Password: (command)`)・ログ・履歴・エラー・AI に出さない。`View override config yaml` は config_override_command しか実行しない。DB を使う Tauri コマンドを足す時は `with_pool` を通すこと (`get_pool` 直だと期限切れから復帰しない)。
- `default_limit` (任意、デフォルト 500、0 で無効) — LIMIT 未指定の SELECT に自動で `LIMIT n` を付与する (db.rs の should_auto_limit。サブクエリ LIMIT / FOR UPDATE 等は保守的にスキップ)。
- `readonly: true` (任意、デフォルト false。sql-agent 互換フォーマットへの queryfolio 独自拡張) — その接続で書き込み系の文 (INSERT / UPDATE / DELETE / DDL 等) の実行を拒否する。判定は db.rs の is_readonly_allowed: leading_keyword が select / with / show / describe / desc / explain / pragma / values / table / call 以外なら拒否し、さらに WITH は CTE 付き DML (insert / update / delete / merge)、SELECT は SELECT INTO、EXPLAIN は EXPLAIN ANALYZE + DML (対象文を実際に実行するため)、PRAGMA は代入形 (`=` を含む `PRAGMA journal_mode = WAL` 等の SQLite の DB 変更) を、リテラル・コメント除去済みの単語境界判定で拒否する。メタコマンドは読み取り系のみなので常に許可。SELECT に副作用のある関数 (nextval 等) や括弧形の設定 PRAGMA までは防げない、あくまで事故防止のガード。
- `allow_dangerous_statements: true` (任意、デフォルト false。queryfolio 独自拡張) — 省略時は危険な文 (WHERE 無しの UPDATE / DELETE、DROP、TRUNCATE) を誤操作による全行破壊・テーブル消失防止のため拒否する (db.rs の dangerous_reason: readonly と同じ scan_sql による単語境界判定。UPDATE/DELETE は where 語の有無、DROP/TRUNCATE は常に危険)。先頭キーワードだけでなく、実際に書き込みが走るラップ形 — `WITH ... DELETE/UPDATE` (Postgres の CTE 付き DML) と `EXPLAIN ANALYZE ...` (対象文を実行する) — の中の危険な DML も対象にする。true にすると実行できるが、フロントは実行前に確認ダイアログ (DangerousConfirmModal) を出す。確認要否の判定は check_dangerous_statement コマンド (db.rs の dangerous_statement_reason)。readonly が先に評価されるため readonly 接続ではこのガードには到達しない。弱点: WITH で無関係な CTE / 外側の SELECT に where があると WHERE 無し DML を見逃す (where を一切含まない典型形は捕捉)。サブクエリ内だけの where も同様に安全側 (許可側) に倒れる。
- `sqlfiles_dir` (任意) でクエリファイル保存先を変更できる。デフォルトは `~/.config/queryfolio/sqlfiles/<folder>/<name>.sql`。**相対パスを書いた場合はカレントディレクトリではなく設定ディレクトリ (`~/.config/queryfolio`) 基準で解決する** (config.rs の `resolve_sqlfiles_dir`)。CLI の `write` は起動したプロセス自身が書き出し、開くのは実行中インスタンス (別プロセス・別 cwd) なので、cwd 基準だと書いた場所と開く場所がズレる。Finder から起動した GUI の cwd (`/`) も基準として無意味。`<folder>` は接続ごとに `folder_name` 設定があればそれを使い、無ければ `<host>_<engine>_<schema>_<user>` を組み立てる (接続 name は使わない。config.rs の `ServerConfig::sqlfiles_folder_name`。パス区切り等はサニタイズ)。既存接続でフォルダ名が変わるとそれまでのクエリファイルは旧フォルダに残る点に注意。各フォルダには接続を説明するメタファイル `_queryfolio.md` が自動生成される (folder_meta.rs。非機密のみ・`.sql` でないため UI の一覧には出ない)。
- `folder_name` (任意、queryfolio 独自拡張) — クエリファイルの保存フォルダ名を明示する。省略時のフォルダ名ルールは上記 `sqlfiles_dir` を参照。
- `ssh_tunnel.identity_agent` (任意、queryfolio 独自拡張) — ssh-agent 認証で使う agent socket を明示する (OpenSSH の IdentityAgent 相当)。`none` で agent を無効化。省略時は `~/.ssh/config` の IdentityAgent → SSH_AUTH_SOCK の順で解決 (tunnel.rs)。鍵を 1Password SSH agent に置き GUI 起動する環境向けの解決策。
- `ssh_tunnel.ssh_config` (任意、queryfolio 独自拡張) — `~/.ssh/config` の Host エイリアス名を書くと、その接続のトンネルを libssh2 でなく system の `ssh` クライアントに委譲する (`ssh -N -L`)。**ProxyJump による多段トンネル**や HostName / User / Port の解決を OpenSSH と `~/.ssh/config` に丸投げできる (例: `ssh_config: pop-three-ec2-staging` と書けば、その Host の `ProxyJump pop-three-bastion` 等がそのまま効く)。このモードでは同じ `ssh_tunnel` 内の host / user / password / private_key_* / identity_agent は無視され、認証もホスト鍵検証も OpenSSH 任せになる (`BatchMode=yes` なので未知ホスト鍵やパスフレーズ要求時は対話せずエラー。agent 認証は agent 側で処理されるため動く)。ssh_config を省略した従来の libssh2 経路はそのまま残る (host が必須)。
- `ai:` (任意) — AI SQL 生成の設定 (`provider: openai` / `api_key` / `model` 任意 / `base_url` 任意 / `tool_reasoning_effort` 任意)。`tool_reasoning_effort` は AI チャット (function calling) のリクエストにだけ付ける `reasoning_effort`。gpt-6-luna / gpt-6-sol / gpt-5.6-luna / gpt-5.6-terra 等の推論モデルは `/v1/chat/completions` で tools を使う場合 `reasoning_effort` が `none` でないと 400 を返すため (`To use function tools, use /v1/responses or set reasoning_effort to 'none'`)、**接続先が OpenAI 公式 (base_url 省略または公式 URL) の時だけ省略時に `none` を送る**。`base_url` で OpenAI 互換 API を指している場合は明示指定した時だけ送る (このパラメータを受け付けない相手で、今まで動いていたチャットを勝手に壊さないため)。空文字を指定すると常に送らない。tools を渡さない SQL 生成 / EXPLAIN 解説のリクエストには付けないので、そちらの推論品質には影響しない。**どの値なら通るかはモデルごとに違う**ため (gpt-4o 系はこのパラメータ自体を受け付けない)、モデル名の一覧は持たず、`reasoning_effort` が原因で 400 になったら**そのパラメータを外して 1 度だけ再送する** (`rejects_reasoning_effort` / `remove_reasoning_effort`)。新しいモデルが出ても設定を足さずに動く。ただし `none` を受け付けないモデル (gpt-6-astra) は、外して再送すると今度は「tools と reasoning_effort (省略時の既定値) を併用できない」という 400 になり、AI チャットが使えない (Chat Completions で救う手段が無い。対応するなら Responses API への移行が要る)。**この再送の直前にも中断を判定する** (`chat_step` / `request_chat_completion` の `is_cancelled` 引数。呼び出し側 `run_ai_chat` の中断判定は `chat_step` の前後にしか無いため、1 通目の応答を待つ間に Stop / Clear / スキーマ切替が入ると 2 通目だけがすり抜けてしまう。破棄済みなら `AppError::Cancelled` で打ち切る)。中断を持たない SQL 生成 / EXPLAIN 解説の経路は `never_cancelled` を渡す。ローカル config.yml のトップレベルと、`config_override_command` で取得する YAML のトップレベルの両方に書ける。**両方ある場合は取得 YAML 側を優先** (マージの結果。API キーを 1Password に置ける)。`ai` はマッピングなので再帰マージされ、取得側に `api_key` だけ書けばローカルの `model` 等は残る。provider は現状 openai のみで、不明値はエラー。AppState にセッションキャッシュされ reset_connections でクリア。
- `QUERYFOLIO_CONFIG_YAML` 環境変数は設定ファイル全体を上書きする開発・テスト用フック (実機 E2E 検証で使用)。

`config.example.yaml` 参照。sqlite は `schema` を DB ファイルパスとして扱う独自拡張。duckdb (queryfolio 独自拡張) も同様に `schema` (無ければ `host`) を DB ファイルパスとして扱うが、ファイルが存在しなければエラー (新規作成しない)。SQL エンジンなのでメタコマンド / EXPLAIN / auto LIMIT / Format / AI / TABLES は他の SQL エンジンと同様に使え、セル編集と `\c` (schema がファイルパスのため) のみ非対応。redis (エイリアス valkey、queryfolio 独自拡張) は `schema` を database 番号として扱い、エディタは 1 行 = 1 コマンド。**エディタ上部の Database: 欄で DB 番号を切り替えられる** (CYBERNEURA-DEV-408。選択肢は `CONFIG GET databases` から作り、取れなければ既定の 16。切替は SQL 系と同じく schema override + プール張り直しで効く)。Writable OFF 中は読み取りコマンドのホワイトリストのみ許可、FLUSHALL / FLUSHDB / SHUTDOWN / DEBUG は `allow_dangerous_statements` が必要。クエリファイルは `.redis` 拡張子で、TABLES / Explain / Format / セル編集 / AI は非対応 (capabilities で UI ごと隠れる)。elasticsearch (エイリアス es / opensearch、queryfolio 独自拡張) は host/port (デフォルト 9200) + user/password (Basic 認証) + `tls: true` (https、queryfolio 独自拡張) で接続し、エディタは Kibana Console 風のリクエストブロック。Writable OFF 中は GET/HEAD + 検索系 POST のホワイトリストのみ許可、インデックス削除 (`DELETE /<index>`) と `_delete_by_query` は `allow_dangerous_statements` が必要。クエリファイルは `.es` 拡張子で、TABLES (インデックス + mapping フィールド) は対応、スキーマ切替 / Explain / Format / セル編集 / AI は非対応。dynamodb (queryfolio 独自拡張) は PartiQL を ExecuteStatement API で実行する SQL エディタのエンジンで、`schema` = AWS リージョン (必須)、`host`/`port` = dynamodb-local 等のエンドポイント上書き (`tls: true` で https、port 省略時 8000)、認証は user/password (静的アクセスキー) → `aws_profile` (queryfolio 独自拡張、~/.aws のプロファイル名) → 既定の credentials chain の順。PartiQL に LIMIT 句が無いため auto LIMIT は付与されず API の limit + max_rows で行数を抑える。クエリファイルは `.sql` 拡張子で、TABLES (テーブル + キースキーマ / 属性定義) と Format は対応、メタコマンド / スキーマ切替 / Explain / セル編集 / AI / SSH トンネルは非対応。**`tables` (末尾のセミコロン可) はテーブル一覧を返す queryfolio 独自の文**で、PartiQL に SHOW TABLES が無いため ExecuteStatement を経由せず ListTables に流す。純粋な読み取りなので readonly / dangerous ガードより前で処理し、Writable OFF でも実行できる (CYBERNEURA-DEV-406)。Writable OFF 中は SELECT のみ、WHERE 無しの UPDATE / DELETE は `allow_dangerous_statements` が必要。

## アプリアイコン

アイコンは `resources/app-icons/generate.py` が**サイズごとに描き分ける** (標準ライブラリのみ)。
出力済みのマスターは `resources/app-icons/rendered/` にあり、`.icns` はそれを束ねたもの。

```shell
python3 resources/app-icons/generate.py resources/app-icons/rendered
python3 ../astragal/resources/app-icons/build_icns.py \
  resources/app-icons/rendered src-tauri/icons/icon.icns
```

`src-tauri/icons/` の PNG は `rendered/` からコピーする
(`32x32.png` ← `icon-32.png` / `64x64.png` ← `icon-64.png` /
`128x128.png` ← `icon-128.png` / `128x128@2x.png` ← `icon-256.png` /
`icon.png` ← `icon-512.png`)。

**1 枚のマスターを縮小して作らないこと。** 円柱の線は 1024px キャンバスで 32px (3.1%) なので、
32px へ縮小すると 1px になって潰れる。1Password の権限ダイアログ
(「Allow Queryfolio to use SSH key」) でアイコンがぼやけていたのがこれ
(CYBERNEURA-DEV-825)。**Electron 製アプリは macOS から最大 32x32 しかアイコンを取れない**
(`app.getFileIcon` の制限) ため、1Password はその 32px を拡大して表示している。
効くのは大きいスロットを足すことではなく **32px の中身**。

`generate.py` は小さいサイズで 2 つの手当てをする:

- `MIN_STROKE` … 16 / 32 / 64px だけ線幅の下限を上げる (128 以上は比例のまま = 従来と同じ絵)
- `BAND_COUNT` … 16px は帯 2 本、32px は 3 本に減らす。4 本のままだと隙間が 1px 未満になって
  白い塊に潰れる

形の数値 (角丸半径・楕円・帯の位置) は既存の 1024px アイコンを採寸して決めた。変えるとアプリの
見た目が変わるので、触る時は `rendered/icon-1024.png` を元画像と見比べること。

`.icns` の中身は `../astragal/resources/app-icons/inspect_icns.py` で一覧できる。
icns を扱う道具は astragal に集約してあり、このリポジトリには置かない。

## 開発上の注意

- **アプリ名の表記はユーザーに見える箇所では「Queryfolio」に統一する** (ウインドウタイトル / ツールバー / productName / 生成される設定ファイルのコメント / README 見出し等)。
  **`f` は小文字**で、`QueryFolio` は誤り (CYBERNEURA-DEV-805 で全面的に直した。キャメルケースに「戻さない」こと)。
  リポジトリ名・bundle identifier (com.cyberneura.queryfolio)・crate 名は全部小文字の queryfolio のまま。
  `productName` は macOS の `.app` 名・アプリメニュー・dmg / インストーラのファイル名を決める。
  **ここを変えたら Homebrew の cask (cyberneura/homebrew-tap の `Casks/queryfolio.rb` の
  `url` / `app` / `name`) を追従させる。順番は「新しい名前の成果物を公開してから cask を直す」**
  — 先に cask を直すと、まだ存在しない成果物を指して `brew install` が 404 になる
  (tap の自動更新は成果物のダウンロードに失敗した cask をそのまま据え置くので、
  公開までの間は古い version のまま動き続ける)。
  なお **バンドル内の実行バイナリ名は `productName` ではなく crate 名** (小文字の
  `queryfolio`)。tauri は `mainBinaryName` を指定しない限り cargo の出力をそのまま使う。

- **アプリ内メッセージ (UI ラベル・トースト・placeholder・エラーメッセージ・自動生成される設定ファイルのコメント) はすべて英語で書く**。Rust の AppError 等、フロントに表示される文字列も対象。コードコメントは日本語でよい。
- **`skills/queryfolio/SKILL.md` は実装の事実を複製しているので、該当箇所を変えたら一緒に直す** (`npx skills add cyberneura/queryfolio` で配布され、エージェントがこれを読んで行動する)。特に Run and Log の定数 (`RUN_LOG_CONFIRM_ROWS` / `MAX_BODY_CHARS` / `MAX_CELL_CHARS` / 注記の文言)、クエリファイルのパーミッションと並び順、`--list-servers` の列の決め方、`resolve_write_content` の stdin の扱い、外部変更ウォッチャの挙動。**散文は実行されないので陳腐化しても CI が落ちない** — 実際、このスキルの元にした旧版は 5 箇所が実装とズレていた (確認ダイアログの閾値 500 → 200、`sqlfiles/` のパーミッション、`config.yml` の 600 化、外部変更の 3-way マージ、一覧の並び順)。
- ユーザーアクションを受ける要素には `data-annotate="<識別子>"` を付ける (E2E テスト用)。
- `window.prompt` / `alert` / `confirm` は使わない (ブラウザ自動化がブロックされる + UX)。
- 64bit 整数は JS の Number.MAX_SAFE_INTEGER を超えると Tauri invoke 境界で丸められるため、db.rs の json_i64 / json_u64 で範囲外は文字列化している。
- **正規表現の文字範囲は必ず `\u` エスケープで書く** (文字リテラルで書かない)。見た目が同じでも別のコードポイントになることがある: `ResultsPane.svelte` の全角判定で `豈-﫿` を U+F900-FAFF (CJK 互換漢字) のつもりで書いたところ、ソース中の「豈」が実際には **U+8C48** で範囲が U+8C48-U+FAFF になり、**サロゲート領域 (U+D800-DFFF) を飲み込んで**いた。その結果 `u` フラグ無しの `.test()` では追加面の文字が全て偶然に幅 2 と判定されていた (絵文字が正しく見えたのは偶然、数学英数字 U+1D400 は誤り)。追加面を扱う場合は `u` フラグ + `for...of` のコードポイント単位反復も併用する。
- **フロントで結果の全セルを走査する処理を足す時は、必ず上限で打ち切る**。`db.rs` は sqlx 経路でセルの文字数を切り詰めないため、100KB の TEXT や長い JSON 文字列がそのままフロントへ届く。コストが「セル数」ではなく「総文字数」に比例する実装は、行仮想化で軽くしたはずの経路に秒単位の同期ブロックを持ち込む (実測: 500 行 × 3 列 × 100KB で 1,971ms)。
- sqlx は Postgres の数値型互換が厳密 (INT4 は i32 でしかデコードできない等)。デコード追加時は ~/.cargo/registry の sqlx ソースで `compatible` 実装を確認すること。
- sqlite `:memory:` はプール接続ごとに別 DB になる。テストでは max_connections(1) にする。
- macOS の署名は package.json の `tauri` スクリプトで `APPLE_SIGNING_IDENTITY` (Developer ID Application: Cyberneura K.K.) を設定済み。`pnpm tauri build` (ローカル) はこれで署名される (公証はしない)。tauri.conf.json の `bundle.macOS.signingIdentity: "-"` は env が無い時の ad-hoc 署名フォールバック (tauri-cli の優先順位は env > config)。CI (`release.yml`) は env の Developer ID で署名し、`APPLE_ID` / `APPLE_PASSWORD` / `APPLE_TEAM_ID` を渡して公証 + staple まで行う。
- 実機検証はテスト用 SQLite DB を作り `QUERYFOLIO_CONFIG_YAML` 環境変数で注入して `pnpm tauri dev` を起動すると、ユーザーの実設定を汚さない。orca computer-use で操作する場合、文字入力は type-text でなく paste-text を使う (type-text は二重配送することがある)。
