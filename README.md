# Pier

MobaXterm 처럼 **SSH 터미널 + 옆의 파일 브라우저(SFTP)** 를 한 창에서 쓰는 맥용 데스크톱 앱.
Tauri 2 (Rust) + xterm.js 로 만들어서 가볍고 빠릅니다.

## 기능

- **SSH 접속**: 비밀번호 / 키 파일(패스프레이즈 지원), 맥 키체인 저장으로 클릭 한 번 접속, 점프 호스트(ProxyJump), 연결이 끊기면 자동 재접속
  - **tmux 자동 연결**: 접속하면 tmux 세션에 붙어서, 접속이 끊겨도 작업이 계속되고 재접속하면 그대로 이어져요
  - **비밀번호 → 키 접속 전환**: 앱이 키를 만들고 서버에 등록한 뒤 키로 로그인되는지 확인하고 접속 정보를 전환
- **터미널**: 탭, 좌우·상하 분할, 로컬 터미널 탭, WebGL 렌더링
- **파일 패널(SFTP)**: 폴더 이동, 다중 선택, 드래그앤드롭/버튼 업로드, 다운로드, 이름 변경, 삭제, 새 폴더
  - 터미널에서 `cd` 하면 파일 패널도 따라가기, 크기/날짜/권한 열 표시 토글
  - 현재 폴더 필터 + 하위 폴더 전체 검색(서버에서 `find`)
  - **권한 변경(chmod/chown)**, **압축하기 / 압축 풀기**(tar.gz, zip 등), 서버 간 파일 복사
- **파일 편집**: 내장 에디터(문법 강조, ⌘S, 서버에서 파일이 바뀌면 경고) 또는 **외부 에디터**(VS Code 등)로 열기 — 저장하면 서버에 자동 반영
- **상태바**: CPU / RAM / 디스크(물리 디스크 목록) / 네트워크 / Load / 온도 / 가동시간, 마우스를 올리면 그래프
  - **서버 상태 알림**: 디스크·CPU·메모리·온도가 기준을 넘으면 맥 알림
- **서버 운영 도구**: 프로세스 보기/종료, **로그 실시간 보기**(/var/log 파일, systemd 서비스, Docker), **Docker 컨테이너 패널**, **SSH 터널링**(로컬 포트 포워딩), 스니펫
- **접속 관리**: 폴더 묶기, 즉시 검색(⌘K), `~/.ssh/config` 가져오기
- 호스트 키 검증(`~/.ssh/known_hosts`), ESC로 모든 창 닫기

> 상태바, 프로세스 보기, 파일 변경 감지는 리눅스 서버 기준입니다 (`/proc`, GNU `find`/`ps` 사용).

## 실행

필요한 것: [Rust](https://rustup.rs), Node.js, Xcode Command Line Tools (맥)

```bash
npm install
npm run tauri dev      # 개발 실행
npm run tauri build    # 배포용 .app / .dmg 빌드
npm test               # 테스트 (아래 참고)
```

### 테스트

- `tests/` : 서버 상태 알림 규칙 등 프론트 로직 단위 테스트 (`node --test`)
- `src-tauri` 의 `cargo test` : 이 맥에 임시 `sshd`(127.0.0.1 전용, 일회용 키)를 띄워서 키 인증, 점프 호스트, SFTP, 터널, 로그 스트리밍, 외부 에디터 동기화까지 실제 SSH 로 검증해요. 실제 `~/.ssh` 는 건드리지 않습니다. (맥/리눅스의 `/usr/sbin/sshd` 필요)
- `tests/ui/mock.html` : Tauri 없이 브라우저에서 화면만 확인하는 가짜 백엔드 페이지 (`npm run dev` 후 `/tests/ui/mock.html`)

## 구조

```
src/                 프론트엔드 (TypeScript)
  main.ts            화면/탭/파일 패널/접속 관리
  terminal.ts        터미널 분할, 이벤트 라우팅
  editor.ts          원격 파일 편집기
  monitor.ts         하단 상태바와 그래프
src-tauri/src/
  ssh.rs             SSH/SFTP, 터미널, 키체인 등 백엔드 (russh)
```

## 이름에 대해

앱 이름은 **Pier** 예요. 이전 이름(`ssh-gui`)으로 저장된 데이터(접속 목록 폴더, 키체인 항목, 앱 식별자 `com.sshgui.app`)는 그대로 유지해서, 이름을 바꿔도 저장해 둔 접속과 비밀번호는 그대로 쓸 수 있어요.

## 보안 메모

- 비밀번호/패스프레이즈는 파일에 저장하지 않고, 저장을 선택하면 macOS 키체인에만 보관합니다.
- 처음 보는 서버는 `known_hosts` 에 등록하고, 호스트 키가 바뀌면 연결을 거부합니다.
- 접속 목록은 `~/Library/Application Support/ssh-gui/connections.json` (비밀번호 제외) 에 저장됩니다.
