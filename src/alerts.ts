/** 서버 상태 알림 규칙 (디스크/CPU/메모리/온도). 화면·알림 전달과 분리해서 순수 로직만 담았다. */
export interface AlertCfg {
  enabled: boolean;
  disk: number;   // 디스크 사용률(%) 이상
  cpu: number;    // CPU 사용률(%) 이상이
  cpuSec: number; //   이 시간(초) 이상 계속될 때
  mem: number;    // 메모리 사용률(%) 이상이
  memSec: number; //   이 시간(초) 이상 계속될 때
  temp: number;   // 온도(°C) 이상
}

export const DEFAULT_ALERTS: AlertCfg = { enabled: true, disk: 90, cpu: 90, cpuSec: 60, mem: 90, memSec: 60, temp: 85 };

export function getAlertCfg(): AlertCfg {
  try { return { ...DEFAULT_ALERTS, ...JSON.parse(localStorage.getItem("alerts") || "{}") }; } catch { return { ...DEFAULT_ALERTS }; }
}
export function setAlertCfg(c: AlertCfg) { localStorage.setItem("alerts", JSON.stringify(c)); }

export interface Sample { cpu: number | null; mem: number | null; disk: number | null; temp: number | null }

const COOLDOWN_MS = 30 * 60 * 1000; // 같은 알림은 30분에 한 번만

/**
 * 샘플이 들어올 때마다 check() 를 부른다. 임계값을 넘으면 notify(제목, 내용) 를 호출한다.
 * - CPU/메모리: 설정한 시간 동안 계속 넘어 있어야 알림 (잠깐 튀는 건 무시)
 * - 디스크/온도: 넘는 즉시
 * - 같은 종류는 쿨다운 동안 다시 알리지 않고, 값이 정상으로 돌아오면 다시 알릴 수 있게 초기화
 */
export function createAlertEngine(
  getCfg: () => AlertCfg,
  notify: (title: string, body: string) => void,
  intervalMs: number,
  now: () => number = Date.now,
) {
  const over: Record<string, number> = { cpu: 0, mem: 0 }; // 연속으로 넘은 시간(ms)
  const last: Record<string, number> = {};

  const fire = (key: string, title: string, body: string) => {
    const t = now();
    if (last[key] !== undefined && t - last[key] < COOLDOWN_MS) return;
    last[key] = t;
    notify(title, body);
  };
  const clear = (key: string) => { delete last[key]; };

  return {
    check(v: Sample) {
      const c = getCfg();
      if (!c.enabled) return;

      for (const [key, val, th, sec, label] of [
        ["cpu", v.cpu, c.cpu, c.cpuSec, "CPU"],
        ["mem", v.mem, c.mem, c.memSec, "메모리"],
      ] as const) {
        if (val !== null && val >= th) {
          over[key] += intervalMs;
          if (over[key] >= sec * 1000) fire(key, `${label} 사용률 높음`, `${label}가 ${Math.round(val)}% (기준 ${th}% 이상, ${sec}초 이상 지속)`);
        } else if (val !== null) { over[key] = 0; clear(key); }
      }
      if (v.disk !== null) {
        if (v.disk >= c.disk) fire("disk", "디스크 용량 부족", `디스크가 ${Math.round(v.disk)}% 찼어요 (기준 ${c.disk}% 이상)`);
        else clear("disk");
      }
      if (v.temp !== null) {
        if (v.temp >= c.temp) fire("temp", "온도 높음", `온도가 ${Math.round(v.temp)}°C 예요 (기준 ${c.temp}°C 이상)`);
        else clear("temp");
      }
    },
  };
}
