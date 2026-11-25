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

struct ExtHandle {
    session_id: String,
    task: tokio::task::JoinHandle<()>,
    decide: mpsc::UnboundedSender<bool>,
    dir: PathBuf,
}

struct TunnelHandle {
    session_id: String,
    task: tokio::task::JoinHandle<()>,
}

struct LocalPty {
    master: Box<dyn portable_pty::MasterPty + Send>,
    writer: Box<dyn std::io::Write + Send>,
    child: Box<dyn portable_pty::Child + Send + Sync>,
}

#[derive(Default)]
pub struct AppState {
    sessions: Arc<Mutex<HashMap<String, Arc<SshSession>>>>,
    tunnels: Mutex<HashMap<String, TunnelHandle>>,
    logs: Mutex<HashMap<String, mpsc::UnboundedSender<()>>>,
    exts: Mutex<HashMap<String, ExtHandle>>,
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
    state.tunnels.lock().await.retain(|_, t| {
        if t.session_id == session_id { t.task.abort(); false } else { true }
    });
    state.exts.lock().await.retain(|_, e| {
        if e.session_id == session_id { e.task.abort(); let _ = std::fs::remove_dir_all(&e.dir); false } else { true }
    });
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
    #[serde(default)]
    tmux: bool, // 접속 시 tmux 세션에 자동 연결
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

// ───────────────────────── 로그 보기 (실시간) ─────────────────────────

/// 로그 후보를 한 번에 모아서 돌려준다: 최근 수정된 /var/log 파일, 실행 중인 systemd 서비스, Docker 컨테이너
#[tauri::command]
pub async fn log_candidates(state: State<'_, AppState>, session_id: String) -> R<String> {
    let s = state.session(&session_id).await?;
    let script = r#"echo '##FILES'
find /var/log -maxdepth 3 -type f -readable \( -name '*.log' -o -name 'syslog' -o -name 'messages' -o -name 'secure' \) -printf '%T@\t%s\t%p\n' 2>/dev/null | sort -rn | head -80
echo '##UNITS'
systemctl list-units --type=service --state=running --no-legend --plain 2>/dev/null | awk '{print $1}' | head -150
echo '##DOCKER'
command -v docker >/dev/null 2>&1 && docker ps -a --format '{{.Names}}\t{{.Status}}\t{{.Image}}' 2>/dev/null | head -100
echo '##END'
"#;
    run_sh(&s, script, 15).await
}

#[derive(Serialize, Clone)]
struct LogData {
    id: String,
    data: String, // base64
}

/// kind: "file" | "journal" | "docker". 끝없이 흘러나오는 로그를 "log-data" 이벤트로 보낸다.
#[tauri::command]
pub async fn log_start(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    view_id: String,
    kind: String,
    target: String,
) -> R<()> {
    let name_ok = |t: &str, extra: &[char]| !t.is_empty() && t.len() <= 200 && t.chars().all(|c| c.is_ascii_alphanumeric() || extra.contains(&c));
    let script = match kind.as_str() {
        "file" => {
            if !target.starts_with('/') || target.len() > 1024 { return Err("로그 파일은 /로 시작하는 전체 경로여야 해요".into()); }
            format!("exec tail -n 300 -F -- {} 2>&1", sh_quote(&target))
        }
        "journal" => {
            if !name_ok(&target, &['.', '_', '-', '@', ':']) { return Err("서비스 이름이 올바르지 않아요".into()); }
            format!("exec journalctl -u {} -n 300 -f --no-pager -o short-iso 2>&1", sh_quote(&target))
        }
        "docker" => {
            if !name_ok(&target, &['.', '_', '-']) { return Err("컨테이너 이름이 올바르지 않아요".into()); }
            format!("exec docker logs --tail 300 -f --timestamps {} 2>&1", sh_quote(&target))
        }
        _ => return Err("지원하지 않는 로그 종류예요".into()),
    };
    let s = state.session(&session_id).await?;
    let (a1, a2, vid1, vid2) = (app.clone(), app.clone(), view_id.clone(), view_id.clone());
    let tx = spawn_log(
        &s,
        &script,
        Arc::new(move |d: &[u8]| { let _ = a1.emit("log-data", LogData { id: vid1.clone(), data: B64.encode(d) }); }),
        Arc::new(move || { let _ = a2.emit("log-end", vid2.clone()); }),
    )
    .await?;
    state.logs.lock().await.insert(view_id, tx);
    Ok(())
}

/// 원격에서 `script` 를 pty 로 실행하고 출력을 계속 on_data 로 보낸다. 반환된 sender 로 신호를 보내면 중지.
/// pty 를 쓰는 이유: 채널을 닫을 때 서버의 tail/journalctl 도 SIGHUP 으로 바로 끝나게 하려는 것.
async fn spawn_log(
    s: &SshSession,
    script: &str,
    on_data: Arc<dyn Fn(&[u8]) + Send + Sync>,
    on_end: Arc<dyn Fn() + Send + Sync>,
) -> R<mpsc::UnboundedSender<()>> {
    let mut ch = s.handle.channel_open_session().await.map_err(e2s)?;
    ch.request_pty(false, "dumb", 220, 50, 0, 0, &[]).await.map_err(e2s)?;
    ch.exec(true, format!("sh -c {}", sh_quote(script))).await.map_err(e2s)?;
    let (tx, mut rx) = mpsc::unbounded_channel::<()>();
    tokio::spawn(async move {
        loop {
            tokio::select! {
                _ = rx.recv() => { let _ = ch.close().await; break; }
                msg = ch.wait() => match msg {
                    Some(ChannelMsg::Data { data }) | Some(ChannelMsg::ExtendedData { data, .. }) => on_data(&data[..]),
                    Some(ChannelMsg::Close) | None => break,
                    _ => {}
                },
            }
        }
        on_end();
    });
    Ok(tx)
}

#[tauri::command]
pub async fn log_stop(state: State<'_, AppState>, view_id: String) -> R<()> {
    if let Some(tx) = state.logs.lock().await.remove(&view_id) {
        let _ = tx.send(());
    }
    Ok(())
}

// ───────────────────────── Docker ─────────────────────────

#[tauri::command]
pub async fn docker_list(state: State<'_, AppState>, session_id: String) -> R<String> {
    let s = state.session(&session_id).await?;
    let script = r#"command -v docker >/dev/null 2>&1 || { echo '##NODOCKER'; exit 0; }
docker ps -a --format '{{.ID}}\t{{.Names}}\t{{.Image}}\t{{.State}}\t{{.Status}}\t{{.Ports}}' 2>&1
echo '##STATS'
docker stats --no-stream --format '{{.Name}}\t{{.CPUPerc}}\t{{.MemUsage}}' 2>&1
"#;
    run_sh(&s, script, 25).await
}

#[tauri::command]
pub async fn docker_action(state: State<'_, AppState>, session_id: String, name: String, action: String) -> R<String> {
    if !matches!(action.as_str(), "start" | "stop" | "restart") {
        return Err("지원하지 않는 동작이에요".into());
    }
    if name.is_empty() || name.len() > 200 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
        return Err("컨테이너 이름이 올바르지 않아요".into());
    }
    let s = state.session(&session_id).await?;
    script_ok(run_sh(&s, &format!("docker {action} {} 2>&1 && echo __OK__\n", sh_quote(&name)), 90).await?)
}

