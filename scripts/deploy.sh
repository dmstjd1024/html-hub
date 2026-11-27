#!/bin/sh
# 맥미니 배포: 시험 → 빌드 → 이전 바이너리 보관 → 재시작 → /healthz 확인. 실패하면 이전 바이너리로 되돌린다.
# 실행: ./scripts/deploy.sh   (건너뛰기: SKIP_TESTS=1)
set -eu
cd "$(dirname "$0")/.."
BIN=target/release/html-hub
LABEL="gui/$(id -u)/dev.eunseong.html-hub"
# 판단은 이 컴퓨터 안에서 한다. 공개 주소(터널)는 터널 문제로 실패할 수 있어 경고만 낸다.
LOCAL="http://$(sed -n 's/^BIND_ADDR=//p' .env | tr -d '"')/healthz"
PUBLIC="$(sed -n 's/^BASE_URL=//p' .env | tr -d '"')/healthz"
DB="$(sed -n 's/^DATABASE_PATH=//p' .env | tr -d '"')"
SNAP_DIR="${BACKUP_DIR:-$HOME/backups/html-hub}/db/manual"

# 시험은 E2E 까지 돈다 (구글 OIDC 설정을 받으므로 네트워크가 필요하다). 급하면 SKIP_TESTS=1.
[ "${SKIP_TESTS:-}" = 1 ] || cargo test --quiet
# 되돌릴 때 DB 도 돌릴 수 있게 배포 전 스냅샷을 남긴다 (일일 백업 순환과 따로, 최근 10개).
umask 077
mkdir -p "$SNAP_DIR"
sqlite3 "$DB" ".backup '$SNAP_DIR/before-deploy-$(date +%Y%m%d-%H%M%S).db'"
ls -1 "$SNAP_DIR"/before-deploy-*.db | sort -r | tail -n +11 | while read -r f; do rm -f "$f"; done
[ -f "$BIN" ] && cp -p "$BIN" "$BIN.prev"
cargo build --release --quiet

healthy() {
  for _ in $(seq 1 20); do
    [ "$(curl -s -m 3 "$LOCAL" || true)" = ok ] && return 0
    sleep 1
  done
  return 1
}

launchctl kickstart -k "$LABEL"
if healthy; then
  echo "배포 완료: $(git rev-parse --short HEAD) ($LOCAL ok)"
  [ "$(curl -s -m 5 "$PUBLIC" || true)" = ok ] || echo "경고: 공개 주소 $PUBLIC 가 답하지 않는다 — 터널(cloudflared)을 확인" >&2
  exit 0
fi
echo "새 버전이 /healthz 에 답하지 않아 이전 바이너리로 되돌린다 (DB 는 그대로. 필요하면 $SNAP_DIR 의 최신 스냅샷)" >&2
[ -f "$BIN.prev" ] && cp -p "$BIN.prev" "$BIN"
launchctl kickstart -k "$LABEL"
healthy && echo "되돌림 완료" >&2 || echo "되돌린 뒤에도 응답 없음 — 로그 확인: ~/Library/Logs/html-hub/err.log" >&2
exit 1
