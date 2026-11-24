import { invoke } from "@tauri-apps/api/core";
import { listen } from "@tauri-apps/api/event";
import { getCurrentWebview } from "@tauri-apps/api/webview";
import { open as openDialog } from "@tauri-apps/plugin-dialog";
import { TermGroup, routeData, routeExit, type TP } from "./terminal";
import { startMonitor } from "./monitor";
import { createEditor, type Ed } from "./editor";

// ───────────── 타입 ─────────────
interface Entry { name: string; path: string; is_dir: boolean; is_symlink: boolean; size: number; mtime: number; perms: string; octal: string; owner: string }
interface Found { path: string; is_dir: boolean; is_symlink: boolean; size: number; mtime: number }
interface Profile { id: string; name: string; host: string; port: number; username: string; auth_type: "password" | "key"; key_path: string; remember?: boolean; group?: string; jump?: string }
interface Sess {
  sid: string; title: string; group: TermGroup;
  connArgs: Record<string, unknown>;
  state: "ok" | "wait" | "lost"; reconnecting: boolean; closed: boolean; banner: HTMLElement | null;
  cwd: string; entries: Entry[]; selected: Set<string>; lastClicked: string | null;
  filter: string; results: Found[] | null; qEl: HTMLInputElement;
  follow: boolean;
  el: HTMLElement; listEl: HTMLElement; pathEl: HTMLInputElement; statusEl: HTMLElement;
  tabEl: HTMLElement; stopMonitor?: () => void;
}
interface TransferEv { id: string; name: string; done: number; total: number; state: "running" | "done" | "error"; message: string }

// ───────────── 유틸 ─────────────
const $ = <T extends HTMLElement>(sel: string, root: ParentNode = document) => root.querySelector(sel) as T;
const esc = (s: string) => s.replace(/[&<>"']/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;", "'": "&#39;" }[c]!));
const fmtSize = (n: number) => {
  if (n < 1024) return `${n} B`;
  const u = ["KB", "MB", "GB", "TB"]; let i = -1;
  do { n /= 1024; i++; } while (n >= 1024 && i < u.length - 1);
  return `${n.toFixed(n < 10 ? 1 : 0)} ${u[i]}`;
};
const fmtDate = (t: number) => {
  if (!t) return "";
  const d = new Date(t * 1000), p = (x: number) => String(x).padStart(2, "0");
  return `${String(d.getFullYear()).slice(2)}-${p(d.getMonth() + 1)}-${p(d.getDate())} ${p(d.getHours())}:${p(d.getMinutes())}`;
};
const errMsg = (e: unknown) => (typeof e === "string" ? e : (e as Error)?.message ?? String(e));
const parentOf = (p: string) => { const t = p.replace(/\/+$/, ""); const i = t.lastIndexOf("/"); return i <= 0 ? "/" : t.slice(0, i); };
const joinPath = (a: string, b: string) => (a.endsWith("/") ? a + b : `${a}/${b}`);

function toast(msg: string, error = false) {
  const el = document.createElement("div");
  el.className = "toast" + (error ? " error" : "");
  el.textContent = msg;
  $("#toasts").appendChild(el);
  setTimeout(() => el.remove(), error ? 6000 : 2500);
}

// 열려 있는 모달들 (맨 위 것부터 ESC 로 닫힘)
const modalStack: (() => void)[] = [];

function modal(html: string, onCancel?: () => void) {
  const ov = document.createElement("div");
  ov.className = "overlay";
  ov.innerHTML = `<div class="modal">${html}</div>`;
  document.body.appendChild(ov);
  let closed = false;
  const close = () => {
    if (closed) return;
    closed = true;
    ov.remove();
    const i = modalStack.indexOf(cancel);
    if (i >= 0) modalStack.splice(i, 1);
  };
  const cancel = () => { const was = closed; close(); if (!was) onCancel?.(); };
  modalStack.push(cancel);
  ov.addEventListener("mousedown", (e) => { if (e.target === ov) cancel(); });
  return { ov, box: $(".modal", ov), close };
}

function askInput(title: string, value = "", secret = false): Promise<string | null> {
  return new Promise((resolve) => {
    const m = modal(`<h3>${esc(title)}</h3><input id="ai" type="${secret ? "password" : "text"}" value="${esc(value)}"/><div class="btns"><button id="c">취소</button><button id="o" class="primary">확인</button></div>`, () => resolve(null));
    const input = $<HTMLInputElement>("#ai", m.box);
    input.focus(); input.select();
    const done = (v: string | null) => { m.close(); resolve(v); };
    $("#c", m.box).onclick = () => done(null);
    $("#o", m.box).onclick = () => done(input.value.trim() || null);
    input.onkeydown = (e) => { if (e.key === "Enter") done(input.value.trim() || null); if (e.key === "Escape") done(null); };
  });
}

function askConfirm(title: string, detail = ""): Promise<boolean> {
  return new Promise((resolve) => {
    const m = modal(`<h3>${esc(title)}</h3><div style="color:var(--dim)">${esc(detail)}</div><div class="btns"><button id="c">취소</button><button id="o" class="primary">확인</button></div>`, () => resolve(false));
    $("#c", m.box).onclick = () => { m.close(); resolve(false); };
    $("#o", m.box).onclick = () => { m.close(); resolve(true); };
    $<HTMLButtonElement>("#o", m.box).focus();
  });
}

// ───────────── 전역 상태 ─────────────
const sessions = new Map<string, Sess>();
interface Local { id: string; title: string; el: HTMLElement; group: TermGroup }
const locals = new Map<string, Local>();
let localSeq = 0;
let active: string = "home";
let profiles: Profile[] = [];
let downloadDir = "~/Downloads";
let xferSeq = 0;
const xferSess = new Map<string, Sess>();

// ───────────── 탭/화면 전환 ─────────────
interface Stat { size: number; mtime: number }
interface Editor { id: string; sess: Sess; path: string; name: string; ed: Ed; el: HTMLElement; dirty: boolean; stat: Stat | null; changed: boolean }
const editors = new Map<string, Editor>();

function renderTabs() {
  const tabs = $("#tabs");
  tabs.innerHTML = "";
  const home = document.createElement("div");
  home.className = "tab" + (active === "home" ? " active" : "");
  home.textContent = "🏠 접속 목록";
  home.onclick = () => activate("home");
  tabs.appendChild(home);
  for (const s of sessions.values()) {
    const t = document.createElement("div");
    t.className = "tab" + (active === s.sid ? " active" : "");
    t.innerHTML = `<span class="dot ${s.state}"></span><span>${esc(s.title)}</span><span class="x">✕</span>`;
    t.onclick = () => activate(s.sid);
    $(".x", t).onclick = (e) => { e.stopPropagation(); closeSession(s); };
    s.tabEl = t;
    tabs.appendChild(t);
    // 이 서버에서 연 편집기 탭
    for (const ed of editors.values()) {
      if (ed.sess !== s) continue;
      const et = document.createElement("div");
      et.className = "tab editor-tab" + (active === ed.id ? " active" : "");
      et.title = `${s.title}:${ed.path}`;
      et.innerHTML = `<span>✎</span><span>${esc(ed.name)}${ed.dirty ? " ●" : ""}</span><span class="x">✕</span>`;
      et.onclick = () => activate(ed.id);
      $(".x", et).onclick = (e) => { e.stopPropagation(); void closeEditor(ed); };
      tabs.appendChild(et);
    }
  }
  for (const l of locals.values()) {
    const t = document.createElement("div");
    t.className = "tab" + (active === l.id ? " active" : "");
    t.innerHTML = `<span>💻</span><span>${esc(l.title)}</span><span class="x">✕</span>`;
    t.onclick = () => activate(l.id);
    $(".x", t).onclick = (e) => { e.stopPropagation(); closeLocal(l); };
    tabs.appendChild(t);
  }
}

function activate(id: string) {
  active = id;
  $("#home").classList.toggle("show", id === "home");
  for (const s of sessions.values()) s.el.classList.toggle("show", s.sid === id);
  for (const e of editors.values()) e.el.classList.toggle("show", e.id === id);
  for (const l of locals.values()) l.el.classList.toggle("show", l.id === id);
  renderTabs();
  const s = sessions.get(id);
  if (s) requestAnimationFrame(() => { s.group.fitAll(); s.group.focus(); });
  const l = locals.get(id);
  if (l) requestAnimationFrame(() => { l.group.fitAll(); l.group.focus(); });
  const e = editors.get(id);
  if (e) { requestAnimationFrame(() => { e.ed.view.requestMeasure(); e.ed.view.focus(); }); void checkRemote(e); }
}

