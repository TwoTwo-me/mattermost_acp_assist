# mattermost-acp-assist

Mattermost 채널의 게시물에 내가 지정한 이모지를 달면 ACP 에이전트가 해당 스레드를 읽고 **자기 자신과의 DM인 self-DM**에 답합니다. 다른 사람이 쓴 게시물도 원문으로 선택할 수 있습니다. 세션은 리액션을 단 계정과 게시물의 조합으로 구분합니다.

한 계정으로 사용할 때는 그 계정의 PAT와 사용자 ID만 등록합니다. 다른 사람의 리액션은 무시하고 원문 채널에는 봇 답변을 쓰지 않습니다. 아래 안내는 이미 운영 중인 Mattermost 서버를 사용하는 경우입니다. Docker나 별도 봇 계정은 필요하지 않습니다.

## 준비물

- 지원 OS와 CPU. Windows는 지원하지 않습니다.
- Node.js 22 이상과 npm. 다운로드 패키지도 ACP 어댑터 실행에 Node가 필요합니다.
- Mattermost의 개인 액세스 토큰인 PAT, 같은 계정의 사용자 ID와 username, 팀 URL 이름, 감시할 채널 ID. 서버에서 PAT 사용을 허용해야 하며 해당 계정이 채널을 읽을 수 있어야 합니다.
- 사용할 모델 계정 또는 API 키. 예시는 Claude를 선택합니다. 모델 서비스 요금은 별도입니다.

바이너리 설치와 소스 빌드 중 하나를 선택한 뒤 공통 설정으로 진행합니다. 명령 예시는 Bash 기준입니다. macOS에서 다른 셸을 사용한다면 먼저 `bash`를 실행합니다.

## 방법 1. 바이너리 패키지 다운로드

