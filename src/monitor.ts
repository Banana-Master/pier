import { invoke } from "@tauri-apps/api/core";
import { createAlertEngine, getAlertCfg } from "./alerts";

const INTERVAL = 2000; // 조회 간격(ms)
const HIST = 90; // 보관 샘플 수 (2초 × 90 = 3분)

const fmtBytes = (n: number) => {
  const u = ["B", "KB", "MB", "GB", "TB"]; let i = 0;
  while (n >= 1024 && i < u.length - 1) { n /= 1024; i++; }
  return `${n.toFixed(n < 10 && i > 0 ? 1 : 0)} ${u[i]}`;
};
const fmtKB = (kb: number) => fmtBytes(kb * 1024);
const fmtUptime = (sec: number) => {
  const d = Math.floor(sec / 86400), h = Math.floor((sec % 86400) / 3600), m = Math.floor((sec % 3600) / 60);
  return d ? `${d}일 ${h}시간` : h ? `${h}시간 ${m}분` : `${m}분`;
};
const level = (pct: number) => (pct < 60 ? "var(--ok)" : pct < 85 ? "#f0b429" : "var(--danger)");

interface Line { data: number[]; color: string; name: string }
interface Graph { title: string; lines: Line[]; max: number | "auto"; fmt: (n: number) => string; footer?: string; html?: string }