async function closeSession(s: Sess) {
  for (const e of [...editors.values()]) if (e.sess === s) { if (!(await closeEditor(e, s))) return; }
  s.stopMonitor?.();
  try { await invoke("ssh_disconnect", { sessionId: s.sid }); } catch { /* ignore */ }
  s.closed = true;
  hideBanner(s);
  s.group.destroy();
  s.el.remove();
  sessions.delete(s.sid);
  activate(sessions.size ? [...sessions.keys()].pop()! : "home");
}

// ───────────── 원격 파일 편집기 ─────────────
async function openEditor(s: Sess, entry: Entry) {
  const id = `e:${s.sid}:${entry.path}`;
  if (editors.has(id)) { activate(id); return; }
  let text: string;
  const stat0 = await invoke<Stat>("sftp_stat", { sessionId: s.sid, path: entry.path }).catch(() => null);
  try {
    text = await invoke<string>("sftp_read_text", { sessionId: s.sid, path: entry.path });
  } catch (er) {
    const msg = errMsg(er);
    const why = msg.startsWith("TOO_BIG") ? `파일이 너무 커서(${fmtSize(Number(msg.split(":")[1]))}) 에디터로 열 수 없어요 (최대 5 MB).`
      : msg === "BINARY" ? "바이너리 파일이라 에디터로 열 수 없어요."
      : msg === "NOT_UTF8" ? "UTF-8 텍스트가 아니라서 열면 내용이 깨질 수 있어요."
      : `파일을 열 수 없어요: ${msg}`;
    if (/^(TOO_BIG|BINARY|NOT_UTF8)/.test(msg) && (await askConfirm(why, "대신 다운로드할까요?"))) void download(s, [entry.path]);
    else if (!/^(TOO_BIG|BINARY|NOT_UTF8)/.test(msg)) toast(why, true);
    return;
  }

  const el = document.createElement("div");
  el.className = "pane editor";
  el.innerHTML = `<div class="ehead"><span class="epath">${esc(s.title)} : ${esc(entry.path)}</span><span class="ewarn" style="display:none">⚠ 서버에서 이 파일이 바뀌었어요 <button class="reload">다시 불러오기</button></span><span class="estate"></span><button class="save primary">저장 (⌘S)</button></div><div class="ebody"></div>`;
  $("#stage").appendChild(el);

  const rec = {} as Editor;
  const save = async () => {
    try {
      // 덮어쓰기 전에 서버 쪽이 바뀌었는지 확인 (다른 사람/프로그램의 수정을 날리지 않도록)
      const now = await invoke<Stat>("sftp_stat", { sessionId: s.sid, path: entry.path }).catch(() => null);
      if (now && rec.stat && (now.mtime !== rec.stat.mtime || now.size !== rec.stat.size)) {
        const ok = await askConfirm("서버에서 이 파일이 바뀌었어요", "내가 연 뒤에 누군가(또는 어떤 프로그램이) 이 파일을 수정했어요. 지금 저장하면 그 변경이 사라집니다. 그래도 덮어쓸까요?");
        if (!ok) return;
      }
      await invoke("sftp_write_text", { sessionId: s.sid, path: entry.path, content: rec.ed.value() });
      rec.ed.markSaved();
      rec.stat = await invoke<Stat>("sftp_stat", { sessionId: s.sid, path: entry.path }).catch(() => rec.stat);
      setChanged(rec, false);
      toast(`저장했어요: ${entry.name}`);
    } catch (er) { toast(`저장 실패: ${errMsg(er)}`, true); }
  };
  const ed = createEditor($(".ebody", el), text, entry.path, {
    onDirty: (d) => {
      rec.dirty = d;
      $(".estate", el).textContent = d ? "● 수정됨" : "";
      renderTabs();
    },
    onSave: () => void save(),
  });
  Object.assign(rec, { id, sess: s, path: entry.path, name: entry.name, ed, el, dirty: false, stat: stat0, changed: false });
  $(".reload", el).onclick = async () => {
    if (rec.dirty && !(await askConfirm("내가 수정한 내용이 사라져요", "서버 버전으로 다시 불러올까요?"))) return;
    try {
      const st = await invoke<Stat>("sftp_stat", { sessionId: s.sid, path: entry.path });
      rec.ed.replace(await invoke<string>("sftp_read_text", { sessionId: s.sid, path: entry.path }));
      rec.stat = st;
      setChanged(rec, false);
      toast("서버 버전을 불러왔어요");
    } catch (er) { toast(errMsg(er), true); }
  };
  $(".save", el).onclick = () => void save();
  editors.set(id, rec);
  activate(id);
}

function setChanged(e: Editor, v: boolean) {
  e.changed = v;
  $(".ewarn", e.el).style.display = v ? "" : "none";
}

/** 서버에서 파일이 바뀌었는지 확인하고 경고를 띄운다. */
async function checkRemote(e: Editor) {
  if (e.sess.reconnecting || e.sess.closed || !e.stat) return;
  const st = await invoke<Stat>("sftp_stat", { sessionId: e.sess.sid, path: e.path }).catch(() => null);
  if (!st) return;
  const changed = st.mtime !== e.stat.mtime || st.size !== e.stat.size;
  if (changed !== e.changed) setChanged(e, changed);
}

async function closeEditor(e: Editor, skipActivate?: Sess): Promise<boolean> {
  if (e.dirty && !(await askConfirm(`“${e.name}”에 저장하지 않은 변경이 있어요`, "저장하지 않고 닫을까요?"))) return false;
  e.ed.view.destroy();
  e.el.remove();
  editors.delete(e.id);
  if (!skipActivate) activate(sessions.has(e.sess.sid) ? e.sess.sid : "home");
  return true;
}

// ───────────── 홈 / 접속 다이얼로그 ─────────────
async function loadProfiles() {
  profiles = await invoke<Profile[]>("profile_list");
  renderHome();
}

let homeQuery = "";
const collapsed = new Set<string>(JSON.parse(localStorage.getItem("collapsedGroups") || "[]"));
const UNGROUPED = "미분류";

function renderHome() {
  const home = $("#home");
  home.innerHTML = `<h2>저장된 접속 <button id="add" class="primary">＋ 새 접속</button><button id="local">💻 로컬 터미널</button><button id="imp" title="~/.ssh/config 에 있는 호스트를 불러옵니다">⤓ ssh config 가져오기</button></h2>
    <input id="hq" class="hq" placeholder="🔍 이름 · 호스트 · 사용자 · 폴더로 검색  (⌘K)" spellcheck="false"/>
    <div class="groups"></div>`;
  const hq = $<HTMLInputElement>("#hq", home);
  hq.value = homeQuery;
  hq.oninput = () => { homeQuery = hq.value; renderGroups(); };
  hq.onkeydown = (e) => { if (e.key === "Escape" && hq.value) { hq.value = ""; homeQuery = ""; renderGroups(); e.stopPropagation(); } };
  $("#add", home).onclick = () => connectDialog(null);
  $("#local", home).onclick = () => void openLocal();
  $("#imp", home).onclick = () => void importSshConfig();
  renderGroups();
}

function renderGroups() {
  const box = $("#home .groups");
  box.innerHTML = "";
  const q = homeQuery.trim().toLowerCase();
  const list = q ? profiles.filter((p) => `${p.name} ${p.host} ${p.username} ${p.group ?? ""}`.toLowerCase().includes(q)) : profiles;
  if (!list.length) {
    box.innerHTML = `<div class="empty">${profiles.length ? "검색 결과가 없어요." : "저장된 접속이 없어요. “＋ 새 접속”으로 추가하거나 ssh config 를 가져오세요."}</div>`;
    return;
  }
  const byGroup = new Map<string, Profile[]>();
  for (const p of list) { const g = p.group?.trim() || UNGROUPED; (byGroup.get(g) ?? byGroup.set(g, []).get(g)!).push(p); }
  const names = [...byGroup.keys()].sort((a, b) => (a === UNGROUPED ? 1 : b === UNGROUPED ? -1 : a.localeCompare(b)));
  const showHeaders = names.length > 1 || names[0] !== UNGROUPED;
  for (const g of names) {
    const items = byGroup.get(g)!.sort((a, b) => a.name.localeCompare(b.name));
    const open = !!q || !collapsed.has(g);
    const sec = document.createElement("div");
    sec.className = "group";
    if (showHeaders) {
      const h = document.createElement("div");
      h.className = "ghead";
      h.innerHTML = `<span class="arrow">${open ? "▾" : "▸"}</span> 📁 ${esc(g)} <span class="cnt">${items.length}</span>`;
      h.onclick = () => { collapsed.has(g) ? collapsed.delete(g) : collapsed.add(g); localStorage.setItem("collapsedGroups", JSON.stringify([...collapsed])); renderGroups(); };
      sec.appendChild(h);
    }
    if (open) {
      const cards = document.createElement("div");
      cards.className = "cards";
      for (const p of items) cards.appendChild(profileCard(p));
      sec.appendChild(cards);
    }
    box.appendChild(sec);
  }
}

