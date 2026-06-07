//! GitHub/Gitea browser-login private-repository sync.
//!
//! The repository stores one encrypted snapshot file. The sync password is
//! never written to that repository; local OAuth tokens and the password are
//! persisted through the OS credential store so automatic sync can survive an
//! app restart.

use std::fs;
use std::path::{Path, PathBuf};
use std::time::Duration;

use anyhow::{anyhow, bail, Context, Result};
use argon2::Argon2;
use base64::engine::general_purpose::{STANDARD, URL_SAFE_NO_PAD};
use base64::Engine as _;
use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use chrono::Utc;
use rand::rngs::OsRng;
use rand::RngCore;
use reqwest::{Client, StatusCode};
use serde::{Deserialize, Serialize};
use sha2::{Digest, Sha256};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::time::timeout;
use url::Url;

use crate::config::{CloudProvider, CloudSyncConfig, ConfigFile, ConfigStore, Session};

const SYNC_FILE: &str = "meatshell-sync.enc";
const KEYRING_SERVICE: &str = "meatshell";
const GITEA_TEA_CLIENT_ID: &str = "d57cb8c4-630c-4168-8324-ec79935e18d4";
const GITHUB_CLI_CLIENT_ID: &str = "178c6fc778ccc68e1d6a";

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncSnapshot {
    pub version: u32,
    pub updated_at: i64,
    pub sessions: Vec<Session>,
    pub download_dir: String,
    pub language: String,
    #[serde(default)]
    pub private_keys: Vec<SyncedPrivateKey>,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SyncedPrivateKey {
    pub session_id: String,
    pub file_name: String,
    pub content_b64: String,
}

#[derive(Debug, Clone)]
pub struct SyncResult {
    pub config: CloudSyncConfig,
    pub downloaded: Option<SyncSnapshot>,
    pub status: String,
}

#[derive(Debug, Clone)]
pub struct LoginResult {
    pub config: CloudSyncConfig,
    pub status: String,
}

#[derive(Debug, Serialize, Deserialize)]
struct EncryptedBundle {
    version: u32,
    kdf: String,
    cipher: String,
    salt_b64: String,
    nonce_b64: String,
    ciphertext_b64: String,
}

#[derive(Debug, Deserialize)]
struct UserInfo {
    login: Option<String>,
    username: Option<String>,
}

#[derive(Debug, Deserialize)]
struct OAuthTokenResponse {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
}

#[derive(Debug, Deserialize)]
struct GitHubDeviceCode {
    device_code: String,
    user_code: String,
    verification_uri: String,
    expires_in: u64,
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct GitHubDevicePoll {
    access_token: Option<String>,
    error: Option<String>,
    error_description: Option<String>,
    interval: Option<u64>,
}

#[derive(Debug, Deserialize)]
struct RepoInfo {
    owner: Option<RepoOwner>,
}

#[derive(Debug, Deserialize)]
struct RepoOwner {
    login: String,
}

#[derive(Debug, Deserialize)]
struct ContentInfo {
    content: Option<String>,
    sha: Option<String>,
}

#[derive(Debug, Clone)]
struct RemoteFile {
    bytes: Vec<u8>,
    sha: String,
}

pub fn normalized_config(
    provider: CloudProvider,
    server_url: String,
    repo_name: String,
    auto_sync: bool,
    interval_minutes: u64,
    current: &CloudSyncConfig,
) -> CloudSyncConfig {
    let mut cfg = current.clone();
    let server = normalize_server(provider, &server_url);
    let account_changed = current.provider != provider || current.server_url != server;
    cfg.provider = provider;
    cfg.server_url = server;
    cfg.repo_name = if repo_name.trim().is_empty() {
        "meatshell-sync".to_string()
    } else {
        repo_name.trim().to_string()
    };
    cfg.auto_sync = auto_sync;
    cfg.interval_minutes = interval_minutes.max(1);
    if account_changed {
        cfg.username.clear();
        cfg.repo_owner.clear();
        cfg.token_key.clear();
        cfg.password_key.clear();
        cfg.last_synced_at = 0;
    }
    cfg
}

pub async fn login<F>(
    mut cfg: CloudSyncConfig,
    sync_password: String,
    mut progress: F,
) -> Result<LoginResult>
where
    F: FnMut(String) + Send,
{
    if sync_password.is_empty() {
        bail!("请输入同步密码");
    }

    let client = http_client()?;
    progress("正在打开浏览器授权...".to_string());
    let (token, username) = match cfg.provider {
        CloudProvider::Github => github_login(&client, &cfg, &mut progress).await?,
        CloudProvider::Gitea => gitea_login(&client, &cfg, &mut progress).await?,
    };

    cfg.username = username.clone();
    if cfg.repo_owner.trim().is_empty() {
        cfg.repo_owner = username.clone();
    }
    cfg.token_key = secret_key(&cfg, "token");
    cfg.password_key = secret_key(&cfg, "sync-password");

    store_secret(&cfg.token_key, &token).context("保存登录凭据失败")?;
    store_secret(&cfg.password_key, &sync_password).context("保存同步密码失败")?;

    ensure_repo(&client, &cfg, &token)
        .await
        .context("创建或检查私有同步仓库失败")?;

    Ok(LoginResult {
        status: format!(
            "{} 已登录为 {}，同步仓库 {} 已准备好",
            cfg.provider.label(),
            cfg.username,
            cfg.repo_name
        ),
        config: cfg,
    })
}

pub async fn sync_now<F>(
    mut cfg: CloudSyncConfig,
    local_config: ConfigFile,
    sync_password: Option<String>,
    mut progress: F,
) -> Result<SyncResult>
where
    F: FnMut(String) + Send,
{
    if cfg.username.trim().is_empty() {
        bail!("请先登录同步账号");
    }
    let token = read_secret(&cfg.token_key).context("读取登录凭据失败，请重新登录")?;
    let password = match sync_password {
        Some(p) if !p.is_empty() => {
            if !cfg.password_key.is_empty() {
                let _ = store_secret(&cfg.password_key, &p);
            }
            p
        }
        _ => read_secret(&cfg.password_key).context("读取同步密码失败，请输入同步密码后再同步")?,
    };

    let client = http_client()?;
    progress("正在检查同步仓库...".to_string());
    ensure_repo(&client, &cfg, &token).await?;

    progress("正在读取远端快照...".to_string());
    let remote = get_sync_file(&client, &cfg, &token).await?;
    if let Some(file) = remote.clone() {
        let remote_snapshot = decrypt_snapshot(&file.bytes, &password)
            .context("远端同步文件无法解密，请检查同步密码")?;
        if remote_snapshot.updated_at > cfg.last_synced_at {
            cfg.last_synced_at = remote_snapshot.updated_at;
            return Ok(SyncResult {
                status: "已从远端恢复较新的同步数据".to_string(),
                config: cfg,
                downloaded: Some(remote_snapshot),
            });
        }
    }

    progress("正在加密并上传本机快照...".to_string());
    let snapshot = build_snapshot(&local_config);
    let encrypted = encrypt_snapshot(&snapshot, &password)?;
    put_sync_file(&client, &cfg, &token, encrypted, remote.map(|f| f.sha)).await?;
    cfg.last_synced_at = snapshot.updated_at;

    Ok(SyncResult {
        status: "已上传本机同步数据".to_string(),
        config: cfg,
        downloaded: None,
    })
}

pub fn apply_downloaded_snapshot(store: &mut ConfigStore, snapshot: SyncSnapshot) -> Result<()> {
    let mut sessions = snapshot.sessions;
    restore_private_keys(&mut sessions, snapshot.private_keys)?;
    store.replace_synced_data(sessions, snapshot.download_dir, snapshot.language);
    Ok(())
}

pub fn clear_saved_secrets(cfg: &CloudSyncConfig) {
    if !cfg.token_key.is_empty() {
        let _ = delete_secret(&cfg.token_key);
    }
    if !cfg.password_key.is_empty() {
        let _ = delete_secret(&cfg.password_key);
    }
}

fn build_snapshot(cfg: &ConfigFile) -> SyncSnapshot {
    SyncSnapshot {
        version: 1,
        updated_at: Utc::now().timestamp_millis(),
        sessions: cfg.sessions.clone(),
        download_dir: cfg.download_dir.clone(),
        language: cfg.language.clone(),
        private_keys: collect_private_keys(&cfg.sessions),
    }
}

fn collect_private_keys(sessions: &[Session]) -> Vec<SyncedPrivateKey> {
    let mut keys = Vec::new();
    for session in sessions {
        let path = session.private_key_path.trim();
        if path.is_empty() {
            continue;
        }
        let Ok(bytes) = fs::read(path) else {
            continue;
        };
        let file_name = Path::new(path)
            .file_name()
            .and_then(|n| n.to_str())
            .unwrap_or("id_key");
        keys.push(SyncedPrivateKey {
            session_id: session.id.clone(),
            file_name: sanitize_file_name(file_name),
            content_b64: STANDARD.encode(bytes),
        });
    }
    keys
}

fn restore_private_keys(sessions: &mut [Session], keys: Vec<SyncedPrivateKey>) -> Result<()> {
    if keys.is_empty() {
        return Ok(());
    }
    let dir = ConfigStore::synced_keys_dir()?;
    fs::create_dir_all(&dir).with_context(|| format!("failed to create {}", dir.display()))?;

    for key in keys {
        let bytes = STANDARD
            .decode(key.content_b64.as_bytes())
            .context("invalid private-key payload")?;
        let path = unique_key_path(&dir, &key.session_id, &key.file_name);
        fs::write(&path, bytes).with_context(|| format!("failed to write {}", path.display()))?;
        set_private_key_permissions(&path);
        if let Some(session) = sessions.iter_mut().find(|s| s.id == key.session_id) {
            session.private_key_path = path.to_string_lossy().replace('\\', "/");
        }
    }
    Ok(())
}

fn unique_key_path(dir: &Path, session_id: &str, name: &str) -> PathBuf {
    let prefix = session_id.chars().take(8).collect::<String>();
    dir.join(format!("{}_{}", prefix, sanitize_file_name(name)))
}

#[cfg(unix)]
fn set_private_key_permissions(path: &Path) {
    use std::os::unix::fs::PermissionsExt;
    let _ = fs::set_permissions(path, fs::Permissions::from_mode(0o600));
}

#[cfg(not(unix))]
fn set_private_key_permissions(_path: &Path) {}

fn sanitize_file_name(name: &str) -> String {
    let mut out = String::with_capacity(name.len());
    for ch in name.chars() {
        if ch.is_ascii_alphanumeric() || matches!(ch, '.' | '_' | '-') {
            out.push(ch);
        } else {
            out.push('_');
        }
    }
    if out.is_empty() {
        "id_key".to_string()
    } else {
        out
    }
}

fn encrypt_snapshot(snapshot: &SyncSnapshot, password: &str) -> Result<Vec<u8>> {
    let plaintext = serde_json::to_vec(snapshot)?;
    let mut salt = [0u8; 16];
    let mut nonce = [0u8; 24];
    OsRng.fill_bytes(&mut salt);
    OsRng.fill_bytes(&mut nonce);
    let key = derive_key(password, &salt)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let ciphertext = cipher
        .encrypt(XNonce::from_slice(&nonce), plaintext.as_ref())
        .map_err(|err| anyhow!("encrypt failed: {err}"))?;
    let bundle = EncryptedBundle {
        version: 1,
        kdf: "argon2id".to_string(),
        cipher: "xchacha20poly1305".to_string(),
        salt_b64: STANDARD.encode(salt),
        nonce_b64: STANDARD.encode(nonce),
        ciphertext_b64: STANDARD.encode(ciphertext),
    };
    Ok(serde_json::to_vec_pretty(&bundle)?)
}

fn decrypt_snapshot(bytes: &[u8], password: &str) -> Result<SyncSnapshot> {
    let bundle: EncryptedBundle = serde_json::from_slice(bytes)?;
    if bundle.kdf != "argon2id" || bundle.cipher != "xchacha20poly1305" {
        bail!("unsupported sync encryption format");
    }
    let salt = STANDARD.decode(bundle.salt_b64.as_bytes())?;
    let nonce = STANDARD.decode(bundle.nonce_b64.as_bytes())?;
    if nonce.len() != 24 {
        bail!("invalid sync nonce");
    }
    let key = derive_key(password, &salt)?;
    let cipher = XChaCha20Poly1305::new((&key).into());
    let plaintext = cipher
        .decrypt(
            XNonce::from_slice(&nonce),
            STANDARD.decode(bundle.ciphertext_b64)?.as_ref(),
        )
        .map_err(|_| anyhow!("decrypt failed"))?;
    Ok(serde_json::from_slice(&plaintext)?)
}

fn derive_key(password: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let mut key = [0u8; 32];
    Argon2::default()
        .hash_password_into(password.as_bytes(), salt, &mut key)
        .map_err(|err| anyhow!("key derivation failed: {err}"))?;
    Ok(key)
}

async fn github_login<F>(
    client: &Client,
    cfg: &CloudSyncConfig,
    progress: &mut F,
) -> Result<(String, String)>
where
    F: FnMut(String) + Send,
{
    let client_id = oauth_client_id(cfg, GITHUB_CLI_CLIENT_ID);
    let code: GitHubDeviceCode = client
        .post("https://github.com/login/device/code")
        .header("Accept", "application/json")
        .form(&[
            ("client_id", client_id.as_str()),
            ("scope", "repo read:user"),
        ])
        .send()
        .await?
        .json()
        .await?;

    let _ = webbrowser::open(&code.verification_uri);
    progress(format!("请在浏览器授权 GitHub，验证码：{}", code.user_code));

    let started = std::time::Instant::now();
    let mut interval = code.interval.unwrap_or(5).max(1);
    loop {
        if started.elapsed() > Duration::from_secs(code.expires_in) {
            bail!("GitHub 授权已超时");
        }
        tokio::time::sleep(Duration::from_secs(interval)).await;
        let poll: GitHubDevicePoll = client
            .post("https://github.com/login/oauth/access_token")
            .header("Accept", "application/json")
            .form(&[
                ("client_id", client_id.as_str()),
                ("device_code", code.device_code.as_str()),
                ("grant_type", "urn:ietf:params:oauth:grant-type:device_code"),
            ])
            .send()
            .await?
            .json()
            .await?;

        if let Some(token) = poll.access_token {
            let username = fetch_username(client, cfg.provider, "", &token).await?;
            return Ok((token, username));
        }
        match poll.error.as_deref() {
            Some("authorization_pending") => {}
            Some("slow_down") => interval += 5,
            Some("expired_token") => bail!("GitHub 授权已过期"),
            Some(err) => bail!(
                "GitHub 授权失败：{}",
                poll.error_description.unwrap_or_else(|| err.to_string())
            ),
            None => {
                if let Some(next) = poll.interval {
                    interval = next.max(1);
                }
            }
        }
    }
}

async fn gitea_login<F>(
    client: &Client,
    cfg: &CloudSyncConfig,
    progress: &mut F,
) -> Result<(String, String)>
where
    F: FnMut(String) + Send,
{
    let server = cfg.server_url.trim_end_matches('/');
    let client_id = oauth_client_id(cfg, GITEA_TEA_CLIENT_ID);
    let verifier = random_urlsafe(64);
    let challenge = URL_SAFE_NO_PAD.encode(Sha256::digest(verifier.as_bytes()));
    let state = random_urlsafe(24);

    let listener = TcpListener::bind("127.0.0.1:0")
        .await
        .context("无法启动本地 OAuth 回调端口")?;
    let port = listener.local_addr()?.port();
    let redirect_uri = format!("http://127.0.0.1:{port}");

    let mut auth_url = Url::parse(&format!("{server}/login/oauth/authorize"))?;
    auth_url
        .query_pairs_mut()
        .append_pair("client_id", &client_id)
        .append_pair("redirect_uri", &redirect_uri)
        .append_pair("response_type", "code")
        .append_pair("scope", "repository user")
        .append_pair("state", &state)
        .append_pair("code_challenge", &challenge)
        .append_pair("code_challenge_method", "S256");

    let _ = webbrowser::open(auth_url.as_str());
    progress("请在浏览器完成 Gitea 授权...".to_string());

    let (code, returned_state) = wait_for_oauth_callback(listener).await?;
    if returned_state != state {
        bail!("OAuth state 校验失败");
    }

    let token_resp: OAuthTokenResponse = client
        .post(format!("{server}/login/oauth/access_token"))
        .header("Accept", "application/json")
        .form(&[
            ("grant_type", "authorization_code"),
            ("code", code.as_str()),
            ("redirect_uri", redirect_uri.as_str()),
            ("client_id", client_id.as_str()),
            ("code_verifier", verifier.as_str()),
        ])
        .send()
        .await?
        .json()
        .await?;
    if let Some(err) = token_resp.error {
        bail!(
            "Gitea 授权失败：{}",
            token_resp.error_description.unwrap_or(err)
        );
    }
    let token = token_resp
        .access_token
        .ok_or_else(|| anyhow!("Gitea 未返回 access_token"))?;
    let username = fetch_username(client, cfg.provider, server, &token).await?;
    Ok((token, username))
}

async fn wait_for_oauth_callback(listener: TcpListener) -> Result<(String, String)> {
    let (mut stream, _) = timeout(Duration::from_secs(120), listener.accept())
        .await
        .context("等待浏览器授权超时")??;
    let mut buf = [0u8; 8192];
    let n = timeout(Duration::from_secs(5), stream.read(&mut buf))
        .await
        .context("读取 OAuth 回调超时")??;
    let request = String::from_utf8_lossy(&buf[..n]);
    let first_line = request.lines().next().unwrap_or_default();
    let path = first_line.split_whitespace().nth(1).unwrap_or("/");
    let callback_url = Url::parse(&format!("http://127.0.0.1{path}"))?;
    let mut code = String::new();
    let mut state = String::new();
    let mut error = String::new();
    for (key, value) in callback_url.query_pairs() {
        match key.as_ref() {
            "code" => code = value.into_owned(),
            "state" => state = value.into_owned(),
            "error" => error = value.into_owned(),
            _ => {}
        }
    }
    let body = if error.is_empty() {
        "<html><body><h3>meatshell login complete</h3><p>You can close this window.</p></body></html>"
    } else {
        "<html><body><h3>meatshell login failed</h3><p>You can close this window.</p></body></html>"
    };
    let response = format!(
        "HTTP/1.1 200 OK\r\nContent-Type: text/html; charset=utf-8\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{}",
        body.len(),
        body
    );
    let _ = stream.write_all(response.as_bytes()).await;
    if !error.is_empty() {
        bail!("浏览器授权失败：{error}");
    }
    if code.is_empty() {
        bail!("OAuth 回调缺少授权码");
    }
    Ok((code, state))
}

async fn fetch_username(
    client: &Client,
    provider: CloudProvider,
    server: &str,
    token: &str,
) -> Result<String> {
    let url = match provider {
        CloudProvider::Github => "https://api.github.com/user".to_string(),
        CloudProvider::Gitea => format!("{}/api/v1/user", server.trim_end_matches('/')),
    };
    let resp = client.get(url).bearer_auth(token).send().await?;
    let text = checked_text(resp).await?;
    let user: UserInfo = serde_json::from_str(&text)?;
    user.login
        .or(user.username)
        .filter(|s| !s.is_empty())
        .ok_or_else(|| anyhow!("无法读取账号用户名"))
}

async fn ensure_repo(client: &Client, cfg: &CloudSyncConfig, token: &str) -> Result<()> {
    if get_repo(client, cfg, token).await?.is_some() {
        return Ok(());
    }
    let api = api_base(cfg);
    let resp = match cfg.provider {
        CloudProvider::Github => {
            client
                .post(format!("{api}/user/repos"))
                .bearer_auth(token)
                .json(&serde_json::json!({
                    "name": cfg.repo_name,
                    "private": true,
                    "auto_init": true,
                }))
                .send()
                .await?
        }
        CloudProvider::Gitea => {
            client
                .post(format!("{api}/user/repos"))
                .bearer_auth(token)
                .json(&serde_json::json!({
                    "name": cfg.repo_name,
                    "private": true,
                    "auto_init": true,
                }))
                .send()
                .await?
        }
    };
    let text = checked_text(resp).await?;
    let repo: RepoInfo = serde_json::from_str(&text).unwrap_or(RepoInfo { owner: None });
    if cfg.repo_owner.is_empty() {
        if let Some(owner) = repo.owner {
            tracing::debug!("created sync repo under {}", owner.login);
        }
    }
    Ok(())
}

async fn get_repo(client: &Client, cfg: &CloudSyncConfig, token: &str) -> Result<Option<RepoInfo>> {
    let api = api_base(cfg);
    let owner = repo_owner(cfg)?;
    let repo = urlencoding::encode(&cfg.repo_name);
    let url = format!("{api}/repos/{owner}/{repo}");
    let resp = client.get(url).bearer_auth(token).send().await?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let text = checked_text(resp).await?;
    Ok(Some(serde_json::from_str(&text)?))
}

async fn get_sync_file(
    client: &Client,
    cfg: &CloudSyncConfig,
    token: &str,
) -> Result<Option<RemoteFile>> {
    let api = api_base(cfg);
    let owner = repo_owner(cfg)?;
    let repo = urlencoding::encode(&cfg.repo_name);
    let path = urlencoding::encode(SYNC_FILE);
    let url = format!("{api}/repos/{owner}/{repo}/contents/{path}");
    let resp = client.get(url).bearer_auth(token).send().await?;
    if resp.status() == StatusCode::NOT_FOUND {
        return Ok(None);
    }
    let text = checked_text(resp).await?;
    let info: ContentInfo = serde_json::from_str(&text)?;
    let mut content = info.content.unwrap_or_default();
    content.retain(|ch| !ch.is_whitespace());
    let bytes = STANDARD
        .decode(content.as_bytes())
        .context("远端同步文件不是有效 base64")?;
    let sha = info.sha.unwrap_or_default();
    Ok(Some(RemoteFile { bytes, sha }))
}

async fn put_sync_file(
    client: &Client,
    cfg: &CloudSyncConfig,
    token: &str,
    content: Vec<u8>,
    sha: Option<String>,
) -> Result<()> {
    let api = api_base(cfg);
    let owner = repo_owner(cfg)?;
    let repo = urlencoding::encode(&cfg.repo_name);
    let path = urlencoding::encode(SYNC_FILE);
    let url = format!("{api}/repos/{owner}/{repo}/contents/{path}");
    let mut body = serde_json::json!({
        "message": "sync meatshell data",
        "content": STANDARD.encode(content),
    });
    if let Some(sha) = sha.filter(|s| !s.is_empty()) {
        body["sha"] = serde_json::Value::String(sha);
    }
    let resp = client
        .put(url)
        .bearer_auth(token)
        .json(&body)
        .send()
        .await?;
    let _ = checked_text(resp).await?;
    Ok(())
}

async fn checked_text(resp: reqwest::Response) -> Result<String> {
    let status = resp.status();
    let text = resp.text().await.unwrap_or_default();
    if !status.is_success() {
        let short = text.chars().take(300).collect::<String>();
        bail!("HTTP {status}: {short}");
    }
    Ok(text)
}

fn api_base(cfg: &CloudSyncConfig) -> String {
    match cfg.provider {
        CloudProvider::Github => "https://api.github.com".to_string(),
        CloudProvider::Gitea => format!("{}/api/v1", cfg.server_url.trim_end_matches('/')),
    }
}

fn repo_owner(cfg: &CloudSyncConfig) -> Result<String> {
    let owner = if cfg.repo_owner.trim().is_empty() {
        cfg.username.trim()
    } else {
        cfg.repo_owner.trim()
    };
    if owner.is_empty() {
        bail!("缺少同步仓库 owner");
    }
    Ok(urlencoding::encode(owner).to_string())
}

fn http_client() -> Result<Client> {
    Client::builder()
        .user_agent(format!("meatshell/{}", env!("CARGO_PKG_VERSION")))
        .timeout(Duration::from_secs(30))
        .build()
        .context("failed to build HTTP client")
}

fn random_urlsafe(len: usize) -> String {
    let mut bytes = vec![0u8; len];
    OsRng.fill_bytes(&mut bytes);
    URL_SAFE_NO_PAD.encode(bytes)
}

fn oauth_client_id(cfg: &CloudSyncConfig, fallback: &str) -> String {
    if !cfg.client_id.trim().is_empty() {
        return cfg.client_id.trim().to_string();
    }
    match cfg.provider {
        CloudProvider::Github => std::env::var("MEATSHELL_GITHUB_CLIENT_ID")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| fallback.to_string()),
        CloudProvider::Gitea => std::env::var("MEATSHELL_GITEA_CLIENT_ID")
            .ok()
            .filter(|s| !s.trim().is_empty())
            .unwrap_or_else(|| fallback.to_string()),
    }
}

fn normalize_server(provider: CloudProvider, server: &str) -> String {
    match provider {
        CloudProvider::Github => "https://github.com".to_string(),
        CloudProvider::Gitea => {
            let trimmed = server.trim().trim_end_matches('/');
            if trimmed.is_empty() {
                "https://gitea.com".to_string()
            } else if trimmed.starts_with("http://") || trimmed.starts_with("https://") {
                trimmed.to_string()
            } else {
                format!("https://{trimmed}")
            }
        }
    }
}

fn secret_key(cfg: &CloudSyncConfig, kind: &str) -> String {
    let server = cfg.server_url.replace(['/', ':'], "_");
    let user = if cfg.username.is_empty() {
        "unknown"
    } else {
        cfg.username.as_str()
    };
    format!("sync:{}:{server}:{user}:{kind}", cfg.provider.as_str())
}

fn store_secret(key: &str, secret: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, key)?;
    entry.set_password(secret)?;
    Ok(())
}

