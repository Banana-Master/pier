use std::{
    collections::HashMap,
    path::{Path, PathBuf},
    sync::{
        atomic::{AtomicU64, Ordering},
        Arc,
    },
    time::{Duration, UNIX_EPOCH},
};

use anyhow::anyhow;
use base64::{engine::general_purpose::STANDARD as B64, Engine};
use russh::{
    client,
    keys::{self, load_secret_key, known_hosts::learn_known_hosts, PrivateKeyWithHashAlg, PublicKeyOrCertificate},
    ChannelMsg, Disconnect,
};
use russh_sftp::client::SftpSession;
use serde::{Deserialize, Serialize};
use tauri::{AppHandle, Emitter, State};
use tokio::{
    io::{AsyncReadExt, AsyncWriteExt},
    sync::{mpsc, Mutex},
};

type R<T> = Result<T, String>;

fn e2s<E: std::fmt::Display>(e: E) -> String {
    e.to_string()
}

// ───────────────────────── 상태 ─────────────────────────

struct Handler {
    host: String,
    port: u16,
}

impl client::Handler for Handler {
    type Error = anyhow::Error;

    // known_hosts 검증 (처음 보는 호스트는 등록, 키가 바뀌었으면 거부)
    async fn check_server_key(
        &mut self,
        key: &PublicKeyOrCertificate,
    ) -> Result<bool, Self::Error> {
        let PublicKeyOrCertificate::PublicKey { key, .. } = key else {
            return Ok(false);
        };
        match keys::check_known_hosts(&self.host, self.port, key) {
            Ok(true) => Ok(true),
            Ok(false) => {
                learn_known_hosts(&self.host, self.port, key)?;
                Ok(true)
            }
            Err(e) => Err(anyhow!(
                "호스트 키가 known_hosts와 다릅니다. 중간자 공격일 수 있어 연결을 거부했어요: {e}"
            )),
        }
    }
}

struct SshSession {
    handle: client::Handle<Handler>,
    sftp: SftpSession,
    _jumps: Vec<client::Handle<Handler>>, // 점프 호스트 연결 (끊기지 않게 유지)
}

enum TermCmd {
    Data(Vec<u8>),
    Resize(u32, u32),
    Close,
}

struct TermHandle {
    session_id: String,
    tx: mpsc::UnboundedSender<TermCmd>,
}

struct LocalPty {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

#[derive(Default)]
pub struct AppState {
    sessions: Mutex<HashMap<String, Arc<SshSession>>>,
    terms: Mutex<HashMap<String, TermHandle>>,
    locals: std::sync::Mutex<HashMap<String, LocalPty>>,
    next_id: AtomicU64,
}

impl AppState {
    fn new_id(&self, prefix: &str) -> String {
        format!("{prefix}{}", self.next_id.fetch_add(1, Ordering::Relaxed))
    }