/** 상태바를 `bar` 안에 그리고 주기적으로 갱신한다. 반환값을 호출하면 정지. */
export function startMonitor(sid: string, bar: HTMLElement, onAlert?: (title: string, body: string) => void): () => void {
  const engine = onAlert ? createAlertEngine(getAlertCfg, onAlert, INTERVAL) : null;
  const hist = { cpu: [] as number[], mem: [] as number[], rx: [] as number[], tx: [] as number[], temp: [] as number[], load: [] as number[] };
  const push = (a: number[], v: number) => { a.push(v); if (a.length > HIST) a.shift(); };

  let prev: { total: number; idle: number; rx: number; tx: number; t: number } | null = null;
  let fails = 0, busy = false, stopped = false, built = false;
  let hover: string | null = null;
  const graphs: Record<string, Graph> = {};

  // 툴팁 (마우스를 올리면 그래프 표시)
  const tip = document.createElement("div");
  tip.className = "stip";
  tip.innerHTML = `<div class="h"></div><canvas width="520" height="160"></canvas><div class="d"></div><div class="f"></div>`;
  document.body.appendChild(tip);
  const tipH = tip.querySelector(".h") as HTMLElement;
  const tipF = tip.querySelector(".f") as HTMLElement;
  const canvas = tip.querySelector("canvas") as HTMLCanvasElement;
  const tipD = tip.querySelector(".d") as HTMLElement;

  const drawGraph = (g: Graph) => {
    tipH.textContent = g.title;
    tipF.textContent = g.footer ?? "";
    canvas.style.display = g.html ? "none" : "block";
    tipD.style.display = g.html ? "block" : "none";
    if (g.html) { tipD.innerHTML = g.html; return; }
    const c = canvas.getContext("2d")!;
    const W = canvas.width, H = canvas.height, padL = 8, padB = 4;
    c.clearRect(0, 0, W, H);
    const all = g.lines.flatMap((l) => l.data);
    const top = g.max === "auto" ? Math.max(1e-9, ...all, 1) * 1.15 : g.max;
    c.strokeStyle = "rgba(255,255,255,.08)"; c.lineWidth = 1;
    for (let i = 0; i <= 4; i++) { const y = Math.round((H - padB) * (i / 4)) + .5; c.beginPath(); c.moveTo(0, y); c.lineTo(W, y); c.stroke(); }
    g.lines.forEach((l, idx) => {
      if (l.data.length < 2) return;
      const step = (W - padL) / (HIST - 1);
      const x = (i: number) => W - (l.data.length - 1 - i) * step;
      const y = (v: number) => (H - padB) - (Math.min(v, top) / top) * (H - padB - 6);
      c.beginPath();
      l.data.forEach((v, i) => (i ? c.lineTo(x(i), y(v)) : c.moveTo(x(i), y(v))));
      c.strokeStyle = l.color; c.lineWidth = 2.5; c.stroke();
      if (idx === 0 && g.lines.length === 1) {
        c.lineTo(x(l.data.length - 1), H); c.lineTo(x(0), H); c.closePath();
        c.fillStyle = l.color + "33"; c.fill();
      }
    });
    c.fillStyle = "#8a90a0"; c.font = "20px -apple-system, sans-serif";
    c.fillText(g.fmt(top), 6, 20);
    // 범례
    let lx = W - 6;
    c.textAlign = "right";
    for (const l of [...g.lines].reverse()) {
      const last = l.data[l.data.length - 1];
      const t = `${l.name} ${last === undefined ? "–" : g.fmt(last)}`;
      c.fillStyle = l.color; c.fillText(t, lx, 20);
      lx -= c.measureText(t).width + 16;
    }
    c.textAlign = "left";
  };

  const showTip = (key: string, target: HTMLElement) => {
    const g = graphs[key];
    if (!g) return;
    hover = key;
    tip.classList.add("show");
    drawGraph(g);
    const r = target.getBoundingClientRect();
    const tw = tip.offsetWidth;
    tip.style.left = Math.max(8, Math.min(r.left + r.width / 2 - tw / 2, innerWidth - tw - 8)) + "px";
    tip.style.bottom = innerHeight - r.top + 8 + "px";
  };
  const hideTip = () => { hover = null; tip.classList.remove("show"); };

  // 항목 생성 (처음 한 번)
  const defs: [string, string, boolean][] = [
    ["cpu", "CPU", true], ["mem", "RAM", true], ["disk", "디스크 /", true],
    ["net", "네트워크", false], ["load", "Load", false], ["temp", "온도", false], ["up", "가동", false],
  ];
  const els: Record<string, { root: HTMLElement; v: HTMLElement; fill: HTMLElement | null }> = {};
  const build = () => {
    bar.innerHTML = "";
    for (const [k, label, hasBar] of defs) {
      const root = document.createElement("span");
      root.className = "st";
      root.innerHTML = `<b>${label}</b>${hasBar ? `<i class="mini"><u></u></i>` : ""}<span class="v">–</span>`;
      root.addEventListener("mouseenter", () => showTip(k, root));
      root.addEventListener("mouseleave", hideTip);
      bar.appendChild(root);
      els[k] = { root, v: root.querySelector(".v") as HTMLElement, fill: root.querySelector("u") };
    }
    built = true;
  };
  const set = (k: string, text: string, pct?: number) => {
    const e = els[k]; if (!e) return;
    e.v.textContent = text;
    if (e.fill && pct !== undefined) { e.fill.style.width = Math.min(100, pct) + "%"; e.fill.style.background = level(pct); }
  };

  const parse = (raw: string) => {
    const m: Record<string, string> = {};
    const pd: string[] = [], mnt: string[] = [];
    for (const line of raw.split("\n")) {
      const i = line.indexOf(" "); if (i <= 0) continue;
      const k = line.slice(0, i), v = line.slice(i + 1).trim();
      if (k === "PD") pd.push(v); else if (k === "MNT") mnt.push(v); else m[k] = v;
    }
    return { m, pd, mnt };
  };
  const esc = (x: string) => x.replace(/[&<>"]/g, (c) => ({ "&": "&amp;", "<": "&lt;", ">": "&gt;", '"': "&quot;" }[c]!));

  const tick = async () => {
    if (busy || stopped) return;
    busy = true;
    try {
      const { m, pd, mnt } = parse(await invoke<string>("sys_stats", { sessionId: sid }));
      let memPct: number | null = null, diskPct: number | null = null;
      if (!m.STAT) throw new Error("no /proc");
      fails = 0;
      if (!built) build();
      const now = Date.now();
      const cores = parseInt(m.CORES || "0", 10);

      // CPU: /proc/stat 누적값의 차이로 사용률 계산
      const f = m.STAT.split(/\s+/).slice(1).map(Number);
      const total = f.slice(0, 8).reduce((a, b) => a + b, 0), idle = (f[3] ?? 0) + (f[4] ?? 0);
      const [rx, tx] = (m.NET || "0 0").split(/\s+/).map(Number);
      let cpuPct: number | null = null, rxs = 0, txs = 0;
      if (prev) {
        const dt = total - prev.total;
        if (dt > 0) cpuPct = Math.max(0, Math.min(100, (1 - (idle - prev.idle) / dt) * 100));
        const sec = Math.max(0.001, (now - prev.t) / 1000);
        rxs = Math.max(0, (rx - prev.rx) / sec); txs = Math.max(0, (tx - prev.tx) / sec);
        if (cpuPct !== null) push(hist.cpu, cpuPct);
        push(hist.rx, rxs); push(hist.tx, txs);
      }
      prev = { total, idle, rx, tx, t: now };
      if (cpuPct !== null) set("cpu", `${cpuPct.toFixed(0)}%${cores ? ` · ${cores}코어` : ""}`, cpuPct);

      // 메모리
      const mem: Record<string, number> = {};
      for (const x of (m.MEM || "").matchAll(/(\w+):=(\d+)/g)) mem[x[1]] = parseInt(x[2], 10);
      if (mem.MemTotal) {
        const used = mem.MemTotal - (mem.MemAvailable ?? 0), pct = (used / mem.MemTotal) * 100;
        memPct = pct;
        push(hist.mem, pct);
        set("mem", `${fmtKB(used)} / ${fmtKB(mem.MemTotal)}`, pct);
        const swap = mem.SwapTotal ? `스왑 ${fmtKB(mem.SwapTotal - (mem.SwapFree ?? 0))} / ${fmtKB(mem.SwapTotal)}` : "스왑 없음";
        graphs.mem = { title: "메모리 사용률", lines: [{ data: hist.mem, color: "#4f8cff", name: "RAM" }], max: 100, fmt: (n) => n.toFixed(0) + "%", footer: swap };
      }

      // 디스크: 물리 디스크 목록 + 마운트별 사용량
      const disks = pd.map((l) => Object.fromEntries([...l.matchAll(/(\w+)="([^"]*)"/g)].map((x) => [x[1], x[2]])));
      const mounts = mnt.map((l) => { const p = l.split(/\s+/); return { dev: p[0], fs: p[1], total: Number(p[2]), used: Number(p[3]), pct: parseInt(p[5], 10), at: p.slice(6).join(" ") }; });
      const root = mounts.find((x) => x.at === "/") ?? mounts[0];
      if (root) {
        diskPct = root.pct;
        set("disk", `${fmtKB(root.used)} / ${fmtKB(root.total)}${disks.length ? ` · 물리 ${disks.length}개` : ""}`, root.pct);
        const kind = (d: Record<string, string>) => (d.TRAN === "nvme" || d.NAME?.startsWith("nvme") ? "NVMe SSD" : d.ROTA === "1" ? "HDD" : "SSD");
        const dHtml = disks.length
          ? disks.map((d) => `<div class="dk"><b>/dev/${esc(d.NAME)}</b> <span>${fmtBytes(Number(d.SIZE))} · ${kind(d)}${d.TRAN && d.TRAN !== "nvme" ? " · " + esc(d.TRAN) : ""}</span><div class="md">${esc(d.MODEL.trim() || "모델 정보 없음")}</div></div>`).join("")
          : `<div class="md">물리 디스크 정보를 가져올 수 없어요 (가상 서버/컨테이너일 수 있음)</div>`;
        const mHtml = mounts.map((x) => `<div class="mt"><span>${esc(x.at)} <em>${esc(x.fs)}</em></span><i class="mini"><u style="width:${x.pct}%;background:${level(x.pct)}"></u></i><span>${fmtKB(x.used)} / ${fmtKB(x.total)} (${x.pct}%)</span></div>`).join("");
        graphs.disk = { title: `물리 디스크 ${disks.length}개 · 마운트 ${mounts.length}개`, lines: [], max: 100, fmt: String, html: `${dHtml}<div class="sep"></div>${mHtml}` };
      }

      // 네트워크 / Load / 온도 / 가동시간
      if (prev && hist.rx.length) set("net", `↓ ${fmtBytes(rxs)}/s  ↑ ${fmtBytes(txs)}/s`);
      const load = (m.LOAD || "").split(/\s+/).map(Number);
      if (load.length === 3) {
        push(hist.load, load[0]);
        set("load", load.map((n) => n.toFixed(2)).join(" "), cores ? (load[0] / cores) * 100 : undefined);
        graphs.load = { title: `Load average (1분) · ${load.map((n) => n.toFixed(2)).join(" / ")}`, lines: [{ data: hist.load, color: "#c084fc", name: "load" }], max: "auto", fmt: (n) => n.toFixed(2), footer: cores ? `코어 ${cores}개 (load가 코어 수를 넘으면 과부하)` : "" };
      }
      const tempC = m.TEMP ? parseInt(m.TEMP, 10) / 1000 : NaN;
      if (Number.isFinite(tempC) && tempC > 0) {
        push(hist.temp, tempC);
        set("temp", `${tempC.toFixed(0)}°C`, tempC);
        graphs.temp = { title: "온도 (가장 높은 센서)", lines: [{ data: hist.temp, color: "#ff8a4c", name: "온도" }], max: "auto", fmt: (n) => n.toFixed(0) + "°C" };
      } else els.temp?.root.classList.add("na");
      if (m.UP) set("up", fmtUptime(parseFloat(m.UP)));

      engine?.check({ cpu: cpuPct, mem: memPct, disk: diskPct, temp: Number.isFinite(tempC) && tempC > 0 ? tempC : null });

      graphs.cpu = { title: "CPU 사용률", lines: [{ data: hist.cpu, color: "#3ecf8e", name: "CPU" }], max: 100, fmt: (n) => n.toFixed(0) + "%", footer: cores ? `${cores}코어` : "" };
      graphs.net = { title: "네트워크 속도", lines: [{ data: hist.rx, color: "#4f8cff", name: "↓" }, { data: hist.tx, color: "#f0b429", name: "↑" }], max: "auto", fmt: (n) => fmtBytes(n) + "/s" };
      if (hover && graphs[hover]) drawGraph(graphs[hover]);
    } catch {
      if (++fails === 3 && !built) bar.innerHTML = `<span class="st-msg">이 서버는 상태 정보를 가져올 수 없어요 (리눅스 서버만 지원)</span>`;
    } finally {
      busy = false;
    }
  };

  bar.innerHTML = `<span class="st-msg">시스템 정보 불러오는 중…</span>`;
  void tick();
  const timer = setInterval(tick, INTERVAL);
  return () => { stopped = true; clearInterval(timer); tip.remove(); };
}