function profileCard(p: Profile): HTMLElement {
  const c = document.createElement("div");
  c.className = "card";
  c.innerHTML = `<div class="name">${esc(p.name)}<span class="badge">${p.auth_type === "key" ? "키" : "비밀번호"}</span>${p.remember ? '<span class="badge" title="키체인에 저장됨">🔑</span>' : ""}${p.jump ? `<span class="badge" title="점프 호스트: ${esc(p.jump)}">↪ ${esc(p.jump)}</span>` : ""}</div>
    <div class="sub">${esc(p.username)}@${esc(p.host)}:${p.port}</div>
    <div class="acts"><button class="edit" title="편집">✎</button><button class="del danger" title="삭제">🗑</button></div>`;
  c.onclick = () => (p.remember ? void quickConnect(p) : connectDialog(p));
  $(".edit", c).onclick = (e) => { e.stopPropagation(); connectDialog(p); };
  $(".del", c).onclick = async (e) => {
    e.stopPropagation();
    if (await askConfirm(`“${p.name}” 삭제할까요?`)) { profiles = await invoke<Profile[]>("profile_delete", { id: p.id }); renderHome(); }
  };
  return c;
}

/** 키체인에 저장된 접속은 클릭 한 번으로 바로 연결. 실패하면 설정 창을 연다. */
async function quickConnect(p: Profile) {
  const v = await invoke<string | null>("secret_get", { profileId: p.id });
  if (!v?.startsWith("v:")) return connectDialog(p); // 키체인에서 못 읽음
  const secret = v.slice(2);
  const auth = p.auth_type === "password"
    ? { type: "password", password: secret }
    : { type: "key", path: p.key_path || "~/.ssh/id_ed25519", passphrase: secret || null };
  toast(`${p.name} 연결 중…`);
  try {
    const args: Record<string, unknown> = { host: p.host, port: p.port, username: p.username, auth };
    if (p.jump) args.jump = await resolveJump(p.jump);
    await openSession(p.name, args);
  }
  catch (e) { toast(`연결 실패: ${errMsg(e)}`, true); connectDialog(p); }
}

function connectDialog(p: Profile | null) {
  const m = modal(`
    <h3>${p ? "접속" : "새 접속"}</h3>
    <div class="field"><label>이름</label><input id="name" placeholder="예: 회사 서버"/></div>
    <div class="field grid2"><div><label>호스트</label><input id="host" placeholder="192.168.0.10 / example.com"/></div><div><label>포트</label><input id="port" value="22"/></div></div>
    <div class="field"><label>사용자</label><input id="user"/></div>
    <div class="field grid2b"><div><label>폴더 (선택)</label><input id="group" list="glist" placeholder="예: 회사 / 개인"/><datalist id="glist">${[...new Set(profiles.map((x) => x.group).filter(Boolean))].map((x) => `<option value="${esc(x!)}">`).join("")}</datalist></div><div><label>점프 호스트 (선택)</label><input id="jump" placeholder="예: bastion"/></div></div>
    <div class="field"><label>인증 방식</label><select id="auth"><option value="password">비밀번호</option><option value="key">키 파일</option></select></div>
    <div id="pw-box"><div class="field"><label>비밀번호</label><input id="pw" type="password"/></div></div>
    <div id="key-box" style="display:none">
      <div class="field"><label>키 파일 경로</label><input id="key" value="~/.ssh/id_ed25519"/></div>
      <div class="field"><label>패스프레이즈 (없으면 비워두기)</label><input id="pp" type="password"/></div>
    </div>
    <label class="check"><input id="save" type="checkbox" checked/> 접속 정보 저장</label>
    <label class="check"><input id="remember" type="checkbox"/> 비밀번호/패스프레이즈를 맥 키체인에 저장 (다음부터 클릭 한 번으로 접속)</label>
    <div class="err" id="err"></div>
    <div class="btns"><button id="cancel">취소</button><button id="go" class="primary">연결</button></div>`);
  const g = (id: string) => $<HTMLInputElement>("#" + id, m.box);
  const auth = $<HTMLSelectElement>("#auth", m.box);
  const sync = () => {
    $("#pw-box", m.box).style.display = auth.value === "password" ? "" : "none";
    $("#key-box", m.box).style.display = auth.value === "key" ? "" : "none";
  };
  auth.onchange = sync;
  if (p) {
    g("name").value = p.name; g("host").value = p.host; g("port").value = String(p.port);
    g("user").value = p.username; g("group").value = p.group ?? ""; g("jump").value = p.jump ?? ""; auth.value = p.auth_type; g("key").value = p.key_path || "~/.ssh/id_ed25519";
    sync();
    if (p.remember) {
      g("remember").checked = true;
      void invoke<string | null>("secret_get", { profileId: p.id }).then((v) => { if (v?.startsWith("v:")) (auth.value === "password" ? g("pw") : g("pp")).value = v.slice(2); });
    }
  }
  (p ? (auth.value === "password" ? g("pw") : g("pp")) : g("host")).focus();
  $("#cancel", m.box).onclick = m.close;

  const go = $<HTMLButtonElement>("#go", m.box);
  const run = async () => {
    const host = g("host").value.trim(), user = g("user").value.trim();
    const port = parseInt(g("port").value, 10) || 22;
    const err = $("#err", m.box);
    if (!host || !user) { err.textContent = "호스트와 사용자를 입력하세요."; return; }
    const type = auth.value as "password" | "key";
    const authArg = type === "password"
      ? { type: "password", password: g("pw").value }
      : { type: "key", path: g("key").value.trim(), passphrase: g("pp").value || null };
    go.disabled = true; go.textContent = "연결 중…"; err.textContent = "";
    try {
      const name = g("name").value.trim() || `${user}@${host}`;
      if ($<HTMLInputElement>("#save", m.box).checked) {
        const pid = p?.id ?? crypto.randomUUID();
        const remember = g("remember").checked;
        profiles = await invoke<Profile[]>("profile_save", {
          profile: { id: pid, name, host, port, username: user, auth_type: type, key_path: type === "key" ? g("key").value.trim() : "", remember, group: g("group").value.trim(), jump: g("jump").value.trim() },
        });
        if (remember) await invoke("secret_set", { profileId: pid, value: "v:" + (type === "password" ? g("pw").value : g("pp").value) });
        else if (p?.remember) await invoke("secret_delete", { profileId: pid });
        renderHome();
      }
      const args: Record<string, unknown> = { host, port, username: user, auth: authArg };
      const jumpSpec = g("jump").value.trim();
      if (jumpSpec) args.jump = await resolveJump(jumpSpec);
      await openSession(name, args);
      m.close();
    } catch (e) {
      err.textContent = errMsg(e);
      go.disabled = false; go.textContent = "연결";
    }
  };
  go.onclick = run;
  m.box.addEventListener("keydown", (e) => { if (e.key === "Enter") run(); });
}