    async fn session(&self, id: &str) -> R<Arc<SshSession>> {
        self.sessions
            .lock()
            .await
            .get(id)
            .cloned()
            .ok_or_else(|| "세션이 없습니다 (연결이 끊겼을 수 있어요)".to_string())
    }
}

fn expand(p: &str) -> PathBuf {
    if let Some(rest) = p.strip_prefix("~/") {
        if let Some(home) = dirs::home_dir() {
            return home.join(rest);
        }
    }
    PathBuf::from(p)
}

// ───────────────────────── 접속 ─────────────────────────

#[derive(Deserialize)]
#[serde(tag = "type", rename_all = "lowercase")]
pub enum Auth {
    Password { password: String },
    Key { path: String, passphrase: Option<String> },
}

#[derive(Deserialize)]
pub struct ConnectArgs {
    host: String,
    port: u16,
    username: String,
    auth: Auth,
    #[serde(default)]
    jump: Option<Box<ConnectArgs>>,
}

struct Opened {
    handle: client::Handle<Handler>,
    jumps: Vec<client::Handle<Handler>>,
}

/// 접속 + 인증까지 끝낸 핸들을 만든다. `jump` 가 있으면 그 호스트를 경유(ProxyJump)한다.
async fn open_handle(args: &ConnectArgs) -> R<Opened> {
    let config = Arc::new(client::Config {
        keepalive_interval: Some(Duration::from_secs(30)),
        keepalive_max: 4,
        ..Default::default()
    });
    let handler = Handler { host: args.host.clone(), port: args.port };
    let timeout = Duration::from_secs(15);

    let (mut handle, jumps) = if let Some(j) = &args.jump {
        let Opened { handle: jh, mut jumps } = Box::pin(open_handle(j))
            .await
            .map_err(|e| format!("점프 호스트({})에 연결하지 못했어요: {e}", j.host))?;
        let ch = jh
            .channel_open_direct_tcpip(args.host.clone(), args.port as u32, "127.0.0.1", 0)
            .await
            .map_err(|e| format!("점프 호스트를 통해 {}:{}에 연결하지 못했어요: {e}", args.host, args.port))?;
        let h = tokio::time::timeout(timeout, client::connect_stream(config, ch.into_stream(), handler))
            .await
            .map_err(|_| "연결 시간 초과 (15초)".to_string())?
            .map_err(e2s)?;
        jumps.push(jh);
        (h, jumps)
    } else {
        let h = tokio::time::timeout(
            timeout,
            client::connect(config, (args.host.as_str(), args.port), handler),
        )
        .await
        .map_err(|_| "연결 시간 초과 (15초)".to_string())?
        .map_err(e2s)?;
        (h, vec![])
    };

    let ok = match &args.auth {
        Auth::Password { password } => handle
            .authenticate_password(&args.username, password.clone())
            .await
            .map_err(e2s)?
            .success(),
        Auth::Key { path, passphrase } => {
            let key = load_secret_key(expand(path), passphrase.as_deref().filter(|p| !p.is_empty()))
                .map_err(|e| format!("키 파일을 읽지 못했어요 ({e}). 패스프레이즈를 확인하세요."))?;
            let hash = handle.best_supported_rsa_hash().await.map_err(e2s)?.flatten();
            handle
                .authenticate_publickey(&args.username, PrivateKeyWithHashAlg::new(Arc::new(key), hash))
                .await
                .map_err(e2s)?
                .success()
        }
    };
    if !ok {
        return Err(format!("{}@{} 인증 실패 (계정/비밀번호/키를 확인하세요)", args.username, args.host));
    }
    Ok(Opened { handle, jumps })
}

async fn establish(args: ConnectArgs) -> R<SshSession> {
    let Opened { handle, jumps } = open_handle(&args).await?;
    let channel = handle.channel_open_session().await.map_err(e2s)?;
    channel.request_subsystem(true, "sftp").await.map_err(e2s)?;
    let sftp = SftpSession::new(channel.into_stream()).await.map_err(e2s)?;
    Ok(SshSession { handle, sftp, _jumps: jumps })
}

#[tauri::command]
pub async fn ssh_connect(state: State<'_, AppState>, args: ConnectArgs) -> R<String> {
    let sess = establish(args).await?;
    let id = state.new_id("s");
    state.sessions.lock().await.insert(id.clone(), Arc::new(sess));
    Ok(id)
}

/// 끊어진 세션을 같은 id 로 다시 연결한다 (프론트의 탭/편집기/상태바가 그대로 유지됨).
#[tauri::command]
pub async fn ssh_reconnect(
    state: State<'_, AppState>,
    session_id: String,
    args: ConnectArgs,
) -> R<()> {
    let sess = establish(args).await?;
    state.terms.lock().await.retain(|_, t| {
        if t.session_id == session_id {
            let _ = t.tx.send(TermCmd::Close);
            false
        } else {
            true
        }
    });
    let old = state.sessions.lock().await.insert(session_id, Arc::new(sess));
    if let Some(o) = old {
        let _ = o.handle.disconnect(Disconnect::ByApplication, "", "en").await;
    }
    Ok(())
}

/// SSH 연결이 살아 있는지 (셸만 종료된 것과 연결 끊김을 구분하는 용도)
#[tauri::command]
pub async fn ssh_alive(state: State<'_, AppState>, session_id: String) -> R<bool> {
    Ok(match state.session(&session_id).await {
        Ok(s) => !s.handle.is_closed(),
        Err(_) => false,
    })
}

#[tauri::command]
pub async fn ssh_disconnect(state: State<'_, AppState>, session_id: String) -> R<()> {
    {
        let mut terms = state.terms.lock().await;
        terms.retain(|_, t| {
            if t.session_id == session_id {
                let _ = t.tx.send(TermCmd::Close);
                false
            } else {
                true
            }
        });
    }
    if let Some(s) = state.sessions.lock().await.remove(&session_id) {
        let _ = s.sftp.close().await;
        let _ = s
            .handle
            .disconnect(Disconnect::ByApplication, "", "en")
            .await;
    }
    Ok(())
}

// ───────────────────────── 터미널 ─────────────────────────

#[derive(Serialize, Clone)]
struct TermData {
    id: String,
    data: String, // base64
}

#[tauri::command]
pub async fn term_open(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    cols: u32,
    rows: u32,
) -> R<String> {
    let s = state.session(&session_id).await?;
    let mut ch = s.handle.channel_open_session().await.map_err(e2s)?;
    ch.request_pty(false, "xterm-256color", cols, rows, 0, 0, &[])
        .await
        .map_err(e2s)?;
    ch.request_shell(true).await.map_err(e2s)?;

    let id = state.new_id("t");
    let (tx, mut rx) = mpsc::unbounded_channel::<TermCmd>();
    state.terms.lock().await.insert(
        id.clone(),
        TermHandle {
            session_id,
            tx,
        },
    );

    let tid = id.clone();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                cmd = rx.recv() => match cmd {
                    Some(TermCmd::Data(d)) => { let _ = ch.data(&d[..]).await; }
                    Some(TermCmd::Resize(c, r)) => { let _ = ch.window_change(c, r, 0, 0).await; }
                    Some(TermCmd::Close) | None => { let _ = ch.close().await; break; }
                },
                msg = ch.wait() => match msg {
                    Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => {
                        let _ = app.emit("term-data", TermData { id: tid.clone(), data: B64.encode(&data[..]) });
                    }
                    Some(ChannelMsg::Close) | None => break,
                    _ => {}
                },
            }
        }
        let _ = app.emit("term-exit", tid);
    });
    Ok(id)
}

