# SSH GUI

MobaXterm 처럼 **SSH 터미널 + 옆의 파일 브라우저(SFTP)** 를 한 창에서 쓰는 맥용 데스크톱 앱.
Tauri 2 (Rust) + xterm.js 로 만들어서 가볍고 빠릅니다.

## 기능

- **SSH 접속**: 비밀번호 / 키 파일(패스프레이즈 지원), 맥 키체인에 저장해서 클릭 한 번으로 접속
- **터미널**: 탭, 좌우·상하 분할, 로컬 터미널 탭, WebGL 렌더링
- **파일 패널(SFTP)**: 폴더 이동, 다중 선택, 드래그앤드롭/버튼 업로드, 다운로드, 이름 변경, 삭제, 새 폴더
  - 터미널에서 `cd` 하면 파일 패널도 따라가기, 크기/날짜/권한 열 표시 토글
  - 현재 폴더 필터 + 하위 폴더 전체 검색(서버에서 `find`)
- **원격 파일 편집기**: 더블클릭으로 열기, 문법 강조(CodeMirror), ⌘S 저장, 서버에서 파일이 바뀌면 경고
- **상태바**: CPU / RAM / 디스크(물리 디스크 목록) / 네트워크 / Load / 온도 / 가동시간, 마우스를 올리면 그래프
- **프로세스 보기**: 목록, 정렬, 필터, 종료(SIGTERM/SIGKILL)
- **서버 간 파일 복사**, **스니펫(자주 쓰는 명령)**
- **접속 관리**: 폴더 묶기, 즉시 검색(⌘K), `~/.ssh/config` 가져오기, 점프 호스트(ProxyJump)
- **연결 끊김 시 자동 재접속**, 호스트 키 검증(`~/.ssh/known_hosts`)

> 상태바, 프로세스 보기, 파일 변경 감지는 리눅스 서버 기준입니다 (`/proc`, GNU `find`/`ps` 사용).

## 실행

필요한 것: [Rust](https://rustup.rs), Node.js, Xcode Command Line Tools (맥)

```bash
npm install
npm run tauri dev      # 개발 실행
npm run tauri build    # 배포용 .app / .dmg 빌드
```

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

## 보안 메모

- 비밀번호/패스프레이즈는 파일에 저장하지 않고, 저장을 선택하면 macOS 키체인에만 보관합니다.
- 처음 보는 서버는 `known_hosts` 에 등록하고, 호스트 키가 바뀌면 연결을 거부합니다.
- 접속 목록은 `~/Library/Application Support/ssh-gui/connections.json` (비밀번호 제외) 에 저장됩니다.
