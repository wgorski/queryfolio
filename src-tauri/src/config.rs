use std::path::PathBuf;
use std::time::Duration;

use serde::{Deserialize, Serialize};

use crate::error::AppError;

/// config_override_command / password_command の実行タイムアウト (秒)。
/// 1Password 等の認証待ちで無限ハングするとコマンド呼び出しが固まるため必須。
const SOURCE_COMMAND_TIMEOUT_SECS: u64 = 60;

/// ~/.config/queryfolio ディレクトリを返す。
pub fn app_config_dir() -> Result<PathBuf, AppError> {
    let home = dirs::home_dir()
        .ok_or_else(|| AppError::Config("Could not determine the home directory".into()))?;
    Ok(home.join(".config").join("queryfolio"))
}

/// パス文字列の先頭の ~ をホームディレクトリに展開する。
pub fn expand_tilde(path: &str) -> PathBuf {
    if path == "~" {
        if let Some(home) = dirs::home_dir() {
            return home;
        }
    }
    if let Some(rest) = path.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(path)
}

/// 初回起動時に自動作成する config.yml のテンプレート。
/// そのままで有効な設定 (接続 0 件) としてパースできる内容にする。
const CONFIG_TEMPLATE: &str = r#"# Queryfolio config file
# See config.example.yaml in the repository for the full format.
# https://github.com/cyberneura/queryfolio

# Connection definitions.
#
# servers:
#   - name: local-sqlite
#     description: "Local SQLite file"
#     engine: sqlite
#     schema: ~/data/example.sqlite3
#   - name: dev-postgres
#     engine: postgres
#     host: localhost
#     port: 5432
#     schema: development_db
#     user: dev_user
#     password: your_password

servers: []

# Keep secrets out of this file by fetching them from elsewhere.
#
# config_override_command runs a command whose stdout must be YAML, and merges
# that YAML over this file. The merge is recursive for mappings; scalars and
# lists (including servers) are replaced wholesale. Any key can be
# overridden this way, not just servers.
#
# config_override_command: op read "op://development/queryfolio/config-yaml"

# Where query files are stored (default: ~/.config/queryfolio/sqlfiles).
# A relative path is resolved against this config directory, not the current
# working directory, so the CLI and a running window always agree on it.
# sqlfiles_dir: ~/queries
#
# Query files live under <sqlfiles_dir>/<folder>/<name>.sql. The per-connection
# folder is <host>_<engine>_<schema>_<user> by default (the connection name is
# not used). Set `folder_name:` on a server to pin the folder explicitly.
"#;

/// 実在する設定ファイルのパスを返す。config.yml / config.yaml のどちらも
/// 無ければ None。
pub fn existing_config_path() -> Result<Option<PathBuf>, AppError> {
    let dir = app_config_dir()?;
    for name in ["config.yml", "config.yaml"] {
        let path = dir.join(name);
        if path.exists() {
            return Ok(Some(path));
        }
    }
    Ok(None)
}

/// config.yml / config.yaml が無ければテンプレートを作成する。
/// 作成した場合は Some(作成パス) を返す。既に存在する場合と、
/// QUERYFOLIO_CONFIG_YAML 環境変数で上書き中の場合は None。
pub fn ensure_config_file() -> Result<Option<String>, AppError> {
    let env_override = std::env::var("QUERYFOLIO_CONFIG_YAML")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false);
    if env_override {
        return Ok(None);
    }
    ensure_config_file_in(&app_config_dir()?)
}

/// dir 内の設定ファイルのパス。config.yml を優先し、無ければ config.yaml、
/// どちらも無ければ config.yml のパスを返す。
fn config_path_in(dir: &std::path::Path) -> PathBuf {
    let yml = dir.join("config.yml");
    if yml.exists() {
        return yml;
    }
    let yaml = dir.join("config.yaml");
    if yaml.exists() {
        return yaml;
    }
    yml
}

fn ensure_config_file_in(dir: &std::path::Path) -> Result<Option<String>, AppError> {
    let yml = dir.join("config.yml");
    let yaml = dir.join("config.yaml");
    if yml.exists() || yaml.exists() {
        // 既存ファイルが緩い権限 (umask 依存の 644 等) で作られていた場合に
        // 所有者のみ (600) へ是正する。config には接続パスワードや SSH 鍵の
        // パスフレーズが平文で入り得るため。是正の主経路は AppConfig::load
        // (起動時の build_menu から走り、フロントの ensure_config_file より
        // 早い) だが、ここでも実施して設定エディタ (read_config_file_in) の
        // 経路や旧バージョン・手動作成のファイルも確実に救済する。
        #[cfg(unix)]
        {
            tighten_config_permissions(&yml)?;
            tighten_config_permissions(&yaml)?;
        }
        return Ok(None);
    }
    std::fs::create_dir_all(dir)?;
    // 作成時からパーミッションを 600 で固定する。std::fs::write だと umask
    // 依存 (通常 644) で作られ、書き込み直後に同一マシンの他ユーザーへ中身を
    // 読まれる隙ができる。create_new (O_EXCL) にすることで、上の exists 判定
    // 後に別プロセスが作った config.yml を truncate してしまう競合も防ぐ。
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::OpenOptionsExt;

        match std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .mode(0o600)
            .open(&yml)
        {
            Ok(mut file) => {
                // mode() は umask で更に絞られるだけだが、異常な umask で所有者
                // ビットが落ちる事態に備え、開いた fd に対して明示的にも設定する。
                use std::os::unix::fs::PermissionsExt;
                file.set_permissions(std::fs::Permissions::from_mode(0o600))?;
                file.write_all(CONFIG_TEMPLATE.as_bytes())?;
                file.sync_all()?;
            }
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                // exists 判定後に別プロセスが作成した。上書きせず権限だけ是正する。
                tighten_config_permissions(&yml)?;
                return Ok(None);
            }
            Err(e) => return Err(e.into()),
        }
    }
    #[cfg(not(unix))]
    std::fs::write(&yml, CONFIG_TEMPLATE)?;
    Ok(Some(yml.display().to_string()))
}

/// 既存の設定ファイルに group / other の許可ビットが立っていたら、
/// 所有者のみ (600) へ絞る。存在しなければ何もしない。macOS では staff
/// グループが全ローカルユーザーで共有されるため、640 でも他ユーザーへ
/// 漏れる。owner-only まで絞るのが安全。
#[cfg(unix)]
fn tighten_config_permissions(path: &std::path::Path) -> Result<(), AppError> {
    use std::os::unix::fs::PermissionsExt;
    let meta = match std::fs::metadata(path) {
        Ok(meta) => meta,
        // 無ければ何もしない (別拡張子や、判定と stat の間に消えた場合)。
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(()),
        // それ以外の I/O エラーは握り潰さず伝播する。黙って return すると
        // 是正できていないのに成功したように見えてしまうため。
        Err(e) => return Err(e.into()),
    };
    // 通常ファイルにのみ適用する。config.yml がディレクトリ (やその symlink)
    // だと 600 にした瞬間に owner の検索ビット (x) が落ちてアクセス不能になり、
    // その後の設定読み込みが失敗する。ファイル以外は触らない。
    if !meta.is_file() {
        return Ok(());
    }
    let mode = meta.permissions().mode() & 0o777;
    if mode & 0o077 != 0 {
        std::fs::set_permissions(path, std::fs::Permissions::from_mode(0o600))?;
    }
    Ok(())
}

/// SSH トンネル設定。sql-agent-mcp-server の config.yaml と互換。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SshTunnelConfig {
    /// SSH host. Not required when `ssh_config` is set (the host is then taken
    /// from the ~/.ssh/config alias by the system ssh client).
    #[serde(default)]
    pub host: String,
    #[serde(default = "default_ssh_port")]
    pub port: u16,
    /// SSH user. Not required when `ssh_config` is set (resolved from
    /// ~/.ssh/config).
    #[serde(default)]
    pub user: String,
    /// queryfolio extension: when set, delegate the tunnel to the system `ssh`
    /// client using this ~/.ssh/config Host alias (`ssh -N -L`). This enables
    /// ProxyJump / multi-hop tunnels and full ssh_config resolution
    /// (HostName / User / Port / ProxyJump). When set, the libssh2 fields
    /// (host / user / password / private_key_* / identity_agent) are ignored;
    /// authentication and host-key checking are handled entirely by OpenSSH.
    #[serde(default)]
    pub ssh_config: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    #[serde(default)]
    pub private_key_path: Option<String>,
    #[serde(default)]
    pub private_key_passphrase: Option<String>,
    /// queryfolio extension: the ssh-agent socket to use for agent
    /// authentication (equivalent to OpenSSH's IdentityAgent). Use "none" to
    /// disable the agent. When omitted, the agent socket is resolved from
    /// ~/.ssh/config (IdentityAgent) and then SSH_AUTH_SOCK. This lets a GUI
    /// launch reach an agent it did not inherit in its environment (e.g. the
    /// 1Password SSH agent).
    #[serde(default)]
    pub identity_agent: Option<String>,
}

fn default_ssh_port() -> u16 {
    22
}

/// 接続先サーバー設定。sql-agent-mcp-server の config.yaml と互換。
/// queryfolio では engine: sqlite を拡張し、schema を DB ファイルパスとして扱う。
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct ServerConfig {
    pub name: String,
    #[serde(default)]
    pub description: Option<String>,
    /// queryfolio 独自拡張: クエリファイルの保存フォルダ名を明示する。
    /// 省略時は <host>_<engine>_<schema>_<user> から組み立てる
    /// (name はフォルダ名には使わない)。sqlfiles_folder_name を参照。
    #[serde(default)]
    pub folder_name: Option<String>,
    pub engine: String,
    #[serde(default)]
    pub host: Option<String>,
    #[serde(default)]
    pub port: Option<u16>,
    #[serde(default)]
    pub schema: Option<String>,
    #[serde(default)]
    pub user: Option<String>,
    #[serde(default)]
    pub password: Option<String>,
    /// queryfolio 独自拡張: 接続時にパスワードを取得するコマンド。
    /// stdout (末尾の改行を除く) をパスワードとして使う。設定の読み込み時や
    /// 接続の選択時には実行せず、DbManager::get_pool が新しいプールを作る時
    /// (と、認証エラーからの復帰時) にだけ実行する。password との併記と、
    /// パスワードを使わないエンジン (sqlite / duckdb) での指定はエラー
    /// (password_command を参照)。
    #[serde(default)]
    pub password_command: Option<String>,
    #[serde(default)]
    pub ssh_tunnel: Option<SshTunnelConfig>,
    /// queryfolio 独自拡張: true の場合、HTTP 系エンジン (elasticsearch) の
    /// 接続に https を使う。省略時 false。
    /// dynamodb ではエンドポイント上書き (host 指定) 時のスキームに使う。
    /// SQL 系エンジン (mysql / postgres) では「TLS を必須にし証明書も検証する」
    /// 指定として扱う (ssl_mode 省略時の既定が verify-full になる)。
    /// redis では TLS 接続 (`rediss://` 相当) にする。証明書は必ず検証する
    /// (engines/redis.rs の connection_addr)。
    #[serde(default)]
    pub tls: bool,
    /// queryfolio 独自拡張: SQL 系エンジン (mysql / postgres) の TLS モード。
    /// disable / prefer / require / verify-ca / verify-full。
    /// 省略時は tls: true なら verify-full、そうでなければ prefer
    /// (sqlx の既定。TLS を試み、張れなければ平文に降格し証明書も検証しない)。
    /// SSH トンネル経由の接続では接続先が 127.0.0.1 になるため、
    /// verify-full は証明書のホスト名検証で失敗する (トンネル自体が暗号化
    /// されているので require までに留めるか省略する)。
    #[serde(default)]
    pub ssl_mode: Option<String>,
    /// queryfolio 独自拡張: 証明書の検証に使うルート CA 証明書 (PEM) のパス。
    /// ~ 展開あり。verify-ca / verify-full で自己署名 CA を使う場合に指定する。
    #[serde(default)]
    pub ssl_root_cert: Option<String>,
    /// queryfolio 独自拡張 (dynamodb 用): aws-config に渡す AWS プロファイル名
    /// (~/.aws/config / credentials)。省略時は既定の credentials chain
    /// (環境変数 → default プロファイル → IMDS)。他のエンジンでは無視される。
    #[serde(default)]
    pub aws_profile: Option<String>,
    /// queryfolio 独自拡張: true の場合、行を返さない文 (INSERT / UPDATE /
    /// DELETE / DDL 等) の実行を拒否する。省略時 false。
    /// SELECT に副作用のある関数 (nextval 等) までは防げない事故防止ガード。
    #[serde(default)]
    pub readonly: bool,
    /// queryfolio 独自拡張: true の場合、危険な文 (WHERE 無しの UPDATE /
    /// DELETE、DROP / TRUNCATE 等) の実行を許可する。省略時 false で、
    /// これらの文は誤操作による全行破壊・テーブル消失を防ぐため拒否される。
    /// true にしても、フロントエンドは実行前に確認を求める。
    #[serde(default)]
    pub allow_dangerous_statements: bool,
    /// queryfolio 独自拡張: 接続一覧での表示グループ名。
    /// servers のグループエントリ (group_name + servers) に
    /// 属するサーバーへ parse_server_entries が設定する。
    /// サーバーエントリ直下の group_name: はグループエントリの検証
    /// (空チェック・未知キー拒否) を迂回するため受け付けない (無視される)。
    #[serde(default, skip_deserializing)]
    pub group_name: Option<String>,
}