#[tauri::command]
pub async fn term_write(state: State<'_, AppState>, term_id: String, data: String) -> R<()> {
    if let Some(t) = state.terms.lock().await.get(&term_id) {
        let _ = t.tx.send(TermCmd::Data(data.into_bytes()));
        return Ok(());
    }
    if let Some(l) = state.locals.lock().unwrap().get_mut(&term_id) {
        use std::io::Write as _;
        let _ = l.writer.write_all(data.as_bytes());
        let _ = l.writer.flush();
    }
    Ok(())
}

#[tauri::command]
pub async fn term_resize(
    state: State<'_, AppState>,
    term_id: String,
    cols: u32,
    rows: u32,
) -> R<()> {
    if let Some(t) = state.terms.lock().await.get(&term_id) {
        let _ = t.tx.send(TermCmd::Resize(cols, rows));
        return Ok(());
    }
    if let Some(l) = state.locals.lock().unwrap().get(&term_id) {
        let _ = l.master.resize(portable_pty::PtySize { rows: rows as u16, cols: cols as u16, pixel_width: 0, pixel_height: 0 });
    }
    Ok(())
}

#[tauri::command]
pub async fn term_close(state: State<'_, AppState>, term_id: String) -> R<()> {
    if let Some(t) = state.terms.lock().await.remove(&term_id) {
        let _ = t.tx.send(TermCmd::Close);
        return Ok(());
    }
    if let Some(mut l) = state.locals.lock().unwrap().remove(&term_id) {
        let _ = l.child.kill();
    }
    Ok(())
}

// ───────────────────────── SFTP ─────────────────────────

#[derive(Serialize)]
pub struct Entry {
    name: String,
    path: String,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    mtime: u64,
    perms: String, // 예: drwxr-xr-x
    octal: String, // 예: 755
    owner: String, // 예: root:root
}

#[tauri::command]
pub async fn sftp_home(state: State<'_, AppState>, session_id: String) -> R<String> {
    let s = state.session(&session_id).await?;
    Ok(s.sftp.canonicalize(".").await.map_err(e2s)?.to_string())
}

#[tauri::command]
pub async fn sftp_realpath(
    state: State<'_, AppState>,
    session_id: String,
    path: String,
) -> R<String> {
    let s = state.session(&session_id).await?;
    Ok(s.sftp.canonicalize(path).await.map_err(e2s)?.to_string())
}

#[tauri::command]
pub async fn sftp_list(
    state: State<'_, AppState>,
    session_id: String,
    path: String,
) -> R<Vec<Entry>> {
    let s = state.session(&session_id).await?;
    let mut out = Vec::new();
    for e in s.sftp.read_dir(path.as_str()).await.map_err(e2s)? {
        let full = e.path().to_string();
        let md = e.metadata();
        let is_symlink = md.is_symlink();
        // 심볼릭 링크는 가리키는 대상이 폴더인지 확인
        let is_dir = if is_symlink {
            s.sftp.metadata(full.as_str()).await.map(|m| m.is_dir()).unwrap_or(false)
        } else {
            md.is_dir()
        };
        let kind = if is_symlink { 'l' } else if is_dir { 'd' } else { '-' };
        let mode = md.permissions.unwrap_or(0);
        let owner = format!(
            "{}:{}",
            md.user.clone().or(md.uid.map(|u| u.to_string())).unwrap_or_else(|| "?".into()),
            md.group.clone().or(md.gid.map(|g| g.to_string())).unwrap_or_else(|| "?".into()),
        );
        out.push(Entry {
            perms: format!("{kind}{}", md.permissions()),
            octal: format!("{:03o}", mode & 0o777),
            owner,
            name: e.file_name().to_string(),
            path: full,
            is_dir,
            is_symlink,
            size: md.len(),
            mtime: md
                .modified()
                .ok()
                .and_then(|t| t.duration_since(UNIX_EPOCH).ok())
                .map(|d| d.as_secs())
                .unwrap_or(0),
        });
    }
    out.sort_by(|a, b| {
        b.is_dir
            .cmp(&a.is_dir)
            .then_with(|| a.name.to_lowercase().cmp(&b.name.to_lowercase()))
    });
    Ok(out)
}

#[tauri::command]
pub async fn sftp_mkdir(state: State<'_, AppState>, session_id: String, path: String) -> R<()> {
    let s = state.session(&session_id).await?;
    s.sftp.create_dir(path).await.map_err(e2s)
}

#[tauri::command]
pub async fn sftp_rename(
    state: State<'_, AppState>,
    session_id: String,
    from: String,
    to: String,
) -> R<()> {
    let s = state.session(&session_id).await?;
    s.sftp.rename(from, to).await.map_err(e2s)
}

async fn remove_rec(sftp: &SftpSession, path: &str, is_dir: bool) -> anyhow::Result<()> {
    if is_dir {
        for e in sftp.read_dir(path).await? {
            let p = e.path().to_string();
            Box::pin(remove_rec(sftp, &p, e.file_type().is_dir())).await?;
        }
        sftp.remove_dir(path).await?;
    } else {
        sftp.remove_file(path).await?;
    }
    Ok(())
}

#[tauri::command]
pub async fn sftp_remove(
    state: State<'_, AppState>,
    session_id: String,
    path: String,
    is_dir: bool,
) -> R<()> {
    let s = state.session(&session_id).await?;
    remove_rec(&s.sftp, &path, is_dir).await.map_err(e2s)
}