// ───────────────────────── 외부 에디터로 열기 (저장하면 서버에 자동 반영) ─────────────────────────

#[derive(Clone, Debug, PartialEq)]
enum ExtEvent {
    Saved,
    Conflict,
    Error(String),
}

#[derive(Serialize, Clone)]
struct ExtEv {
    id: String,
    name: String,
    kind: &'static str, // saved | conflict | error
    message: String,
}

async fn remote_stat(sess: &SshSession, path: &str) -> Option<(u64, u64)> {
    let md = sess.sftp.metadata(path).await.ok()?;
    Some((md.len(), md.modified().ok()?.duration_since(UNIX_EPOCH).ok()?.as_secs()))
}

async fn download_to(sftp: &SftpSession, remote: &str, local: &Path) -> anyhow::Result<()> {
    let mut r = sftp.open(remote).await?;
    let mut f = tokio::fs::File::create(local).await?;
    tokio::io::copy(&mut r, &mut f).await?;
    f.flush().await?;
    Ok(())
}

async fn upload_file(sftp: &SftpSession, local: &Path, remote: &str) -> anyhow::Result<()> {
    let mut f = tokio::fs::File::open(local).await?;
    let mut w = sftp.create(remote).await?;
    tokio::io::copy(&mut f, &mut w).await?;
    w.flush().await?;
    w.shutdown().await?;
    Ok(())
}

/// 로컬 파일이 저장될 때마다 서버로 올린다. 내가 연 뒤에 서버 쪽이 바뀌었으면 덮어쓰기 전에 Conflict 를 알리고
/// `decide` 로 답(true=덮어쓰기, false=이번엔 건너뛰기)이 올 때까지 기다린다.
fn spawn_ext_watch(
    sessions: Sessions,
    sid: String,
    remote: String,
    local: PathBuf,
    baseline: Option<(u64, u64)>,
    interval: Duration,
    on_event: Arc<dyn Fn(ExtEvent) + Send + Sync>,
    mut decide: mpsc::UnboundedReceiver<bool>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mtime = |p: PathBuf| async move { tokio::fs::metadata(p).await.ok().and_then(|m| m.modified().ok()) };
        let mut base = baseline;
        let mut last = mtime(local.clone()).await;
        loop {
            tokio::time::sleep(interval).await;
            let Some(mt) = mtime(local.clone()).await else { continue };
            if Some(mt) == last {
                continue;
            }
            // 에디터가 아직 쓰는 중일 수 있으니 한 번 더 기다렸다가 값이 안정되면 올린다
            tokio::time::sleep(interval / 2).await;
            if mtime(local.clone()).await != Some(mt) {
                continue;
            }
            last = Some(mt);
            let Some(sess) = sessions.lock().await.get(&sid).cloned() else {
                on_event(ExtEvent::Error("서버와 연결이 끊겨 있어요".into()));
                continue;
            };
            if let (Some(b), Some(c)) = (base, remote_stat(&sess, &remote).await) {
                if b != c {
                    on_event(ExtEvent::Conflict);
                    match decide.recv().await {
                        Some(true) => {}
                        Some(false) => continue,
                        None => return,
                    }
                }
            }
            match upload_file(&sess.sftp, &local, &remote).await {
                Ok(()) => {
                    base = remote_stat(&sess, &remote).await;
                    on_event(ExtEvent::Saved);
                }
                Err(e) => on_event(ExtEvent::Error(e.to_string())),
            }
        }
    })
}

fn ext_cache_root() -> Option<PathBuf> {
    dirs::cache_dir().map(|d| d.join("ssh-gui").join("ext"))
}

/// 앱 시작 때 지난 실행에서 남은 외부 편집 임시 파일을 지운다 (감시는 재시작하면 이어지지 않으므로)
pub fn cleanup_ext_cache() {
    if let Some(r) = ext_cache_root() {
        let _ = std::fs::remove_dir_all(r);
    }
}