// ───────────── 세션 열기 ─────────────
async function openSession(title: string, args: Record<string, unknown>) {
  const sid = await invoke<string>("ssh_connect", { args });

  const el = document.createElement("div");
  el.className = "pane";
  el.innerHTML = `
    <div class="main">
    <div class="files">
      <div class="fbar"><button class="up" title="상위 폴더">↑</button><input class="path" spellcheck="false"/><button class="ref" title="새로고침">⟳</button><button class="upl" title="업로드">⬆</button><button class="mk" title="새 폴더">＋📁</button></div>
      <div class="opts">
        <label><input type="checkbox" class="fol" checked/> 터미널 경로 따라가기</label>
        <label><input type="checkbox" class="hsz"/> 크기 숨기기</label>
        <label><input type="checkbox" class="hdt"/> 날짜 숨기기</label>
        <label><input type="checkbox" class="spm"/> 권한 보기</label>
        <label>더블클릭 <select class="dblmode"><option value="ask">매번 묻기</option><option value="edit">에디터로 열기</option><option value="download">다운로드</option></select></label>
      </div>
      <div class="fsearch"><input class="q" placeholder="🔍 이름 검색 (입력하면 현재 폴더 필터)" spellcheck="false"/><button class="deep" title="하위 폴더까지 서버에서 검색 (Enter)">하위 검색</button></div>
      <div class="flist"></div>
      <div class="fstatus"></div>
    </div>
    <div class="gutter"></div>
    <div class="termwrap">${SPLIT_BUTTONS}</div>
    </div>
    <div class="statusbar"><div class="stitems"></div><button class="procbtn" title="실행 중인 프로세스 보기/종료">📋 프로세스</button></div>`;
  $("#stage").appendChild(el);

  const group = new TermGroup($(".termwrap", el), {
    open: (cols, rows) => invoke<string>("term_open", { sessionId: sid, cols, rows }),
    onCwd: (p) => { if (s.follow && p !== s.cwd) void navigate(s, p, true); },
    injectHook: () => s.follow,
    onPaneExit: (g, tp) => void onPaneExit(s, g, tp),
  });
  const s: Sess = {
    sid, title, group, connArgs: args, state: "ok", reconnecting: false, closed: false, banner: null,
    cwd: "/", entries: [], selected: new Set(), lastClicked: null, follow: true, filter: "", results: null, qEl: $<HTMLInputElement>(".q", el),
    el, listEl: $(".flist", el), pathEl: $<HTMLInputElement>(".path", el), statusEl: $(".fstatus", el), tabEl: el,
  };
  sessions.set(sid, s);
  activate(sid);

  wireSplit(group, el);
  await group.addPane();

  wireFilePanel(s);
  wireSearch(s);
  const home = await invoke<string>("sftp_home", { sessionId: sid });
  await navigate(s, home);
  s.stopMonitor = startMonitor(sid, $(".stitems", el));
  $(".procbtn", el).onclick = () => openProcs(s);
  group.focus();
}

const SPLIT_BUTTONS = `<div class="tctl"><button class="snip" title="스니펫 (자주 쓰는 명령)">⚡</button><button class="splitr" title="터미널 좌우 분할">⬌</button><button class="splitd" title="터미널 상하 분할">⬍</button></div>`;

function wireSplit(group: TermGroup, root: HTMLElement) {
  const add = (dir: "row" | "column") => void group.addPane(dir).catch((e) => toast(`터미널을 열 수 없어요: ${errMsg(e)}`, true));
  $(".splitr", root).onclick = () => add("row");
  $(".splitd", root).onclick = () => add("column");
  $(".snip", root).onclick = (e) => snippetMenu(group, e);
}

// ───────────── 스니펫 / 매크로 ─────────────
interface Snippet { id: string; name: string; cmd: string; enter: boolean }

function loadSnips(): Snippet[] {
  try { const v = JSON.parse(localStorage.getItem("snippets") || "null"); if (Array.isArray(v)) return v; } catch { /* 기본값 사용 */ }
  const seed: Snippet[] = [
    { id: crypto.randomUUID(), name: "docker ps -a", cmd: "docker ps -a", enter: true },
    { id: crypto.randomUUID(), name: "디스크 사용량", cmd: "df -h", enter: true },
    { id: crypto.randomUUID(), name: "메모리", cmd: "free -h", enter: true },
  ];
  localStorage.setItem("snippets", JSON.stringify(seed));
  return seed;
}
const saveSnips = (l: Snippet[]) => localStorage.setItem("snippets", JSON.stringify(l));

function runSnippet(group: TermGroup, sn: Snippet) {
  const tp = group.focused ?? group.panes[0];
  if (!tp) return;
  void invoke("term_write", { termId: tp.termId, data: sn.cmd + (sn.enter ? "\n" : "") });
  tp.term.focus();
}

function snippetMenu(group: TermGroup, ev: MouseEvent) {
  const items: [string, () => void][] = loadSnips().map((sn) => [sn.name, () => runSnippet(group, sn)]);
  if (items.length) items.push(["-", () => {}]);
  items.push(["스니펫 관리…", openSnippetManager]);
  popMenu(items, ev);
}

function openSnippetManager() {
  const m = modal(`
    <h3>스니펫 관리</h3>
    <div class="sn-list"></div>
    <div class="sn-form">
      <input id="sn-name" placeholder="이름 (메뉴에 표시)"/>
      <textarea id="sn-cmd" rows="3" placeholder="실행할 명령  예: tail -f /var/log/nginx/error.log" spellcheck="false"></textarea>
      <label class="check"><input id="sn-enter" type="checkbox" checked/> 선택하면 바로 실행 (Enter 자동 입력)</label>
    </div>
    <div class="btns"><button id="sn-new">새로 입력</button><button id="sn-save" class="primary">저장</button><button id="x">닫기</button></div>`);
  let editing: string | null = null;
  const name = $<HTMLInputElement>("#sn-name", m.box), cmd = $<HTMLTextAreaElement>("#sn-cmd", m.box), enter = $<HTMLInputElement>("#sn-enter", m.box);
  const list = $(".sn-list", m.box);
  const reset = () => { editing = null; name.value = ""; cmd.value = ""; enter.checked = true; name.focus(); };
  const render = () => {
    const all = loadSnips();
    list.innerHTML = all.length ? "" : `<div class="empty" style="padding:8px 0">저장된 스니펫이 없어요.</div>`;
    for (const sn of all) {
      const r = document.createElement("div");
      r.className = "sn-row" + (editing === sn.id ? " sel" : "");
      r.innerHTML = `<span class="n">${esc(sn.name)}</span><span class="c">${esc(sn.cmd.split("\n")[0])}</span><button class="e" title="편집">✎</button><button class="d danger" title="삭제">🗑</button>`;
      $(".e", r).onclick = () => { editing = sn.id; name.value = sn.name; cmd.value = sn.cmd; enter.checked = sn.enter; render(); };
      $(".d", r).onclick = () => { saveSnips(loadSnips().filter((x) => x.id !== sn.id)); if (editing === sn.id) reset(); render(); };
      list.appendChild(r);
    }
  };
  $("#sn-save", m.box).onclick = () => {
    if (!name.value.trim() || !cmd.value.trim()) { toast("이름과 명령을 입력하세요", true); return; }
    const all = loadSnips();
    const rec: Snippet = { id: editing ?? crypto.randomUUID(), name: name.value.trim(), cmd: cmd.value.replace(/\n+$/, ""), enter: enter.checked };
    const i = all.findIndex((x) => x.id === rec.id);
    if (i >= 0) all[i] = rec; else all.push(rec);
    saveSnips(all); reset(); render();
  };
  $("#sn-new", m.box).onclick = () => { reset(); render(); };
  $("#x", m.box).onclick = m.close;
  render(); name.focus();
}

// ───────────── 서버 간 파일 복사 ─────────────
function copyToServer(s: Sess, paths: string[]) {
  const targets = [...sessions.values()];
  const m = modal(`
    <h3>다른 서버로 복사</h3>
    <div style="color:var(--dim);margin-bottom:12px">${esc(s.title)} 의 ${paths.length}개 항목 (${esc(paths.slice(0, 2).map((x) => x.split("/").pop()).join(", "))}${paths.length > 2 ? " …" : ""})</div>
    <div class="field"><label>대상 서버</label><select id="cs">${targets.map((t) => `<option value="${t.sid}">${esc(t.title)}${t === s ? " (같은 서버)" : ""}</option>`).join("")}</select></div>
    <div class="field"><label>대상 폴더</label><input id="cp" spellcheck="false"/></div>
    <div class="err" id="cerr"></div>
    <div class="btns"><button id="x">취소</button><button id="go" class="primary">복사</button></div>`);
  const sel = $<HTMLSelectElement>("#cs", m.box), path = $<HTMLInputElement>("#cp", m.box);
  const sync = () => { path.value = sessions.get(sel.value)?.cwd ?? "/"; };
  const other = targets.find((t) => t !== s);
  if (other) sel.value = other.sid;
  sync(); sel.onchange = sync;
  $("#x", m.box).onclick = m.close;
  const go = $<HTMLButtonElement>("#go", m.box);
  go.onclick = async () => {
    const dst = sessions.get(sel.value)!, dir = path.value.trim();
    if (!dir) return;
    if (dst === s && paths.some((p) => parentOf(p) === dir.replace(/\/+$/, ""))) { $("#cerr", m.box).textContent = "같은 폴더로는 복사할 수 없어요."; return; }
    m.close();
    const id = `x${++xferSeq}`;
    try {
      await invoke("sftp_copy", { fromSession: s.sid, toSession: dst.sid, transferId: id, paths, dstDir: dir });
      toast(`복사 완료: ${dst.title}:${dir}`);
      if (dst.cwd === dir) void navigate(dst, dst.cwd, true);
    } catch (e) { toast(`복사 실패: ${errMsg(e)}`, true); }
  };
  path.focus(); path.select();
}