/// 文字列の安定した短いハッシュ (FNV-1a 64bit の先頭 8 hex)。
/// AWS アクセスキー ID のような「そのまま出したくないが接続の区別には使いたい」
/// 識別子をフォルダ名に落とすために使う (非可逆・依存クレート不要)。
fn stable_hash_hex(input: &str) -> String {
    let mut hash: u64 = 0xcbf29ce484222325;
    for byte in input.as_bytes() {
        hash ^= u64::from(*byte);
        hash = hash.wrapping_mul(0x100000001b3);
    }
    format!("{:08x}", (hash >> 32) as u32)
}

/// フォルダ名としてファイルシステム上安全になるようサニタイズする。
/// パス区切り (/ \) や NUL を _ に置換し、先頭ドット (不可視/相対) を避ける。
/// query_files::validate_component が拒否する文字を事前に潰しておく。
fn sanitize_folder_component(raw: &str) -> String {
    let mut s: String = raw
        .chars()
        .map(|c| match c {
            '/' | '\\' | '\0' => '_',
            _ => c,
        })
        .collect();
    s = s.trim().to_string();
    if s.is_empty() {
        return "_".to_string();
    }
    if s.starts_with('.') {
        s.insert(0, '_');
    }
    s
}

/// SQL 系エンジン (mysql / postgres) の TLS モード。
/// 名前と意味は libpq / MySQL クライアントの ssl-mode に合わせている。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum SqlSslMode {
    /// TLS を使わない
    Disable,
    /// TLS を試み、張れなければ平文に降格する。証明書は検証しない
    /// (sqlx の既定)
    Prefer,
    /// TLS を必須にする。証明書は検証しない
    /// (経路の盗聴は防げるが、中間者は防げない)。
    /// libpq は ssl_root_cert があれば verify-ca 相当になるが、**sqlx 0.8 は
    /// Require を必ず accept_invalid_certs で扱う** (sqlx-postgres の
    /// connection/tls.rs) ため、ルート CA を渡しても検証されない。
    /// 検証したい場合は verify-ca / verify-full を明示する必要がある
    Require,
    /// TLS を必須にし、サーバー証明書が信頼された CA のものか検証する
    VerifyCa,
    /// VerifyCa に加えて、接続先ホスト名が証明書と一致するか検証する
    VerifyFull,
}

impl SqlSslMode {
    /// 平文へ降格しうるモードか (UI / ログでの注意喚起用)
    pub fn allows_plaintext(self) -> bool {
        matches!(self, SqlSslMode::Disable | SqlSslMode::Prefer)
    }

    /// 設定に書く文字列表現 (ConnectionInfo でフロントへ渡す値でもある)
    pub fn as_str(self) -> &'static str {
        match self {
            SqlSslMode::Disable => "disable",
            SqlSslMode::Prefer => "prefer",
            SqlSslMode::Require => "require",
            SqlSslMode::VerifyCa => "verify-ca",
            SqlSslMode::VerifyFull => "verify-full",
        }
    }
}

impl ServerConfig {
    /// SQL 系エンジンの実効 TLS モードを返す。
    ///
    /// 優先順位は ssl_mode (明示) → tls: true なら verify-full → prefer。
    /// 既定を prefer のままにしているのは後方互換のため (いきなり verify-full に
    /// すると、社内の自己署名証明書などで動いていた接続が壊れる)。
    /// prefer は「TLS が張れなければ平文に降格し、張れても証明書を検証しない」
    /// ため、直接接続では tls: true か ssl_mode の指定を推奨する。
    pub fn sql_ssl_mode(&self) -> Result<SqlSslMode, AppError> {
        let Some(raw) = self.ssl_mode.as_deref() else {
            return Ok(if self.tls {
                SqlSslMode::VerifyFull
            } else {
                SqlSslMode::Prefer
            });
        };

        // 空文字を「未設定」に倒すと、書いたつもりの設定が黙って無視される
        let normalized = raw.trim().to_ascii_lowercase().replace('_', "-");
        match normalized.as_str() {
            "disable" => Ok(SqlSslMode::Disable),
            "prefer" => Ok(SqlSslMode::Prefer),
            "require" => Ok(SqlSslMode::Require),
            "verify-ca" => Ok(SqlSslMode::VerifyCa),
            "verify-full" => Ok(SqlSslMode::VerifyFull),
            other => Err(AppError::Config(format!(
                "Server '{}': unsupported ssl_mode '{}'. \
                 Use one of: disable / prefer / require / verify-ca / verify-full",
                self.name, other
            ))),
        }
    }

    /// ssl_root_cert の設定値を返す (未設定なら None)。
    ///
    /// 検証を行わないモード (disable / prefer / require) と併記されている場合は
    /// エラーにする。sqlx は検証しないモードでルート CA を**黙って無視する**ため、
    /// 「CA を指定したのだから検証されている」という誤解を放置すると、中間者を
    /// 受け入れたまま安全だと思い込むことになる。
    pub fn sql_ssl_root_cert(&self) -> Result<Option<&str>, AppError> {
        let Some(raw) = self.ssl_root_cert.as_deref().map(str::trim) else {
            return Ok(None);
        };
        if raw.is_empty() {
            return Err(AppError::Config(format!(
                "Server '{}': ssl_root_cert is empty",
                self.name
            )));
        }

        let mode = self.sql_ssl_mode()?;
        if !matches!(mode, SqlSslMode::VerifyCa | SqlSslMode::VerifyFull) {
            return Err(AppError::Config(format!(
                "Server '{}': ssl_root_cert is ignored with ssl_mode: {} \
                 (the certificate is not verified). Use ssl_mode: verify-ca or \
                 verify-full to verify it, or remove ssl_root_cert.",
                self.name,
                mode.as_str()
            )));
        }
        Ok(Some(raw))
    }

    /// password_command の設定値を返す (未設定なら None)。
    ///
    /// 次の場合はエラーにする (ssl_root_cert と同じく接続時に検証する。
    /// 設定の読み込み時に弾くと、1 接続の誤りで接続一覧ごと失われるため):
    /// - password と併記されている。どちらかを黙って優先すると、書いた方の
    ///   設定が効いていないことに気付けない (テンプレートの password を
    ///   打ち消したい場合は `password: null` と書く)
    /// - パスワードを使わないエンジン (sqlite / duckdb)
    /// - 空文字 (書いたつもりの設定を「未設定」に倒さない)
    pub fn password_command(&self) -> Result<Option<&str>, AppError> {
        let Some(raw) = self.password_command.as_deref() else {
            return Ok(None);
        };
        let command = raw.trim();
        if command.is_empty() {
            return Err(AppError::Config(format!(
                "Server '{}': password_command is empty",
                self.name
            )));
        }
        if self.password.is_some() {
            return Err(AppError::Config(format!(
                "Server '{}': set either password or password_command, not both \
                 (to drop a password inherited from a template, write `password: null`)",
                self.name
            )));
        }
        if let Ok(crate::db::Engine::Sqlite | crate::db::Engine::DuckDb) =
            crate::db::parse_engine(&self.engine)
        {
            return Err(AppError::Config(format!(
                "Server '{}': password_command cannot be used with {} \
                 (the engine does not use a password)",
                self.name, self.engine
            )));
        }
        Ok(Some(command))
    }

    /// クエリファイルの保存フォルダ名を返す。
    /// folder_name が設定されていればそれを使い、無ければ
    /// <host>_<engine>_<schema>_<user> を組み立てる (name は使わない)。
    /// パス要素として安全になるよう区切り文字等はサニタイズする。
    pub fn sqlfiles_folder_name(&self) -> String {
        if let Some(folder) = self.folder_name.as_deref() {
            let folder = folder.trim();
            if !folder.is_empty() {
                return sanitize_folder_component(folder);
            }
        }
        // dynamodb の user は AWS アクセスキー ID (資格情報の識別子) なので
        // フォルダ名にそのまま出さない。代わりに非機密の識別子
        // (aws_profile 名、静的キーなら短いハッシュ) で接続を区別する —
        // 同一リージョンでプロファイル/キーだけ違う 2 接続が同じフォルダに
        // 落ちてクエリファイルが混ざるのを防ぐ
        let dynamodb_discriminator;
        let user = if self.engine.eq_ignore_ascii_case("dynamodb") {
            // 認証の解決順 (user/password → aws_profile → 既定チェーン) と
            // 同じ優先順で識別子を選ぶ。逆にすると「静的キーが実効・profile は
            // 無視」の 2 接続が同じ profile 名フォルダに落ちて混ざる
            if let Some(user) =
                self.user.as_deref().map(str::trim).filter(|s| !s.is_empty())
            {
                dynamodb_discriminator = format!("key-{}", stable_hash_hex(user));
                &dynamodb_discriminator
            } else if let Some(profile) =
                self.aws_profile.as_deref().map(str::trim).filter(|s| !s.is_empty())
            {
                profile
            } else {
                ""
            }
        } else {
            self.user.as_deref().unwrap_or("")
        };
        let joined = [
            self.host.as_deref().unwrap_or(""),
            self.engine.as_str(),
            self.schema.as_deref().unwrap_or(""),
            user,
        ]
        .join("_");
        sanitize_folder_component(&joined)
    }
}

/// フロントエンドに渡す SSH トンネル情報。パスワードや鍵等の機密は含めない。
#[derive(Debug, Clone, Serialize)]
pub struct SshTunnelInfo {
    pub host: String,
    pub port: u16,
    pub user: String,
    /// queryfolio extension: the ~/.ssh/config Host alias when the tunnel is
    /// delegated to the system ssh client. null in the libssh2 mode (host /
    /// port / user are used instead).
    pub ssh_config: Option<String>,
}

impl From<&SshTunnelConfig> for SshTunnelInfo {
    fn from(tunnel: &SshTunnelConfig) -> Self {
        Self {
            host: tunnel.host.clone(),
            port: tunnel.port,
            user: tunnel.user.clone(),
            ssh_config: tunnel
                .ssh_config
                .as_ref()
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty()),
        }
    }
}

