// The hub page: copy buttons, and web sign-up with the proof of work solved
// in Web Workers (pow-worker.js), one sub-puzzle at a time, every worker
// searching its own slice of nonces.
"use strict";

const NS = "urn:xmpp:ibr-pow:0";

for (const b of document.querySelectorAll("[data-copy]")) {
  b.addEventListener("click", async () => {
    const text = document.getElementById(b.dataset.copy).textContent;
    try {
      await navigator.clipboard.writeText(text);
      b.textContent = "Copied";
    } catch {
      b.textContent = "Select and copy";
    }
    setTimeout(() => (b.textContent = "Copy"), 2000);
  });
}

// The agent instructions message the person's address once they type a
// username in the sign-up form.
const agentText = document.getElementById("agent-text");
const agentTemplate = agentText ? agentText.textContent : "";
function fillUsername(name) {
  if (!agentText) return;
  const placeholder = agentText.dataset.placeholder;
  const user = (name || "").trim().toLowerCase();
  const valid = /^[a-z0-9][a-z0-9._-]{0,63}$/.test(user);
  agentText.textContent = valid ? agentTemplate.split(placeholder).join(user) : agentTemplate;
}

const form = document.getElementById("signup-form");
if (form) {
  form.username.addEventListener("input", () => fillUsername(form.username.value));
  fillUsername(form.username.value);
}
const status = document.getElementById("signup-status");
const bar = document.getElementById("signup-progress");

function say(text, kind) {
  status.textContent = text;
  status.className = kind || "";
}

// One sub-puzzle with `n` workers; resolves to the nonce (16 hex digits).
function solveOne(prefix, index, bits, n, onHashes) {
  return new Promise((resolve, reject) => {
    const hi = crypto.getRandomValues(new Uint32Array(1))[0];
    const workers = [];
    const stop = () => workers.forEach((w) => w.terminate());
    for (let k = 0; k < n; k++) {
      const w = new Worker("/pow-worker.js");
      w.onmessage = (e) => {
        if (e.data.nonce) {
          stop();
          resolve(e.data.nonce);
        } else if (e.data.progress) {
          onHashes(e.data.progress);
        }
      };
      w.onerror = (e) => {
        stop();
        reject(new Error(e.message || "worker failed"));
      };
      w.postMessage({ prefix, index, bits, start: k, step: n, hi });
      workers.push(w);
    }
  });
}

async function signUp(username, password, invite) {
  const r = await fetch("/api/challenge", { cache: "no-store" });
  if (!r.ok) throw new Error("Sign-up is not available right now.");
  const c = await r.json();
  const nonces = [];
  if (c.required && !invite) {
    const prefix = new TextEncoder().encode(`${NS}\0${c.challenge}\0${c.domain}\0${username}\0`);
    const n = Math.max(1, Math.min(navigator.hardwareConcurrency || 4, 16));
    const perPuzzle = 2 ** c.bits;
    let done = 0;
    const started = performance.now();
    for (let i = 0; i < c.count; i++) {
      let thisOne = 0;
      const nonce = await solveOne(prefix, i, c.bits, n, (h) => {
        done += h;
        thisOne += h;
        bar.value = (i + Math.min(0.95, thisOne / perPuzzle)) / c.count;
        const rate = done / ((performance.now() - started) / 1000) / 1e6;
        say(`Proof of work ${i}/${c.count} · ${rate.toFixed(1)} MH/s`);
      });
      nonces.push(nonce);
      bar.value = (i + 1) / c.count;
    }
  }
  const body = { username, password, challenge: c.challenge, solution: nonces.join(","), invite: invite || null };
  const res = await fetch("/api/register", {
    method: "POST",
    headers: { "Content-Type": "application/json" },
    body: JSON.stringify(body),
  });
  const out = await res.json();
  if (!res.ok) {
    if (["challenge-expired", "challenge-outdated", "challenge-used"].includes(out.error)) {
      throw Object.assign(new Error(out.text), { retry: true });
    }
    throw new Error(out.text || out.error);
  }
  return out.jid;
}

if (form) {
  form.addEventListener("submit", async (e) => {
    e.preventDefault();
    const username = form.username.value.trim().toLowerCase();
    const password = form.password.value;
    const invite = new URLSearchParams(location.search).get("preauth") || "";
    if (!/^[a-z0-9][a-z0-9._-]{0,63}$/.test(username)) {
      say("Usernames use a–z, 0–9, '.', '_' and '-'.", "error");
      return;
    }
    if (password.length < 8) {
      say("Choose a password of at least 8 characters.", "error");
      return;
    }
    form.querySelector("button").disabled = true;
    bar.hidden = false;
    bar.value = 0;
    try {
      let jid;
      try {
        jid = await signUp(username, password, invite);
      } catch (err) {
        if (!err.retry) throw err;
        say("New challenge…");
        jid = await signUp(username, password, invite);
      }
      bar.value = 1;
      say(`Created ${jid}`, "ok");
      form.reset();
    } catch (err) {
      say(err.message, "error");
    } finally {
      form.querySelector("button").disabled = false;
    }
  });
}
