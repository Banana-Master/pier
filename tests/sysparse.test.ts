import { test } from "node:test";
import assert from "node:assert/strict";
import { parseMountLine, diskKind } from "../src/sysparse.ts";

test("df 가공 결과를 해석한다 (사용률 열이 5번째, 마운트 위치가 그 뒤)", () => {
  // 서버 스크립트: df -PkT | awk '{print "MNT",$1,$2,$3,$4,$6,$7}' 에서 "MNT " 를 뗀 값 (라즈베리파이 실제 형태)
  const m = parseMountLine("/dev/nvme0n1p2 ext4 244190646 35651584 16% /");
  assert.deepEqual(m, { dev: "/dev/nvme0n1p2", fs: "ext4", total: 244190646, used: 35651584, pct: 16, at: "/" });
  const b = parseMountLine("/dev/mmcblk0p1 vfat 516096 192512 38% /boot/firmware");
  assert.equal(b?.pct, 38);
  assert.equal(b?.at, "/boot/firmware");
});

test("사용률 칸이 비정상이면 사용량/전체로 직접 계산하고, 쓸 수 없는 줄은 버린다", () => {
  assert.equal(parseMountLine("/dev/sda1 ext4 1000 250 - /data")?.pct, 25);
  assert.equal(parseMountLine("/dev/sda1 ext4"), null);
  assert.equal(parseMountLine("/dev/sda1 ext4 abc def 10% /x"), null);
  assert.equal(parseMountLine(""), null);
});

test("디스크 종류: NVMe / SD카드 / USB / SSD / HDD", () => {
  assert.equal(diskKind({ NAME: "nvme0n1", TRAN: "nvme", ROTA: "0" }), "NVMe SSD");
  assert.equal(diskKind({ NAME: "mmcblk0", TRAN: "", ROTA: "0" }), "SD/eMMC 카드");
  assert.equal(diskKind({ NAME: "sda", TRAN: "usb", ROTA: "1" }), "USB 저장장치", "USB 메모리가 ROTA=1 로 보고돼도 HDD 로 표시하면 안 됨");
  assert.equal(diskKind({ NAME: "sdb", TRAN: "sata", ROTA: "1" }), "HDD");
  assert.equal(diskKind({ NAME: "sdc", TRAN: "sata", ROTA: "0" }), "SSD");
});
