import { test } from "node:test";
import assert from "node:assert/strict";
import { createAlertEngine, DEFAULT_ALERTS, type AlertCfg } from "../src/alerts.ts";

const setup = (over: Partial<AlertCfg> = {}) => {
  let t = 0;
  const msgs: string[] = [];
  const cfg = { ...DEFAULT_ALERTS, ...over };
  const eng = createAlertEngine(() => cfg, (title) => msgs.push(title), 2000, () => t);
  const tick = (v: Partial<{ cpu: number | null; mem: number | null; disk: number | null; temp: number | null }>) => {
    t += 2000;
    eng.check({ cpu: null, mem: null, disk: null, temp: null, ...v });
  };
  return { msgs, tick, advance: (ms: number) => (t += ms) };
};

test("디스크는 임계값을 넘는 즉시 알리고, 쿨다운 동안은 반복하지 않는다", () => {
  const { msgs, tick, advance } = setup();
  tick({ disk: 80 }); assert.equal(msgs.length, 0);
  tick({ disk: 92 }); assert.deepEqual(msgs, ["디스크 용량 부족"]);
  for (let i = 0; i < 10; i++) tick({ disk: 93 });
  assert.equal(msgs.length, 1, "쿨다운 중에는 같은 알림 금지");
  advance(31 * 60 * 1000); tick({ disk: 93 });
  assert.equal(msgs.length, 2, "30분 뒤에는 다시 알림");
});

test("CPU 는 설정한 시간 이상 계속 높을 때만 알린다 (순간 급등은 무시)", () => {
  const { msgs, tick } = setup({ cpu: 90, cpuSec: 10 });
  for (let i = 0; i < 4; i++) tick({ cpu: 99 });   // 8초
  assert.equal(msgs.length, 0);
  tick({ cpu: 50 });                                // 정상으로 돌아오면 카운터 초기화
  for (let i = 0; i < 4; i++) tick({ cpu: 99 });   // 다시 8초
  assert.equal(msgs.length, 0, "초기화 후 다시 처음부터 세야 함");
  tick({ cpu: 99 });                                // 10초 도달
  assert.deepEqual(msgs, ["CPU 사용률 높음"]);
});

test("값이 없는 샘플(null)은 카운터를 깨지 않는다", () => {
  const { msgs, tick } = setup({ mem: 90, memSec: 6 });
  tick({ mem: 95 }); tick({ mem: null }); tick({ mem: 95 }); tick({ mem: 95 });
  assert.deepEqual(msgs, ["메모리 사용률 높음"]);
});

test("정상으로 돌아왔다가 다시 넘으면 쿨다운과 무관하게 다시 알린다", () => {
  const { msgs, tick } = setup();
  tick({ temp: 90 }); assert.equal(msgs.length, 1);
  tick({ temp: 60 });   // 정상 복귀 → 초기화
  tick({ temp: 91 });
  assert.equal(msgs.length, 2);
});

test("알림을 끄면 아무것도 보내지 않는다", () => {
  const { msgs, tick } = setup({ enabled: false });
  tick({ disk: 99, temp: 99 });
  assert.equal(msgs.length, 0);
});