[최신 릴리스](https://github.com/TwoTwo-me/mattermost_acp_assist/releases/latest)에서 플랫폼에 맞는 `.tar.gz`와 같은 이름의 `.tar.gz.sha256`을 받습니다. GitHub 로그인 없이 다운로드할 수 있습니다. GitHub가 자동으로 제공하는 **Source code** 압축 파일은 실행 패키지가 아닙니다.

릴리스 워크플로는 다음 네 대상을 만듭니다. 각 대상의 검사 통과 여부는 해당 릴리스의 Actions 실행에서 확인할 수 있습니다.

| 실행 환경 | `TARGET` | 기본 OS 요구 사항 |
| --- | --- | --- |
| Linux x86-64 | `x86_64-unknown-linux-gnu` | glibc 2.35 이상, Ubuntu 22.04 이상 등 |
| Linux ARM64 | `aarch64-unknown-linux-gnu` | glibc 2.35 이상, Ubuntu 22.04 이상 등 |
| macOS Intel | `x86_64-apple-darwin` | macOS 15 이상 |
| macOS Apple Silicon | `aarch64-apple-darwin` | macOS 15 이상 |

Linux musl 기반 배포판과 Windows용 바이너리는 제공하지 않습니다. 다른 OS나 CPU용 파일을 이름만 바꿔서 실행할 수는 없습니다.

터미널에서는 다음처럼 다운로드합니다. `TARGET`과 `VERSION`을 사용할 릴리스에 맞춥니다.

```bash
VERSION=v0.1.3
TARGET=aarch64-apple-darwin
ARCHIVE="mattermost-acp-assist-${VERSION}-${TARGET}.tar.gz"
mkdir -p "$HOME/Downloads/acp-assist"
cd "$HOME/Downloads/acp-assist"
DOWNLOAD_URL="https://github.com/TwoTwo-me/mattermost_acp_assist/releases/download/$VERSION"
curl --fail --location --output "$ARCHIVE" "$DOWNLOAD_URL/$ARCHIVE"
curl --fail --location --output "$ARCHIVE.sha256" "$DOWNLOAD_URL/$ARCHIVE.sha256"
```

이미 GitHub CLI에 로그인했다면 curl 대신 다음 명령을 사용할 수도 있습니다.

```bash
gh release download "$VERSION" \
	--repo TwoTwo-me/mattermost_acp_assist \
	--pattern "$ARCHIVE" --pattern "$ARCHIVE.sha256"
```

다운로드한 파일의 SHA256을 확인합니다. 브라우저로 받았다면 같은 폴더에서 `ARCHIVE`를 실제 파일 이름으로 설정합니다.

```bash
if [ "$(uname -s)" = Darwin ]; then
	shasum -a 256 -c "$ARCHIVE.sha256"
else
	sha256sum --check "$ARCHIVE.sha256"
fi
```

`OK`가 나온 경우에만 압축을 풉니다. 패키지에는 실행 파일, README, 환경설정 예제, ACP 설정, npm 의존성 목록과 잠금 파일, 제3자 라이선스·저작권 고지인 `THIRD_PARTY_LICENSES.txt`가 들어 있습니다. 소스, 개인 토큰, 모델 인증정보, `node_modules`는 포함하지 않습니다.

```bash
mkdir -p "$HOME/.local/share"
tar -xzf "$ARCHIVE" -C "$HOME/.local/share"
INSTALL_DIR="$HOME/.local/share/${ARCHIVE%.tar.gz}"
cd "$INSTALL_DIR"
npm ci --omit=dev
APP="$INSTALL_DIR/mattermost-acp-assist"
```

이 방법에는 Rust와 Bun이 필요하지 않습니다. `node_modules/.bin`만 따로 복사하지 마세요. 어댑터 패키지 본체와 의존성도 필요하므로 사용할 장비에서 `npm ci`를 실행합니다.

macOS 배포 바이너리는 Apple 개발자 서명과 공증을 포함하지 않습니다. Gatekeeper가 실행을 막으면 출처와 체크섬을 확인한 뒤 시스템 설정의 개인정보 보호 및 보안에서 해당 앱의 실행 허용 항목을 확인합니다. 조직 정책으로 실행이 차단된 경우에는 관리자에게 문의합니다. 시스템 전체 보안 설정을 끄거나 폴더 전체의 격리 속성을 제거하지 마세요.

## 방법 2. 소스를 받아 직접 빌드

Node.js 22 이상과 npm에 더해 Rust stable, Cargo, 해당 OS의 C 링커가 필요합니다. macOS는 Xcode Command Line Tools, Ubuntu는 `build-essential`을 준비합니다. 소스는 GitHub 로그인 없이 받을 수 있습니다.

```bash
git clone https://github.com/TwoTwo-me/mattermost_acp_assist.git
cd mattermost_acp_assist
INSTALL_DIR="$(pwd)"
npm ci --omit=dev
cargo build --release --locked
cp acp/registry.example.json acp/registry.json
APP="$INSTALL_DIR/target/release/mattermost-acp-assist"
```

위 `cp`는 새로 받은 저장소의 개발용 두 계정 레지스트리를 한 계정 예제로 바꿉니다. 이미 사용 중인 설치에서는 기존 레지스트리를 덮어쓰지 말고 직접 수정합니다. 다운로드 패키지에는 처음부터 한 계정 예제가 `acp/registry.json`으로 들어 있습니다.

## 공통 설정. 내 계정 하나 등록

### 1. Mattermost 연결 설정

새 설치에서 환경설정 예제를 복사하고 권한을 제한합니다. 기존 `.env`가 있다면 덮어쓰지 않습니다.

```bash
cd "$INSTALL_DIR"
cp -n .env.example .env
chmod 600 .env
```

`.env`를 편집합니다. 예제의 값을 실제 값으로 바꿉니다.

```dotenv
MM_URL=https://mattermost.example.com
MM_TEAM=your-team
MM_MONITOR_CHANNEL_IDS=your-channel-id
MM_OWNER1_TOKEN=your-personal-access-token
MM_OWNER1_USER_ID=your-mattermost-user-id
TRIGGER_EMOJI=robot,robot_face
ACP_RESPONSE_LANGUAGE=Korean
```

- `MM_URL`은 사용하는 Mattermost 서버 주소입니다. HTTPS는 OS 신뢰 저장소를 사용하므로 사내 CA가 필요하면 OS에 설치합니다.
- `MM_TEAM`은 팀 표시명이 아니라 주소의 팀 이름입니다. `https://mattermost.example.com/my-team/channels/town-square`라면 `my-team`입니다.
- `MM_MONITOR_CHANNEL_IDS`는 채널 ID입니다. 채널 이름이 아닙니다. 여러 ID는 쉼표로 구분합니다.
- `MM_OWNER1_TOKEN`과 `MM_OWNER1_USER_ID`는 반드시 같은 계정의 값이어야 합니다. `owner1`은 앱 내부 설정 라벨입니다.
- `TRIGGER_EMOJI`는 필수 설정입니다. 예제는 `:robot:`과 `:robot_face:`를 선택합니다. 설정 값에는 양끝 콜론을 쓰지 않습니다.
- `ACP_RESPONSE_LANGUAGE`는 답변 언어이며 생략하면 `Korean`입니다. 코드, 식별자, 인용문과 URL은 원래 표기를 유지하도록 지시합니다.

기존 여러 계정 설정을 가져왔다면 사용하지 않을 `MM_OWNER2_TOKEN`과 `MM_OWNER2_USER_ID` 같은 쌍을 모두 제거합니다. 프로세스 환경변수에도 남아 있지 않은지 확인합니다. 프로세스 환경변수가 `.env`보다 우선합니다.

사용자 ID와 username을 모르면 자신의 서버에서 다음 API를 조회할 수 있습니다. PAT는 입력할 때 화면에 표시되지 않으며 curl의 명령행 인수에도 넣지 않습니다. 응답의 `id`와 `username`을 사용합니다.

```bash
MM_URL=https://mattermost.example.com
read -r -s -p 'Mattermost PAT: ' MM_PAT
printf '\n'
printf 'Authorization: Bearer %s\n' "$MM_PAT" | \
	curl --fail --silent --show-error --header @- "$MM_URL/api/v4/users/me"
unset MM_PAT
```

### 2. 실제 username과 ACP 작업 폴더 연결

`$INSTALL_DIR/acp/registry.json`의 키를 실제 Mattermost username으로 바꿉니다. 표시명, 이메일, 사용자 ID, `owner1` 라벨이 아닙니다. 실제 username이 `alice`라면 다음과 같습니다.

```json
{
	"alice": {
		"agent": "claude",
		"cwd": "."
	}
}
```

`cwd: "."`는 앱을 실행한 프로젝트 폴더를 사용합니다. 생략하거나 빈 문자열로 두어도 같습니다. 절대 경로를 넣으면 그 폴더를 고정해서 사용합니다. `.` 이외의 상대 경로는 설치 설정 폴더 기준입니다. 예를 들어 `workspaces/owner1`은 현재 프로젝트가 아니라 설치 폴더 아래의 경로입니다.

선택한 폴더는 미리 존재해야 합니다. 어댑터 프로세스와 ACP `session/new` 모두 이 폴더를 사용합니다. Claude Code의 `CLAUDE.md`, `.claude/`, `.mcp.json` 같은 프로젝트 설정도 여기에서 읽힐 수 있습니다. 앱 바이너리를 프로젝트마다 복사할 필요는 없습니다.

### 3. 모델과 도구 인증

`acp/agents.json`은 어댑터 실행 명령과 기본값을 정의합니다. 기본 Claude 항목은 `haiku` 모델과 `medium` effort를 요청합니다. JSON을 바꾸지 않고 `.env`에서 다음처럼 선택할 수 있습니다.

```dotenv
ACP_CLAUDE_MODEL=haiku
ACP_CLAUDE_EFFORT=medium
```

모델은 모든 에이전트에 대해 `ACP_<에이전트 이름 대문자>_MODEL`로 지정할 수 있습니다. 예를 들어 Codex는 `ACP_CODEX_MODEL`을 사용합니다. 우선순위는 프로세스 환경변수, `.env`, `agents.json`의 `model` 순입니다. 미설정이면 JSON 기본값을 유지합니다. `ACP_CLAUDE_MODEL=`처럼 빈 값으로 덮어쓰면 JSON의 모델 선택도 비활성화하고 별도의 `session/set_model` 요청을 보내지 않습니다.

`haiku` 같은 별칭은 어댑터나 서비스 변경에 따라 새 모델 버전으로 연결될 수 있습니다. 지원하는 정확한 모델 ID를 사용하면 해당 버전을 지정할 수 있습니다. 사용할 수 없는 모델 ID는 어댑터가 거부할 수 있습니다.

`ACP_CLAUDE_EFFORT`는 기본 `claude` 에이전트의 `sessionMeta.claudeCode.options.effort`를 덮어씁니다. 현재 고정한 SDK가 받는 값은 `low`, `medium`, `high`, `max`입니다. SDK 설명은 `max`를 Opus 4.6 전용으로 표기하므로 모든 모델에서 모든 단계가 동작한다고 가정하지 마세요. 사용하는 모델과 어댑터의 지원 범위 안에서 선택합니다.

effort도 프로세스 환경변수가 `.env`보다 우선합니다. 미설정 또는 빈 값이면 JSON의 기존 effort를 그대로 유지합니다. model, effort, mode와 extraArgs는 각각 적용하며 effort 변경은 다른 옵션을 지우지 않습니다. 지원하지 않는 effort나 잘못된 JSON 구조는 시작 전에 오류를 냅니다. `.env` 변경은 앱을 재시작한 뒤 만드는 새 세션부터 적용됩니다.

Claude를 사용할 때는 해당 장비의 Claude Code 로그인을 완료하거나 Anthropic API 키를 프로세스 환경변수로 전달합니다. API 키를 쓰려면 앱을 실행할 Bash에서 다음처럼 입력합니다.

```bash
read -r -s -p 'Anthropic API key: ' ANTHROPIC_API_KEY
printf '\n'
export ANTHROPIC_API_KEY
```

`.env`에 모델 API 키를 적는 것만으로는 어댑터에 전달되지 않습니다. 이 앱은 `.env`를 셸 환경변수로 자동 export하지 않습니다. 이미 로그인한 Claude Code를 사용한다면 별도의 API 키 입력은 필요하지 않습니다.

Codex를 선택하려면 registry의 `agent`를 `codex`로 바꾸고 해당 어댑터에서 사용할 Codex 인증과 모델 설정을 준비합니다. 사용하지 않는 어댑터의 모델 계정까지 인증할 필요는 없습니다. 어댑터가 번들한 CLI 버전과 기존 사용자 설정이 호환되지 않으면 초기화 오류가 날 수 있습니다.

비공개 위키는 프로젝트의 MCP 또는 해당 에이전트 도구에 별도로 연결하고 인증합니다. Mattermost PAT는 위키나 모델 인증을 대신하지 않습니다. 일반 웹 주소도 에이전트에 웹 조회 도구가 있어야 실제로 읽을 수 있습니다.

### 4. 선택 사항. ACP 권한 모드와 Claude 내부 CLI 옵션

Mattermost에는 대화형 승인 UI가 없습니다. 앱은 기존에도 ACP 권한 요청에서 `allow_once`, 없으면 `allow_always`를 자동 선택합니다. 이 동작과 Claude 내부의 권한 모드는 별개입니다.

신뢰하는 작업 폴더에서 Claude의 권한 검사를 생략하려면 `.env`에 다음 설정을 직접 추가합니다. 기본 예제에는 주석으로만 들어 있으며 자동으로 활성화하지 않습니다.

```dotenv
ACP_CLAUDE_MODE=bypassPermissions
```

앱은 `session/new`가 광고한 `modes.availableModes`에서 요청한 ID를 확인하고, 모델 설정 뒤 `session/set_mode`의 성공 응답을 받은 다음에만 질문을 보냅니다. 어댑터가 해당 모드를 제공하지 않거나 변경을 거부하면 질문을 보내지 않고 실패합니다. 지원하지 않는 모드로 자동 대체하지 않습니다. 실제 샌드박스 밖에서 root로 실행하는 경우 Claude 어댑터는 bypass 모드를 제공하지 않을 수 있으며 앱은 이 제한을 우회하지 않습니다. 제한을 피하려고 `IS_SANDBOX`를 임의로 설정하지 마세요.

다른 에이전트도 `ACP_<에이전트 이름 대문자>_MODE`로 지원하는 모드 ID를 지정할 수 있습니다. `acp/agents.json`의 각 에이전트에 `mode` 문자열로 설정할 수도 있습니다. 우선순위는 프로세스 환경변수, `.env`, JSON 설정 순입니다. `ACP_CLAUDE_MODE=`처럼 빈 값으로 덮어쓰면 JSON의 mode도 비활성화하고 별도의 mode RPC를 보내지 않습니다. `.env` 변경은 실행 중 자동으로 읽지 않습니다. 앱을 다시 시작한 뒤 만드는 새 세션부터 적용됩니다.

현재 `claude-code-acp` 어댑터는 자체 명령행의 임의 인수를 Claude CLI로 전달하지 않습니다. `command` 배열 뒤에 `--allow-dangerously-skip-permissions`를 붙여도 모드 선택을 대신하지 못합니다. 이 플래그 자체도 bypass 사용을 허용하는 옵션이지 활성화하는 옵션이 아니며, 현재 어댑터는 허용되는 환경에서 이미 이를 설정합니다.

여러 **내부 Claude CLI 옵션**을 추가하려면 JSON 객체인 `ACP_CLAUDE_EXTRA_ARGS`를 사용합니다. 다음 예제는 `claude`라는 이름의 에이전트에만 적용합니다.

```dotenv
ACP_CLAUDE_EXTRA_ARGS={"max-turns":"5","verbose":null}
```

키에는 앞의 `--` 없이 ASCII 영문자, 숫자, 하이픈으로 된 옵션 이름을 쓰고 첫 글자는 영문자나 숫자로 시작합니다. 문자열 값은 하나의 인수로 전달하고 `null`은 값 없는 플래그를 뜻합니다. 예를 들어 `"max-turns":"5"`는 `--max-turns 5`, `"verbose":null`은 `--verbose`가 됩니다. 숫자 값 `5`나 불리언 `true`는 허용하지 않습니다. 셸 문자열을 나누거나 평가하지 않습니다.

이 객체는 기존 `sessionMeta.claudeCode.options.extraArgs`에 병합하며 같은 키는 환경설정 값이 우선합니다. 기존 effort와 다른 metadata는 유지합니다. 프로세스 환경변수가 `.env`보다 우선합니다. 미설정, 빈 문자열, `{}`는 기존 extraArgs를 지우지 않습니다. 비어 있지 않은 설정이 있는데 `claude` 에이전트가 없거나 JSON 구조가 잘못되면 시작 전에 오류를 냅니다.

`acp/agents.json`의 `command`와 `env`는 어댑터 프로세스 설정이고, `sessionMeta`와 위 extraArgs는 해당 어댑터가 지원하는 내부 설정입니다. 알 수 없는 CLI 옵션의 지원을 보장하지 않습니다. extraArgs는 SDK가 정한 플래그를 덮어쓰거나 프로토콜을 깨뜨릴 수 있는 고급 설정입니다. 권한 변경에는 extraArgs의 `permission-mode`나 `dangerously-skip-permissions` 대신 `ACP_CLAUDE_MODE`를 사용해야 어댑터의 모드 상태와 일치합니다.

권한 검사를 생략해도 API 토큰이나 OS 계정의 접근 범위가 늘어나는 것은 아닙니다. 하지만 읽기 중심 프롬프트는 쓰기 방지 장치가 아니므로 원문 스레드나 웹 페이지의 악의적인 내용이 도구 실행으로 이어질 위험이 있습니다. 제한이 필요하면 별도 OS 계정이나 실제 권한 제한이 있는 컨테이너에서 실행합니다.

## 실행과 사용

### 공개 웹 조회 먼저 확인

앞에서 설정한 `INSTALL_DIR`과 `APP`을 같은 셸에서 사용합니다. 새 셸이라면 두 값을 절대 경로로 다시 설정합니다. 프로젝트 폴더로 이동한 뒤 `PROFILE`을 실제 registry username으로 지정합니다.

```bash
cd /path/to/your-project
PROFILE=alice
ACP_ASSIST_HOME="$INSTALL_DIR" "$APP" smoke \
	--web https://docs.python.org/3/library/tomllib.html \
	--profile "$PROFILE" \
	--question '이 문서의 TOML 지원과 Python 도입 버전을 확인하고 출처를 알려줘.'
```

이 명령은 실제 모델을 호출하므로 비용이 발생할 수 있습니다. Mattermost에는 게시하지 않지만 현재 구현은 공통 설정을 읽으므로 Mattermost와 registry 설정을 먼저 완료해야 합니다. 요청 URL을 읽은 ACP `fetch` 완료 이벤트와 답변의 출처 URL을 확인하면 `PASS`를 출력합니다. 답변 내용 자체는 원문과 대조합니다. 인수 없는 `smoke`는 모든 어댑터를 검사하며 일부 실제 에이전트 오류에도 종료 코드 0을 반환하므로 모델 사용 가능 여부의 판정에는 위 웹 검증을 사용합니다.

### 내 리액션을 self-DM으로 받기

```bash
ACP_ASSIST_HOME="$INSTALL_DIR" "$APP" listen --as owner1
```

`listening as owner1`이 보이면 등록한 계정으로 감시 채널의 게시물에 지정한 리액션을 추가합니다. 다른 사람이 작성한 게시물도 선택할 수 있습니다. **리액션을 추가한 계정이 내 계정일 때만** 세션을 만듭니다. `--as owner1`은 `.env`의 설정 라벨이고 `--profile alice`는 registry의 실제 username이라는 차이에 주의합니다.

앱은 설정한 계정의 PAT와 사용자 ID로 그 계정의 self-DM을 사용합니다. 세션 정보와 답변은 그 self-DM에 게시하며 원문 채널에 답하지 않습니다. 다른 계정이 단 리액션, 다른 DM의 메시지, 앱이 작성한 메시지는 새 질문으로 처리하지 않습니다. 여러 소유자를 등록해서 실행하면 각자 자기 리액션과 자기 self-DM을 사용합니다.

처음에는 선택한 게시물이 속한 루트와 모든 답글을 시간순으로 읽습니다. 그 뒤 self-DM의 **해당 세션 스레드에 답글**을 달면 같은 ACP 세션으로 후속 질문을 보냅니다. 원문을 매번 자동 재조회하지는 않습니다. 최신 내용이 필요하면 재조회를 요청할 수 있으며, 에이전트는 프롬프트에 포함된 `read` 명령을 셸 도구로 실행합니다.

직접 스레드를 다시 읽으려면 다음 명령을 사용합니다.

```bash
ACP_ASSIST_HOME="$INSTALL_DIR" "$APP" read --as owner1 \
	'https://mattermost.example.com/your-team/pl/your-post-id'
```

같은 Mattermost origin의 permalink 또는 post ID만 사용합니다. `--as` 없이 실행하려면 선택 설정인 `MM_READER_TOKEN`이 필요합니다.

종료는 `Ctrl+C`입니다. 앱은 소유한 ACP 프로세스의 종료를 확인하지만 임시 파일은 삭제하지 않습니다. 재시작하면 이전 ACP 세션은 복구되지 않습니다. 원문 게시물에서 트리거 리액션을 제거했다가 다시 추가하면 새 세션과 self-DM 스레드를 만듭니다.

### 연결이 끊어졌을 때

최초 시작의 REST 조회, WebSocket 연결 또는 인증에 실패하면 기존처럼 시작 오류로 종료합니다. 한 번 연결된 뒤 WebSocket이 끊어지면 프로그램이 실행 중인 동안 같은 계정으로 자동 재접속합니다. 대기 시간은 1, 2, 4, 8, 16, 최대 30초이며 60초 이상 연결을 유지한 뒤 끊어졌을 때만 1초로 돌아갑니다.

인증 후 15초 간격으로 Ping을 보내고 해당 Ping의 Pong을 5초 안에 받지 못하면 연결을 다시 만듭니다. 채팅이 없는 정상 연결은 Pong이 오면 유지됩니다. 재접속은 기존 ACP 세션을 다시 만들지 않으며 유휴 세션 만료도 추가하지 않습니다.

연결이 끊긴 동안 놓친 이벤트나 앱 실행 전에 이미 달린 리액션은 복구하지 않습니다. 재접속 뒤 새로 들어오는 이벤트부터 처리합니다. 과거 메시지를 반복 조회하거나 최근 N개만 감시하는 방식은 아닙니다. 읽을 수 있는 오래된 게시물에 지금 새 리액션을 달면 같은 조건으로 처리합니다. 동시 실행 세션 수 제한은 없습니다.

### 콘솔에 표시하는 내용

시작 시 stdout에 `listening as owner1 (...) on ... monitor channel(s) at ...`를 출력합니다. 정상 처리 중에는 질문, 답변, 도구 호출이나 에이전트 stderr를 실시간으로 콘솔에 출력하지 않습니다. 세션 정보와 답변은 self-DM에서 확인합니다. 실패 메시지에는 보관한 에이전트 stderr의 마지막 부분이 포함될 수 있습니다.

WebSocket 연결이 끊기면 stderr에 소유자 라벨, 정적인 실패 종류와 재시도 대기 시간을 표시합니다. 예를 들어 `Mattermost websocket (owner1) disconnected: heartbeat timed out`, `retry in 1s`, `retry failed: connection failed`, `reconnected`가 출력됩니다. 재접속 로그에는 PAT, 인증 payload, 원격 오류 본문이나 채팅 내용을 넣지 않습니다.

## 임시 폴더 수동 정리

세션마다 OS 임시 폴더의 `mattermost-acp-assist-<uid>` 아래에 `session-*` 폴더를 만듭니다. 임시 메모와 다운로드는 그 경로를 사용하도록 지시합니다. ACP 작업 폴더를 바꾸거나 Git worktree를 만들지는 않습니다.

유휴 시간에 따른 세션 만료나 폴더 삭제는 없습니다. 정상 종료, 실패, 웹 smoke 뒤에도 파일을 남깁니다. 삭제가 필요할 때만 다음 명령을 실행합니다.

```bash
"$APP" cleanup-scratch --dry-run
"$APP" cleanup-scratch
```

이 명령은 Mattermost 인증이나 설정 파일 없이 실행할 수 있습니다. 리스너와 같은 OS 사용자 및 `TMPDIR` 설정에서 실행합니다. 출력의 `would_remove`는 삭제 예정 수, `removed`는 삭제 수입니다. 파일 소유 표식과 잠금을 검증한 폴더만 삭제하며 실행 중이거나 종료 확인이 안 된 폴더는 `active_or_uncertain`으로 건너뜁니다. 검증되지 않은 폴더와 심볼릭 링크는 `unverified`로 표시합니다.

강제 종료 뒤에는 자식 프로세스가 살아 있을 수 있습니다. 이런 폴더는 잠금이 풀려도 사용 중 표식이 남아 있으면 삭제하지 않습니다. 관련 프로세스가 모두 종료되었는지 직접 확인한 뒤 OS 파일 도구로 정리합니다. 강제 삭제 옵션은 없습니다. 이전 버전의 표식 없는 `acp-assist-*` 폴더도 이름만으로 삭제하지 않습니다.

루트 잠금이 사용 중이면 명령을 다시 실행합니다. 삭제 오류는 개별 원인과 건수를 출력하고 종료 코드 1을 반환합니다. 프로세스 그룹에서 분리된 백그라운드 서비스는 앱이 종료를 확인할 수 없으므로 정리 전에 직접 종료해야 합니다.

## 설정 파일 위치와 보안

- `ACP_ASSIST_HOME`은 **프로세스 환경변수**로 지정합니다. 앱은 이 폴더의 `acp/agents.json`을 요구합니다. 생략하면 실행 폴더와 상위 폴더에서 찾고, 없으면 바이너리 바로 옆을 찾습니다. 바이너리 폴더의 상위까지 검색하지는 않습니다.
- 기본 env 파일은 설정 폴더의 `.env`입니다. `ACP_ASSIST_ENV`로 다른 파일을 선택할 수 있으며 상대 경로는 설정 폴더 기준입니다.
- 기본 registry는 `acp/registry.json`입니다. `ACP_REGISTRY`로 바꿀 수 있으며 상대 경로는 설정 폴더 기준입니다.
- `agents.json`의 `self` 명령은 현재 바이너리로 바뀝니다. `/`가 들어간 상대 실행 파일 경로는 설정 폴더 기준입니다. 단순 명령 이름은 `PATH`에서 찾습니다. 나머지 인수의 상대 경로는 자동 변환하지 않습니다.
- `state/sessions.json`은 설정 폴더에 자동 생성됩니다. 설정 폴더에 쓰기 권한이 필요합니다. 기존 기록을 보존하려면 업데이트할 때 이 폴더도 보관합니다. `.env`와 실제 registry 설정도 따로 보관합니다.

PAT, 모델 키, `.env`, `state/`를 Git이나 배포 압축 파일에 넣지 마세요. 이 저장소의 패키징 스크립트는 실행에 필요한 예제 파일만 명시적으로 선택합니다.

self-DM은 공개 채널에 답변을 게시하지 않는다는 뜻이지 모델 서비스에 데이터를 보내지 않는다는 뜻은 아닙니다. 원문 스레드와 후속 질문은 선택한 모델로 전달됩니다. 조직의 데이터 처리 정책과 모델 서비스 사용 조건을 확인합니다.

임시 폴더와 읽기 중심 프롬프트는 보안 샌드박스나 쓰기 방지 장치가 아닙니다. 앱은 ACP 권한 요청에 `allow_once`를 우선 선택하고, 없으면 `allow_always`를 자동 선택합니다. 사용자에게 대화형 승인을 요청하지 않습니다. 따라서 프로젝트 훅, MCP, 에이전트 도구가 파일을 변경하거나 외부 작업을 실행할 수 있습니다. 접근을 제한하려면 별도의 OS 계정, 컨테이너 또는 실제 권한 제한을 적용한 샌드박스에서 실행합니다. 앱의 파일 잠금도 같은 사용자 권한의 악의적인 경로 변경을 막는 보안 경계는 아닙니다.

## 유지보수자용 빌드와 릴리스

다음 명령은 소스 저장소에서 사용합니다. 패키징과 패키지 검증에는 Python 3.12 이상이 필요합니다. `TARGET`은 현재 장비의 native Rust target으로 지정합니다.

```bash
TARGET=aarch64-apple-darwin
VERSION=v0.1.3
rustup component add rust-docs rust-src
python3 scripts/generate-notices.py --check
cargo test --locked --target "$TARGET"
MACOSX_DEPLOYMENT_TARGET=15.0 cargo build --release --locked --target "$TARGET"
python3 scripts/package-release.py \
	--binary "target/$TARGET/release/mattermost-acp-assist" \
	--target "$TARGET" --version "$VERSION"
python3 scripts/verify-release.py \
	"dist/mattermost-acp-assist-$VERSION-$TARGET.tar.gz" --install-adapters
```

macOS 릴리스는 `MACOSX_DEPLOYMENT_TARGET=15.0`으로 빌드합니다. 버전은 `Cargo.toml`과 일치해야 합니다. 패키징 스크립트는 기존 바이너리를 포장하며 빌드하지 않습니다. 바이너리를 빌드한 것과 같은 Rust toolchain으로 패키징해야 합니다. 명시적 파일 목록만 사용하고 tar 메타데이터와 gzip 시간을 고정하므로 같은 파일과 toolchain 고지로 같은 압축 파일을 만들 수 있습니다.

`Cargo.lock`을 변경하면 `python3 scripts/generate-notices.py`로 추적 중인 `THIRD_PARTY_LICENSES.txt`를 다시 생성합니다. 생성기는 네 배포 대상의 일반·빌드 의존성을 보수적으로 포함하며 각 crate의 라이선스, 저작권과 NOTICE 원문을 수집합니다. `ring`의 별도 소스 헤더 고지도 포함합니다. 이 목록은 바이너리에 실제 포함된 코드만의 목록이 아닙니다.

패키징할 때 같은 toolchain의 MIT/Apache 라이선스, COPYRIGHT, `COPYRIGHT-library.html`과 설치된 `rust-src` library 트리의 라이선스·저작권·NOTICE 파일을 하나의 고지 파일에 덧붙입니다. 필요한 문서나 소스가 없으면 실패하며 CI도 `rust-docs`, `rust-src`를 설치합니다. Homebrew 등 rustup 이외의 Rust 배포판은 동일한 버전의 문서·소스를 갖춰야 합니다. 원본 HTML 고지는 그대로 보존합니다. 수집된 고지는 법률 검토나 모든 배포 형태에 대한 보증이 아니며 이 앱 자체의 라이선스를 부여하지 않습니다. npm 어댑터는 설치된 각 패키지의 별도 고지를 확인합니다.

검증 스크립트는 SHA256, 파일 목록, 실행 권한을 확인합니다. 임시 폴더에 푼 바이너리로 설정 없는 cleanup과 내장 스텁의 ACP 초기화, 세션 생성, 질문 응답을 확인합니다. `--install-adapters`는 그 임시 패키지에 npm 의존성을 설치하고 어댑터의 ACP 초기화까지만 확인합니다. 모델 질문이나 Mattermost 게시 요청은 보내지 않습니다.

`.github/workflows/release.yml`은 네 native runner에서 테스트, 릴리스 빌드, 압축 해제 검증을 수행합니다. 네 작업이 모두 성공해야 릴리스를 게시합니다. `workflow_dispatch`를 브랜치에서 실행하면 패키지만 Actions artifact로 남깁니다. `v*` 태그를 push하거나 해당 태그에서 수동 실행하면 릴리스에 `.tar.gz`, 개별 `.sha256`, 통합 `SHA256SUMS`를 게시합니다.

업로드는 draft 릴리스에서 진행하며 네 압축 파일, 네 개별 체크섬과 통합 체크섬을 모두 올린 뒤 게시합니다. 업로드가 실패하면 같은 태그의 작업을 다시 실행해 draft를 복구할 수 있습니다. 이미 게시한 릴리스는 덮어쓰지 않고 오류로 종료합니다. 게시 후 수정이 필요하면 `Cargo.toml` 버전을 올리고 새 태그를 사용합니다.

검토한 변경을 커밋하고 push한 뒤 버전 태그를 게시합니다.

```bash
git tag v0.1.3
git push origin v0.1.3
```

### 선택 사항. 로컬 Mattermost 실험

운영 서버를 쓰는 설치에는 `lab/`가 필요하지 않습니다. 개발용 Mattermost와 테스트 계정을 따로 만들 때만 Docker 환경에서 `bash lab/start.sh`, `bash lab/seed.sh`, `bash lab/verify.sh`를 사용합니다. 시드 스크립트는 로컬 실험용 설정을 생성하므로 운영 `.env`가 있는 설치에서 실행하지 않습니다. `probe`도 실험 서버의 전체 리액션 흐름을 검증하는 유지보수 명령입니다.

원격 실험 서버용 `scripts/seed-proxmox.sh`에는 서버 기본값이 없습니다. 직접 관리하는 테스트 서버에 한해서 `MM_URL`에 HTTPS 주소, `SSH_HOST`에 SSH 대상, `APP_CONTAINER`에 Mattermost Docker 컨테이너 이름을 명시해 실행합니다. `CREDS`와 `ENV_FILE`은 생성할 로컬 실험용 인증정보·환경 파일 경로입니다. 기존 운영 설정을 대상으로 실행하지 마세요.