// ───────────────────────── 전송 (업로드/다운로드) ─────────────────────────

#[derive(Serialize, Clone)]
struct Transfer {
    id: String,
    name: String,
    done: u64,
    total: u64,
    state: &'static str, // running | done | error
    message: String,
}

struct Ctx {
    app: AppHandle,
    id: String,
}

impl Ctx {
    fn emit(&self, name: &str, done: u64, total: u64, state: &'static str, message: String) {
        let _ = self.app.emit(
            "transfer",
            Transfer {
                id: self.id.clone(),
                name: name.to_string(),
                done,
                total,
                state,
                message,
            },
        );
    }
}

const CHUNK: usize = 256 * 1024;

async fn download_rec(
    sftp: &SftpSession,
    remote: &str,
    local: &Path,
    ctx: &Ctx,
) -> anyhow::Result<()> {
    let md = sftp.metadata(remote).await?;
    if md.is_dir() {
        tokio::fs::create_dir_all(local).await?;
        for e in sftp.read_dir(remote).await? {
            let name = e.file_name().to_string();
            let rp = e.path().to_string();
            Box::pin(download_rec(sftp, &rp, &local.join(&name), ctx)).await?;
        }
        return Ok(());
    }
    let name = remote.rsplit('/').next().unwrap_or(remote).to_string();
    let total = md.len();
    let mut src = sftp.open(remote).await?;
    let mut dst = tokio::fs::File::create(local).await?;
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    loop {
        let n = src.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n]).await?;
        done += n as u64;
        ctx.emit(&name, done, total, "running", String::new());
    }
    dst.flush().await?;
    Ok(())
}

async fn upload_rec(
    sftp: &SftpSession,
    local: &Path,
    remote: &str,
    ctx: &Ctx,
) -> anyhow::Result<()> {
    let md = tokio::fs::metadata(local).await?;
    if md.is_dir() {
        let _ = sftp.create_dir(remote).await; // 이미 있으면 무시
        let mut rd = tokio::fs::read_dir(local).await?;
        while let Some(e) = rd.next_entry().await? {
            let name = e.file_name().to_string_lossy().to_string();
            let rp = format!("{}/{}", remote.trim_end_matches('/'), name);
            Box::pin(upload_rec(sftp, &e.path(), &rp, ctx)).await?;
        }
        return Ok(());
    }
    let name = local
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let total = md.len();
    let mut src = tokio::fs::File::open(local).await?;
    let mut dst = sftp.create(remote).await?;
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    loop {
        let n = src.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        dst.write_all(&buf[..n]).await?;
        done += n as u64;
        ctx.emit(&name, done, total, "running", String::new());
    }
    dst.flush().await?;
    dst.shutdown().await?;
    Ok(())
}

/// 원격 파일/폴더들을 로컬 폴더로 내려받기. 진행률은 "transfer" 이벤트로 전달.
#[tauri::command]
pub async fn sftp_download(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    transfer_id: String,
    remote_paths: Vec<String>,
    local_dir: String,
) -> R<()> {
    let s = state.session(&session_id).await?;
    let ctx = Ctx { app, id: transfer_id };
    let base = expand(&local_dir);
    for rp in remote_paths {
        let name = rp.trim_end_matches('/').rsplit('/').next().unwrap_or("download").to_string();
        if let Err(e) = download_rec(&s.sftp, &rp, &base.join(&name), &ctx).await {
            ctx.emit(&name, 0, 0, "error", e.to_string());
            return Err(e.to_string());
        }
    }
    ctx.emit("", 0, 0, "done", String::new());
    Ok(())
}

/// 로컬 파일/폴더들을 원격 폴더로 올리기.
#[tauri::command]
pub async fn sftp_upload(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    transfer_id: String,
    local_paths: Vec<String>,
    remote_dir: String,
) -> R<()> {
    let s = state.session(&session_id).await?;
    let ctx = Ctx { app, id: transfer_id };
    for lp in local_paths {
        let lp = PathBuf::from(lp);
        let name = lp
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let rp = format!("{}/{}", remote_dir.trim_end_matches('/'), name);
        if let Err(e) = upload_rec(&s.sftp, &lp, &rp, &ctx).await {
            ctx.emit(&name, 0, 0, "error", e.to_string());
            return Err(e.to_string());
        }
    }
    ctx.emit("", 0, 0, "done", String::new());
    Ok(())
}

// ───────────────────────── 저장된 접속 정보 (비밀번호는 저장 안 함) ─────────────────────────

#[derive(Serialize, Deserialize, Clone)]
pub struct Profile {
    id: String,
    name: String,
    host: String,
    port: u16,
    username: String,
    auth_type: String, // "password" | "key"
    key_path: String,
    #[serde(default)]
    remember: bool, // 비밀번호/패스프레이즈를 키체인에 저장했는지
    #[serde(default)]
    group: String, // 폴더 이름 ("" = 미분류)
    #[serde(default)]
    jump: String, // ProxyJump (예: bastion 또는 user@host:22)
}