/// 설치돼 있는 외부 에디터 후보 목록
#[tauri::command]
pub fn ext_apps() -> Vec<String> {
    let known = ["Visual Studio Code", "Cursor", "Zed", "Sublime Text", "Nova", "BBEdit", "TextMate", "IntelliJ IDEA", "PyCharm", "WebStorm", "TextEdit"];
    let mut roots = vec![PathBuf::from("/Applications"), PathBuf::from("/System/Applications")];
    if let Some(h) = dirs::home_dir() {
        roots.push(h.join("Applications"));
    }
    known
        .iter()
        .filter(|n| roots.iter().any(|r| r.join(format!("{n}.app")).exists()))
        .map(|n| n.to_string())
        .collect()
}

/// 서버 파일을 임시 폴더로 내려받아 외부 에디터로 열고, 저장될 때마다 서버에 올린다.
/// `app_name` 이 비어 있으면 시스템 기본 앱으로 연다.
#[tauri::command]
pub async fn ext_open(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    remote_path: String,
    app_name: String,
) -> R<String> {
    if !app_name.chars().all(|c| c.is_alphanumeric() || c == ' ' || c == '.' || c == '-' || c == '_' || c == '+') {
        return Err("에디터 이름이 올바르지 않아요".into());
    }
    let s = state.session(&session_id).await?;
    let md = s.sftp.metadata(remote_path.as_str()).await.map_err(e2s)?;
    if md.is_dir() {
        return Err("폴더는 외부 에디터로 열 수 없어요".into());
    }
    if md.len() > 200 * 1024 * 1024 {
        return Err("파일이 너무 커요 (최대 200 MB)".into());
    }
    let id = state.new_id("x");
    let dir = ext_cache_root().ok_or("캐시 폴더를 찾지 못했어요")?.join(&id);
    std::fs::create_dir_all(&dir).map_err(e2s)?;
    let name = remote_path.rsplit('/').next().unwrap_or("file").to_string();
    let local = dir.join(&name);
    download_to(&s.sftp, &remote_path, &local).await.map_err(e2s)?;
    let baseline = remote_stat(&s, &remote_path).await;

    let (dtx, drx) = mpsc::unbounded_channel::<bool>();
    let (app2, id2, name2) = (app.clone(), id.clone(), name.clone());
    let on_event: Arc<dyn Fn(ExtEvent) + Send + Sync> = Arc::new(move |ev| {
        let (kind, message) = match ev {
            ExtEvent::Saved => ("saved", String::new()),
            ExtEvent::Conflict => ("conflict", String::new()),
            ExtEvent::Error(m) => ("error", m),
        };
        let _ = app2.emit("ext-event", ExtEv { id: id2.clone(), name: name2.clone(), kind, message });
    });
    let task = spawn_ext_watch(state.sessions.clone(), session_id.clone(), remote_path, local.clone(), baseline, Duration::from_millis(700), on_event, drx);
    state.exts.lock().await.insert(id.clone(), ExtHandle { session_id, task, decide: dtx, dir });

    let mut cmd = std::process::Command::new("open");
    if !app_name.is_empty() {
        cmd.arg("-a").arg(&app_name);
    }
    cmd.arg(&local);
    let status = cmd.status().map_err(e2s)?;
    if !status.success() {
        return Err(format!("“{}” 으로 열지 못했어요. 에디터 이름을 확인하세요", if app_name.is_empty() { "기본 앱" } else { &app_name }));
    }
    Ok(id)
}