// ───────────── ~/.ssh/config 가져오기 / ProxyJump ─────────────
interface SshHost { alias: string; hostname: string; user: string; port: number; identity_file: string; proxy_jump: string }

async function importSshConfig() {
  let hosts: SshHost[];
  try { hosts = await invoke<SshHost[]>("ssh_config_hosts"); }
  catch (e) { toast(errMsg(e), true); return; }
  if (!hosts.length) { toast("~/.ssh/config 에서 가져올 호스트를 찾지 못했어요", true); return; }
  const exists = (h: SshHost) => profiles.some((p) => p.host === h.hostname && p.port === h.port && p.username === h.user);
  const m = modal(`
    <h3>~/.ssh/config 가져오기</h3>
    <div style="color:var(--dim);margin-bottom:8px">${hosts.length}개 호스트를 찾았어요. 가져올 항목을 고르세요. (폴더: “ssh config”)</div>
    <label class="check"><input id="all" type="checkbox" checked/> 전체 선택</label>
    <div class="imp-list"></div>
    <div class="btns"><button id="x">취소</button><button id="go" class="primary">가져오기</button></div>`);
  m.box.classList.add("wide");
  const list = $(".imp-list", m.box);
  hosts.forEach((h, i) => {
    const dup = exists(h);
    const r = document.createElement("label");
    r.className = "imp-row" + (dup ? " dup" : "");
    r.innerHTML = `<input type="checkbox" data-i="${i}" ${dup ? "disabled" : "checked"}/><b>${esc(h.alias)}</b><span>${esc(h.user)}@${esc(h.hostname)}:${h.port}</span><em>${h.identity_file ? "🔑 " + esc(h.identity_file) : "비밀번호"}${h.proxy_jump ? " · ↪ " + esc(h.proxy_jump) : ""}${dup ? " · 이미 있음" : ""}</em>`;
    list.appendChild(r);
  });
  const boxes = () => [...list.querySelectorAll<HTMLInputElement>("input[data-i]:not(:disabled)")];
  $<HTMLInputElement>("#all", m.box).onchange = (e) => boxes().forEach((b) => (b.checked = (e.target as HTMLInputElement).checked));
  $("#x", m.box).onclick = m.close;
  $("#go", m.box).onclick = async () => {
    const chosen = boxes().filter((b) => b.checked).map((b) => hosts[+b.dataset.i!]);
    m.close();
    try {
      for (const h of chosen) {
        profiles = await invoke<Profile[]>("profile_save", {
          profile: { id: crypto.randomUUID(), name: h.alias, host: h.hostname, port: h.port, username: h.user, auth_type: h.identity_file ? "key" : "password", key_path: h.identity_file, remember: false, group: "ssh config", jump: h.proxy_jump },
        });
      }
      renderHome();
      toast(`${chosen.length}개 접속을 가져왔어요`);
    } catch (e) { toast(`가져오기 실패: ${errMsg(e)}`, true); }
  };
}

type Hop = { host: string; port: number; username: string; auth: Record<string, unknown>; jump?: Hop };

/** 저장된 접속의 인증 정보를 만든다. 키체인/키 파일에 없으면 비밀번호를 물어본다. */
async function authFor(p: Profile): Promise<Record<string, unknown>> {
  if (p.remember) {
    const v = await invoke<string | null>("secret_get", { profileId: p.id });
    if (v?.startsWith("v:")) {
      const secret = v.slice(2);
      return p.auth_type === "password" ? { type: "password", password: secret } : { type: "key", path: p.key_path || "~/.ssh/id_ed25519", passphrase: secret || null };
    }
  }
  if (p.auth_type === "key") return { type: "key", path: p.key_path || "~/.ssh/id_ed25519", passphrase: null };
  const pw = await askInput(`점프 호스트 ${p.name} (${p.username}@${p.host}) 비밀번호`, "", true);
  if (pw === null) throw new Error("점프 호스트 인증이 취소되었어요");
  return { type: "password", password: pw };
}

/** ProxyJump 문자열("a,b" / "user@host:22")을 중첩된 jump 구조로 풀어낸다. */
async function resolveJump(spec: string): Promise<Hop | undefined> {
  let chain: Hop | undefined;
  for (const hop of spec.split(",").map((x) => x.trim()).filter(Boolean)) {
    const prof = profiles.find((p) => p.name === hop);
    let h: Hop;
    if (prof) {
      h = { host: prof.host, port: prof.port, username: prof.username, auth: await authFor(prof) };
      if (prof.jump && !chain) h.jump = await resolveJump(prof.jump);
    } else {
      const m = /^(?:([^@]+)@)?([^:]+)(?::(\d+))?$/.exec(hop);
      if (!m) throw new Error(`점프 호스트를 해석할 수 없어요: ${hop}`);
      h = { host: m[2], port: m[3] ? +m[3] : 22, username: m[1] ?? "", auth: { type: "key", path: "~/.ssh/id_ed25519", passphrase: null } };
      if (!h.username) throw new Error(`점프 호스트 사용자를 알 수 없어요 (user@host 형식으로 적거나 저장된 접속 이름을 쓰세요): ${hop}`);
    }
    if (chain) h.jump = chain;
    chain = h;
  }
  return chain;
}

// ───────────── 프로세스 보기 / 종료 ─────────────
interface Proc { pid: number; user: string; cpu: number; mem: number; etime: string; cmd: string }

function parseProcs(raw: string): Proc[] {
  const out: Proc[] = [];
  for (const l of raw.split("\n")) {
    const m = /^\s*(\d+)\s+(\S+)\s+([\d.]+)\s+([\d.]+)\s+(\S+)\s+(.*)$/.exec(l);
    if (m) out.push({ pid: +m[1], user: m[2], cpu: +m[3], mem: +m[4], etime: m[5], cmd: m[6] });
  }
  return out;
}