/// フロントエンドに渡す接続先情報。パスワード等の機密は含めない。
#[derive(Debug, Clone, Serialize)]
pub struct ConnectionInfo {
    pub name: String,
    pub description: Option<String>,
    pub engine: String,
    pub has_ssh_tunnel: bool,
    /// 接続先ホスト (未設定なら null)
    pub host: Option<String>,
    /// 接続先ポート (未設定なら null)
    pub port: Option<u16>,
    /// 接続ユーザー (未設定なら null)
    pub user: Option<String>,
    /// 設定上のデフォルト database (スキーマ)
    pub schema: Option<String>,
    /// SSH トンネル情報 (機密を除く)。トンネル未使用なら null
    pub ssh_tunnel: Option<SshTunnelInfo>,
    /// 読み取り専用接続 (書き込み系の文の実行を拒否する)
    pub readonly: bool,
    /// 危険な文 (WHERE 無し UPDATE/DELETE、DROP/TRUNCATE 等) の実行を許可する。
    /// フロントエンドは true の接続でも実行前に確認を求める
    pub allow_dangerous_statements: bool,
    /// 接続一覧での表示グループ名 (グループ未所属なら null)
    pub group_name: Option<String>,
    /// パスワードを password_command で取得する接続か。
    /// コマンドの文字列も出力も渡さない (表示は「コマンド由来」の事実だけ)
    pub password_from_command: bool,
    /// 実効 TLS モード (SqlSslMode の文字列表現)。
    /// mysql / postgres は ssl_mode / tls から解決した値、redis は tls: true なら
    /// verify-full (証明書もホスト名も検証する)、false なら disable。
    /// 他のエンジン、および ssl_mode の値が不正な場合は null。
    /// フロントは「暗号化されない可能性がある直接接続」の表示に使う。
    /// (フィールド名の sql_ 接頭辞は SQL 系専用だった頃の名残)
    pub sql_ssl_mode: Option<String>,
    /// エンジンの能力宣言 (エディタ言語・ファイル拡張子・UI の出し分け)。
    /// フロントはエンジン名ではなくこれで UI を出し分ける。
    pub capabilities: crate::engines::EngineCapabilities,
}

impl From<&ServerConfig> for ConnectionInfo {
    fn from(server: &ServerConfig) -> Self {
        Self {
            name: server.name.clone(),
            description: server.description.clone(),
            engine: server.engine.clone(),
            has_ssh_tunnel: server.ssh_tunnel.is_some(),
            host: server.host.clone(),
            port: server.port,
            user: server.user.clone(),
            schema: server.schema.clone(),
            ssh_tunnel: server.ssh_tunnel.as_ref().map(SshTunnelInfo::from),
            readonly: server.readonly,
            allow_dangerous_statements: server.allow_dangerous_statements,
            group_name: server.group_name.clone(),
            password_from_command: server.password_command.is_some(),
            // エンジン名の別名 (mariadb / postgresql) も拾うため parse_engine を通す。
            // エンジン名や ssl_mode の値が不正な設定は接続時にエラーになるので、
            // ここでは表示を諦めて null にする
            sql_ssl_mode: match crate::db::parse_engine(&server.engine) {
                Ok(crate::db::Engine::MySql) | Ok(crate::db::Engine::Postgres) => server
                    .sql_ssl_mode()
                    .ok()
                    .map(|mode| mode.as_str().to_string()),
                // redis は tls の有無がそのまま TLS / 平文になる (中間のモードが
                // 無い)。平文でも disable として出すのは、TLS を書いたつもりの
                // 接続が平文で繋がっていることに気付ける手段がこれしか無いため
                // (CYBERNEURA-DEV-420)
                Ok(crate::db::Engine::Redis) => Some(
                    if server.tls {
                        SqlSslMode::VerifyFull
                    } else {
                        SqlSslMode::Disable
                    }
                    .as_str()
                    .to_string(),
                ),
                _ => None,
            },
            capabilities: crate::engines::capabilities_for_name(&server.engine),
        }
    }
}

/// 設定を外部コマンドの YAML で上書きするためのトップレベルキー。
/// 値はコマンド文字列で、その stdout (YAML) を設定全体へ再帰マージする。
pub const CONFIG_OVERRIDE_COMMAND_KEY: &str = "config_override_command";

/// フロントエンドの情報表示用。設定の解決結果 (機密を含まない)。
#[derive(Debug, Serialize)]
pub struct ConfigInfo {
    pub config_path: String,
    pub config_exists: bool,
    pub source: String,
    pub sqlfiles_dir: String,
}

/// ~/.config/queryfolio/config.yml (無ければ config.yaml) のパース結果。
///
/// トップレベルキー:
/// - servers: サーバー定義リスト
/// - server_templates: 接続情報の雛形
/// - sqlfiles_dir: クエリファイル保存ディレクトリ (任意)
/// - config_override_command: 設定を上書きする YAML を取得するコマンド (任意)
///
/// `load` はローカルのファイルだけを読む (同期)。`load_merged` は加えて
/// config_override_command を実行し、取得 YAML を再帰マージした設定を返す。
/// コマンド実行は 1Password 等で数秒かかり Touch ID を要求することもあるため、
/// 呼び出し側 (AppState) でセッションキャッシュすること。
pub struct AppConfig {
    doc: serde_yaml::Mapping,
    /// 読み込んだファイルのパス。QUERYFOLIO_CONFIG_YAML 環境変数由来なら None
    source_path: Option<PathBuf>,
    /// load_merged で実際に適用した config_override_command。
    /// マージ後の doc からはキーを落とすため、表示用にここへ退避する。
    applied_override: Option<String>,
}

impl AppConfig {
    /// 設定をロードする。
    /// QUERYFOLIO_CONFIG_YAML 環境変数があればそれを設定ファイルの内容として
    /// 扱う (開発・テスト用オーバーライド)。無ければ config.yml / config.yaml を読む。
    pub fn load() -> Result<Self, AppError> {
        if let Ok(yaml) = std::env::var("QUERYFOLIO_CONFIG_YAML") {
            if !yaml.trim().is_empty() {
                let doc = parse_mapping(&yaml, "env QUERYFOLIO_CONFIG_YAML")?;
                return Ok(Self {
                    doc,
                    source_path: None,
                    applied_override: None,
                });
            }
        }

        let path = Self::find_config_path()?;
        if !path.exists() {
            return Err(AppError::Config(format!(
                "Config file not found. Create {} (see config.example.yaml)",
                path.display()
            )));
        }
        // 読み込む前に、緩い権限で置かれた設定ファイルを所有者のみへ是正する。
        // build_menu からの load はフロントの ensure_config_file より先に走る
        // ため、ここを是正の主経路にする (config は平文の接続パスワードや SSH
        // 鍵パスフレーズを含み得る)。
        #[cfg(unix)]
        tighten_config_permissions(&path)?;
        let text = std::fs::read_to_string(&path)?;
        let doc = parse_mapping(&text, &path.display().to_string())?;
        Ok(Self {
            doc,
            source_path: Some(path),
            applied_override: None,
        })
    }

    /// ローカル設定を読み、`config_override_command` があればそれを実行して
    /// 取得 YAML を再帰マージした設定を返す。
    ///
    /// マージは取得 YAML 側が優先。マッピング同士は再帰的に混ぜ、
    /// スカラー・シーケンス (servers を含む) は丸ごと置き換える
    /// (リストの要素単位マージは、どれが「同じ項目」かを決められないため行わない)。
    pub async fn load_merged() -> Result<Self, AppError> {
        let mut config = Self::load()?;
        let Some(command) = config.override_command()? else {
            return Ok(config);
        };
        let yaml = run_source_command(&command).await?;
        let overrides = parse_mapping(&yaml, &format!("{CONFIG_OVERRIDE_COMMAND_KEY}: {command}"))?;
        merge_mapping(&mut config.doc, &overrides);
        // 取得 YAML 側が config_override_command を持っていても再帰取得はしない。
        // 適用済みであることを表すためキー自体を落とす (info の表示はローカル
        // 側の値を使うため、ここで消しても表示には影響しない)。
        config.doc.remove(CONFIG_OVERRIDE_COMMAND_KEY);
        config.applied_override = Some(command);
        Ok(config)
    }

    /// config.yml を優先し、無ければ config.yaml、どちらも無ければ
    /// デフォルトの config.yml のパスを返す。
    fn find_config_path() -> Result<PathBuf, AppError> {
        Ok(config_path_in(&app_config_dir()?))
    }

    /// LIMIT 未指定の SELECT に自動付与する行数上限。
    /// 省略時は 500。0 を指定すると無効。
    pub fn default_limit(&self) -> u64 {
        self.doc
            .get("default_limit")
            .and_then(|v| v.as_u64())
            .unwrap_or(500)
    }

    /// クエリファイルの保存ディレクトリを解決する。
    ///
    /// 相対パスが書かれていた場合は**カレントディレクトリではなく設定ディレクトリ
    /// (`~/.config/queryfolio`) を基準に解決する**。CLI の `write` は起動した
    /// プロセス自身が書き出す一方、開くのは実行中インスタンス (別プロセス・別 cwd)
    /// なので、cwd 基準だと 2 つのプロセスが違う場所を指してしまう
    /// (書いたファイルが開けず、意図しないディレクトリに残る)。GUI を Finder から
    /// 起動した時の cwd (`/`) も基準として無意味なため、プロセスに依存しない
    /// 基準へ寄せる。
    pub fn resolve_sqlfiles_dir(&self) -> Result<PathBuf, AppError> {
        match self.doc.get("sqlfiles_dir").and_then(|v| v.as_str()) {
            Some(dir) if !dir.trim().is_empty() => {
                let path = expand_tilde(dir);
                if path.is_absolute() {
                    Ok(path)
                } else {
                    Ok(app_config_dir()?.join(path))
                }
            }
            _ => Ok(app_config_dir()?.join("sqlfiles")),
        }
    }

    /// 設定を上書きする YAML を取得するコマンド (未設定なら None)。
    ///
    /// キーが存在するのに文字列でない・空文字の場合はエラーにする。
    /// 黙って「未設定」に倒すと、オーバーライド側の接続情報や readonly が
    /// 適用されないままローカル設定で動いてしまい、事故に気付けないため。
    pub fn override_command(&self) -> Result<Option<String>, AppError> {
        let Some(value) = self.doc.get(CONFIG_OVERRIDE_COMMAND_KEY) else {
            return Ok(None);
        };
        let command = value.as_str().map(str::trim).ok_or_else(|| {
            AppError::Config(format!("{CONFIG_OVERRIDE_COMMAND_KEY} must be a string"))
        })?;
        if command.is_empty() {
            return Err(AppError::Config(format!(
                "{CONFIG_OVERRIDE_COMMAND_KEY} is empty"
            )));
        }
        Ok(Some(command.to_string()))
    }

    /// トップレベルの `ai:` セクション (未検証の生値)。
    /// load_merged 済みなら取得 YAML 側の ai が反映されている。
    pub fn ai(&self) -> Option<serde_yaml::Value> {
        self.doc.get("ai").cloned()
    }

    /// 接続サーバー一覧を解決する。
    /// 取得を伴わない (config_override_command の適用は load_merged で済んでいる)。
    pub fn resolve_servers(&self) -> Result<Vec<ServerConfig>, AppError> {
        let servers = self
            .doc
            .get("servers")
            .ok_or_else(|| AppError::Config("The config has no servers key".into()))?
            .as_sequence()
            .cloned()
            .ok_or_else(|| {
                // 旧方式 (sql_servers: {command|env|file: ...}) からの移行案内。
                // 単に「リストであるべき」とだけ言われても、どう直すか分からない
                // (旧キー名のままの設定は reject_renamed_keys 側で同じ案内を出す)
                AppError::Config(format!(
                    "servers must be a list of server definitions. \
                     Source declarations (command / env / file) were \
                     removed; use the top-level {CONFIG_OVERRIDE_COMMAND_KEY} instead \
                     (note: it runs without a shell, so use an absolute path, e.g. \
                     `{CONFIG_OVERRIDE_COMMAND_KEY}: /bin/cat /Users/you/secrets/servers.yaml`)"
                ))
            })?;
        let templates = self
            .doc
            .get("server_templates")
            .and_then(|v| v.as_sequence())
            .cloned()
            .unwrap_or_default();
        parse_server_entries(&servers, &templates, "config")
    }