fn profiles_path() -> R<PathBuf> {
    let dir = dirs::config_dir()
        .ok_or("설정 폴더를 찾지 못했어요")?
        .join("ssh-gui");
    std::fs::create_dir_all(&dir).map_err(e2s)?;
    Ok(dir.join("connections.json"))
}

fn read_profiles() -> R<Vec<Profile>> {
    let p = profiles_path()?;
    if !p.exists() {
        return Ok(vec![]);
    }
    serde_json::from_str(&std::fs::read_to_string(p).map_err(e2s)?).map_err(e2s)
}

fn write_profiles(list: &[Profile]) -> R<()> {
    std::fs::write(profiles_path()?, serde_json::to_string_pretty(list).map_err(e2s)?).map_err(e2s)
}

#[tauri::command]
pub fn profile_list() -> R<Vec<Profile>> {
    read_profiles()
}

#[tauri::command]
pub fn profile_save(profile: Profile) -> R<Vec<Profile>> {
    let mut list = read_profiles()?;
    match list.iter_mut().find(|p| p.id == profile.id) {
        Some(p) => *p = profile,
        None => list.push(profile),
    }
    write_profiles(&list)?;
    Ok(list)
}

#[tauri::command]
pub fn profile_delete(id: String) -> R<Vec<Profile>> {
    let _ = secret_delete(id.clone());
    let mut list = read_profiles()?;
    list.retain(|p| p.id != id);
    write_profiles(&list)?;
    Ok(list)
}

#[tauri::command]
pub fn default_download_dir() -> String {
    dirs::download_dir()
        .map(|p| p.to_string_lossy().to_string())
        .unwrap_or_else(|| "~/Downloads".into())
}

// ───────────────────────── 시스템 상태 (CPU/RAM/디스크/네트워크/온도) ─────────────────────────

// 원격 셸 종류와 무관하게 동작하도록 `sh -s` 의 stdin 으로 흘려보낸다. (리눅스 /proc 기반)
const STATS_SCRIPT: &str = r#"
echo "STAT $(head -n1 /proc/stat)"
echo "MEM $(awk '/^(MemTotal|MemAvailable|SwapTotal|SwapFree):/{printf "%s=%s ",$1,$2}' /proc/meminfo)"
echo "LOAD $(cut -d' ' -f1-3 /proc/loadavg)"
echo "UP $(cut -d' ' -f1 /proc/uptime)"
echo "DISK $(df -Pk / | awk 'NR==2{print $2,$3,$5}')"
echo "NET $(sed 's/:/ /' /proc/net/dev | awk 'NR>2 && $1!="lo"{rx+=$2;tx+=$10}END{print rx+0,tx+0}')"
echo "TEMP $(cat /sys/class/thermal/thermal_zone*/temp 2>/dev/null | sort -n | tail -n1)"
echo "CORES $(nproc 2>/dev/null)"
lsblk -dnbP -o NAME,SIZE,TYPE,ROTA,TRAN,MODEL 2>/dev/null | grep 'TYPE="disk"' | sed 's/^/PD /'
df -PkT 2>/dev/null | awk 'NR>1 && $1 ~ /^\// && $2!~/^(tmpfs|devtmpfs|squashfs|overlay|efivarfs)$/ {print "MNT",$1,$2,$3,$4,$6,$7}'
"#;

#[tauri::command]
pub async fn sys_stats(state: State<'_, AppState>, session_id: String) -> R<String> {
    let s = state.session(&session_id).await?;
    let work = async {
        let mut ch = s.handle.channel_open_session().await.map_err(e2s)?;
        ch.exec(true, "sh -s").await.map_err(e2s)?;
        ch.data(STATS_SCRIPT.as_bytes()).await.map_err(e2s)?;
        ch.eof().await.map_err(e2s)?;
        let mut out = Vec::new();
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::Data { data } => out.extend_from_slice(&data[..]),
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        let _ = ch.close().await;
        Ok::<String, String>(String::from_utf8_lossy(&out).to_string())
    };
    tokio::time::timeout(Duration::from_secs(6), work)
        .await
        .map_err(|_| "상태 조회 시간 초과".to_string())?
}

// ───────────────────────── 원격 파일 편집 ─────────────────────────

const MAX_EDIT_BYTES: u64 = 5 * 1024 * 1024;

#[tauri::command]
pub async fn sftp_read_text(
    state: State<'_, AppState>,
    session_id: String,
    path: String,
) -> R<String> {
    let s = state.session(&session_id).await?;
    let size = s.sftp.metadata(path.as_str()).await.map_err(e2s)?.len();
    if size > MAX_EDIT_BYTES {
        return Err(format!("TOO_BIG:{}", size));
    }
    let bytes = s.sftp.read(path).await.map_err(e2s)?;
    if bytes.iter().take(8000).any(|&b| b == 0) {
        return Err("BINARY".into());
    }
    String::from_utf8(bytes).map_err(|_| "NOT_UTF8".to_string())
}

#[tauri::command]
pub async fn sftp_write_text(
    state: State<'_, AppState>,
    session_id: String,
    path: String,
    content: String,
) -> R<()> {
    let s = state.session(&session_id).await?;
    let mut f = s.sftp.create(path).await.map_err(e2s)?; // 기존 파일이면 내용을 비우고 권한은 유지
    f.write_all(content.as_bytes()).await.map_err(e2s)?;
    f.flush().await.map_err(e2s)?;
    f.shutdown().await.map_err(e2s)?;
    Ok(())
}

