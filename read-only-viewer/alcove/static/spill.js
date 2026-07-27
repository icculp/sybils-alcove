// Spillout: the recent event stream of one session.
//
// The picker is built from /api/sessions so a session is always chosen by id —
// the server resolves the id to a path, and the browser never sees or sends one.

const qs = new URLSearchParams(location.search);
const esc = t => (t == null ? '' : String(t)).replace(/[&<>"]/g,
  c => ({'&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;'}[c]));

const clock = ts => {
  const d = new Date(ts);
  return isNaN(d) ? '' : d.toLocaleTimeString('en-GB', {hour12: false});
};

// Tool arguments render as `key=value` pairs rather than raw JSON: the useful
// question is almost always "which file / which command", and braces bury it.
function args(value) {
  if (value == null) return '';
  if (typeof value !== 'object') return esc(String(value));
  if (Array.isArray(value)) return esc(JSON.stringify(value));
  return Object.entries(value).map(([k, v]) => {
    const flat = typeof v === 'object' && v !== null
      ? JSON.stringify(v) : String(v);
    return `<span class="a"><i>${esc(k)}</i>=${esc(flat)}</span>`;
  }).join('  ');
}

const LABEL = {assistant: 'says', user: 'user', tool_use: 'calls',
               tool_result: 'result', reasoning: 'thinks', compact: 'compact'};

function row(e) {
  const cls = e.kind + (e.error ? ' err' : '');
  const cut = e.truncated ? '<span class="cut">TRUNCATED</span>' : '';
  let body;
  if (e.kind === 'tool_use') {
    body = `<div class="tool"><b>${esc(e.name)}</b>`
         + `<span class="args">${args(e.args)}</span></div>`;
  } else if (e.kind === 'reasoning') {
    // Never render this as empty text — see the note in the header.
    body = 'reasoning (not recorded on disk)';
  } else if (e.kind === 'compact') {
    body = '<span class="muted">— context compacted —</span>';
  } else {
    body = `<pre>${esc(e.text)}</pre>`;
  }
  return `<div class="ev ${cls}"><span class="t">${esc(clock(e.ts))}</span>`
       + `<span class="k">${LABEL[e.kind] || e.kind}</span>`
       + `<div class="c">${body}${cut}</div></div>`;
}

let sessions = [];
let lastHtml = '';
// Follow the tail only while auto-refresh is on; a manual refresh should leave
// the reader exactly where they were.
let following = true;

function pickerOptions() {
  const who = document.getElementById('who');
  const want = qs.get('session') ? `${qs.get('session')}|${qs.get('agent') || ''}` : '';
  who.innerHTML = sessions.map(s =>
    `<option value="${esc(s.value)}"${s.value === want ? ' selected' : ''}>`
    + `${esc(s.text)}</option>`).join('');
}

async function loadSessions() {
  const r = await fetch('/api/sessions', {cache: 'no-store'});
  const d = await r.json();
  sessions = [];
  for (const s of d.sessions || []) {
    const live = s.state === 'running' ? '● ' : s.state === 'writing' ? '◐ ' : '  ';
    sessions.push({value: `${s.session_id}|`,
                   text: `${live}${s.harness} ${s.label} — ${s.project || s.cwd || ''}`});
    for (const sub of s.subagents || []) {
      // A subagent with no transcript cannot be streamed, so it is not offered.
      if (sub.no_transcript) continue;
      sessions.push({value: `${s.session_id}|${sub.id}`,
                     text: `    ↳ ${sub.live ? '● ' : ''}${sub.role || 'subagent'} `
                         + `${sub.label}`});
    }
  }
  pickerOptions();
}

async function load() {
  const [session, agent] = document.getElementById('who').value.split('|');
  const mins = document.getElementById('mins').value;
  if (!session) return;
  const url = `/api/spill?session=${encodeURIComponent(session)}`
            + `&agent=${encodeURIComponent(agent || '')}&minutes=${mins}&limit=200`;
  const out = document.getElementById('out');
  try {
    const r = await fetch(url, {cache: 'no-store'});
    const d = await r.json();
    if (d.error) {
      out.innerHTML = `<p class="muted">${esc(d.error)}</p>`;
      lastHtml = '';
      return;
    }

    const head = [
      `<span class="hz">${esc(d.harness)}</span>`,
      `<span class="sid">${esc(d.label)}</span>`,
      d.model ? `<span class="model">${esc(d.model)}</span>` : '',
      d.role ? `<span class="pill">${esc(d.role)}</span>` : '',
      d.cwd ? `<span>${esc(d.cwd)}</span>` : '',
      `<span class="grow"></span>`,
      `<span>${d.shown} of ${d.matched} events</span>`,
    ].join(' ');

    const empty = d.window_minutes
      ? `Nothing in the last ${d.window_minutes} minutes. The session may be idle`
        + ` — widen the window to see whether it ever spoke.`
      : 'No events in the tail window.';

    const html = `<div class="bar">${head}</div>`
      // State the ceiling rather than letting a partial view look complete.
      + '<div class="note">Reasoning is <b>not shown because it is not saved</b>:'
      + ' both harnesses write the thinking record with the text stripped'
      + ' (a signature or an encrypted blob). Every other line is verbatim.'
      + ' This reads the tail of the transcript, so a long session starts'
      + ' mid-stream.</div>'
      + (d.events.length
          ? `<div class="stream">${d.events.map(row).join('')}</div>`
          : `<p class="muted">${empty}</p>`);

    // A 5s poll that rewrites identical HTML would collapse any text selection
    // and jump the scroll position mid-read. Only touch the DOM on a change,
    // and when new events arrive while pinned to the bottom, follow them.
    if (html !== lastHtml) {
      const atBottom = innerHeight + scrollY >= document.body.scrollHeight - 40;
      out.innerHTML = html;
      lastHtml = html;
      if (atBottom && following) scrollTo(0, document.body.scrollHeight);
    }
  } catch (e) {
    out.innerHTML = `<p class="muted">error: ${esc(e.message)}</p>`;
  }
}

function syncUrl() {
  const [session, agent] = document.getElementById('who').value.split('|');
  const next = new URLSearchParams({session, ...(agent ? {agent} : {})});
  history.replaceState(null, '', `?${next}`);
}

document.getElementById('who').addEventListener('change', () => { syncUrl(); load(); });
document.getElementById('mins').addEventListener('change', load);
document.getElementById('now').addEventListener('click',
  () => { following = false; load().then(() => { following = true; }); });
document.getElementById('wrap').addEventListener('change', ev =>
  document.body.classList.toggle('wrap', ev.target.checked));

let timer = null;
function autoTick() {
  clearInterval(timer);
  if (document.getElementById('auto').checked) timer = setInterval(load, 5000);
}
document.getElementById('auto').addEventListener('change', autoTick);

(async () => { await loadSessions(); await load(); autoTick(); })();
