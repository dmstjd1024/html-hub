#!/bin/sh
# DB 스냅샷과 문서 파일을 BACKUP_DIR 로 복사한다. 환경변수: DATABASE_PATH, FILES_DIR, BACKUP_DIR
# 선택: BACKUP_REMOTE (rsync 대상, 예: user@host:backups/html-hub/) 를 주면 다른 기계로도 보낸다.
# 선택: ALERT_WEBHOOK (Slack 수신 웹후크 주소) 를 주면 실패했을 때 알린다.
set -eu
notify() {
  [ -n "${ALERT_WEBHOOK:-}" ] || return 0
  curl -fsS -m 10 -H 'Content-Type: application/json' -d "{\"text\":\"$1\"}" "$ALERT_WEBHOOK" >/dev/null || true
}
trap 'rc=$?; [ "$rc" -eq 0 ] || { echo "백업 실패: 종료 코드 $rc ($(date "+%F %T"))" >&2; notify "html-hub 백업 실패 (종료 코드 $rc, $(hostname -s))"; }' EXIT
: "${DATABASE_PATH:?}" "${FILES_DIR:?}" "${BACKUP_DIR:?}"
umask 077
mkdir -p "$BACKUP_DIR/db" "$BACKUP_DIR/files"

# 쓰는 중에도 일관된 스냅샷이 되도록 sqlite 의 백업 기능을 쓴다. 14개만 남긴다.
sqlite3 "$DATABASE_PATH" ".backup '$BACKUP_DIR/db/html-hub-$(date +%Y%m%d-%H%M%S).db'"
ls -1 "$BACKUP_DIR"/db/*.db | sort -r | tail -n +15 | while read -r f; do rm -f "$f"; done

# 숨긴 문서도 영구 보관이므로(정책서 D4) --delete 를 쓰지 않는다.
rsync -a "$FILES_DIR/" "$BACKUP_DIR/files/"

if [ -n "${BACKUP_REMOTE:-}" ]; then
  rsync -a "$BACKUP_DIR/" "$BACKUP_REMOTE"
fi
echo "백업 완료: $(date '+%F %T')"