function openProcs(s: Sess) {
  const m = modal(`
    <h3>프로세스 · ${esc(s.title)}</h3>
    <div class="ptool">
      <input id="pq" placeholder="🔍 이름 / PID / 사용자 필터" spellcheck="false"/>
      <select id="ps"><option value="cpu">CPU 순</option><option value="mem">메모리 순</option><option value="pid">PID 순</option></select>
      <label><input id="pa" type="checkbox" checked/> 3초마다 새로고침</label>
      <span id="pc" style="color:var(--dim)"></span>
    </div>
    <div class="ptable"><table><thead><tr><th>PID</th><th>사용자</th><th>CPU%</th><th>MEM%</th><th>경과</th><th>명령</th><th></th></tr></thead><tbody></tbody></table></div>
    <div class="err" id="perr"></div>
    <div class="btns"><button id="x">닫기</button></div>`);
  m.box.classList.add("wide");
  const q = $<HTMLInputElement>("#pq", m.box), sortSel = $<HTMLSelectElement>("#ps", m.box), auto = $<HTMLInputElement>("#pa", m.box);
  const tbody = $("tbody", m.box);
  let procs: Proc[] = [], busy = false;

  const render = () => {
    const f = q.value.trim().toLowerCase();
    let list = f ? procs.filter((p) => `${p.pid} ${p.user} ${p.cmd}`.toLowerCase().includes(f)) : [...procs];
    list.sort(sortSel.value === "pid" ? (a, b) => a.pid - b.pid : sortSel.value === "mem" ? (a, b) => b.mem - a.mem : (a, b) => b.cpu - a.cpu);
    list = list.slice(0, 200);
    $("#pc", m.box).textContent = `${list.length}개 표시 / 전체 ${procs.length}개`;
    tbody.innerHTML = "";
    for (const p of list) {
      const tr = document.createElement("tr");
      const hot = p.cpu >= 50 ? ' class="hot"' : "";
      tr.innerHTML = `<td>${p.pid}</td><td>${esc(p.user)}</td><td${hot}>${p.cpu.toFixed(1)}</td><td>${p.mem.toFixed(1)}</td><td>${esc(p.etime)}</td><td class="cmd" title="${esc(p.cmd)}">${esc(p.cmd)}</td><td class="pact"><button class="term" title="정상 종료 요청 (SIGTERM)">종료</button><button class="kill danger" title="강제 종료 (SIGKILL)">강제</button></td>`;
      $(".term", tr).onclick = () => void killProc(p, "TERM");
      $(".kill", tr).onclick = () => void killProc(p, "KILL");
      tbody.appendChild(tr);
    }
  };

  const refresh = async () => {
    if (busy || !m.ov.isConnected) return;
    busy = true;
    try {
      procs = parseProcs(await invoke<string>("sys_procs", { sessionId: s.sid }));
      $("#perr", m.box).textContent = procs.length ? "" : "프로세스 목록을 가져오지 못했어요 (리눅스 서버만 지원)";
      render();
    } catch (e) { $("#perr", m.box).textContent = errMsg(e); }
    finally { busy = false; }
  };

  const killProc = async (p: Proc, sig: "TERM" | "KILL") => {
    const label = sig === "KILL" ? "강제 종료 (SIGKILL)" : "종료 (SIGTERM)";
    if (!(await askConfirm(`PID ${p.pid} ${label}할까요?`, p.cmd.length > 120 ? p.cmd.slice(0, 120) + "…" : p.cmd))) return;
    try { toast(await invoke<string>("proc_kill", { sessionId: s.sid, pid: p.pid, signal: sig })); }
    catch (e) { toast(`실패: ${errMsg(e)}`, true); }
    setTimeout(() => void refresh(), 500);
  };

  q.oninput = render; sortSel.onchange = render;
  $("#x", m.box).onclick = m.close;
  const timer = setInterval(() => { if (!m.ov.isConnected) clearInterval(timer); else if (auto.checked) void refresh(); }, 3000);
  q.focus();
  void refresh();
}

// ───────────── 연결 끊김 / 자동 재접속 ─────────────
const sleep = (ms: number) => new Promise((r) => setTimeout(r, ms));

function showBanner(s: Sess, text: string, buttons: [string, () => void][] = []) {
  hideBanner(s);
  const b = document.createElement("div");
  b.className = "tbanner";
  b.innerHTML = `<span>${esc(text)}</span>`;
  for (const [label, fn] of buttons) { const btn = document.createElement("button"); btn.textContent = label; btn.onclick = fn; b.appendChild(btn); }
  s.group.wrap.appendChild(b);
  s.banner = b;
}
function hideBanner(s: Sess) { s.banner?.remove(); s.banner = null; }
function setState(s: Sess, st: Sess["state"]) { s.state = st; renderTabs(); }

async function onPaneExit(s: Sess, g: TermGroup, tp: TP) {
  if (s.reconnecting || s.closed) return;
  const alive = await invoke<boolean>("ssh_alive", { sessionId: s.sid }).catch(() => false);
  if (alive) {
    // 연결은 살아 있고 셸만 종료됨 (exit 입력 등)
    if (g.panes.length > 1) g.removePane(tp);
    else showBanner(s, "셸이 종료되었어요", [
      ["새 셸 열기", () => { hideBanner(s); void g.reopen(tp, "새 셸").catch((e) => toast(errMsg(e), true)); }],
      ["탭 닫기", () => void closeSession(s)],
    ]);
    return;
  }
  void reconnect(s);
}

async function reconnect(s: Sess) {
  if (s.reconnecting || s.closed) return;
  s.reconnecting = true;
  setState(s, "wait");
  const delays = [1000, 2000, 4000, 8000, 15000, 30000];
  let lastErr = "";
  for (let i = 0; i < delays.length; i++) {
    if (s.closed) return;
    showBanner(s, `연결이 끊어졌어요 · 재연결 시도 중 (${i + 1}/${delays.length})…`, [["탭 닫기", () => void closeSession(s)]]);
    await sleep(delays[i]);
    if (s.closed) return;
    try {
      await invoke("ssh_reconnect", { sessionId: s.sid, args: s.connArgs });
      for (const tp of [...s.group.panes]) await s.group.reopen(tp).catch(() => {});
      hideBanner(s);
      s.reconnecting = false;
      setState(s, "ok");
      toast("다시 연결했어요");
      void navigate(s, s.cwd, true);
      return;
    } catch (e) { lastErr = errMsg(e); }
  }
  s.reconnecting = false;
  setState(s, "lost");
  showBanner(s, `재연결에 실패했어요: ${lastErr}`, [["다시 시도", () => void reconnect(s)], ["탭 닫기", () => void closeSession(s)]]);
}

// ───────────── 로컬 터미널 ─────────────
async function openLocal() {
  const id = `l${++localSeq}`;
  const el = document.createElement("div");
  el.className = "pane";
  el.innerHTML = `<div class="termwrap">${SPLIT_BUTTONS}</div>`;
  $("#stage").appendChild(el);
  const rec = {} as Local;
  const group = new TermGroup($(".termwrap", el), {
    open: (cols, rows) => invoke<string>("local_open", { cols, rows }),
    onPaneExit: (g, tp) => { if (g.panes.length > 1) g.removePane(tp); else closeLocal(rec); },
  });
  Object.assign(rec, { id, title: `로컬 ${localSeq}`, el, group });
  locals.set(id, rec);
  wireSplit(group, el);
  activate(id);
  try { await group.addPane(); } catch (e) { closeLocal(rec); toast(`로컬 터미널을 열 수 없어요: ${errMsg(e)}`, true); }
}

function closeLocal(l: Local) {
  if (!locals.has(l.id)) return;
  l.group.destroy();
  l.el.remove();
  locals.delete(l.id);
  activate(sessions.size ? [...sessions.keys()].pop()! : "home");
}

// ───────────── 파일 패널 ─────────────
function wireFilePanel(s: Sess) {
  const el = s.el;
  $(".up", el).onclick = () => void navigate(s, parentOf(s.cwd));
  $(".ref", el).onclick = () => void navigate(s, s.cwd);
  $(".mk", el).onclick = () => void mkdirHere(s);
  $(".upl", el).onclick = (e) => popMenu([["파일 선택해서 업로드…", () => void pickUpload(s, false)], ["폴더 선택해서 업로드…", () => void pickUpload(s, true)]], e);
  $<HTMLInputElement>(".fol", el).onchange = (e) => { s.follow = (e.target as HTMLInputElement).checked; };

  // 크기/날짜 열 숨기기 (설정은 기억해 둠)
  for (const [cls, cb, key] of [["hide-size", ".hsz", "hideSize"], ["hide-date", ".hdt", "hideDate"], ["show-perm", ".spm", "showPerm"]] as const) {
    const box = $<HTMLInputElement>(cb, el);
    box.checked = localStorage.getItem(key) === "1";
    el.classList.toggle(cls, box.checked);
    box.onchange = () => {
      localStorage.setItem(key, box.checked ? "1" : "0");
      for (const x of sessions.values()) {   // 모든 세션에 동일하게 적용
        x.el.classList.toggle(cls, box.checked);
        $<HTMLInputElement>(cb, x.el).checked = box.checked;
      }
    };
  }
  const dbl = $<HTMLSelectElement>(".dblmode", el);
  dbl.value = getOpenMode();
  dbl.onchange = () => setOpenMode(dbl.value as OpenMode);
  s.pathEl.onkeydown = (e) => { if (e.key === "Enter") void navigate(s, s.pathEl.value.trim() || "/"); };

  // 폴더 폭 조절
  const gutter = $(".gutter", el), files = $(".files", el);
  gutter.addEventListener("pointerdown", (e) => {
    e.preventDefault();
    gutter.setPointerCapture(e.pointerId);
    document.body.classList.add("resizing");
    const left = files.getBoundingClientRect().left;
    const move = (ev: PointerEvent) => {
      files.style.width = Math.max(200, Math.min(ev.clientX - left, innerWidth - 320)) + "px";
      s.group.fitAll();
    };
    const up = (ev: PointerEvent) => {
      gutter.releasePointerCapture(ev.pointerId);
      gutter.removeEventListener("pointermove", move);
      gutter.removeEventListener("pointerup", up);
      gutter.removeEventListener("pointercancel", up);
      document.body.classList.remove("resizing");
      s.group.fitAll();
    };
    gutter.addEventListener("pointermove", move);
    gutter.addEventListener("pointerup", up);
    gutter.addEventListener("pointercancel", up);
  });

  s.listEl.oncontextmenu = (e) => { e.preventDefault(); if (e.target === s.listEl) { s.selected.clear(); renderFiles(s); } showMenu(s, e); };
}