#[tauri::command]
pub async fn ext_resolve(state: State<'_, AppState>, id: String, overwrite: bool) -> R<()> {
    if let Some(e) = state.exts.lock().await.get(&id) {
        let _ = e.decide.send(overwrite);
    }
    Ok(())
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

    // HOME 환경변수를 바꾸는 테스트끼리 동시에 돌지 않게 한다
    static HOME_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn generated_key_is_valid_openssh() {
        let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("sshgui-test-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        unsafe { std::env::set_var("HOME", &tmp) }; // 테스트 전용: 실제 ~/.ssh 를 건드리지 않게

        // 패스프레이즈 없는 키
        let k = key_generate("t1".into(), "".into()).unwrap();
        assert!(k.public.starts_with("ssh-ed25519 "), "{}", k.public);
        let path = tmp.join(".ssh/ssh-gui_t1");
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(&path).unwrap().permissions().mode() & 0o777, 0o600);
        // ssh-keygen 이 개인키에서 같은 공개키를 뽑아내야 한다
        let out = std::process::Command::new("ssh-keygen").args(["-y", "-f"]).arg(&path).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));
        let derived = String::from_utf8_lossy(&out.stdout).trim().to_string();
        assert_eq!(derived.split_whitespace().take(2).collect::<Vec<_>>(), k.public.split_whitespace().take(2).collect::<Vec<_>>());
        // 같은 이름 재생성은 거부
        assert!(key_generate("t1".into(), "".into()).is_err());

        // 패스프레이즈 있는 키: 맞는 패스프레이즈로만 열려야 한다
        key_generate("t2".into(), "s3cret-pass".into()).unwrap();
        let p2 = tmp.join(".ssh/ssh-gui_t2");
        assert!(russh::keys::load_secret_key(&p2, None).is_err());
        assert!(russh::keys::load_secret_key(&p2, Some("wrong")).is_err());
        assert!(russh::keys::load_secret_key(&p2, Some("s3cret-pass")).is_ok());
        let out = std::process::Command::new("ssh-keygen").args(["-y", "-P", "s3cret-pass", "-f"]).arg(&p2).output().unwrap();
        assert!(out.status.success(), "{}", String::from_utf8_lossy(&out.stderr));

        // 잘못된 이름 거부
        assert!(key_generate("../evil".into(), "".into()).is_err());
        let _ = std::fs::remove_dir_all(&tmp);
    }

    struct Sshd(std::process::Child);
    impl Drop for Sshd {
        fn drop(&mut self) { let _ = self.0.kill(); let _ = self.0.wait(); }
    }

    fn free_port() -> u16 {
        std::net::TcpListener::bind("127.0.0.1:0").unwrap().local_addr().unwrap().port()
    }

    /// 127.0.0.1 에만 열리는 일회용 sshd (임시 호스트키, 공개키 인증만)
    fn start_sshd(dir: &std::path::Path, port: u16, authorized: &std::path::Path) -> Sshd {
        let hk = dir.join("host_key");
        assert!(std::process::Command::new("ssh-keygen").args(["-q", "-t", "ed25519", "-N", "", "-f"]).arg(&hk).status().unwrap().success());
        let conf = dir.join("sshd_config");
        std::fs::write(&conf, format!(
            "Port {port}\nListenAddress 127.0.0.1\nHostKey {}\nAuthorizedKeysFile {}\nPasswordAuthentication no\nKbdInteractiveAuthentication no\nPubkeyAuthentication yes\nUsePAM no\nStrictModes no\nPidFile {}\nLogLevel ERROR\nAllowTcpForwarding yes\nSubsystem sftp /usr/libexec/sftp-server\n",
            hk.display(), authorized.display(), dir.join("sshd.pid").display()
        )).unwrap();
        let child = std::process::Command::new("/usr/sbin/sshd").args(["-D", "-e", "-f"]).arg(&conf)
            .stdout(std::process::Stdio::null()).stderr(std::process::Stdio::null()).spawn().expect("sshd 실행 실패");
        for _ in 0..50 {
            if std::net::TcpStream::connect(("127.0.0.1", port)).is_ok() { return Sshd(child); }
            std::thread::sleep(std::time::Duration::from_millis(100));
        }
        panic!("sshd 가 시작되지 않았어요");
    }

    #[tokio::test]
    async fn real_sshd_end_to_end() {
        let _g = HOME_LOCK.lock().unwrap_or_else(|e| e.into_inner());
        let tmp = std::env::temp_dir().join(format!("sshgui-e2e-{}", std::process::id()));
        std::fs::create_dir_all(&tmp).unwrap();
        unsafe { std::env::set_var("HOME", &tmp) }; // known_hosts 등이 실제 ~/.ssh 에 쓰이지 않게

        let k = key_generate("e2e".into(), "".into()).unwrap();
        let authorized = tmp.join("authorized_keys");
        std::fs::write(&authorized, format!("{}\n", k.public)).unwrap();
        let port = free_port();
        let _sshd = start_sshd(&tmp, port, &authorized);
        let user = std::env::var("USER").unwrap_or_else(|_| String::from_utf8_lossy(&std::process::Command::new("id").arg("-un").output().unwrap().stdout).trim().to_string());

        let args = |jump: Option<Box<ConnectArgs>>, key: &str| ConnectArgs {
            host: "127.0.0.1".into(), port, username: user.clone(),
            auth: Auth::Key { path: key.into(), passphrase: None }, jump,
        };

        // 1) 키 인증 + 호스트 키 TOFU(처음엔 등록, 두 번째는 일치 확인)
        let sess = establish(args(None, &k.path)).await.expect("키 로그인 실패");
        let sess2 = establish(args(None, &k.path)).await.expect("두 번째 로그인(known_hosts 일치 확인) 실패");
        drop(sess2);
        // 틀린 키는 거부되어야 함
        let bad = key_generate("bad".into(), "".into()).unwrap();
        assert!(establish(args(None, &bad.path)).await.is_err(), "등록 안 된 키로 로그인되면 안 됨");

        // 2) 원격 명령 실행 (run_sh)
        let out = run_sh(&sess, "echo hello-$((1+2))\n", 10).await.unwrap();
        assert_eq!(out.trim(), "hello-3");
        // 3) 권한 변경/압축 스크립트를 원격에서 실제로 실행
        let work = tmp.join("work dir");
        std::fs::create_dir_all(work.join("sub")).unwrap();
        std::fs::write(work.join("a b.txt"), "data").unwrap();
        let wq = sh_quote(&work.to_string_lossy());
        let o = run_sh(&sess, &format!("cd {wq} && {{ tar -czf 'x.tar.gz' -- 'a b.txt' 'sub'; }} 2>&1 && echo __OK__\n"), 20).await.unwrap();
        assert!(script_ok(o).is_ok());
        assert!(work.join("x.tar.gz").exists());
        let o = run_sh(&sess, &format!("{{ chmod -R 700 {wq}/sub {wq}/'a b.txt'; }} 2>&1 && echo __OK__\n"), 20).await.unwrap();
        assert!(script_ok(o).is_ok());
        use std::os::unix::fs::PermissionsExt;
        assert_eq!(std::fs::metadata(work.join("a b.txt")).unwrap().permissions().mode() & 0o777, 0o700);

        // 4) SFTP: 쓰고, 읽고, 목록
        sess.sftp.write(format!("{}/via-sftp.txt", work.display()), b"sftp-ok").await.unwrap();
        assert_eq!(sess.sftp.read(format!("{}/via-sftp.txt", work.display())).await.unwrap(), b"sftp-ok");
        let names: Vec<String> = sess.sftp.read_dir(work.to_string_lossy().to_string()).await.unwrap().map(|e| e.file_name().to_string()).collect();
        assert!(names.contains(&"a b.txt".to_string()) && names.contains(&"via-sftp.txt".to_string()), "{names:?}");

        // 5) 터널링: 내 맥 포트 → (SSH) → 에코 서버
        let echo = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let echo_port = echo.local_addr().unwrap().port();
        tokio::spawn(async move {
            loop {
                let Ok((mut c, _)) = echo.accept().await else { break };
                tokio::spawn(async move {
                    let (mut r, mut w) = c.split();
                    let _ = tokio::io::copy(&mut r, &mut w).await;
                });
            }
        });
        let sessions: Sessions = Arc::new(Mutex::new(HashMap::new()));
        sessions.lock().await.insert("s1".into(), Arc::new(sess));
        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let tport = listener.local_addr().unwrap().port();
        let errs = Arc::new(std::sync::Mutex::new(Vec::<String>::new()));
        let e2 = errs.clone();
        let task = spawn_tunnel(sessions.clone(), "s1".into(), listener, "127.0.0.1".into(), echo_port, Arc::new(move |m| e2.lock().unwrap().push(m)));
        let mut c = tokio::net::TcpStream::connect(("127.0.0.1", tport)).await.unwrap();
        c.write_all(b"ping-through-tunnel").await.unwrap();
        let mut buf = vec![0u8; 19];
        tokio::time::timeout(Duration::from_secs(5), c.read_exact(&mut buf)).await.expect("터널 응답 시간 초과").unwrap();
        assert_eq!(&buf, b"ping-through-tunnel");
        // 연결 거부되는 서버쪽 포트 → 에러 콜백
        let dead = free_port();
        let l2 = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let p2 = l2.local_addr().unwrap().port();
        let e3 = errs.clone();
        let task2 = spawn_tunnel(sessions.clone(), "s1".into(), l2, "127.0.0.1".into(), dead, Arc::new(move |m| e3.lock().unwrap().push(m)));
        let _ = tokio::net::TcpStream::connect(("127.0.0.1", p2)).await.unwrap();
        for _ in 0..30 { if !errs.lock().unwrap().is_empty() { break; } tokio::time::sleep(Duration::from_millis(100)).await; }
        assert!(!errs.lock().unwrap().is_empty(), "연결 거부 시 오류가 보고되어야 함");
        task.abort(); task2.abort();

        // 6) 점프 호스트: 같은 sshd 를 경유해서 다시 접속 (ProxyJump)
        let via = establish(args(Some(Box::new(args(None, &k.path))), &k.path)).await.expect("점프 호스트 경유 접속 실패");
        assert_eq!(run_sh(&via, "echo via-jump\n", 10).await.unwrap().trim(), "via-jump");

        // 7) 실시간 로그: tail -F 로 새 줄이 흘러오는지, 중지하면 서버의 tail 이 끝나는지
        let logf = tmp.join("app.log");
        std::fs::write(&logf, "line-1\nline-2\n").unwrap();
        let got = Arc::new(std::sync::Mutex::new(String::new()));
        let ended = Arc::new(std::sync::atomic::AtomicBool::new(false));
        let (g2, e2) = (got.clone(), ended.clone());
        let stop = spawn_log(
            &via,
            &format!("exec tail -n 300 -F -- {} 2>&1", sh_quote(&logf.to_string_lossy())),
            Arc::new(move |d: &[u8]| g2.lock().unwrap().push_str(&String::from_utf8_lossy(d))),
            Arc::new(move || e2.store(true, std::sync::atomic::Ordering::SeqCst)),
        ).await.unwrap();
        let wait_for = |needle: &'static str| {
            let got = got.clone();
            async move {
                for _ in 0..50 { if got.lock().unwrap().contains(needle) { return true; } tokio::time::sleep(Duration::from_millis(100)).await; }
                false
            }
        };
        assert!(wait_for("line-2").await, "기존 로그 줄이 와야 함: {:?}", got.lock().unwrap());
        use std::io::Write as _;
        std::fs::OpenOptions::new().append(true).open(&logf).unwrap().write_all(b"NEW-LINE-ERROR\n").unwrap();
        assert!(wait_for("NEW-LINE-ERROR").await, "실시간으로 추가된 줄이 와야 함: {:?}", got.lock().unwrap());
        let tail_pat = format!("tail -n 300 -F -- {}", logf.display());
        let alive = || std::process::Command::new("pgrep").args(["-f", &tail_pat]).output().map(|o| o.status.success()).unwrap_or(false);
        assert!(alive(), "스트리밍 중에는 서버에 tail 프로세스가 있어야 함");
        stop.send(()).unwrap();
        for _ in 0..50 { if ended.load(std::sync::atomic::Ordering::SeqCst) && !alive() { break; } tokio::time::sleep(Duration::from_millis(100)).await; }
        assert!(ended.load(std::sync::atomic::Ordering::SeqCst), "중지하면 종료 콜백이 불려야 함");
        assert!(!alive(), "중지하면 서버의 tail 프로세스도 끝나야 함 (좀비 방지)");

        // 8) 외부 에디터 동기화: 로컬 저장 → 서버 반영, 서버가 바뀌었으면 충돌 확인
        let remote = tmp.join("ext-remote.txt");
        std::fs::write(&remote, "v1").unwrap();
        let local = tmp.join("ext-local.txt");
        let (rs, rl) = (remote.to_string_lossy().to_string(), local.clone());
        let sess1 = sessions.lock().await.get("s1").cloned().unwrap();
        download_to(&sess1.sftp, &rs, &rl).await.unwrap();
        assert_eq!(std::fs::read_to_string(&local).unwrap(), "v1");
        let events = Arc::new(std::sync::Mutex::new(Vec::<ExtEvent>::new()));
        let ev2 = events.clone();
        let (dtx, drx) = mpsc::unbounded_channel::<bool>();
        let baseline = remote_stat(&sess1, &rs).await;
        let watch = spawn_ext_watch(sessions.clone(), "s1".into(), rs.clone(), rl.clone(), baseline, Duration::from_millis(100), Arc::new(move |e| ev2.lock().unwrap().push(e)), drx);
        let wait_ev = |n: usize| {
            let events = events.clone();
            async move { for _ in 0..80 { if events.lock().unwrap().len() >= n { return true; } tokio::time::sleep(Duration::from_millis(100)).await; } false }
        };
        tokio::time::sleep(Duration::from_millis(300)).await;
        std::fs::write(&local, "v2").unwrap();                       // 에디터에서 저장
        assert!(wait_ev(1).await, "저장 이벤트가 와야 함");
        assert_eq!(events.lock().unwrap()[0], ExtEvent::Saved);
        assert_eq!(std::fs::read_to_string(&remote).unwrap(), "v2", "서버에 반영돼야 함");

        tokio::time::sleep(Duration::from_millis(1100)).await;       // 서버 쪽 mtime(초 단위)이 달라지도록
        std::fs::write(&remote, "changed-on-server").unwrap();       // 다른 사람이 서버에서 수정
        std::fs::write(&local, "v3-local").unwrap();
        assert!(wait_ev(2).await, "충돌 이벤트가 와야 함");
        assert_eq!(events.lock().unwrap()[1], ExtEvent::Conflict);
        dtx.send(false).unwrap();                                    // 이번엔 건너뛰기
        tokio::time::sleep(Duration::from_millis(600)).await;
        assert_eq!(std::fs::read_to_string(&remote).unwrap(), "changed-on-server", "건너뛰면 서버 내용이 그대로여야 함");
        assert_eq!(events.lock().unwrap().len(), 2, "건너뛴 뒤 추가 이벤트가 없어야 함");

        std::fs::write(&local, "v4-local").unwrap();                 // 다시 저장 → 충돌 → 덮어쓰기
        assert!(wait_ev(3).await);
        assert_eq!(events.lock().unwrap()[2], ExtEvent::Conflict);
        dtx.send(true).unwrap();
        assert!(wait_ev(4).await, "덮어쓰기 후 저장 이벤트가 와야 함");
        assert_eq!(events.lock().unwrap()[3], ExtEvent::Saved);
        assert_eq!(std::fs::read_to_string(&remote).unwrap(), "v4-local");
        watch.abort();

        let _ = std::fs::remove_dir_all(&tmp);
    }
}