// ───────────────────────── 파일/폴더 검색 (서버에서 find 실행) ─────────────────────────

#[derive(Serialize)]
pub struct Found {
    path: String,
    is_dir: bool,
    is_symlink: bool,
    size: u64,
    mtime: u64,
}

fn sh_quote(s: &str) -> String {
    format!("'{}'", s.replace('\'', "'\\''"))
}

/// `root` 아래에서 이름에 `pattern` 이 들어간 항목을 찾는다 (대소문자 무시, 최대 500개).
/// 패턴에 * ? [ 가 있으면 글롭으로 그대로, 없으면 *패턴* 으로 검색한다.
#[tauri::command]
pub async fn sftp_find(
    state: State<'_, AppState>,
    session_id: String,
    root: String,
    pattern: String,
) -> R<Vec<Found>> {
    let s = state.session(&session_id).await?;
    let pat = if pattern.contains(['*', '?', '[']) { pattern } else { format!("*{pattern}*") };
    let script = format!(
        "T=''; command -v timeout >/dev/null 2>&1 && T='timeout 20'\n\
         $T find {root} \\( -path /proc -o -path /sys -o -path /dev \\) -prune -o -iname {pat} -printf '%y\\t%s\\t%T@\\t%p\\n' 2>/dev/null | head -n 500\n",
        root = sh_quote(&root),
        pat = sh_quote(&pat),
    );

    let work = async {
        let mut ch = s.handle.channel_open_session().await.map_err(e2s)?;
        ch.exec(true, "sh -s").await.map_err(e2s)?;
        ch.data(script.as_bytes()).await.map_err(e2s)?;
        ch.eof().await.map_err(e2s)?;
        let mut out = Vec::new();
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::Data { data } => out.extend_from_slice(&data[..]),
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        let _ = ch.close().await;
        Ok::<Vec<u8>, String>(out)
    };
    let out = tokio::time::timeout(Duration::from_secs(30), work)
        .await
        .map_err(|_| "검색 시간 초과 (30초)".to_string())??;

    Ok(String::from_utf8_lossy(&out)
        .lines()
        .filter_map(|l| {
            let mut p = l.splitn(4, '\t');
            let (ty, size, mtime, path) = (p.next()?, p.next()?, p.next()?, p.next()?);
            Some(Found {
                path: path.to_string(),
                is_dir: ty == "d",
                is_symlink: ty == "l",
                size: size.parse().unwrap_or(0),
                mtime: mtime.parse::<f64>().unwrap_or(0.0) as u64,
            })
        })
        .collect())
}

// ───────────────────────── 로컬 터미널 (이 맥의 셸) ─────────────────────────

#[tauri::command]
pub async fn local_open(
    app: AppHandle,
    state: State<'_, AppState>,
    cols: u32,
    rows: u32,
) -> R<String> {
    use portable_pty::{native_pty_system, CommandBuilder, PtySize};
    use std::io::Read as _;

    let pair = native_pty_system()
        .openpty(PtySize { rows: rows as u16, cols: cols as u16, pixel_width: 0, pixel_height: 0 })
        .map_err(e2s)?;
    let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/zsh".into());
    let mut cmd = CommandBuilder::new(shell);
    cmd.arg("-l");
    cmd.env("TERM", "xterm-256color");
    if let Some(h) = dirs::home_dir() {
        cmd.cwd(h);
    }
    let child = pair.slave.spawn_command(cmd).map_err(e2s)?;
    drop(pair.slave);
    let mut reader = pair.master.try_clone_reader().map_err(e2s)?;
    let writer = pair.master.take_writer().map_err(e2s)?;

    let id = state.new_id("l");
    state.locals.lock().unwrap().insert(id.clone(), LocalPty { master: pair.master, writer, child });

    let tid = id.clone();
    std::thread::spawn(move || {
        let mut buf = [0u8; 8192];
        loop {
            match reader.read(&mut buf) {
                Ok(0) | Err(_) => break,
                Ok(n) => {
                    let _ = app.emit("term-data", TermData { id: tid.clone(), data: B64.encode(&buf[..n]) });
                }
            }
        }
        let _ = app.emit("term-exit", tid);
    });
    Ok(id)
}

// ───────────────────────── 맥 키체인에 비밀번호 저장 ─────────────────────────

const KEYCHAIN_SERVICE: &str = "ssh-gui";