async function navigate(s: Sess, path: string, quiet = false) {
  try {
    const real = await invoke<string>("sftp_realpath", { sessionId: s.sid, path });
    const entries = await invoke<Entry[]>("sftp_list", { sessionId: s.sid, path: real });
    s.cwd = real; s.entries = entries; s.selected.clear(); s.lastClicked = null;
    s.filter = ""; s.results = null; s.qEl.value = "";
    s.pathEl.value = real;
    renderFiles(s);
  } catch (e) {
    if (!quiet) toast(`폴더를 열 수 없어요: ${errMsg(e)}`, true);
  }
}

function renderFiles(s: Sess) {
  if (s.results) return renderResults(s);
  const list = s.listEl;
  list.innerHTML = "";
  const q = s.filter.toLowerCase();
  const shown = q ? s.entries.filter((e) => e.name.toLowerCase().includes(q)) : s.entries;
  const mk = (icon: string, name: string, size: string, date: string, perm?: Entry) => {
    const r = document.createElement("div");
    r.className = "row";
    const pm = perm ? `<span class="pm" title="${esc(`${perm.perms} (${perm.octal})  ${perm.owner}`)}">${perm.perms}</span>` : `<span class="pm"></span>`;
    r.innerHTML = `<span class="ic">${icon}</span><span class="nm" title="${esc(name)}">${esc(name)}</span>${pm}<span class="sz">${size}</span><span class="dt">${date}</span>`;
    return r;
  };
  if (s.cwd !== "/") {
    const up = mk("📁", "..", "", "");
    up.ondblclick = () => void navigate(s, parentOf(s.cwd));
    list.appendChild(up);
  }
  for (const e of shown) {
    const r = mk(e.is_dir ? "📁" : e.is_symlink ? "🔗" : "📄", e.name, e.is_dir ? "" : fmtSize(e.size), fmtDate(e.mtime), e);
    r.dataset.path = e.path;
    if (s.selected.has(e.path)) r.classList.add("sel");
    r.onclick = (ev) => {
      if (ev.shiftKey && s.lastClicked) {
        const a = shown.findIndex((x) => x.path === s.lastClicked), b = shown.findIndex((x) => x.path === e.path);
        s.selected.clear();
        for (let i = Math.min(a, b); i <= Math.max(a, b); i++) s.selected.add(shown[i].path);
      } else if (ev.metaKey || ev.ctrlKey) {
        s.selected.has(e.path) ? s.selected.delete(e.path) : s.selected.add(e.path);
        s.lastClicked = e.path;
      } else { s.selected = new Set([e.path]); s.lastClicked = e.path; }
      renderFiles(s);
    };
    r.ondblclick = () => (e.is_dir ? void navigate(s, e.path) : void openFile(s, e));
    r.oncontextmenu = (ev) => {
      ev.preventDefault(); ev.stopPropagation();
      if (!s.selected.has(e.path)) { s.selected = new Set([e.path]); s.lastClicked = e.path; renderFiles(s); }
      showMenu(s, ev);
    };
    list.appendChild(r);
  }
  const n = s.selected.size;
  s.statusEl.textContent = n ? `${n}개 선택됨` : q ? `${shown.length} / ${s.entries.length}개 (필터 중)` : `${s.entries.length}개 항목`;
}

// ───────────── 검색 ─────────────
function wireSearch(s: Sess) {
  const q = s.qEl;
  q.oninput = () => { if (s.results) { s.results = null; } s.filter = q.value.trim(); renderFiles(s); };
  q.onkeydown = (e) => {
    if (e.key === "Enter" && q.value.trim()) void deepSearch(s);
    if (e.key === "Escape") clearSearch(s);
  };
  $(".deep", s.el).onclick = () => { if (q.value.trim()) void deepSearch(s); else q.focus(); };
}

function clearSearch(s: Sess) {
  s.qEl.value = ""; s.filter = ""; s.results = null; renderFiles(s);
}

async function deepSearch(s: Sess) {
  const pattern = s.qEl.value.trim();
  s.statusEl.textContent = `“${pattern}” 검색 중… (${s.cwd} 아래)`;
  try {
    s.results = await invoke<Found[]>("sftp_find", { sessionId: s.sid, root: s.cwd, pattern });
    s.selected.clear();
    renderFiles(s);
  } catch (e) { toast(`검색 실패: ${errMsg(e)}`, true); renderFiles(s); }
}

function renderResults(s: Sess) {
  const list = s.listEl, res = s.results!;
  list.innerHTML = "";
  const bar = document.createElement("div");
  bar.className = "rbar";
  bar.innerHTML = `<span>“${esc(s.qEl.value.trim())}” 검색 결과 ${res.length}개${res.length >= 500 ? " (500개까지만 표시)" : ""}</span><button>✕ 닫기</button>`;
  $("button", bar).onclick = () => clearSearch(s);
  list.appendChild(bar);
  if (!res.length) list.insertAdjacentHTML("beforeend", `<div class="empty" style="padding:20px 14px">찾는 항목이 없어요.</div>`);
  const asEntry = (f: Found): Entry => ({ name: f.path.split("/").pop() || f.path, path: f.path, is_dir: f.is_dir, is_symlink: f.is_symlink, size: f.size, mtime: f.mtime, perms: "", octal: "", owner: "" });
  for (const f of res) {
    const e = asEntry(f);
    const r = document.createElement("div");
    r.className = "row" + (s.selected.has(f.path) ? " sel" : "");
    r.innerHTML = `<span class="ic">${f.is_dir ? "📁" : f.is_symlink ? "🔗" : "📄"}</span><span class="nm" title="${esc(f.path)}">${esc(e.name)}<em>${esc(parentOf(f.path))}</em></span><span class="sz">${f.is_dir ? "" : fmtSize(f.size)}</span><span class="dt">${fmtDate(f.mtime)}</span>`;
    r.onclick = () => { s.selected = new Set([f.path]); renderResults(s); };
    r.ondblclick = () => (f.is_dir ? void revealIn(s, f.path, true) : void openFile(s, e));
    r.oncontextmenu = (ev) => {
      ev.preventDefault(); ev.stopPropagation();
      popMenu([
        ...(f.is_dir ? [] : [["에디터로 열기", () => void openEditor(s, e)] as [string, () => void]]),
        ["폴더에서 보기", () => void revealIn(s, f.path, false)],
        [`다운로드`, () => void download(s, [f.path])],
      ], ev);
    };
    list.appendChild(r);
  }
  s.statusEl.textContent = `검색 결과 ${res.length}개 · 더블클릭: ${"열기"} / 우클릭: 폴더에서 보기`;
}

/** 검색 결과 항목이 있는 폴더로 이동해서 그 항목을 선택. into=true 면 폴더 안으로 들어간다. */
async function revealIn(s: Sess, path: string, into: boolean) {
  await navigate(s, into ? path : parentOf(path));
  if (into) return;
  s.selected = new Set([path]); s.lastClicked = path; renderFiles(s);
  const row = [...s.listEl.querySelectorAll<HTMLElement>(".row")].find((x) => x.dataset.path === path);
  row?.scrollIntoView({ block: "center" });
}

function showMenu(s: Sess, ev: MouseEvent) {
  document.querySelectorAll(".menu").forEach((x) => x.remove());
  const sel = s.entries.filter((e) => s.selected.has(e.path));
  const items: [string, () => void][] = [];
  if (sel.length) {
    if (sel.length === 1 && !sel[0].is_dir) items.push(["에디터로 열기", () => void openEditor(s, sel[0])]);
    items.push([`다른 서버로 복사… (${sel.length})`, () => copyToServer(s, sel.map((e) => e.path))]);
    items.push([`다운로드 (${sel.length})`, () => void download(s, sel.map((e) => e.path))]);
    if (sel.length === 1) items.push(["이름 변경", () => void renameEntry(s, sel[0])]);
    items.push(["삭제", () => void removeEntries(s, sel)]);
    items.push(["-", () => {}]);
  }
  items.push(["새 폴더", () => void mkdirHere(s)], ["파일 업로드…", () => void pickUpload(s, false)], ["폴더 업로드…", () => void pickUpload(s, true)], ["새로고침", () => void navigate(s, s.cwd)]);
  popMenu(items, ev);
}