// ───────────────────────── tmux ─────────────────────────

fn valid_name(n: &str) -> bool {
    !n.is_empty() && n.len() <= 64 && n.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_')
}

/// tmux 세션 안에서 현재 보고 있는 칸의 작업 폴더 (파일 패널 "경로 따라가기"용).
/// tmux 안에서는 셸의 OSC 7 경로 알림이 바깥으로 전달되지 않아 직접 물어본다.
#[tauri::command]
pub async fn tmux_cwd(state: State<'_, AppState>, session_id: String, name: String) -> R<String> {
    if !valid_name(&name) {
        return Err("잘못된 tmux 세션 이름".into());
    }
    let s = state.session(&session_id).await?;
    let out = run_sh(&s, &format!("tmux display-message -p -t {} '#{{pane_current_path}}' 2>/dev/null\n", sh_quote(&name)), 5).await?;
    Ok(out.trim().to_string())
}

// ───────────────────────── 권한 변경 / 압축 / 압축 풀기 ─────────────────────────

fn script_ok(out: String) -> R<String> {
    if out.contains("__OK__") { Ok(out.replace("__OK__", "").trim().to_string()) } else { Err(out.trim().to_string()) }
}

fn valid_mode(m: &str) -> bool {
    (3..=4).contains(&m.len()) && m.chars().all(|c| ('0'..='7').contains(&c))
}