    /// 情報表示用のサマリを返す (機密を含まない)。
    pub fn info(&self) -> Result<ConfigInfo, AppError> {
        let config_path = match &self.source_path {
            Some(path) => path.display().to_string(),
            None => "(env QUERYFOLIO_CONFIG_YAML)".to_string(),
        };
        // マージ後は doc からキーを落としているため applied_override を先に見る。
        // load (マージ前) の設定でも表示できるよう doc 側もフォールバックで見る。
        let source = match self
            .applied_override
            .clone()
            .or_else(|| self.override_command().ok().flatten())
        {
            Some(command) => format!("{CONFIG_OVERRIDE_COMMAND_KEY}: {command}"),
            None => "inline".to_string(),
        };
        Ok(ConfigInfo {
            config_path,
            config_exists: true,
            source,
            sqlfiles_dir: self.resolve_sqlfiles_dir()?.display().to_string(),
        })
    }
}

/// 設定の解決に失敗した時の情報表示用サマリ (ファイルが無い / YAML が壊れて
/// いる / 取得コマンドが失敗した場合)。フロントが常に何かを表示できるよう、
/// エラー文言を source に載せて返す。
pub fn config_info_error(error: &AppError) -> ConfigInfo {
    // 失敗には「ファイルが無い」以外に「存在するが YAML が壊れている」場合が
    // あるため、存在判定はパースの成否と独立に行う
    let (config_path, config_exists) = match AppConfig::find_config_path() {
        Ok(path) => (path.display().to_string(), path.exists()),
        Err(_) => (String::new(), false),
    };
    ConfigInfo {
        config_path,
        config_exists,
        source: format!("(error: {error})"),
        sqlfiles_dir: String::new(),
    }
}

/// QUERYFOLIO_CONFIG_YAML 環境変数で設定が上書きされているか。
/// 上書き中は編集対象のファイルが存在しないため、エディタから編集できない。
fn config_env_override() -> bool {
    std::env::var("QUERYFOLIO_CONFIG_YAML")
        .map(|v| !v.trim().is_empty())
        .unwrap_or(false)
}

/// 設定エディタ用に config.yml の中身を読む。
/// ファイルがまだ無い場合はテンプレートを作成してから読む。
pub fn read_config_file() -> Result<String, AppError> {
    if config_env_override() {
        return Err(AppError::Config(
            "The config is overridden by QUERYFOLIO_CONFIG_YAML, so there is no file to edit"
                .into(),
        ));
    }
    read_config_file_in(&app_config_dir()?)
}

fn read_config_file_in(dir: &std::path::Path) -> Result<String, AppError> {
    ensure_config_file_in(dir)?;
    Ok(std::fs::read_to_string(config_path_in(dir))?)
}

/// 設定エディタからの保存。YAML として妥当なことを確認してから書き込む。
///
/// 書き込みは一時ファイル + rename で行い、途中で失敗しても既存の設定を
/// 半端な内容で壊さないようにする。
pub fn write_config_file(content: &str) -> Result<String, AppError> {
    if config_env_override() {
        return Err(AppError::Config(
            "The config is overridden by QUERYFOLIO_CONFIG_YAML, so it cannot be saved".into(),
        ));
    }
    write_config_file_in(&app_config_dir()?, content)
}

fn write_config_file_in(dir: &std::path::Path, content: &str) -> Result<String, AppError> {
    // 壊れた YAML をそのまま保存すると次回起動で接続一覧を失うため、
    // 保存前にマッピングとしてパースできることを確認する
    parse_mapping(content, "the edited config")?;

    std::fs::create_dir_all(dir)?;
    let path = config_path_in(dir);

    // config は接続パスワードや SSH 鍵パスフレーズを平文で含み得るため、
    // 常に所有者のみ (600) で書く。既存が 644/640 等でも 600 へ絞り、
    // ensure_config_file_in / AppConfig::load の是正方針と揃える (既存権限を
    // 引き継ぐと macOS の共有 staff グループ経由で他ユーザーへ漏れ得る)。
    #[cfg(unix)]
    let mode = 0o600;

    let temp = path.with_extension("yml.tmp");
    // 作成時からパーミッションを指定する。書いてから set_permissions すると、
    // その間だけ umask 依存 (通常 644) の権限で中身が置かれ、パスワードを
    // 含む設定を同一マシンの他ユーザーに読まれる隙ができる
    #[cfg(unix)]
    {
        use std::io::Write;
        use std::os::unix::fs::{OpenOptionsExt, PermissionsExt};

        let mut file = std::fs::OpenOptions::new()
            .write(true)
            .create(true)
            .truncate(true)
            .mode(mode)
            .open(&temp)?;
        // mode は新規作成時にしか効かないため、前回の中断等で temp が
        // 残っていた場合に備えて明示的にも設定する
        std::fs::set_permissions(&temp, std::fs::Permissions::from_mode(mode))?;
        file.write_all(content.as_bytes())?;
        file.sync_all()?;
    }
    #[cfg(not(unix))]
    std::fs::write(&temp, content)?;
    std::fs::rename(&temp, &path)?;
    Ok(path.display().to_string())
}

/// config_override_command が設定されているか。
/// メニュー項目の出し分けに使う。設定が読めない場合は false。
pub fn has_config_override_command() -> bool {
    AppConfig::load()
        .map(|c| c.override_command().unwrap_or(None).is_some())
        .unwrap_or(false)
}

/// config_override_command を実行して、取得した生の YAML を返す。
/// コピー用ビュー用 (表示先では編集できるが保存はしない)。未設定ならエラーにする。
///
/// AppState のマージ済み設定キャッシュは**意図的に経由しない**。このビューは
/// 保管場所 (1Password 等) の現在値を確認・整形してコピーする用途なので、
/// 起動時に取得したキャッシュではなく毎回最新を取りに行く。開くたびに
/// コマンドが 1 回走る (1Password なら都度認証が要る場合がある)。
pub async fn fetch_override_config_yaml() -> Result<String, AppError> {
    let config = AppConfig::load()?;
    match config.override_command()? {
        Some(command) => run_source_command(&command).await,
        None => Err(AppError::Config(format!(
            "The config has no {CONFIG_OVERRIDE_COMMAND_KEY}"
        ))),
    }
}

/// 取得 YAML (over) をローカル設定 (base) へ再帰的にマージする。over 側が優先。
/// 値がどちらもマッピングの時だけ中へ入って混ぜ、それ以外 (スカラー・
/// シーケンス) は over で丸ごと置き換える。servers のようなリストを
/// 要素単位でマージしないのは、どの要素が「同じ項目」かを決める安定した
/// 同一性が無いため (name 一致で混ぜると意図しない部分適用が起きる)。
fn merge_mapping(base: &mut serde_yaml::Mapping, over: &serde_yaml::Mapping) {
    for (key, over_value) in over {
        match (base.get_mut(key), over_value) {
            (Some(serde_yaml::Value::Mapping(base_map)), serde_yaml::Value::Mapping(over_map)) => {
                merge_mapping(base_map, over_map);
            }
            _ => {
                base.insert(key.clone(), over_value.clone());
            }
        }
    }
}

fn parse_mapping(yaml_text: &str, source: &str) -> Result<serde_yaml::Mapping, AppError> {
    let doc: serde_yaml::Value = serde_yaml::from_str(yaml_text)
        .map_err(|e| AppError::Config(format!("Failed to parse YAML from {source}: {e}")))?;
    let mapping = doc.as_mapping().cloned().ok_or_else(|| {
        AppError::Config(format!("{source} is not a YAML mapping"))
    })?;
    reject_renamed_keys(&mapping, source)?;
    Ok(mapping)
}

/// 旧キー名 (sql_servers / sql_server_templates) を明示的に拒否する。
/// 黙って無視すると「接続が 1 件も出てこない」だけの状態になり原因が分からない
/// ため、リネームを案内するエラーにする。
fn reject_renamed_keys(doc: &serde_yaml::Mapping, source: &str) -> Result<(), AppError> {
    for (old, new) in [
        ("sql_servers", "servers"),
        ("sql_server_templates", "server_templates"),
    ] {
        let Some(value) = doc.get(old) else {
            continue;
        };
        let mut message = format!("'{old}' in {source} was renamed to '{new}'");
        if old == "sql_servers" {
            message.push_str(" (group entries use 'servers' too)");
            // 旧方式 (sql_servers: {command|env|file: ...}) は改名より前に廃止済み。
            // 改名だけ案内すると「リストに直したのに動かない」で二度詰まるため、
            // 値がマッピングならソース宣言の移行先も同時に伝える
            if value.is_mapping() {
                message.push_str(&format!(
                    ". Source declarations (command / env / file) were also removed; \
                     write a list of server definitions and fetch secrets with the \
                     top-level {CONFIG_OVERRIDE_COMMAND_KEY}"
                ));
            }
        }
        return Err(AppError::Config(message));
    }
    Ok(())
}

/// サーバーエントリ (グループの内外を問わない) に残った旧キーを拒否する。
/// トップレベルの reject_renamed_keys ではネスト位置まで届かないため、
/// parse_server_entries の各エントリでここを通す。
fn reject_renamed_server_key(entry: &serde_yaml::Value, source: &str) -> Result<(), AppError> {
    let has_old_key = entry
        .as_mapping()
        .is_some_and(|m| m.contains_key("sql_servers"));
    if has_old_key {
        return Err(AppError::Config(format!(
            "'sql_servers' in a servers entry in {source} was renamed to 'servers'"
        )));
    }
    Ok(())
}

/// servers のリスト項目をパースする。項目は次のどちらか:
/// - サーバー定義そのもの
/// - グループエントリ (group_name + ネストした servers リスト)。
///   ネストしたサーバーへフラット化し、各サーバーの group_name に記録する。
///   グループの中にさらにグループを書く再帰は禁止 (深さ 1 まで)。
fn parse_server_entries(
    servers: &[serde_yaml::Value],
    templates: &[serde_yaml::Value],
    source: &str,
) -> Result<Vec<ServerConfig>, AppError> {
    let mut result = Vec::new();
    for entry_value in servers {
        let entry = entry_value.as_mapping().ok_or_else(|| {
            AppError::Config(format!("A servers entry in {source} is not a mapping"))
        })?;
        reject_renamed_server_key(entry_value, source)?;
        if !entry.contains_key("servers") {
            result.push(parse_server_entry(entry_value, templates, source)?);
            continue;
        }

        // グループエントリ。typo をサイレントに飲み込まないよう未知キーは拒否する
        for (key, _) in entry {
            let key = key.as_str().unwrap_or_default();
            if key != "group_name" && key != "servers" {
                return Err(AppError::Config(format!(
                    "Unknown key '{key}' in a servers group entry in {source} \
                     (only group_name / servers are allowed)"
                )));
            }
        }
        let group_name = entry
            .get("group_name")
            .and_then(|v| v.as_str())
            .map(str::trim)
            .filter(|s| !s.is_empty())
            .ok_or_else(|| {
                AppError::Config(format!(
                    "A servers group entry in {source} requires a non-empty group_name"
                ))
            })?;
        let grouped = entry
            .get("servers")
            .and_then(|v| v.as_sequence())
            .ok_or_else(|| {
                AppError::Config(format!(
                    "servers in group '{group_name}' in {source} must be a list"
                ))
            })?;
        for server_value in grouped {
            // グループ内のサーバーに残った旧キーもここで拾う。素通りさせると
            // ServerConfig の unknown field として捨てられ、`missing field
            // \`name\`` という無関係なエラーになってしまう
            reject_renamed_server_key(server_value, source)?;
            let is_nested_group = server_value
                .as_mapping()
                .is_some_and(|m| m.contains_key("servers"));
            if is_nested_group {
                return Err(AppError::Config(format!(
                    "Nested groups are not allowed in group '{group_name}' in {source}"
                )));
            }
            let mut server = parse_server_entry(server_value, templates, source)?;
            server.group_name = Some(group_name.to_string());
            result.push(server);
        }
    }
    Ok(result)
}

fn parse_server_entry(
    server_value: &serde_yaml::Value,
    templates: &[serde_yaml::Value],
    source: &str,
) -> Result<ServerConfig, AppError> {
    let expanded = expand_template(server_value, templates)?;
    serde_yaml::from_value(expanded).map_err(|e| {
        AppError::Config(format!(
            "Failed to parse a servers entry in {source}: {e}"
        ))
    })
}

