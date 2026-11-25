import { invoke } from "@tauri-apps/api/core";
import { Terminal } from "@xterm/xterm";
import { FitAddon } from "@xterm/addon-fit";
import { WebglAddon } from "@xterm/addon-webgl";
import "@xterm/xterm/css/xterm.css";

/** 터미널 한 칸 (분할되면 한 그룹에 여러 개) */
export interface TP { term: Terminal; fit: FitAddon; termId: string; el: HTMLElement; slot: number }

export interface GroupOpts {
  /** 셸을 열고 termId 를 돌려준다 (SSH 셸 / 로컬 셸) */
  open: (cols: number, rows: number, slot: number) => Promise<string>;
  /** 셸이 알려주는 현재 경로(OSC 7) */
  onCwd?: (path: string) => void;
  /** 열린 직후 경로 알림 훅을 주입할지 (원격 bash/zsh 용) */
  injectHook?: () => boolean;
  /** 셸/연결이 끝났을 때 */
  onPaneExit: (g: TermGroup, tp: TP) => void;
}

// ───────────── 백엔드 이벤트 라우팅 ─────────────
const registry = new Map<string, { g: TermGroup; tp: TP }>();
const pending = new Map<string, Uint8Array[]>();

export function routeData(id: string, bytes: Uint8Array) {
  const hit = registry.get(id);
  if (hit) hit.tp.term.write(bytes);
  else { const a = pending.get(id) ?? []; a.push(bytes); pending.set(id, a); }
}

export function routeExit(id: string) {
  const hit = registry.get(id);
  if (hit) hit.g.opts.onPaneExit(hit.g, hit.tp);
}

// 원격 셸에 심는 훅: 프롬프트가 뜰 때마다 현재 경로를 OSC 7 로 알려준다 (bash/zsh)
const HOOK = ` __o7(){ printf '\\033]7;file://%s%s\\007' "$HOSTNAME" "$PWD"; }; if [ -n "$ZSH_VERSION" ]; then precmd_functions+=(__o7); else PROMPT_COMMAND="__o7;$PROMPT_COMMAND"; fi; clear\n`;

export class TermGroup {
  panes: TP[] = [];
  dir: "row" | "column" = "row";
  focused: TP | null = null;
  private ro: ResizeObserver;

  constructor(public wrap: HTMLElement, public opts: GroupOpts) {
    wrap.classList.add("tgroup");
    this.ro = new ResizeObserver(() => this.fitAll());
    this.ro.observe(wrap);
  }

  private bind(tp: TP) {
    registry.set(tp.termId, { g: this, tp });
    for (const c of pending.get(tp.termId) ?? []) tp.term.write(c);
    pending.delete(tp.termId);
  }

  private layout() {
    this.wrap.classList.toggle("col", this.dir === "column");
    this.wrap.classList.toggle("multi", this.panes.length > 1);
  }

  /** 새 터미널 칸을 추가하고 셸을 연다. */
  async addPane(dir?: "row" | "column"): Promise<TP> {
    if (dir) this.dir = dir;
    const el = document.createElement("div");
    el.className = "tpane";
    el.innerHTML = `<button class="tclose" title="이 터미널 닫기">✕</button>`;
    this.wrap.appendChild(el);

    const term = new Terminal({
      fontFamily: 'Menlo, "SF Mono", Consolas, monospace', fontSize: 13, cursorBlink: true,
      scrollback: 10000, allowProposedApi: true, macOptionClickForcesSelection: true,
      theme: { background: "#14161b", foreground: "#d7dae0", cursor: "#4f8cff", selectionBackground: "#3d5a99" },
    });
    const fit = new FitAddon();
    term.loadAddon(fit);
    const body = document.createElement("div");
    body.className = "tbody";
    el.appendChild(body);
    term.open(body);
    try {
      const gl = new WebglAddon();
      gl.onContextLoss(() => gl.dispose());
      term.loadAddon(gl);
    } catch { /* WebGL 불가 시 기본 렌더러 */ }

    // 칸마다 고정 번호(slot): tmux 세션 이름과 재접속 시 복구에 쓴다
    const used = new Set(this.panes.map((p) => p.slot));
    let slot = 1;
    while (used.has(slot)) slot++;
    const tp: TP = { term, fit, termId: "", el, slot };
    this.panes.push(tp);
    this.focused = tp;
    this.layout();
    this.fitAll();

    term.parser.registerOscHandler(7, (data) => {
      if (this.focused !== tp && this.panes.length > 1) return true;
      const m = /^file:\/\/[^/]*(\/.*)$/.exec(data);
      if (m) this.opts.onCwd?.(decodeURIComponent(m[1]));
      return true;
    });
    el.addEventListener("mousedown", () => { this.focused = tp; });
    $on(el, ".tclose", () => { void invoke("term_close", { termId: tp.termId }); this.removePane(tp); });

    try {
      tp.termId = await this.opts.open(term.cols, term.rows, slot);
    } catch (e) {
      this.removePane(tp);
      throw e;
    }
    this.bind(tp);
    term.onData((d) => void invoke("term_write", { termId: tp.termId, data: d }));
    term.onResize(({ cols, rows }) => void invoke("term_resize", { termId: tp.termId, cols, rows }));
    this.injectHook(tp);
    term.focus();
    return tp;
  }

  private injectHook(tp: TP) {
    setTimeout(() => {
      if (this.opts.injectHook?.()) void invoke("term_write", { termId: tp.termId, data: HOOK });
    }, 600);
  }

  /** 연결이 새로 만들어진 뒤, 같은 화면(스크롤백 유지)에 새 셸을 다시 연다. */
  async reopen(tp: TP, msg = "다시 연결되었습니다") {
    registry.delete(tp.termId);
    tp.termId = await this.opts.open(tp.term.cols, tp.term.rows, tp.slot);
    this.bind(tp);
    tp.term.write(`\r\n\x1b[32m[${msg}]\x1b[0m\r\n`);
    this.injectHook(tp);
  }

  removePane(tp: TP) {
    registry.delete(tp.termId);
    tp.term.dispose();
    tp.el.remove();
    this.panes = this.panes.filter((p) => p !== tp);
    if (this.focused === tp) this.focused = this.panes[this.panes.length - 1] ?? null;
    this.layout();
    this.fitAll();
    this.focus();
  }

  fitAll() { for (const p of this.panes) { try { p.fit.fit(); } catch { /* 숨김 상태 */ } } }
  focus() { (this.focused ?? this.panes[0])?.term.focus(); }

  /** 모든 셸을 닫고 정리 */
  destroy() {
    this.ro.disconnect();
    for (const p of [...this.panes]) { void invoke("term_close", { termId: p.termId }); registry.delete(p.termId); p.term.dispose(); p.el.remove(); }
    this.panes = [];
  }
}

function $on(root: HTMLElement, sel: string, fn: () => void) {
  root.querySelector<HTMLElement>(sel)!.onclick = fn;
}