fn valid_owner(o: &str) -> bool {
    let ok = |p: &str| p.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-');
    match o.split_once(':') {
        Some((u, g)) => ok(u) && ok(g) && !(u.is_empty() && g.is_empty()),
        None => !o.is_empty() && ok(o),
    }
}

/// 권한(chmod)과 선택적으로 소유자(chown)를 바꾼다. `recursive` 면 폴더 안까지 적용.
#[tauri::command]
pub async fn remote_chmod(
    state: State<'_, AppState>,
    session_id: String,
    paths: Vec<String>,
    mode: String,
    recursive: bool,
    owner: String,
) -> R<String> {
    if !valid_mode(&mode) {
        return Err("권한은 755 같은 8진수 3~4자리여야 해요".into());
    }
    if !owner.is_empty() && !valid_owner(&owner) {
        return Err("소유자 형식이 올바르지 않아요 (예: www-data:www-data)".into());
    }
    if paths.is_empty() {
        return Err("대상이 없어요".into());
    }
    let s = state.session(&session_id).await?;
    let list = paths.iter().map(|p| sh_quote(p)).collect::<Vec<_>>().join(" ");
    let r = if recursive { "-R " } else { "" };
    // 경로는 항상 절대 경로(/ 로 시작)라서 옵션으로 오해될 일이 없다. (BSD chmod 는 모드 뒤의 -- 를 파일로 읽는다)
    let mut cmd = format!("chmod {r}{mode} {list}");
    if !owner.is_empty() {
        cmd.push_str(&format!(" && chown {r}{owner} {list}"));
    }
    script_ok(run_sh(&s, &format!("{{ {cmd}; }} 2>&1 && echo __OK__\n"), 120).await?)
}