/// config_override_command を実行して stdout を返す。
async fn run_source_command(command: &str) -> Result<String, AppError> {
    let stdout = run_setting_command(
        command,
        CONFIG_OVERRIDE_COMMAND_KEY,
        Duration::from_secs(SOURCE_COMMAND_TIMEOUT_SECS),
    )
    .await?;
    Ok(String::from_utf8_lossy(&stdout).to_string())
}

/// 接続の password_command を実行してパスワードを返す。
///
/// パスワードは stdout から末尾の `\r` / `\n` だけを取り除いたもの。それ以外は
/// 一切触らない (RDS の IAM 認証トークンは `&` `=` `%` を含むため、trim や
/// デコードをすると別の文字列になる)。
///
/// **stdout の中身はエラーメッセージにも出さない** (パスワードそのものなので)。
/// エラーに載せるのはコマンドと終了コード・stderr だけ。
pub async fn run_password_command(server_name: &str, command: &str) -> Result<String, AppError> {
    run_password_command_with_timeout(
        server_name,
        command,
        Duration::from_secs(SOURCE_COMMAND_TIMEOUT_SECS),
    )
    .await
}

async fn run_password_command_with_timeout(
    server_name: &str,
    command: &str,
    timeout: Duration,
) -> Result<String, AppError> {
    let setting = format!("password_command of server '{server_name}'");
    let stdout = run_setting_command(command, &setting, timeout).await?;
    // lossy 変換すると不正なバイトが U+FFFD に化けた「違うパスワード」で
    // 認証を試みることになるので、UTF-8 でなければエラーにする
    let stdout = String::from_utf8(stdout)
        .map_err(|_| AppError::Config(format!("{setting} printed non-UTF-8 output: {command}")))?;
    Ok(stdout.trim_end_matches(['\r', '\n']).to_string())
}

/// 設定に書かれたコマンド (config_override_command / password_command) を
/// 実行して stdout を返す。setting はエラーメッセージで設定項目を名指すラベル。
///
/// shlex で argv に分解し、シェルを介さず実行する。シェルメタ文字が混入しても
/// 解釈されないためコマンドインジェクションの余地が無い。その代わり
/// パイプ・リダイレクト・変数展開は使えない (単一コマンド前提)。
///
/// エラーメッセージにはコマンドと stderr を載せるが、**stdout は載せない**
/// (password_command の stdout はパスワードそのもの)。
async fn run_setting_command(
    command: &str,
    setting: &str,
    timeout: Duration,
) -> Result<Vec<u8>, AppError> {
    let argv = shlex::split(command).ok_or_else(|| {
        AppError::Config(format!(
            "Failed to parse {setting} (unbalanced quotes?): {command}"
        ))
    })?;
    if argv.is_empty() {
        return Err(AppError::Config(format!("{setting} is empty")));
    }

    let output = tokio::time::timeout(
        timeout,
        tokio::process::Command::new(&argv[0])
            .args(&argv[1..])
            // Finder / Dock から起動した GUI の PATH は最小構成 (/usr/bin:/bin 等) で、
            // Homebrew の op / aws 等が見つからないため定番パスを補う
            .env("PATH", supplemented_path())
            // タイムアウトで future が drop された時に子プロセスを残さない
            // (認証待ちでハングした op が遺児化し、リトライで多重起動するのを防ぐ)
            .kill_on_drop(true)
            .output(),
    )
    .await
    .map_err(|_| {
        AppError::Config(format!(
            "{setting} timed out ({}s): {command} \
             (it may be hanging on 1Password or another auth prompt)",
            timeout.as_secs_f64()
        ))
    })?
    .map_err(|e| AppError::Config(format!("Failed to run {setting}: {command}: {e}")))?;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(AppError::Config(format!(
            "{setting} exited with an error (code={:?}): {command}\nstderr: {}",
            output.status.code(),
            stderr.trim()
        )));
    }

    if output.stdout.iter().all(u8::is_ascii_whitespace) {
        return Err(AppError::Config(format!(
            "{setting} produced no output: {command}"
        )));
    }
    Ok(output.stdout)
}

/// PATH に Homebrew 等の定番ディレクトリを補ったものを返す。
fn supplemented_path() -> String {
    supplement_path(&std::env::var("PATH").unwrap_or_default())
}

pub(crate) fn supplement_path(base: &str) -> String {
    // 補うのは Unix の定番ディレクトリで、区切り文字も `:` 前提。Windows の PATH は
    // `;` 区切りなので、そのまま `:` で連結すると PATH の最後の要素が
    // `C:\last\entry:/opt/homebrew/bin` という実在しない 1 要素に化けて消える。
    // Windows で足す意味のあるディレクトリも無いので、素通しする。
    if cfg!(windows) {
        return base.to_string();
    }
    let mut path = base.to_string();
    for extra in ["/opt/homebrew/bin", "/usr/local/bin"] {
        let already = base.split(':').any(|p| p == extra);
        if !already {
            if !path.is_empty() {
                path.push(':');
            }
            path.push_str(extra);
        }
    }
    path
}

