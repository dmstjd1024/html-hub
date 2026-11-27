// html-hub 바깥 감시와 원격 백업.
// - 5분마다(Durable Object 알람) HEALTH_URL 을 본다. 두 번 연달아 실패하면 텔레그램, 회복하면 한 번 더. 맥미니가 통째로 꺼져도 잡는다.
// - 마지막 백업이 26시간보다 오래되면 알린다 (맥미니가 죽어 백업이 안 오는 경우).
// - PUT /backup/<이름> 으로 맥미니가 암호화한 백업을 받아 R2 에 둔다. KEEP 개만 남긴다. GET /backup/latest 로 내려받는다.
// 상태는 R2 의 monitor-state.json 한 파일에 둔다.
import { DurableObject } from "cloudflare:workers";

const EVERY_MS = 5 * 60 * 1000;
const STATE = "monitor-state.json";
const NAME = /^html-hub-\d{8}-\d{6}\.tar\.gz\.enc$/;

async function telegram(env, text) {
  await fetch(`https://api.telegram.org/bot${env.TG_TOKEN}/sendMessage`, {
    method: "POST",
    headers: { "content-type": "application/json" },
    body: JSON.stringify({ chat_id: env.TG_CHAT, text }),
  });
}

async function healthy(url) {
  try {
    const r = await fetch(url, { signal: AbortSignal.timeout(10000), cf: { cacheTtl: 0 } });
    return r.ok && (await r.text()).trim() === "ok";
  } catch {
    return false;
  }
}

async function check(env) {
  const saved = await env.BACKUPS.get(STATE);
  const s = saved ? await saved.json() : { fails: 0, alerted: false, staleAlerted: false };
  const notes = [];

  if (await healthy(env.HEALTH_URL)) {
    if (s.alerted) notes.push(`✅ html-hub 회복됨 (바깥 감시: ${env.HEALTH_URL})`);
    s.fails = 0;
    s.alerted = false;
  } else {
    s.fails += 1;
    if (s.fails === 2 && !s.alerted) {
      s.alerted = true;
      notes.push(`🚨 html-hub 가 바깥에서 응답하지 않는다 (${env.HEALTH_URL}). 맥미니 전원·로그인(FileVault)·터널을 확인`);
    }
  }

  const list = await env.BACKUPS.list({ prefix: "html-hub-" });
  const latest = list.objects.sort((a, b) => b.uploaded - a.uploaded)[0];
  const newest = latest?.uploaded.getTime();
  const stale = !newest || Date.now() - newest > 26 * 3600 * 1000;
  if (stale && !s.staleAlerted) {
    s.staleAlerted = true;
    notes.push("🚨 html-hub 원격 백업이 26시간 넘게 오지 않았다 (R2 html-hub-backups)");
  } else if (!stale && s.staleAlerted) {
    s.staleAlerted = false;
    notes.push("✅ html-hub 원격 백업이 다시 들어왔다");
  }

  for (const n of notes) await telegram(env, n);
  const previousAt = s.at ?? null;
  s.at = new Date().toISOString();
  await env.BACKUPS.put(STATE, JSON.stringify(s));
  return { ...s, previousAt, newestBackup: latest ? { key: latest.key, at: new Date(newest).toISOString(), size: latest.size } : null, sent: notes.length };
}

// 감시 시계. 계정의 크론 한도(무료 5개)를 다른 Worker 가 쓰고, GitHub 예약 실행은 돌지 않아 알람을 쓴다.
export class Clock extends DurableObject {
  // 알람이 없으면 건다. 인증된 요청(매일의 백업 올리기 포함)마다 불러, 끊겨도 다시 돈다.
  async start() {
    if ((await this.ctx.storage.getAlarm()) === null) await this.ctx.storage.setAlarm(Date.now() + EVERY_MS);
  }

  async alarm() {
    // 다음 알람을 먼저 건다. 감시가 실패해도 시계는 멈추지 않는다 (던지면 재시도로 실패 횟수가 겹친다).
    await this.ctx.storage.setAlarm(Date.now() + EVERY_MS);
    try {
      await check(this.env);
    } catch (e) {
      console.error("감시 실패", e);
    }
  }
}

export default {
  async fetch(req, env) {
    const url = new URL(req.url);
    if (!env.UPLOAD_SECRET || req.headers.get("authorization") !== `Bearer ${env.UPLOAD_SECRET}`) {
      return new Response("unauthorized", { status: 401 });
    }
    await env.CLOCK.get(env.CLOCK.idFromName("monitor")).start();
    if (req.method === "PUT" && url.pathname.startsWith("/backup/")) {
      const name = url.pathname.slice("/backup/".length);
      if (!NAME.test(name)) return new Response("bad name", { status: 400 });
      await env.BACKUPS.put(name, req.body);
      const keys = (await env.BACKUPS.list({ prefix: "html-hub-" })).objects.map((o) => o.key).sort().reverse();
      for (const k of keys.slice(Number(env.KEEP))) await env.BACKUPS.delete(k);
      return new Response("ok");
    }
    // 복구용: 암호화된 백업을 내려받는다 (latest 는 가장 최근 것). 풀려면 1Password 의 키가 있어야 한다.
    if (req.method === "GET" && url.pathname.startsWith("/backup/")) {
      let name = url.pathname.slice("/backup/".length);
      if (name === "latest") {
        const objs = (await env.BACKUPS.list({ prefix: "html-hub-" })).objects.sort((a, b) => b.uploaded - a.uploaded);
        if (!objs.length) return new Response("no backup", { status: 404 });
        name = objs[0].key;
      }
      if (!NAME.test(name)) return new Response("bad name", { status: 400 });
      const obj = await env.BACKUPS.get(name);
      if (!obj) return new Response("not found", { status: 404 });
      return new Response(obj.body, { headers: { "content-disposition": `attachment; filename="${name}"` } });
    }
    // 손으로 감시를 한 번 돌려 상태를 본다 (previousAt 이 직전 감시 시각).
    if (req.method === "POST" && url.pathname === "/check") {
      return Response.json(await check(env));
    }
    return new Response("not found", { status: 404 });
  },
};
