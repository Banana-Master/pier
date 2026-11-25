/** 서버가 돌려준 시스템 정보 텍스트를 해석하는 순수 함수들 (화면·Tauri 와 분리해서 테스트 가능) */

export interface Mount { dev: string; fs: string; total: number; used: number; pct: number; at: string }

/**
 * `df -PkT` 를 awk 로 가공한 한 줄을 해석한다.
 * 형식: "<장치> <파일시스템> <전체KB> <사용KB> <사용률%> <마운트 위치>"  (앞의 "MNT " 는 이미 뗀 상태)
 */
export function parseMountLine(line: string): Mount | null {
  const p = line.trim().split(/\s+/);
  if (p.length < 6) return null;
  const total = Number(p[2]), used = Number(p[3]);
  if (!Number.isFinite(total) || !Number.isFinite(used)) return null;
  let pct = parseInt(p[4], 10);
  if (!Number.isFinite(pct)) pct = total > 0 ? Math.round((used / total) * 100) : 0; // 사용률 칸이 깨졌으면 직접 계산
  return { dev: p[0], fs: p[1], total, used, pct, at: p.slice(5).join(" ") };
}

/** `lsblk -P` 로 얻은 디스크 한 개의 종류를 사람이 읽을 이름으로 */
export function diskKind(d: { NAME?: string; TRAN?: string; ROTA?: string }): string {
  const name = d.NAME ?? "";
  if (d.TRAN === "nvme" || name.startsWith("nvme")) return "NVMe SSD";
  if (name.startsWith("mmcblk")) return "SD/eMMC 카드";
  // USB 메모리/외장 디스크는 ROTA 값이 믿을 수 없어서(메모리도 1로 보고하기도 함) 연결 방식으로 표시한다
  if (d.TRAN === "usb") return "USB 저장장치";
  return d.ROTA === "1" ? "HDD" : "SSD";
}
