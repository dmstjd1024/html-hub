#!/bin/bash
# html-hub 감시 (launchd 로 5분마다): 서버(로컬)·공개 주소(터널)·백업 나이를 보고 상태가 바뀔 때만 알린다.
# 두 번 연달아 실패해야 알린다 (배포 재시작 같은 잠깐의 끊김은 넘긴다). 회복하면 한 번 더 알린다.
# 환경변수: ALERT_TELEGRAM_LIB (telegram_send 함수를 주는 스크립트), BIND_ADDR, BASE_URL, BACKUP_DIR
# 맥 자체가 꺼지면 이 감시도 멈춘다 — 그건 바깥 감시가 잡아야 한다.
set -u
# shellcheck disable=SC1090
source "$ALERT_TELEGRAM_LIB"
ST="${STATE_DIR:-$HOME/.local/state/html-hub-watch}"
mkdir -p "$ST"

check() { # 이름 설명 명령...
  local name=$1 what=$2; shift 2
  if "$@"; then
    if [ -f "$ST/$name.alerted" ]; then
      telegram_send "✅ html-hub 회복됨 (이전 문제: $what)"
    fi
    rm -f "$ST/$name.fails" "$ST/$name.alerted"
    return
  fi
  local n=$(( $(cat "$ST/$name.fails" 2>/dev/null || echo 0) + 1 ))
  echo "$n" > "$ST/$name.fails"
  echo "$(date '+%F %T') 실패 $n: $what"
  if [ "$n" -eq 2 ] && [ ! -f "$ST/$name.alerted" ]; then
    touch "$ST/$name.alerted"
    telegram_send "🚨 html-hub: $what"
  fi
}

ok_url() { [ "$(curl -s -m 10 "$1" || true)" = ok ]; }
fresh_backup() { find "$BACKUP_DIR/db" -maxdepth 1 -name 'html-hub-*.db' -mmin -$((26 * 60)) | grep -q .; }

check local "서버가 응답하지 않는다 (http://$BIND_ADDR/healthz). 로그: ~/Library/Logs/html-hub/err.log" ok_url "http://$BIND_ADDR/healthz"
check public "공개 주소가 응답하지 않는다 — 터널(cloudflared) 확인 ($BASE_URL/healthz)" ok_url "$BASE_URL/healthz"
check backup "26시간 넘게 새 백업이 없다 ($BACKUP_DIR/db). 로그: ~/Library/Logs/html-hub/backup.log" fresh_backup