fn read_secret(key: &str) -> Result<String> {
    if key.is_empty() {
        bail!("empty secret key");
    }
    let entry = keyring::Entry::new(KEYRING_SERVICE, key)?;
    Ok(entry.get_password()?)
}

fn delete_secret(key: &str) -> Result<()> {
    let entry = keyring::Entry::new(KEYRING_SERVICE, key)?;
    entry.delete_credential()?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{AuthMethod, Secret};

    #[test]
    fn encrypted_snapshot_roundtrip() {
        let snapshot = SyncSnapshot {
            version: 1,
            updated_at: 42,
            sessions: vec![Session {
                id: "s1".to_string(),
                name: "demo".to_string(),
                host: "127.0.0.1".to_string(),
                port: 22,
                user: "root".to_string(),
                auth: AuthMethod::Password,
                password: Secret::new("pw"),
                private_key_path: String::new(),
                proxy: String::new(),
                last_used: None,
            }],
            download_dir: "/tmp".to_string(),
            language: "zh".to_string(),
            private_keys: Vec::new(),
        };
        let encrypted = encrypt_snapshot(&snapshot, "sync-pass").unwrap();
        let decrypted = decrypt_snapshot(&encrypted, "sync-pass").unwrap();
        assert_eq!(decrypted.updated_at, 42);
        assert_eq!(decrypted.sessions[0].password.as_str(), "pw");
    }
}