#[cfg(target_os = "macos")]
#[tauri::command]
pub fn secret_set(profile_id: String, value: String) -> R<()> {
    security_framework::passwords::set_generic_password(KEYCHAIN_SERVICE, &profile_id, value.as_bytes()).map_err(e2s)
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub fn secret_get(profile_id: String) -> R<Option<String>> {
    match security_framework::passwords::get_generic_password(KEYCHAIN_SERVICE, &profile_id) {
        Ok(b) => Ok(Some(String::from_utf8_lossy(&b).to_string())),
        Err(_) => Ok(None), // 없음/접근 거부 모두 "저장된 값 없음"으로 취급
    }
}

#[cfg(target_os = "macos")]
#[tauri::command]
pub fn secret_delete(profile_id: String) -> R<()> {
    let _ = security_framework::passwords::delete_generic_password(KEYCHAIN_SERVICE, &profile_id);
    Ok(())
}

#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub fn secret_set(_profile_id: String, _value: String) -> R<()> { Err("이 OS에서는 키체인 저장을 지원하지 않아요".into()) }
#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub fn secret_get(_profile_id: String) -> R<Option<String>> { Ok(None) }
#[cfg(not(target_os = "macos"))]
#[tauri::command]
pub fn secret_delete(_profile_id: String) -> R<()> { Ok(()) }

// ───────────────────────── 원격 파일 변경 감지 ─────────────────────────

#[derive(Serialize)]
pub struct Stat {
    size: u64,
    mtime: u64,
}

#[tauri::command]
pub async fn sftp_stat(state: State<'_, AppState>, session_id: String, path: String) -> R<Stat> {
    let s = state.session(&session_id).await?;
    let md = s.sftp.metadata(path).await.map_err(e2s)?;
    Ok(Stat {
        size: md.len(),
        mtime: md.modified().ok().and_then(|t| t.duration_since(UNIX_EPOCH).ok()).map(|d| d.as_secs()).unwrap_or(0),
    })
}

// ───────────────────────── 원격 명령 실행 도우미 / 프로세스 ─────────────────────────

/// `sh -s` 의 stdin 으로 스크립트를 흘려 실행하고 (stdout+stderr) 출력을 돌려준다.
async fn run_sh(s: &SshSession, script: &str, secs: u64) -> R<String> {
    let work = async {
        let mut ch = s.handle.channel_open_session().await.map_err(e2s)?;
        ch.exec(true, "sh -s").await.map_err(e2s)?;
        ch.data(script.as_bytes()).await.map_err(e2s)?;
        ch.eof().await.map_err(e2s)?;
        let mut out = Vec::new();
        while let Some(msg) = ch.wait().await {
            match msg {
                ChannelMsg::Data { data } | ChannelMsg::ExtendedData { data, .. } => out.extend_from_slice(&data[..]),
                ChannelMsg::Eof | ChannelMsg::Close => break,
                _ => {}
            }
        }
        let _ = ch.close().await;
        Ok::<String, String>(String::from_utf8_lossy(&out).to_string())
    };
    tokio::time::timeout(Duration::from_secs(secs), work)
        .await
        .map_err(|_| "서버 응답 시간 초과".to_string())?
}

#[tauri::command]
pub async fn sys_procs(state: State<'_, AppState>, session_id: String) -> R<String> {
    let s = state.session(&session_id).await?;
    run_sh(&s, "ps -eo pid=,user=,pcpu=,pmem=,etime=,args= --sort=-pcpu 2>&1 | head -n 150\n", 8).await
}

#[tauri::command]
pub async fn proc_kill(state: State<'_, AppState>, session_id: String, pid: u32, signal: String) -> R<String> {
    if pid < 2 {
        return Err("PID 1 이하는 종료할 수 없어요".into());
    }
    let sig = match signal.as_str() {
        "TERM" | "KILL" | "HUP" | "INT" | "STOP" | "CONT" => signal,
        _ => return Err("지원하지 않는 시그널입니다".into()),
    };
    let s = state.session(&session_id).await?;
    let out = run_sh(&s, &format!("kill -{sig} {pid} 2>&1 && echo __OK__\n"), 8).await?;
    if out.contains("__OK__") { Ok(format!("PID {pid} 에 {sig} 신호를 보냈어요")) } else { Err(out.trim().to_string()) }
}

// ───────────────────────── 서버 간 파일 복사 ─────────────────────────

async fn copy_rec(src: &SftpSession, dst: &SftpSession, from: &str, to: &str, ctx: &Ctx) -> anyhow::Result<()> {
    let md = src.metadata(from).await?;
    if md.is_dir() {
        let _ = dst.create_dir(to).await;
        for e in src.read_dir(from).await? {
            let name = e.file_name().to_string();
            let fp = e.path().to_string();
            let tp = format!("{}/{}", to.trim_end_matches('/'), name);
            Box::pin(copy_rec(src, dst, &fp, &tp, ctx)).await?;
        }
        return Ok(());
    }
    let name = from.rsplit('/').next().unwrap_or(from).to_string();
    let total = md.len();
    let mut r = src.open(from).await?;
    let mut w = dst.create(to).await?;
    let mut buf = vec![0u8; CHUNK];
    let mut done = 0u64;
    loop {
        let n = r.read(&mut buf).await?;
        if n == 0 {
            break;
        }
        w.write_all(&buf[..n]).await?;
        done += n as u64;
        ctx.emit(&name, done, total, "running", String::new());
    }
    w.flush().await?;
    w.shutdown().await?;
    Ok(())
}

/// 한 서버의 파일/폴더를 다른 서버(또는 같은 서버의 다른 경로)로 복사. 내 맥을 거쳐 스트리밍한다.
#[tauri::command]
pub async fn sftp_copy(
    app: AppHandle,
    state: State<'_, AppState>,
    from_session: String,
    to_session: String,
    transfer_id: String,
    paths: Vec<String>,
    dst_dir: String,
) -> R<()> {
    let a = state.session(&from_session).await?;
    let b = state.session(&to_session).await?;
    let ctx = Ctx { app, id: transfer_id };
    for p in paths {
        let name = p.trim_end_matches('/').rsplit('/').next().unwrap_or("copy").to_string();
        let to = format!("{}/{}", dst_dir.trim_end_matches('/'), name);
        if let Err(e) = copy_rec(&a.sftp, &b.sftp, &p, &to, &ctx).await {
            ctx.emit(&name, 0, 0, "error", e.to_string());
            return Err(e.to_string());
        }
    }
    ctx.emit("", 0, 0, "done", String::new());
    Ok(())
}

// ───────────────────────── ~/.ssh/config 가져오기 ─────────────────────────

#[derive(Serialize)]
pub struct SshHost {
    alias: String,
    hostname: String,
    user: String,
    port: u16,
    identity_file: String,
    proxy_jump: String,
}

fn parse_ssh_config(text: &str) -> Vec<SshHost> {
    struct Block {
        patterns: Vec<String>,
        opts: HashMap<String, String>,
    }
    let mut blocks: Vec<Block> = vec![];
    let mut cur: Option<Block> = None;
    for raw in text.lines() {
        let line = raw.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        let Some((k, v)) = line.split_once(|c: char| c.is_whitespace() || c == '=') else { continue };
        let k = k.trim().to_lowercase();
        let v = v.trim_start_matches(|c: char| c.is_whitespace() || c == '=').trim().trim_matches('"').to_string();
        match k.as_str() {
            "host" => {
                if let Some(b) = cur.take() { blocks.push(b); }
                cur = Some(Block { patterns: v.split_whitespace().map(String::from).collect(), opts: HashMap::new() });
            }
            "match" => {
                if let Some(b) = cur.take() { blocks.push(b); } // Match 블록은 지원하지 않음 (옵션 무시)
            }
            _ => {
                if let Some(b) = cur.as_mut() {
                    b.opts.entry(k).or_insert(v);
                }
            }
        }
    }
    if let Some(b) = cur.take() { blocks.push(b); }

    let defaults: Vec<&Block> = blocks.iter().filter(|b| b.patterns.iter().any(|p| p == "*")).collect();
    let mut out = vec![];
    for b in &blocks {
        for pat in &b.patterns {
            if pat.contains(['*', '?', '!']) {
                continue;
            }
            let get = |key: &str| -> String {
                b.opts
                    .get(key)
                    .or_else(|| defaults.iter().find_map(|d| d.opts.get(key)))
                    .cloned()
                    .unwrap_or_default()
            };
            let hostname = get("hostname");
            out.push(SshHost {
                alias: pat.clone(),
                hostname: if hostname.is_empty() { pat.clone() } else { hostname },
                user: { let u = get("user"); if u.is_empty() { std::env::var("USER").unwrap_or_default() } else { u } },
                port: get("port").parse().unwrap_or(22),
                identity_file: get("identityfile").split_whitespace().next().unwrap_or("").to_string(),
                proxy_jump: { let j = get("proxyjump"); if j.eq_ignore_ascii_case("none") { String::new() } else { j } },
            });
        }
    }
    out
}

#[tauri::command]
pub fn ssh_config_hosts() -> R<Vec<SshHost>> {
    let path = dirs::home_dir().ok_or("홈 폴더를 찾지 못했어요")?.join(".ssh").join("config");
    let text = std::fs::read_to_string(&path).map_err(|e| match e.kind() {
        std::io::ErrorKind::NotFound => "~/.ssh/config 파일이 아직 없어요. (ssh config 를 쓴 적이 없다면 정상이에요. 접속은 “＋ 새 접속”으로 추가하세요)".to_string(),
        std::io::ErrorKind::PermissionDenied => "~/.ssh/config 를 읽을 권한이 없어요 (파일 권한을 확인하세요)".to_string(),
        _ => format!("~/.ssh/config 를 읽지 못했어요: {e}"),
    })?;
    Ok(parse_ssh_config(&text))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parses_ssh_config() {
        let cfg = r#"
# 주석
Host *
    User defaultuser
    IdentityFile ~/.ssh/default_key

Host bastion
    HostName 203.0.113.10
    User ec2-user
    Port 2222

Host web1 web2
    HostName=10.0.0.5
    ProxyJump bastion
    IdentityFile ~/.ssh/web_key

Host *.internal !secret
    User nobody

Match host foo
    User ignored
"#;
        let h = parse_ssh_config(cfg);
        let names: Vec<_> = h.iter().map(|x| x.alias.as_str()).collect();
        assert_eq!(names, vec!["bastion", "web1", "web2"]); // 와일드카드/부정 패턴 제외

        let b = &h[0];
        assert_eq!((b.hostname.as_str(), b.user.as_str(), b.port), ("203.0.113.10", "ec2-user", 2222));
        assert_eq!(b.identity_file, "~/.ssh/default_key"); // Host * 기본값 상속

        let w = &h[1];
        assert_eq!(w.hostname, "10.0.0.5"); // '=' 구분자
        assert_eq!(w.user, "defaultuser");
        assert_eq!(w.port, 22);
        assert_eq!(w.proxy_jump, "bastion");
        assert_eq!(w.identity_file, "~/.ssh/web_key"); // 자기 설정이 기본값보다 우선
    }
}
