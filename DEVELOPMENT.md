# 개발 안내

Pier는 Tauri 2(Rust) + TypeScript(xterm.js, CodeMirror)로 만들었어요. 그냥 쓰기만 할 거면 이 문서는 필요 없어요.
([README](README.md)에 설치 방법이 있어요.)

## 실행

필요한 것: [Rust](https://rustup.rs), Node.js, Xcode Command Line Tools (맥)

```bash
npm install
npm run tauri dev      # 개발 실행
npm run tauri build    # 배포용 .app / .dmg 빌드 (src-tauri/target/release/bundle/ 아래에 생겨요)
npm test               # 테스트 (아래 참고)
```

## 테스트

- `tests/` : 서버 상태 알림 규칙, 시스템 정보 해석 같은 프론트 로직 단위 테스트 (`node --test`)
- `src-tauri` 의 `cargo test` : 이 맥에 임시 `sshd`(127.0.0.1 전용, 일회용 키)를 띄워서 키 인증, 점프 호스트, SFTP, 터널, 로그 스트리밍, 외부 에디터 동기화까지 실제 SSH 로 검증해요. 실제 `~/.ssh` 는 건드리지 않아요. (`/usr/sbin/sshd` 필요)
- `tests/ui/mock.html` : Tauri 없이 브라우저에서 화면만 확인하는 가짜 백엔드 페이지 (`npm run dev` 후 `/tests/ui/mock.html`)

## 구조

```
src/                 프론트엔드 (TypeScript)
  main.ts            화면/탭/파일 패널/접속 관리/각종 도구 창
  terminal.ts        터미널 분할, 이벤트 라우팅
  editor.ts          내장 파일 편집기
  monitor.ts         하단 상태바와 그래프
  alerts.ts          서버 상태 알림 규칙
  sysparse.ts        서버가 돌려준 시스템 정보 해석
src-tauri/src/
  ssh.rs             SSH/SFTP, 터미널, 키체인, 터널, 로그 등 백엔드 (russh)
assets/icon.svg      아이콘 원본
```

## 아이콘 다시 만들기

원본은 `assets/icon.svg` 예요. 바꾸고 나서 아래 명령으로 모든 크기(맥 `.icns`, Windows `.ico`, PNG)를 다시 만들 수 있어요.

```bash
npx tauri icon assets/icon.svg
```

안드로이드/iOS 폴더가 같이 생기면 지워도 돼요. 이 앱은 데스크톱 전용이에요.

## 이름에 대해

앱 이름은 **Pier** 지만, 예전 이름(`ssh-gui`)으로 저장되는 데이터(접속 목록 폴더, 키체인 항목, 앱 식별자 `com.sshgui.app`)는 일부러 그대로 뒀어요. 바꾸면 이미 저장해 둔 접속과 비밀번호를 못 찾게 되거든요.