function popMenu(items: [string, () => void][], ev: MouseEvent) {
  document.querySelectorAll(".menu").forEach((x) => x.remove());
  const menu = document.createElement("div");
  menu.className = "menu";
  menu.style.left = Math.min(ev.clientX, innerWidth - 180) + "px";
  menu.style.top = Math.min(ev.clientY, innerHeight - items.length * 32 - 20) + "px";
  for (const [label, fn] of items) {
    if (label === "-") { menu.appendChild(document.createElement("hr")); continue; }
    const d = document.createElement("div");
    d.textContent = label;
    d.onclick = () => { menu.remove(); fn(); };
    menu.appendChild(d);
  }
  document.body.appendChild(menu);
  const off = () => { menu.remove(); removeEventListener("mousedown", off, true); };
  setTimeout(() => addEventListener("mousedown", off, true), 0);
}

async function mkdirHere(s: Sess) {
  const name = await askInput("새 폴더 이름");
  if (!name) return;
  try { await invoke("sftp_mkdir", { sessionId: s.sid, path: joinPath(s.cwd, name) }); await navigate(s, s.cwd); }
  catch (e) { toast(errMsg(e), true); }
}

async function renameEntry(s: Sess, e: Entry) {
  const name = await askInput("새 이름", e.name);
  if (!name || name === e.name) return;
  try { await invoke("sftp_rename", { sessionId: s.sid, from: e.path, to: joinPath(s.cwd, name) }); await navigate(s, s.cwd); }
  catch (er) { toast(errMsg(er), true); }
}

async function removeEntries(s: Sess, list: Entry[]) {
  const label = list.length === 1 ? `“${list[0].name}”` : `${list.length}개 항목`;
  if (!(await askConfirm(`${label} 삭제할까요?`, "서버에서 영구 삭제되며 되돌릴 수 없어요."))) return;
  try {
    for (const e of list) await invoke("sftp_remove", { sessionId: s.sid, path: e.path, isDir: e.is_dir });
    toast("삭제했어요");
  } catch (er) { toast(errMsg(er), true); }
  await navigate(s, s.cwd);
}

// ───────────── 파일을 열 때 선택창 ─────────────
type OpenMode = "ask" | "edit" | "download";
const getOpenMode = (): OpenMode => (localStorage.getItem("openMode") as OpenMode) || "ask";
const setOpenMode = (m: OpenMode) => {
  localStorage.setItem("openMode", m);
  document.querySelectorAll<HTMLSelectElement>(".dblmode").forEach((x) => (x.value = m));
};

function openFile(s: Sess, e: Entry) {
  const mode = getOpenMode();
  if (mode === "edit") return void openEditor(s, e);
  if (mode === "download") return void download(s, [e.path]);
  const m = modal(`
    <h3>${esc(e.name)}</h3>
    <div style="color:var(--dim);margin-bottom:14px">${fmtSize(e.size)} · 어떻게 열까요?</div>
    <div class="choices">
      <button id="ed" class="primary">✎ 에디터로 열기</button>
      <button id="dl">⬇ 다운로드</button>
    </div>
    <label class="check"><input id="rem" type="checkbox"/> 이 선택을 기억하고 다음부터 묻지 않기</label>
    <div class="btns"><button id="x">취소</button></div>`);
  const remember = (v: OpenMode) => { if ($<HTMLInputElement>("#rem", m.box).checked) setOpenMode(v); };
  $("#ed", m.box).onclick = () => { remember("edit"); m.close(); void openEditor(s, e); };
  $("#dl", m.box).onclick = () => { remember("download"); m.close(); void download(s, [e.path]); };
  $("#x", m.box).onclick = m.close;
  $<HTMLButtonElement>("#ed", m.box).focus();
  m.box.addEventListener("keydown", (ev) => { if (ev.key === "Escape") m.close(); });
}

// ───────────── 전송 ─────────────
async function download(s: Sess, paths: string[]) {
  const id = `x${++xferSeq}`;
  xferSess.set(id, s);
  try { await invoke("sftp_download", { sessionId: s.sid, transferId: id, remotePaths: paths, localDir: downloadDir }); toast(`다운로드 완료 → ${downloadDir}`); }
  catch (e) { toast(errMsg(e), true); }
}

async function pickUpload(s: Sess, directory: boolean) {
  try {
    const r = await openDialog({ multiple: true, directory, title: directory ? "업로드할 폴더 선택" : "업로드할 파일 선택" });
    const paths = r == null ? [] : Array.isArray(r) ? r : [r];
    if (paths.length) void upload(s, paths);
  } catch (e) { toast(errMsg(e), true); }
}

async function upload(s: Sess, localPaths: string[]) {
  const id = `x${++xferSeq}`;
  xferSess.set(id, s);
  try { await invoke("sftp_upload", { sessionId: s.sid, transferId: id, localPaths, remoteDir: s.cwd }); await navigate(s, s.cwd); }
  catch (e) { toast(errMsg(e), true); }
}

function onTransfer(ev: TransferEv) {
  const box = $("#transfers");
  let el = document.getElementById("tr-" + ev.id);
  if (ev.state === "done" && !ev.name) { el?.remove(); return; }
  if (!el) { el = document.createElement("div"); el.id = "tr-" + ev.id; el.className = "tr"; el.innerHTML = `<div class="t"></div><div class="bar"><i></i></div>`; box.appendChild(el); }
  const pct = ev.total ? Math.min(100, (ev.done / ev.total) * 100) : 0;
  $(".t", el).textContent = ev.state === "error" ? `실패: ${ev.name} – ${ev.message}` : `${ev.name}  ${fmtSize(ev.done)} / ${fmtSize(ev.total)}`;
  $<HTMLElement>(".bar i", el).style.width = pct + "%";
  el.className = "tr" + (ev.state === "error" ? " error" : "");
  if (ev.state === "error") setTimeout(() => el?.remove(), 8000);
}

// ───────────── 시작 ─────────────
async function init() {
  // 배포 빌드에서는 웹뷰 기본 우클릭 메뉴(새로고침 등)를 막는다. 입력칸/터미널/에디터는 복사·붙여넣기용으로 허용.
  if (import.meta.env.PROD) {
    addEventListener("contextmenu", (e) => {
      if (!(e.target as HTMLElement).closest("input, textarea, .xterm, .cm-content")) e.preventDefault();
    });
  }
  // ⌘K / Ctrl+K: 접속 목록으로 이동해서 바로 검색
  addEventListener("keydown", (e) => {
    if ((e.metaKey || e.ctrlKey) && e.key.toLowerCase() === "k") {
      e.preventDefault();
      activate("home");
      const hq = document.querySelector<HTMLInputElement>("#hq");
      hq?.focus(); hq?.select();
    }
  });
  // ESC: 컨텍스트 메뉴 → 맨 위 모달 순으로 닫기
  addEventListener("keydown", (e) => {
    if (e.key !== "Escape") return;
    const menu = document.querySelector(".menu");
    if (menu) { menu.remove(); e.preventDefault(); e.stopPropagation(); return; }
    const top = modalStack[modalStack.length - 1];
    if (top) { top(); e.preventDefault(); e.stopPropagation(); }
  }, true);
  $("#new-tab").onclick = () => connectDialog(null);
  $("#new-local").onclick = () => void openLocal();
  downloadDir = await invoke<string>("default_download_dir");

  await listen<{ id: string; data: string }>("term-data", ({ payload }) => {
    routeData(payload.id, Uint8Array.from(atob(payload.data), (c) => c.charCodeAt(0)));
  });
  await listen<string>("term-exit", ({ payload }) => routeExit(payload));
  await listen<TransferEv>("transfer", ({ payload }) => onTransfer(payload));

  // 드래그앤드롭 업로드
  await getCurrentWebview().onDragDropEvent((e) => {
    const s = sessions.get(active);
    const p = e.payload;
    if (!s) return;
    if (p.type === "over" || p.type === "enter") s.listEl.classList.add("drop");
    else if (p.type === "leave") s.listEl.classList.remove("drop");
    else if (p.type === "drop") { s.listEl.classList.remove("drop"); if (p.paths.length) void upload(s, p.paths); }
  });

  setInterval(() => { const e = editors.get(active); if (e) void checkRemote(e); }, 15000);

  renderTabs();
  $("#home").classList.add("show");
  await loadProfiles();
}

void init();