/// `template: <名前>` を持つサーバーエントリに、server_templates の
/// 同名テンプレートをシャローマージで継承させる。
/// サーバー側で指定したキーはテンプレートの同名キーを上書きする。
fn expand_template(
    server_value: &serde_yaml::Value,
    templates: &[serde_yaml::Value],
) -> Result<serde_yaml::Value, AppError> {
    let server_map = server_value
        .as_mapping()
        .ok_or_else(|| AppError::Config("A servers entry is not a mapping".into()))?;

    let template_name = match server_map.get("template").and_then(|v| v.as_str()) {
        Some(name) => name.to_string(),
        None => return Ok(server_value.clone()),
    };

    let template = templates
        .iter()
        .filter_map(|t| t.as_mapping())
        .find(|t| {
            t.get("name").and_then(|v| v.as_str()) == Some(template_name.as_str())
        })
        .ok_or_else(|| {
            AppError::Config(format!(
                "Template '{template_name}' not found in server_templates"
            ))
        })?;

    let mut merged = template.clone();
    // テンプレート自身の name はサーバー名ではないので除去する
    merged.remove("name");
    for (key, value) in server_map {
        if key.as_str() == Some("template") {
            continue;
        }
        merged.insert(key.clone(), value.clone());
    }
    Ok(serde_yaml::Value::Mapping(merged))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn config_from_yaml(yaml: &str) -> AppConfig {
        AppConfig {
            doc: parse_mapping(yaml, "test").unwrap(),
            source_path: None,
            applied_override: None,
        }
    }

    #[tokio::test]
    async fn test_inline_servers() {
        let config = config_from_yaml(
            r#"
servers:
  - name: dev-postgres
    description: "dev"
    engine: postgres
    host: localhost
    port: 5432
    schema: dev_db
    user: dev_user
    password: secret
"#,
        );
        let servers = config.resolve_servers().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "dev-postgres");
        assert_eq!(servers[0].port, Some(5432));
        assert!(servers[0].ssh_tunnel.is_none());
        assert!(servers[0].group_name.is_none());
    }

    #[tokio::test]
    async fn test_grouped_servers() {
        // グループエントリはフラット化され、各サーバーに group_name が付く。
        // グループと直書きサーバーの混在も設定順のまま解決される
        let config = config_from_yaml(
            r#"
servers:
  - group_name: production
    servers:
      - name: prod-main
        engine: mysql
        host: prod.example.com
      - name: prod-replica
        engine: mysql
        host: replica.example.com
  - name: standalone
    engine: sqlite
    schema: /tmp/x.db
  - group_name: development
    servers:
      - name: dev-db
        engine: postgres
        host: localhost
"#,
        );
        let servers = config.resolve_servers().unwrap();
        let summary: Vec<(&str, Option<&str>)> = servers
            .iter()
            .map(|s| (s.name.as_str(), s.group_name.as_deref()))
            .collect();
        assert_eq!(
            summary,
            vec![
                ("prod-main", Some("production")),
                ("prod-replica", Some("production")),
                ("standalone", None),
                ("dev-db", Some("development")),
            ]
        );
        // ConnectionInfo にも伝わる
        let info = ConnectionInfo::from(&servers[0]);
        assert_eq!(info.group_name.as_deref(), Some("production"));
    }

    #[tokio::test]
    async fn test_flat_entry_group_name_is_ignored() {
        // サーバーエントリ直下の group_name: はグループエントリの検証を
        // 迂回できてしまうため、デシリアライズしない (無視される)
        let config = config_from_yaml(
            r#"
servers:
  - name: sneaky
    engine: sqlite
    schema: /tmp/x.db
    group_name: bypassed
"#,
        );
        let servers = config.resolve_servers().unwrap();
        assert!(servers[0].group_name.is_none());
    }

    #[tokio::test]
    async fn test_group_requires_non_empty_group_name() {
        let config = config_from_yaml(
            r#"
servers:
  - group_name: ""
    servers:
      - name: a
        engine: sqlite
        schema: /tmp/a.db
"#,
        );
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("group_name"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_group_rejects_nested_group() {
        let config = config_from_yaml(
            r#"
servers:
  - group_name: outer
    servers:
      - group_name: inner
        servers:
          - name: a
            engine: sqlite
            schema: /tmp/a.db
"#,
        );
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("Nested groups"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_group_rejects_unknown_key() {
        // グループエントリの typo (servers: 等) をサイレントに無視しない
        let config = config_from_yaml(
            r#"
servers:
  - group_name: g
    servers: []
    description: typo-extra-key
"#,
        );
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("Unknown key 'description'"), "unexpected error: {err}");
    }

    #[tokio::test]
    async fn test_group_with_template() {
        // グループ内のサーバーでも server_templates を継承できる
        let config = config_from_yaml(
            r#"
servers:
  - group_name: shared
    servers:
      - name: db-a
        template: base
        schema: a_db
server_templates:
  - name: base
    engine: mysql
    host: db.example.com
    port: 3306
    user: shared_user
"#,
        );
        let servers = config.resolve_servers().unwrap();
        assert_eq!(servers[0].name, "db-a");
        assert_eq!(servers[0].engine, "mysql");
        assert_eq!(servers[0].host.as_deref(), Some("db.example.com"));
        assert_eq!(servers[0].schema.as_deref(), Some("a_db"));
        assert_eq!(servers[0].group_name.as_deref(), Some("shared"));
    }

    #[tokio::test]
    async fn test_readonly_flag() {
        // readonly は省略可能 (デフォルト false)。true 指定は ConnectionInfo に伝わる
        let config = config_from_yaml(
            r#"
servers:
  - name: writable-db
    engine: sqlite
    schema: /tmp/x.db
  - name: readonly-db
    engine: sqlite
    schema: /tmp/x.db
    readonly: true
"#,
        );
        let servers = config.resolve_servers().unwrap();
        assert!(!servers[0].readonly);
        assert!(servers[1].readonly);
        assert!(!ConnectionInfo::from(&servers[0]).readonly);
        assert!(ConnectionInfo::from(&servers[1]).readonly);
    }

    #[tokio::test]
    async fn test_connection_info_exposes_host_port_user_and_ssh() {
        // ConnectionInfo は host/port/user と SSH トンネル情報 (機密を除く) を
        // フロントへ渡す。パスワードや鍵は含めない。
        let config = config_from_yaml(
            r#"
servers:
  - name: tunneled-db
    engine: postgres
    host: 10.0.0.5
    port: 5432
    user: app_user
    password: db-secret
    schema: app_db
    ssh_tunnel:
      host: bastion.example.com
      port: 2222
      user: jump
      password: ssh-secret
      private_key_path: /home/me/.ssh/id_ed25519
"#,
        );
        let servers = config.resolve_servers().unwrap();
        let info = ConnectionInfo::from(&servers[0]);
        assert_eq!(info.host.as_deref(), Some("10.0.0.5"));
        assert_eq!(info.port, Some(5432));
        assert_eq!(info.user.as_deref(), Some("app_user"));
        assert!(info.has_ssh_tunnel);
        // 機密がシリアライズに漏れないことを確認する
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("db-secret"));
        assert!(!json.contains("ssh-secret"));
        assert!(!json.contains("id_ed25519"));
        let ssh = info.ssh_tunnel.expect("ssh tunnel info");
        assert_eq!(ssh.host, "bastion.example.com");
        assert_eq!(ssh.port, 2222);
        assert_eq!(ssh.user, "jump");
    }

    #[tokio::test]
    async fn test_inline_with_template() {
        let config = config_from_yaml(
            r#"
servers:
  - template: shared-host
    name: app-db
    schema: app_db
  - template: shared-host
    name: log-db
    schema: log_db
    port: 3307
server_templates:
  - name: shared-host
    engine: mysql
    host: db.example.com
    port: 3306
    user: shared_user
    password: shared_password
"#,
        );
        let servers = config.resolve_servers().unwrap();
        assert_eq!(servers.len(), 2);
        assert_eq!(servers[0].engine, "mysql");
        assert_eq!(servers[0].host.as_deref(), Some("db.example.com"));
        assert_eq!(servers[0].port, Some(3306));
        // サーバー側の指定がテンプレートを上書きする
        assert_eq!(servers[1].port, Some(3307));
    }

    /// 上書き YAML をコマンドで取得して設定へ再帰マージする経路のテスト用に、
    /// ローカル設定 + 取得 YAML から load_merged 相当の処理を組み立てる。
    /// (load_merged 自体は実ファイルを読むため、ここではマージ部分を検証する)
    fn merged_from(local_yaml: &str, fetched_yaml: &str) -> AppConfig {
        let mut config = config_from_yaml(local_yaml);
        let overrides = parse_mapping(fetched_yaml, "test override").unwrap();
        merge_mapping(&mut config.doc, &overrides);
        config.doc.remove(CONFIG_OVERRIDE_COMMAND_KEY);
        config.applied_override = Some("test-command".to_string());
        config
    }

    /// 「渡した YAML を 1 行そのまま吐くだけ」の config_override_command を組み立てる。
    ///
    /// Windows には `/bin/echo` が無いので cmd.exe の echo を使う (この 2 つのテストは
    /// リリースビルドと同じ OS で回る = Windows でも走る)。
    ///
    /// 引数に空白を含めないのが肝。空白があると std が引数を引用符で囲むため、
    /// cmd.exe の echo はその引用符ごと出力してしまい YAML が壊れる。echo は
    /// 受け取った引数を空白で連ねて出すので、shlex に分けさせれば同じ 1 行になる。
    /// そのため呼び出し側は YAML を**二重引用符付きのスカラー**として書く
    /// (`: ` を含む文字列を YAML の平文スカラーには書けないが、引用すれば
    /// バックスラッシュエスケープが要らず、shlex も空白で素直に分割できる)。
    fn echo_command(yaml: &str) -> String {
        if cfg!(windows) {
            format!("cmd /c echo {yaml}")
        } else {
            format!("/bin/echo {yaml}")
        }
    }

    /// load_merged の実経路 (設定読み込み → コマンド実行 → マージ) を通す。
    /// QUERYFOLIO_CONFIG_YAML を使うのでこのプロセスで env を触る唯一のテスト
    /// (他のテストは config_from_yaml を使い env を読まない)。
    #[tokio::test]
    async fn test_load_merged_runs_command_and_merges_result() {
        let command = echo_command("default_limit: 7");
        std::env::set_var(
            "QUERYFOLIO_CONFIG_YAML",
            format!("servers: []\ndefault_limit: 500\nconfig_override_command: \"{command}\"\n"),
        );
        let config = AppConfig::load_merged().await.unwrap();
        std::env::remove_var("QUERYFOLIO_CONFIG_YAML");

        // 取得 YAML の値が適用され、キー自体は落ちている
        assert_eq!(config.default_limit(), 7);
        assert!(config.override_command().unwrap().is_none());
        assert!(config.info().unwrap().source.contains("echo"));
    }

    #[tokio::test]
    async fn test_override_command_is_executed_and_merged() {
        // echo で上書き YAML を出力させ、load_merged と同じ経路を通す。
        // 1 行に収めるためフロースタイルで書く (echo に改行は出せない)
        let yaml = run_source_command(&echo_command(
            "servers: [{name: fetched, engine: sqlite, schema: /tmp/x.db}]",
        ))
        .await
        .unwrap();
        assert!(yaml.contains("fetched"));
    }

    #[test]
    fn test_override_replaces_servers_wholesale() {
        // servers はリストなので要素マージではなく丸ごと置き換わる
        let config = merged_from(
            r#"
servers:
  - name: local-a
    engine: sqlite
    schema: /tmp/a.db
  - name: local-b
    engine: sqlite
    schema: /tmp/b.db
"#,
            r#"
servers:
  - name: fetched-only
    engine: sqlite
    schema: /tmp/c.db
"#,
        );
        let servers = config.resolve_servers().unwrap();
        assert_eq!(servers.len(), 1);
        assert_eq!(servers[0].name, "fetched-only");
    }

    #[test]
    fn test_override_can_set_any_top_level_key() {
        // servers 以外のキーも上書きできる (旧方式との最大の違い)
        let config = merged_from(
            "servers: []\ndefault_limit: 500\nsqlfiles_dir: ~/local\n",
            "default_limit: 42\n",
        );
        assert_eq!(config.default_limit(), 42);
        // 上書き YAML に無いキーはローカルの値が残る
        assert!(config
            .resolve_sqlfiles_dir()
            .unwrap()
            .to_string_lossy()
            .ends_with("local"));
    }

    #[test]
    fn test_override_merges_mappings_recursively() {
        // マッピング同士は再帰的に混ざる (ローカルの model は残り api_key だけ上書き)
        let config = merged_from(
            "servers: []\nai:\n  provider: openai\n  model: local-model\n  api_key: sk-local\n",
            "ai:\n  api_key: sk-fetched\n",
        );
        let ai = config.ai().unwrap();
        assert_eq!(ai.get("api_key").and_then(serde_yaml::Value::as_str), Some("sk-fetched"));
        assert_eq!(ai.get("model").and_then(serde_yaml::Value::as_str), Some("local-model"));
        assert_eq!(ai.get("provider").and_then(serde_yaml::Value::as_str), Some("openai"));
    }

    #[test]
    fn test_override_ai_wins_over_local_ai() {
        // API キーを 1Password 側に置く運用: 取得 YAML の ai が優先される
        let config = merged_from(
            "servers: []\nai:\n  api_key: sk-local\n",
            "ai:\n  api_key: sk-fetched\n",
        );
        let ai = config.ai().unwrap();
        assert_eq!(ai.get("api_key").and_then(serde_yaml::Value::as_str), Some("sk-fetched"));
    }

    #[test]
    fn test_local_ai_survives_without_override_ai() {
        let config = merged_from("servers: []\nai:\n  api_key: sk-local\n", "default_limit: 10\n");
        let ai = config.ai().unwrap();
        assert_eq!(ai.get("api_key").and_then(serde_yaml::Value::as_str), Some("sk-local"));
    }

    #[test]
    fn test_override_key_is_dropped_after_merge() {
        // 取得 YAML 側が config_override_command を持っていても再帰取得はしない
        let config = merged_from(
            "servers: []\nconfig_override_command: local-cmd\n",
            "config_override_command: fetched-cmd\nservers: []\n",
        );
        assert!(config.override_command().unwrap().is_none());
        // 適用済みコマンドは info の表示用に残る
        assert!(config.info().unwrap().source.contains("test-command"));
    }

    #[test]
    fn test_no_override_command_reports_inline() {
        let config = config_from_yaml("servers: []\n");
        assert!(config.override_command().unwrap().is_none());
        assert_eq!(config.info().unwrap().source, "inline");
    }

    #[test]
    fn test_override_command_is_read_from_config() {
        let config = config_from_yaml("servers: []\nconfig_override_command: op read x\n");
        assert_eq!(config.override_command().unwrap().as_deref(), Some("op read x"));
        assert!(config.info().unwrap().source.contains("op read x"));
    }

    #[test]
    fn test_blank_override_command_is_error() {
        // 空文字を黙って「未設定」に倒すと、オーバーライドが効かないまま
        // ローカル設定で動いていることに気付けない
        let config = config_from_yaml("servers: []\nconfig_override_command: \"   \"\n");
        let err = config.override_command().unwrap_err().to_string();
        assert!(err.contains("is empty"));
    }

    #[test]
    fn test_non_string_override_command_is_error() {
        // 旧方式のマッピング形式を書いてしまった場合も含め、型誤りは黙認しない
        for yaml in [
            "servers: []\nconfig_override_command: 123\n",
            "servers: []\nconfig_override_command:\n  command: op read x\n",
        ] {
            let config = config_from_yaml(yaml);
            let err = config.override_command().unwrap_err().to_string();
            assert!(err.contains("must be a string"), "unexpected error: {err}");
        }
    }

    #[test]
    fn test_old_servers_source_declaration_explains_migration() {
        // 旧方式の設定のまま上げたユーザーに移行先を伝える
        let config = config_from_yaml("servers:\n  file: ~/secrets/servers.yaml\n");
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("config_override_command"), "unexpected error: {err}");
    }

    #[test]
    fn test_servers_mapping_is_rejected() {
        // 旧方式のソース宣言をキーだけ改名して書いてもサポートしない
        let config = config_from_yaml("servers:\n  command: op read x\n");
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("must be a list"));
    }

    #[test]
    fn test_renamed_keys_are_rejected_with_guidance() {
        // sql_servers / sql_server_templates は servers / server_templates へ改名済み。
        // 黙って無視すると接続 0 件で原因が分からないためエラーにする
        let err = parse_mapping("sql_servers: []\n", "test").unwrap_err().to_string();
        assert!(err.contains("renamed to 'servers'"), "unexpected error: {err}");

        let err = parse_mapping("servers: []\nsql_server_templates: []\n", "test")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("renamed to 'server_templates'"),
            "unexpected error: {err}"
        );
        // テンプレートはグループエントリに書けないので、その注記は付けない
        assert!(!err.contains("group entries"), "unexpected error: {err}");
    }

    #[test]
    fn test_old_source_declaration_under_old_key_explains_migration() {
        // 旧キー + 旧方式のソース宣言。改名だけ案内すると「リストに直したのに
        // 動かない」で二度詰まるため、移行先も同時に伝える
        let err = parse_mapping("sql_servers:\n  file: ~/secrets/servers.yaml\n", "test")
            .unwrap_err()
            .to_string();
        assert!(err.contains("renamed to 'servers'"), "unexpected error: {err}");
        assert!(
            err.contains(CONFIG_OVERRIDE_COMMAND_KEY),
            "unexpected error: {err}"
        );
    }

    #[test]
    fn test_renamed_key_in_group_entry_is_rejected() {
        let config = config_from_yaml(
            "\
servers:
  - group_name: Production
    sql_servers:
      - name: a
        engine: sqlite
        schema: /tmp/a.db
",
        );
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("renamed to 'servers'"), "unexpected error: {err}");
    }

    #[test]
    fn test_renamed_key_inside_a_group_is_rejected() {
        // グループの中のサーバーに残った旧キー。ServerConfig は unknown field を
        // 黙って捨てるため、拒否しないと無関係なエラーになる
        let config = config_from_yaml(
            "\
servers:
  - group_name: Production
    servers:
      - group_name: Nested
        sql_servers:
          - name: a
            engine: sqlite
            schema: /tmp/a.db
",
        );
        let err = config.resolve_servers().unwrap_err().to_string();
        assert!(err.contains("renamed to 'servers'"), "unexpected error: {err}");
    }

    #[test]
    fn test_default_limit() {
        let config = config_from_yaml("servers: []\n");
        assert_eq!(config.default_limit(), 500);
        let config = config_from_yaml("servers: []\ndefault_limit: 100\n");
        assert_eq!(config.default_limit(), 100);
        let config = config_from_yaml("servers: []\ndefault_limit: 0\n");
        assert_eq!(config.default_limit(), 0);
    }

    #[test]
    fn test_sqlfiles_dir_default_and_custom() {
        let config = config_from_yaml("servers: []\n");
        let default_dir = config.resolve_sqlfiles_dir().unwrap();
        assert!(default_dir.ends_with(".config/queryfolio/sqlfiles"));

        let config = config_from_yaml("servers: []\nsqlfiles_dir: ~/my-queries\n");
        let custom = config.resolve_sqlfiles_dir().unwrap();
        assert_eq!(custom, dirs::home_dir().unwrap().join("my-queries"));

        // 相対パスは cwd ではなく設定ディレクトリ基準 (プロセスに依存しない)。
        // CLI (書き出す側) と実行中インスタンス (開く側) は cwd が違うため。
        let config = config_from_yaml("servers: []\nsqlfiles_dir: my-queries\n");
        let relative = config.resolve_sqlfiles_dir().unwrap();
        assert_eq!(relative, app_config_dir().unwrap().join("my-queries"));

        let config = config_from_yaml("servers: []\nsqlfiles_dir: ./a/../b\n");
        let relative = config.resolve_sqlfiles_dir().unwrap();
        assert_eq!(relative, app_config_dir().unwrap().join("./a/../b"));
    }

    #[test]
    fn test_ensure_config_file_in() {
        let dir = std::env::temp_dir().join(format!(
            "queryfolio-ensure-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // 無ければ作成して Some(パス) を返す
        let created = ensure_config_file_in(&dir).unwrap();
        assert!(created.is_some());
        assert!(dir.join("config.yml").exists());

        // 新規作成は 600 (umask 依存の 644 で作らない)
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.join("config.yml"))
                .unwrap()
                .permissions()
                .mode()
                & 0o777;
            assert_eq!(mode, 0o600);
        }

        // 既に存在すれば None (上書きしない)
        assert!(ensure_config_file_in(&dir).unwrap().is_none());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 既存の config.yml が緩い権限 (644) で置かれていたら、起動時の
    /// ensure_config_file_in が 600 へ是正する (中身は変えない)。
    #[cfg(unix)]
    #[test]
    fn test_ensure_config_file_in_tightens_existing_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "queryfolio-ensure-perm-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();

        // 他ユーザーから読める 644 で手動作成された既存ファイルを模す
        let path = dir.join("config.yml");
        std::fs::write(&path, "servers: []\n").unwrap();
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o644)).unwrap();

        // 既存なので None を返しつつ、権限は 600 へ是正される
        assert!(ensure_config_file_in(&dir).unwrap().is_none());
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
        // 中身は書き換えない
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "servers: []\n");

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// config.yml がディレクトリの場合、tighten はパーミッションを変えない
    /// (600 にすると検索ビットが落ちてアクセス不能になるため触らない)。
    #[cfg(unix)]
    #[test]
    fn test_tighten_config_permissions_skips_directory() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "queryfolio-tighten-dir-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        // config.yml という名前のディレクトリ (異常状態) を作る
        let as_dir = dir.join("config.yml");
        std::fs::create_dir_all(&as_dir).unwrap();
        std::fs::set_permissions(&as_dir, std::fs::Permissions::from_mode(0o755)).unwrap();

        tighten_config_permissions(&as_dir).unwrap();

        // ディレクトリの権限は変えない (600 にしない)
        let mode = std::fs::metadata(&as_dir).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o755);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 設定エディタの読み書き。無ければテンプレートを作ってから読み、
    /// 保存した内容がそのまま読み戻せる。
    #[test]
    fn test_read_write_config_file_in() {
        let dir = std::env::temp_dir().join(format!(
            "queryfolio-editor-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // ファイルが無い状態でもテンプレートが作られて読める
        let initial = read_config_file_in(&dir).unwrap();
        assert!(initial.contains("servers"));

        let edited = "servers:\n  - name: edited\n    engine: sqlite\n    schema: /tmp/a.db\n";
        let saved_path = write_config_file_in(&dir, edited).unwrap();
        assert_eq!(saved_path, dir.join("config.yml").display().to_string());
        assert_eq!(read_config_file_in(&dir).unwrap(), edited);
        // 一時ファイルを残さない
        assert!(!dir.join("config.yml.tmp").exists());

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 壊れた YAML は保存を拒否し、既存の設定を残す。
    #[test]
    fn test_write_config_file_in_rejects_invalid_yaml() {
        let dir = std::env::temp_dir().join(format!(
            "queryfolio-editor-invalid-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        let valid = "servers: []\n";
        write_config_file_in(&dir, valid).unwrap();

        // マッピングとしてパースできない内容
        assert!(write_config_file_in(&dir, "servers: [\n").is_err());
        // YAML ではあるがマッピングではない
        assert!(write_config_file_in(&dir, "- just\n- a list\n").is_err());
        // 既存の内容は壊れていない
        assert_eq!(read_config_file_in(&dir).unwrap(), valid);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// 保存時は常に 600 で書く (新規も、緩い既存権限の是正も)。
    #[cfg(unix)]
    #[test]
    fn test_write_config_file_in_permissions() {
        use std::os::unix::fs::PermissionsExt;

        let dir = std::env::temp_dir().join(format!(
            "queryfolio-editor-perm-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);

        // 新規作成は 600
        write_config_file_in(&dir, "servers: []\n").unwrap();
        let path = dir.join("config.yml");
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        // 既存が緩い権限 (640) でも、保存時に 600 へ絞る
        std::fs::set_permissions(&path, std::fs::Permissions::from_mode(0o640)).unwrap();
        write_config_file_in(&dir, "servers: []\n# edited\n").unwrap();
        let mode = std::fs::metadata(&path).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);

        let _ = std::fs::remove_dir_all(&dir);
    }

    /// config.yaml (拡張子 yaml) を使っている場合も、そのファイルへ保存する。
    #[test]
    fn test_write_config_file_in_keeps_yaml_extension() {
        let dir = std::env::temp_dir().join(format!(
            "queryfolio-editor-yaml-ext-test-{}",
            std::process::id()
        ));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("config.yaml"), "servers: []\n").unwrap();

        let edited = "servers: []\n# edited\n";
        let saved_path = write_config_file_in(&dir, edited).unwrap();
        assert_eq!(saved_path, dir.join("config.yaml").display().to_string());
        assert!(!dir.join("config.yml").exists());
        assert_eq!(
            std::fs::read_to_string(dir.join("config.yaml")).unwrap(),
            edited
        );

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[tokio::test]
    async fn test_config_template_is_valid() {
        // テンプレートはそのままで有効な設定 (接続 0 件) としてパースできること
        let config = config_from_yaml(CONFIG_TEMPLATE);
        let servers = config.resolve_servers().unwrap();
        assert!(servers.is_empty());
        config.resolve_sqlfiles_dir().unwrap();
    }

    #[test]
    fn test_expand_tilde() {
        let home = dirs::home_dir().unwrap();
        assert_eq!(expand_tilde("~/foo/bar"), home.join("foo/bar"));
        assert_eq!(expand_tilde("~"), home);
        assert_eq!(expand_tilde("/abs/path"), PathBuf::from("/abs/path"));
    }

    #[test]
    #[cfg(unix)]
    fn test_supplement_path() {
        // 無ければ追加される
        let path = supplement_path("/usr/bin:/bin");
        assert!(path.split(':').any(|p| p == "/opt/homebrew/bin"));
        assert!(path.split(':').any(|p| p == "/usr/local/bin"));
        // 既にあれば重複追加しない
        let path = supplement_path("/opt/homebrew/bin:/usr/bin");
        let count = path.split(':').filter(|p| *p == "/opt/homebrew/bin").count();
        assert_eq!(count, 1);
    }

    #[test]
    #[cfg(windows)]
    fn test_supplement_path_windows_passthrough() {
        // Windows の PATH は `;` 区切りなので何も足さずに素通しする。
        // 特に最後の要素が壊れないこと (`C:\tools:/opt/homebrew/bin` にならない) を見る。
        let base = r"C:\Windows\system32;C:\tools";
        assert_eq!(supplement_path(base), base);
        // 空の PATH でも空のまま返る (`:/opt/homebrew/bin` のような値を作らない)。
        assert_eq!(supplement_path(""), "");
    }

    #[test]
    fn test_connection_info_hides_password() {
        let server = ServerConfig {
            name: "s".into(),
            description: None,
            folder_name: None,
            engine: "mysql".into(),
            host: Some("h".into()),
            port: Some(3306),
            schema: Some("db".into()),
            user: Some("u".into()),
            password: Some("secret".into()),
            password_command: None,
            ssh_tunnel: None,
            tls: false,
            ssl_mode: None,
            ssl_root_cert: None,
            aws_profile: None,
            readonly: false,
            allow_dangerous_statements: false,
            group_name: None,
        };
        let info = ConnectionInfo::from(&server);
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("secret"));
    }

    /// servers の 1 エントリ目を解決する (テンプレート展開込み)
    fn first_server(yaml: &str) -> ServerConfig {
        config_from_yaml(yaml)
            .resolve_servers()
            .unwrap()
            .into_iter()
            .next()
            .unwrap()
    }

    #[test]
    fn test_password_command_is_read() {
        let server = first_server(
            "servers:\n  - name: iam\n    engine: postgres\n    host: h\n    user: u\n    \
             password_command: aws rds generate-db-auth-token --hostname h\n",
        );
        assert_eq!(
            server.password_command().unwrap(),
            Some("aws rds generate-db-auth-token --hostname h")
        );
        assert!(server.password.is_none());
        // 未設定なら None (従来どおり password を使う)
        let plain = first_server(
            "servers:\n  - name: p\n    engine: postgres\n    password: secret\n",
        );
        assert_eq!(plain.password_command().unwrap(), None);
    }

    #[test]
    fn test_password_and_password_command_together_is_error() {
        let server = first_server(
            "servers:\n  - name: both\n    engine: mysql\n    password: secret\n    \
             password_command: /bin/echo x\n",
        );
        let err = server.password_command().unwrap_err().to_string();
        assert!(err.contains("either password or password_command"), "{err}");
        assert!(err.contains("'both'"), "{err}");
    }

    #[test]
    fn test_password_command_is_rejected_for_file_engines() {
        for engine in ["sqlite", "sqlite3", "duckdb"] {
            let server = first_server(&format!(
                "servers:\n  - name: f\n    engine: {engine}\n    schema: /tmp/x.db\n    \
                 password_command: /bin/echo x\n"
            ));
            let err = server.password_command().unwrap_err().to_string();
            assert!(err.contains("cannot be used with"), "{engine}: {err}");
        }
        // パスワードを使うエンジンでは受け付ける
        for engine in ["mysql", "postgres", "redis", "elasticsearch", "dynamodb"] {
            let server = first_server(&format!(
                "servers:\n  - name: n\n    engine: {engine}\n    \
                 password_command: /bin/echo x\n"
            ));
            assert!(server.password_command().unwrap().is_some(), "{engine}");
        }
    }

    #[test]
    fn test_blank_password_command_is_error() {
        let server = first_server(
            "servers:\n  - name: b\n    engine: postgres\n    password_command: \"  \"\n",
        );
        assert!(server.password_command().unwrap_err().to_string().contains("is empty"));
    }

    #[test]
    fn test_password_command_is_inherited_from_template() {
        let yaml = "servers:\n  - name: qa\n    template: iam\n    host: qa-db\n\
                    server_templates:\n  - name: iam\n    engine: postgres\n    user: iam_ro\n    \
                    password_command: /usr/local/bin/token --user iam_ro\n";
        let server = first_server(yaml);
        assert_eq!(server.name, "qa");
        assert_eq!(
            server.password_command().unwrap(),
            Some("/usr/local/bin/token --user iam_ro")
        );

        // テンプレートの password とサーバー側の password_command は併記扱い
        // (どちらかを黙って優先しない)。password: null で打ち消せる
        let base = "server_templates:\n  - name: t\n    engine: postgres\n    password: shared\n";
        let both = first_server(&format!(
            "servers:\n  - name: s\n    template: t\n    password_command: /bin/echo x\n{base}"
        ));
        assert!(both.password_command().is_err());
        let overridden = first_server(&format!(
            "servers:\n  - name: s\n    template: t\n    password: null\n    \
             password_command: /bin/echo x\n{base}"
        ));
        assert_eq!(overridden.password_command().unwrap(), Some("/bin/echo x"));
    }

    #[test]
    fn test_connection_info_marks_password_command_without_the_command() {
        let server = first_server(
            "servers:\n  - name: iam\n    engine: postgres\n    \
             password_command: /opt/secret-fetcher --token-for prod\n",
        );
        let info = ConnectionInfo::from(&server);
        assert!(info.password_from_command);
        let json = serde_json::to_string(&info).unwrap();
        assert!(!json.contains("secret-fetcher"), "{json}");

        let plain = first_server("servers:\n  - name: p\n    engine: postgres\n");
        assert!(!ConnectionInfo::from(&plain).password_from_command);
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_password_command_trims_only_trailing_newlines() {
        // RDS の IAM トークンのように & = % を含む値。前後の空白や途中の
        // 文字はそのまま残し、末尾の \r\n だけを落とす
        let password = run_password_command(
            "iam",
            r"/usr/bin/printf ' tok&en=a%%2Fb?X-Amz-Signature=ab== \r\n\n'",
        )
        .await
        .unwrap();
        assert_eq!(password, " tok&en=a%2Fb?X-Amz-Signature=ab== ");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_password_command_empty_output_is_error() {
        for command in ["/usr/bin/true", r"/usr/bin/printf '\n'"] {
            let err = run_password_command("iam", command).await.unwrap_err().to_string();
            assert!(err.contains("produced no output"), "{command}: {err}");
            assert!(err.contains("password_command of server 'iam'"), "{err}");
        }
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_password_command_failure_shows_stderr_but_not_stdout() {
        // stdout は "hunter2"。コマンド自体はエラーに載ってよいので、コマンドの
        // 文字列には "hunter2" が現れないように組み立てる
        let err = run_password_command(
            "iam",
            "/bin/sh -c 'printf %s%s hunter 2; echo token expired >&2; exit 3'",
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("password_command of server 'iam'"), "{err}");
        assert!(err.contains("code=Some(3)"), "{err}");
        assert!(err.contains("token expired"), "{err}");
        assert!(!err.contains("hunter2"), "{err}");
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_password_command_timeout() {
        let started = std::time::Instant::now();
        let err = run_password_command_with_timeout(
            "iam",
            "/bin/sleep 10",
            Duration::from_millis(200),
        )
        .await
        .unwrap_err()
        .to_string();
        assert!(err.contains("timed out (0.2s)"), "{err}");
        assert!(started.elapsed() < Duration::from_secs(5));
    }

    #[cfg(unix)]
    #[tokio::test]
    async fn test_password_command_rejects_non_utf8_output() {
        let err = run_password_command("iam", r"/usr/bin/printf '\377\376'")
            .await
            .unwrap_err()
            .to_string();
        assert!(err.contains("non-UTF-8"), "{err}");
    }

    fn server_with(
        folder_name: Option<&str>,
        host: Option<&str>,
        engine: &str,
        schema: Option<&str>,
        user: Option<&str>,
    ) -> ServerConfig {
        ServerConfig {
            name: "conn-name".into(),
            description: None,
            folder_name: folder_name.map(|s| s.to_string()),
            engine: engine.into(),
            host: host.map(|s| s.to_string()),
            port: None,
            schema: schema.map(|s| s.to_string()),
            user: user.map(|s| s.to_string()),
            password: None,
            password_command: None,
            ssh_tunnel: None,
            tls: false,
            ssl_mode: None,
            ssl_root_cert: None,
            aws_profile: None,
            readonly: false,
            allow_dangerous_statements: false,
            group_name: None,
        }
    }

    /// TLS の実効モードは「明示 ssl_mode → tls: true なら verify-full → prefer」。
    /// 既定が prefer (平文へ降格しうる) であることは後方互換の意図的な選択なので、
    /// 変更に気付けるようテストで固定しておく。
    #[test]
    fn test_sql_ssl_mode() {
        let mut s = server_with(None, Some("h"), "postgres", Some("db"), Some("u"));

        // 何も指定しなければ sqlx 既定と同じ prefer
        assert_eq!(s.sql_ssl_mode().unwrap(), SqlSslMode::Prefer);
        assert!(s.sql_ssl_mode().unwrap().allows_plaintext());

        // tls: true は verify-full 相当
        s.tls = true;
        assert_eq!(s.sql_ssl_mode().unwrap(), SqlSslMode::VerifyFull);
        assert!(!s.sql_ssl_mode().unwrap().allows_plaintext());

        // ssl_mode は tls より優先する
        s.ssl_mode = Some("require".into());
        assert_eq!(s.sql_ssl_mode().unwrap(), SqlSslMode::Require);

        // 大文字・アンダースコア・前後の空白を許容する
        s.ssl_mode = Some("  VERIFY_CA ".into());
        assert_eq!(s.sql_ssl_mode().unwrap(), SqlSslMode::VerifyCa);

        s.ssl_mode = Some("disable".into());
        assert_eq!(s.sql_ssl_mode().unwrap(), SqlSslMode::Disable);
        assert!(s.sql_ssl_mode().unwrap().allows_plaintext());

        // 未知の値は黙って既定に倒さずエラーにする
        s.ssl_mode = Some("verify".into());
        assert!(s.sql_ssl_mode().is_err());
        s.ssl_mode = Some("".into());
        assert!(s.sql_ssl_mode().is_err());
    }

    /// ssl_root_cert は検証を行うモードでしか意味を持たない。
    /// sqlx は検証しないモードでルート CA を黙って無視するため、
    /// 併記された設定は「検証されている」という誤解を生む。エラーで気付かせる。
    #[test]
    fn test_sql_ssl_root_cert_requires_verifying_mode() {
        let mut s = server_with(None, Some("h"), "postgres", Some("db"), Some("u"));
        s.ssl_root_cert = Some("~/certs/ca.pem".into());

        // 検証しないモードとの併記はエラー
        assert!(s.sql_ssl_root_cert().is_err()); // 既定 = prefer
        s.ssl_mode = Some("require".into());
        assert!(s.sql_ssl_root_cert().is_err());
        s.ssl_mode = Some("disable".into());
        assert!(s.sql_ssl_root_cert().is_err());

        // 検証するモードなら通る
        s.ssl_mode = Some("verify-ca".into());
        assert_eq!(s.sql_ssl_root_cert().unwrap(), Some("~/certs/ca.pem"));
        s.ssl_mode = None;
        s.tls = true; // = verify-full
        assert_eq!(s.sql_ssl_root_cert().unwrap(), Some("~/certs/ca.pem"));

        // 空文字はエラー、未設定は None
        s.ssl_root_cert = Some("  ".into());
        assert!(s.sql_ssl_root_cert().is_err());
        s.ssl_root_cert = None;
        assert_eq!(s.sql_ssl_root_cert().unwrap(), None);
    }

    /// ConnectionInfo の sql_ssl_mode は SQL 系エンジンと redis に載る
    /// (フロントは接続の詳細ツールチップにこの値を出す)。
    #[test]
    fn test_connection_info_sql_ssl_mode() {
        let s = server_with(None, Some("h"), "postgres", Some("db"), Some("u"));
        assert_eq!(
            ConnectionInfo::from(&s).sql_ssl_mode.as_deref(),
            Some("prefer")
        );

        // エンジン名の別名も拾う
        let mut s = server_with(None, Some("h"), "mariadb", Some("db"), Some("u"));
        s.tls = true;
        assert_eq!(
            ConnectionInfo::from(&s).sql_ssl_mode.as_deref(),
            Some("verify-full")
        );

        // TLS モードを持たないエンジンは null
        let s = server_with(None, None, "sqlite", Some("/tmp/x.sqlite3"), None);
        assert!(ConnectionInfo::from(&s).sql_ssl_mode.is_none());

        // 不正な ssl_mode は表示を諦めて null (接続時にエラーになる)
        let mut s = server_with(None, Some("h"), "postgres", Some("db"), Some("u"));
        s.ssl_mode = Some("bogus".into());
        assert!(ConnectionInfo::from(&s).sql_ssl_mode.is_none());

        // redis は tls の有無をそのまま出す。平文でも disable として出すことで、
        // tls を書いたつもりの接続が平文で繋がっていることに気付ける
        // (CYBERNEURA-DEV-420)
        let s = server_with(None, Some("h"), "redis", Some("0"), None);
        assert_eq!(
            ConnectionInfo::from(&s).sql_ssl_mode.as_deref(),
            Some("disable")
        );

        let mut s = server_with(None, Some("h"), "valkey", Some("0"), None);
        s.tls = true;
        assert_eq!(
            ConnectionInfo::from(&s).sql_ssl_mode.as_deref(),
            Some("verify-full")
        );
    }

    #[test]
    fn test_sqlfiles_folder_name() {
        // folder_name があればそれを使う (name は使わない)
        let s = server_with(Some("my-folder"), Some("h"), "mysql", Some("db"), Some("u"));
        assert_eq!(s.sqlfiles_folder_name(), "my-folder");

        // folder_name が空文字列ならフォールバック
        let s = server_with(Some("   "), Some("h"), "mysql", Some("db"), Some("u"));
        assert_eq!(s.sqlfiles_folder_name(), "h_mysql_db_u");

        // folder_name 無し → <host>_<engine>_<schema>_<user>
        let s = server_with(
            None,
            Some("db.example.com"),
            "postgres",
            Some("prod"),
            Some("app"),
        );
        assert_eq!(s.sqlfiles_folder_name(), "db.example.com_postgres_prod_app");

        // sqlite: host/user 無し、schema はファイルパス → 区切りをサニタイズ
        let s = server_with(None, None, "sqlite", Some("/Users/me/data.db"), None);
        assert_eq!(s.sqlfiles_folder_name(), "_sqlite__Users_me_data.db_");

        // 先頭ドットは避ける (不可視/相対パス化を防ぐ)
        let s = server_with(Some(".hidden"), None, "sqlite", None, None);
        assert_eq!(s.sqlfiles_folder_name(), "_.hidden");
    }

    #[test]
    fn test_sqlfiles_folder_name_dynamodb_discriminator() {
        let mut server = ServerConfig {
            name: "ddb".into(),
            description: None,
            folder_name: None,
            engine: "dynamodb".into(),
            host: None,
            port: None,
            schema: Some("ap-northeast-1".into()),
            user: Some("AKIAEXAMPLEKEYID".into()),
            password: Some("secret".into()),
            password_command: None,
            ssh_tunnel: None,
            readonly: false,
            allow_dangerous_statements: false,
            group_name: None,
            tls: false,
            ssl_mode: None,
            ssl_root_cert: None,
            aws_profile: None,
        };
        // アクセスキー ID はフォルダ名に出さず、短いハッシュで区別する
        let folder = server.sqlfiles_folder_name();
        assert!(!folder.contains("AKIAEXAMPLEKEYID"), "{folder}");
        assert!(folder.contains("key-"), "{folder}");
        // 別のキーなら別のフォルダになる
        let mut other = server.clone();
        other.user = Some("AKIAOTHERKEYID".into());
        assert_ne!(folder, other.sqlfiles_folder_name());
        // aws_profile があればプロファイル名 (非機密) を使う
        server.user = None;
        server.password = None;
        server.aws_profile = Some("myprofile".into());
        let folder = server.sqlfiles_folder_name();
        assert!(folder.contains("myprofile"), "{folder}");
        // 両方ある時は認証の優先順に合わせて静的キー側 (ハッシュ) を使う
        server.user = Some("AKIAEXAMPLEKEYID".into());
        server.password = Some("secret".into());
        let folder = server.sqlfiles_folder_name();
        assert!(folder.contains("key-"), "{folder}");
        assert!(!folder.contains("myprofile"), "{folder}");
        // 同一ハッシュの安定性
        assert_eq!(stable_hash_hex("abc"), stable_hash_hex("abc"));
        assert_ne!(stable_hash_hex("abc"), stable_hash_hex("abd"));
    }
}