/// `cwd` 안의 `names` 를 하나의 압축 파일로 묶는다. format: "tgz" | "zip"
#[tauri::command]
pub async fn remote_archive(
    state: State<'_, AppState>,
    session_id: String,
    cwd: String,
    names: Vec<String>,
    out: String,
    format: String,
) -> R<String> {
    if out.is_empty() || out.contains('/') || out.starts_with('-') {
        return Err("압축 파일 이름이 올바르지 않아요".into());
    }
    if names.is_empty() || names.iter().any(|n| n.contains('/')) {
        return Err("대상이 올바르지 않아요".into());
    }
    let s = state.session(&session_id).await?;
    let list = names.iter().map(|n| sh_quote(n)).collect::<Vec<_>>().join(" ");
    let o = sh_quote(&out);
    let make = match format.as_str() {
        "tgz" => format!("tar -czf {o} -- {list}"),
        "zip" => format!("command -v zip >/dev/null 2>&1 || {{ echo '서버에 zip 이 설치돼 있지 않아요 (tar.gz 를 쓰세요)'; exit 1; }}; zip -qr {o} -- {list}"),
        _ => return Err("지원하지 않는 형식이에요".into()),
    };
    let script = format!(
        "cd {} || exit 1\n[ -e {o} ] && {{ echo '같은 이름의 파일이 이미 있어요'; exit 1; }}\n{{ {make}; }} 2>&1 && echo __OK__\n",
        sh_quote(&cwd)
    );
    script_ok(run_sh(&s, &script, 900).await?)
}

/// 압축 파일을 `dest` 폴더에 푼다 (같은 이름의 파일은 덮어쓴다).
#[tauri::command]
pub async fn remote_extract(
    state: State<'_, AppState>,
    session_id: String,
    path: String,
    dest: String,
) -> R<String> {
    let lower = path.to_lowercase();
    let (p, d) = (sh_quote(&path), sh_quote(&dest));
    let cmd = if lower.ends_with(".tar.gz") || lower.ends_with(".tgz") {
        format!("tar -xzf {p} -C {d}")
    } else if lower.ends_with(".tar.bz2") || lower.ends_with(".tbz2") {
        format!("tar -xjf {p} -C {d}")
    } else if lower.ends_with(".tar.xz") || lower.ends_with(".txz") {
        format!("tar -xJf {p} -C {d}")
    } else if lower.ends_with(".tar") {
        format!("tar -xf {p} -C {d}")
    } else if lower.ends_with(".zip") {
        format!("command -v unzip >/dev/null 2>&1 || {{ echo '서버에 unzip 이 설치돼 있지 않아요'; exit 1; }}; unzip -oq {p} -d {d}")
    } else if lower.ends_with(".gz") {
        format!("gunzip -kf {p}")
    } else {
        return Err("지원하지 않는 압축 형식이에요 (.tar.gz .tgz .tar.bz2 .tar.xz .tar .zip .gz)".into());
    };
    let s = state.session(&session_id).await?;
    script_ok(run_sh(&s, &format!("{{ {cmd}; }} 2>&1 && echo __OK__\n"), 900).await?)
}

// ───────────────────────── SSH 키 만들기 / 서버에 등록 ─────────────────────────

#[derive(Serialize)]
pub struct KeyInfo {
    path: String,   // 개인키 경로 (~ 로 시작하는 표기)
    public: String, // 공개키 한 줄
}

/// 내 맥에 ed25519 키 한 쌍을 만든다 (~/.ssh/ssh-gui_<name>, 권한 600). 이미 있으면 거부.
#[tauri::command]
pub fn key_generate(name: String, passphrase: String) -> R<KeyInfo> {
    use russh::keys::{Algorithm, PrivateKey};
    use std::io::Write as _;
    use std::os::unix::fs::OpenOptionsExt as _;

    if name.is_empty() || name.len() > 60 || !name.chars().all(|c| c.is_ascii_alphanumeric() || c == '.' || c == '_' || c == '-') {
        return Err("키 이름에는 영문/숫자/.-_ 만 쓸 수 있어요".into());
    }
    let dir = dirs::home_dir().ok_or("홈 폴더를 찾지 못했어요")?.join(".ssh");
    std::fs::create_dir_all(&dir).map_err(e2s)?;
    {
        use std::os::unix::fs::PermissionsExt as _;
        let _ = std::fs::set_permissions(&dir, std::fs::Permissions::from_mode(0o700));
    }
    let path = dir.join(format!("ssh-gui_{name}"));
    let pub_path = dir.join(format!("ssh-gui_{name}.pub"));
    if path.exists() || pub_path.exists() {
        return Err(format!("~/.ssh/ssh-gui_{name} 이(가) 이미 있어요. 다른 이름을 쓰세요"));
    }

    let mut rng = rand::rng();
    let mut key = PrivateKey::random(&mut rng, Algorithm::Ed25519).map_err(e2s)?;
    key.set_comment(format!("ssh-gui@{name}"));
    let key = if passphrase.is_empty() { key } else { key.encrypt(&mut rng, passphrase.as_bytes()).map_err(e2s)? };

    let pem = key.to_openssh(russh::keys::ssh_key::LineEnding::LF).map_err(e2s)?;
    let public = key.public_key().to_openssh().map_err(e2s)?;

    let mut f = std::fs::OpenOptions::new().write(true).create_new(true).mode(0o600).open(&path).map_err(e2s)?;
    f.write_all(pem.as_bytes()).map_err(e2s)?;
    std::fs::write(&pub_path, format!("{public}\n")).map_err(e2s)?;
    Ok(KeyInfo { path: format!("~/.ssh/ssh-gui_{name}"), public })
}

