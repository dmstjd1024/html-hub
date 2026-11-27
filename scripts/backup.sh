#!/bin/sh
# DB 스냅샷과 문서 파일을 BACKUP_DIR 로 복사한다. 환경변수: DATABASE_PATH, FILES_DIR, BACKUP_DIR
# 선택: BACKUP_REMOTE (rsync 대상, 예: user@host:backups/html-hub/) 를 주면 다른 기계로도 보낸다.
# 선택: 실패했을 때 알린다 — ALERT_TELEGRAM_LIB (telegram_send 를 주는 스크립트) 나 ALERT_WEBHOOK (Slack 수신 웹후크).
set -eu
notify() {
  if [ -n "${ALERT_TELEGRAM_LIB:-}" ]; then
    bash -c 'source "$1" && telegram_send "$2"' _ "$ALERT_TELEGRAM_LIB" "$1" >/dev/null 2>&1 || true
  fi
  if [ -n "${ALERT_WEBHOOK:-}" ]; then
    curl -fsS -m 10 -H 'Content-Type: application/json' -d "{\"text\":\"$1\"}" "$ALERT_WEBHOOK" >/dev/null || true
  fi
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
# 관리자가 완전 삭제한 문서 파일은 백업에서도 지운다 (서버가 FILES_DIR 옆 purged.txt 에 적는다).
PURGED="$(dirname "$FILES_DIR")/purged.txt"
if [ -f "$PURGED" ]; then
  while IFS= read -r f; do
    case "$f" in */*|..*|'') continue ;; esac
    rm -f "$BACKUP_DIR/files/$f"
  done < "$PURGED"
fi

# 선택: 바깥(R2)으로 — 최신 DB 스냅샷과 파일을 묶어 암호화한 뒤 Worker(ops/worker)로 올린다. 30개를 남긴다.
# shortcut: 묶음 하나로 올리므로 Worker 요청 한도(무료 100MB)를 넘으면 실패해 알림이 온다. 그때 파일을 나눠 올린다.
if [ -n "${BACKUP_UPLOAD_URL:-}" ]; then
  : "${BACKUP_UPLOAD_SECRET:?}" "${BACKUP_KEY_FILE:?}"
  latest="$(ls -1 "$BACKUP_DIR"/db/html-hub-*.db | sort | tail -n 1)"
  name="html-hub-$(date +%Y%m%d-%H%M%S).tar.gz.enc"
  tmp="$(mktemp)"
  tar -czf - -C "$BACKUP_DIR" "db/$(basename "$latest")" files \
    | openssl enc -aes-256-cbc -pbkdf2 -iter 200000 -salt -pass "file:$BACKUP_KEY_FILE" -out "$tmp"
  curl -fsS -m 300 -X PUT -H "Authorization: Bearer $BACKUP_UPLOAD_SECRET" --data-binary "@$tmp" "$BACKUP_UPLOAD_URL/backup/$name" >/dev/null
  rm -f "$tmp"
fi

if [ -n "${BACKUP_REMOTE:-}" ]; then
  # 로컬 백업 폴더가 기준이다. 완전 삭제와 오래된 DB 스냅샷 정리를 원격에도 따라가게 한다.
  rsync -a --delete "$BACKUP_DIR/" "$BACKUP_REMOTE"
fi
echo "백업 완료: $(date '+%F %T')"
