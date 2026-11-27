#!/bin/sh
# 맥미니 배포: 시험 → 빌드 → 이전 바이너리 보관 → 재시작 → /healthz 확인. 실패하면 이전 바이너리로 되돌린다.
# 실행: ./scripts/deploy.sh   (건너뛰기: SKIP_TESTS=1)
set -eu
cd "$(dirname "$0")/.."
BIN=target/release/html-hub
LABEL="gui/$(id -u)/dev.eunseong.html-hub"
URL="$(sed -n 's/^BASE_URL=//p' .env | tr -d '"')/healthz"

[ "${SKIP_TESTS:-}" = 1 ] || cargo test --quiet
[ -f "$BIN" ] && cp -p "$BIN" "$BIN.prev"
cargo build --release --quiet

healthy() {
  for _ in $(seq 1 20); do
    [ "$(curl -s -m 3 "$URL" || true)" = ok ] && return 0
    sleep 1
  done
  return 1
}

launchctl kickstart -k "$LABEL"
if healthy; then
  echo "배포 완료: $(git rev-parse --short HEAD) ($URL ok)"
  exit 0
fi
echo "새 버전이 /healthz 에 답하지 않아 이전 바이너리로 되돌린다" >&2
[ -f "$BIN.prev" ] && cp -p "$BIN.prev" "$BIN"
launchctl kickstart -k "$LABEL"
healthy && echo "되돌림 완료" >&2 || echo "되돌린 뒤에도 응답 없음 — 로그 확인: ~/Library/Logs/html-hub/err.log" >&2
exit 1