/// 현재 접속한 서버의 ~/.ssh/authorized_keys 에 공개키를 추가한다 (이미 있으면 그대로 둠).
#[tauri::command]
pub async fn key_install(state: State<'_, AppState>, session_id: String, public_key: String) -> R<String> {
    let k = public_key.trim();
    let ok_type = ["ssh-ed25519 ", "ssh-rsa ", "ecdsa-sha2-"].iter().any(|t| k.starts_with(t));
    if !ok_type || k.contains('\n') || k.contains('\r') {
        return Err("올바른 공개키 한 줄이 아니에요".into());
    }
    let s = state.session(&session_id).await?;
    let q = sh_quote(k);
    let script = format!(
        "umask 077\nmkdir -p \"$HOME/.ssh\" && touch \"$HOME/.ssh/authorized_keys\" || exit 1\n\
         grep -qxF {q} \"$HOME/.ssh/authorized_keys\" || printf '%s\\n' {q} >> \"$HOME/.ssh/authorized_keys\"\n\
         chmod 700 \"$HOME/.ssh\"; chmod 600 \"$HOME/.ssh/authorized_keys\"\n\
         echo __OK__\n"
    );
    script_ok(run_sh(&s, &script, 20).await?)
}

/// 접속 정보로 실제 로그인이 되는지만 확인하고 바로 끊는다 (키 전환 후 검증용).
#[tauri::command]
pub async fn ssh_test(args: ConnectArgs) -> R<()> {
    let Opened { handle, .. } = open_handle(&args).await?;
    let _ = handle.disconnect(Disconnect::ByApplication, "", "en").await;
    Ok(())
}

// ───────────────────────── SSH 터널링 (로컬 포트 포워딩) ─────────────────────────

type Sessions = Arc<Mutex<HashMap<String, Arc<SshSession>>>>;

/// listener 로 들어오는 연결마다 SSH 채널(direct-tcpip)을 열어 양방향으로 이어 준다.
fn spawn_tunnel(
    sessions: Sessions,
    sid: String,
    listener: tokio::net::TcpListener,
    host: String,
    port: u16,
    on_err: Arc<dyn Fn(String) + Send + Sync>,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        let mut conns = tokio::task::JoinSet::new();
        loop {
            tokio::select! {
                acc = listener.accept() => {
                    let Ok((mut tcp, peer)) = acc else { continue };
                    let (sessions, sid, host, on_err) = (sessions.clone(), sid.clone(), host.clone(), on_err.clone());
                    conns.spawn(async move {
                        // 재접속으로 세션이 교체돼도 새 연결을 쓰도록, 연결마다 현재 세션을 찾는다
                        let Some(sess) = sessions.lock().await.get(&sid).cloned() else { return };
                        match sess.handle.channel_open_direct_tcpip(host.clone(), port as u32, "127.0.0.1", peer.port() as u32).await {
                            Ok(ch) => {
                                let mut st = ch.into_stream();
                                let _ = tokio::io::copy_bidirectional(&mut tcp, &mut st).await;
                            }
                            Err(e) => on_err(format!("서버가 {host}:{port} 에 연결하지 못했어요: {e}")),
                        }
                    });
                }
                Some(_) = conns.join_next(), if !conns.is_empty() => {}
            }
        }
    })
}

#[derive(Serialize, Clone)]
struct TunnelErr {
    id: String,
    message: String,
}

/// 내 맥의 127.0.0.1:`local_port` 로 들어온 연결을, 서버를 통해 `remote_host:remote_port` 로 이어 준다.
/// (ssh -L local_port:remote_host:remote_port 와 같은 동작. 내 맥 안에서만 열리고 외부 접속은 받지 않는다.)
#[tauri::command]
pub async fn tunnel_start(
    app: AppHandle,
    state: State<'_, AppState>,
    session_id: String,
    tunnel_id: String,
    local_port: u16,
    remote_host: String,
    remote_port: u16,
) -> R<()> {
    if local_port < 1024 {
        return Err("내 맥 포트는 1024 이상으로 정하세요 (1024 미만은 관리자 권한이 필요해요)".into());
    }
    if remote_host.is_empty() || remote_host.len() > 253 || remote_port == 0 {
        return Err("서버 쪽 주소/포트가 올바르지 않아요".into());
    }
    state.session(&session_id).await?;
    if state.tunnels.lock().await.contains_key(&tunnel_id) {
        return Err("이미 켜져 있는 터널이에요".into());
    }
    let listener = tokio::net::TcpListener::bind(("127.0.0.1", local_port)).await.map_err(|e| {
        if e.kind() == std::io::ErrorKind::AddrInUse {
            format!("포트 {local_port} 은(는) 이미 다른 프로그램이 쓰고 있어요. 다른 포트를 고르세요")
        } else {
            e2s(e)
        }
    })?;

    let (app2, tid) = (app.clone(), tunnel_id.clone());
    let on_err: Arc<dyn Fn(String) + Send + Sync> = Arc::new(move |message| {
        let _ = app2.emit("tunnel-error", TunnelErr { id: tid.clone(), message });
    });
    let task = spawn_tunnel(state.sessions.clone(), session_id.clone(), listener, remote_host, remote_port, on_err);
    state.tunnels.lock().await.insert(tunnel_id, TunnelHandle { session_id, task });
    Ok(())
}

#[tauri::command]
pub async fn tunnel_stop(state: State<'_, AppState>, tunnel_id: String) -> R<()> {
    if let Some(t) = state.tunnels.lock().await.remove(&tunnel_id) {
        t.task.abort(); // JoinSet 이 함께 정리되어 진행 중인 연결도 끊긴다
    }
    Ok(())
}
