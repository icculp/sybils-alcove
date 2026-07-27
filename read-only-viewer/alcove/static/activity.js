// Activity over time, read from the store rather than the transcript tail — the
// live view physically cannot show history, because it only ever reads the last
// ~1MB of each file.
//
// Two charts, not one with two y-axes: turns are in the hundreds and output
// tokens in the hundreds of thousands. A dual-axis chart would let the reader
// infer a relationship from crossing lines that the scales invented.

const SERIES = [
  {key: 'claude', label: 'Claude Code', varname: '--series-1'},
  {key: 'codex',  label: 'Codex',       varname: '--series-2'},
];

const esc = t => (t == null ? '' : String(t)).replace(/[&<>"]/g,
  c => ({'&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;'}[c]));
const fmt = n => (n == null ? '—' : n.toLocaleString('en-US'));
const short = n => n == null ? '—'
  : n >= 1e9 ? (n / 1e9).toFixed(1) + 'B'
  : n >= 1e6 ? (n / 1e6).toFixed(1) + 'M'
  : n >= 1e3 ? (n / 1e3).toFixed(1) + 'k' : String(n);

// Clean axis numbers (0 / 1,000 / 2,000), never raw maxima.
function niceMax(v) {
  if (v <= 0) return 1;
  const mag = Math.pow(10, Math.floor(Math.log10(v)));
  for (const step of [1, 2, 2.5, 5, 10]) {
    if (v <= step * mag) return step * mag;
  }
  return 10 * mag;
}

function byDay(rows) {
  const days = new Map();
  for (const r of rows) {
    const d = days.get(r.day) || {day: r.day, claude: 0, codex: 0,
                                  claude_out: 0, codex_out: 0, sessions: 0};
    d[r.harness] = (d[r.harness] || 0) + r.turns;
    d[r.harness + '_out'] = (d[r.harness + '_out'] || 0) + r.output;
    d.sessions += r.sessions;
    days.set(r.day, d);
  }
  return [...days.values()].sort((a, b) => a.day < b.day ? -1 : 1);
}

// One stacked-column chart. `keys` are the stacked fields, bottom-first.
// Bars cap at 24px and never fill their band: the leftover is air, and a 2px
// surface gap separates both adjacent columns and stacked segments — the gap does
// the separating, never a border drawn around a mark.
function chart(days, keys, opts) {
  const W = 900, H = 250, M = {t: 14, r: 14, b: 32, l: 58};
  const pw = W - M.l - M.r, ph = H - M.t - M.b;
  const totals = days.map(d => keys.reduce((a, k) => a + (d[k.field] || 0), 0));
  const max = niceMax(Math.max(1, ...totals));
  const band = pw / Math.max(1, days.length);
  const bw = Math.max(2, Math.min(24, band - 2));
  const y = v => M.t + ph - (v / max) * ph;
  const GAP = 2;

  let g = '';
  // Gridlines first so marks sit above them.
  const ticks = [0, 0.25, 0.5, 0.75, 1].map(f => Math.round(max * f));
  g += '<g class="grid">' + ticks.map(t =>
    `<line x1="${M.l}" y1="${y(t)}" x2="${W - M.r}" y2="${y(t)}"/>`).join('') + '</g>';
  g += ticks.map(t =>
    `<text class="tick" x="${M.l - 8}" y="${y(t) + 3}" text-anchor="end">${
      opts.compact ? short(t) : fmt(t)}</text>`).join('');

  const peak = totals.indexOf(Math.max(...totals));
  days.forEach((d, i) => {
    const x = M.l + band * i + (band - bw) / 2;
    let acc = 0;
    const segs = [];
    keys.forEach((k, ki) => {
      const v = d[k.field] || 0;
      if (v <= 0) return;
      const top = y(acc + v), bottom = y(acc);
      // Trim the segment top by the gap for every segment that has another
      // stacked above it; the topmost keeps its full height and gets the
      // rounded data-end.
      const isTop = keys.slice(ki + 1).every(kk => !(d[kk.field] > 0));
      const h = Math.max(0.5, bottom - top - (isTop ? 0 : GAP));
      const r = Math.min(4, h, bw / 2);
      segs.push(isTop
        // Rounded at the data-end, square at the baseline.
        ? `<path d="M${x} ${top + h} L${x} ${top + r} Q${x} ${top} ${x + r} ${top}
             L${x + bw - r} ${top} Q${x + bw} ${top} ${x + bw} ${top + r}
             L${x + bw} ${top + h} Z" fill="var(${k.varname})"/>`
        : `<rect x="${x}" y="${top + GAP}" width="${bw}" height="${h}"
             fill="var(${k.varname})"/>`);
      acc += v;
    });
    const tip = JSON.stringify(
      keys.map(k => [k.label, d[k.field] || 0, k.varname]));
    g += `<g class="col" data-day="${esc(d.day)}" data-tip="${esc(tip)}">`
       + `<rect class="hit" x="${M.l + band * i}" y="${M.t}" width="${band}"`
       + ` height="${ph}" fill="transparent"/>${segs.join('')}</g>`;
    // Label selectively: only the peak column. A number on every bar is chaos.
    if (i === peak && totals[i] > 0) {
      const ty = y(totals[i]) - 6;
      g += `<text class="dlabel" x="${x + bw / 2}" y="${Math.max(M.t + 8, ty)}"`
         + ` text-anchor="middle">${opts.compact ? short(totals[i]) : fmt(totals[i])}</text>`;
    }
  });

  // X labels thin out so they never collide: at most ~10 across the width.
  const every = Math.ceil(days.length / 10);
  days.forEach((d, i) => {
    if (i % every) return;
    g += `<text class="tick" x="${M.l + band * i + band / 2}" y="${H - 12}"`
       + ` text-anchor="middle">${esc(d.day.slice(5))}</text>`;
  });

  return `<svg viewBox="0 0 ${W} ${H}" role="img" aria-label="${esc(opts.aria)}">`
       + g + '</svg>';
}

function legend(keys) {
  return '<p class="legend">' + keys.map(k =>
    `<span class="key"><span class="sw" style="background:var(${k.varname})"></span>`
    + `${esc(k.label)}</span>`).join('') + '</p>';
}

function table(days) {
  let h = '<table class="data"><tr><th>day</th><th>claude turns</th>'
        + '<th>codex turns</th><th>claude output</th><th>sessions</th></tr>';
  for (const d of [...days].reverse()) {
    h += `<tr><td>${esc(d.day)}</td><td>${fmt(d.claude)}</td>`
       + `<td>${fmt(d.codex)}</td><td>${fmt(d.claude_out)}</td>`
       + `<td>${fmt(d.sessions)}</td></tr>`;
  }
  return h + '</table>';
}

let showTable = false;

function render(d) {
  const days = byDay(d.rows);
  const t = d.totals || {};
  const spanDays = t.first_ts && t.last_ts
    ? Math.max(1, Math.round((new Date(t.last_ts) - new Date(t.first_ts)) / 864e5))
    : null;

  const tiles = [
    ['turns recorded', fmt(t.turns)],
    ['output tokens', short(t.output)],
    ['sessions', fmt(t.sessions)],
    ['days of history', spanDays == null ? '—' : fmt(spanDays)],
  ].map(([k, v]) =>
    `<div class="tile"><div class="v">${v}</div><div class="k">${k}</div></div>`
  ).join('');

  const turnKeys = SERIES.map(s => ({...s, field: s.key}));
  // Output tokens are Claude-only, so this chart carries ONE series and needs no
  // legend box — the subtitle names it. Stacking a flat-zero Codex series would
  // imply Codex spent no tokens, when the truth is it records no per-turn figure.
  const outKeys = [{...SERIES[0], field: 'claude_out'}];

  document.getElementById('out').innerHTML =
      `<div class="tiles">${tiles}</div>`
    + '<div class="card"><h2>Turns per day</h2>'
    + '<p class="sub">One row per assistant message, deduped by message id.</p>'
    + legend(turnKeys)
    + chart(days, turnKeys, {aria: 'turns per day by harness', compact: false})
    + '</div>'
    + '<div class="card"><h2>Claude output tokens per day</h2>'
    + '<p class="sub">Claude only — Codex reports cumulative session totals, so no '
    + 'per-turn attribution exists and inventing one would be a guess.</p>'
    + chart(days, outKeys, {aria: 'claude output tokens per day', compact: true})
    + '</div>'
    + `<div class="card${showTable ? '' : ' hidden'}" id="tablecard">`
    + '<h2>Table</h2>' + table(days) + '</div>';
  wireTips();
}

// An HTML chart is interactive by default: every column gets a tooltip, and the
// hit target is the whole band, not just the drawn bar.
function wireTips() {
  let tip = document.querySelector('.tip');
  if (!tip) {
    tip = document.createElement('div');
    tip.className = 'tip';
    document.body.appendChild(tip);
  }
  for (const col of document.querySelectorAll('.col')) {
    col.addEventListener('mousemove', e => {
      const rows = JSON.parse(col.dataset.tip || '[]').map(r => {
        const [label, value, varname] = r;
        return `<div class="r"><span class="sw" style="background:var(${varname})">`
             + `</span>${esc(label)}<span class="n">${fmt(+value)}</span></div>`;
      }).join('');
      tip.innerHTML = `<b>${esc(col.dataset.day)}</b>${rows}`;
      tip.style.display = 'block';
      const pad = 14;
      const w = tip.offsetWidth, h = tip.offsetHeight;
      tip.style.left = Math.min(e.clientX + pad, innerWidth - w - 4) + 'px';
      tip.style.top = Math.max(4, e.clientY - h - pad) + 'px';
    });
    col.addEventListener('mouseleave', () => { tip.style.display = 'none'; });
  }
}

async function load() {
  const days = document.getElementById('range').value;
  try {
    const r = await fetch(`/api/activity?days=${days}`, {cache: 'no-store'});
    if (!r.ok) throw new Error('HTTP ' + r.status);
    const d = await r.json();
    if (!d.rows.length) {
      // "Unreadable" and "empty" are different problems with different fixes,
      // so say which one this is instead of always blaming an empty store.
      document.getElementById('out').innerHTML = d.unavailable
        ? `<p class="muted">Store unavailable: ${esc(d.unavailable)}<br>`
          + `${esc(d.hint || '')}</p>`
        : '<p class="muted">The store is empty. Populate it with '
          + '<code>python3 alcove.py --ingest-only</code>.</p>';
      return;
    }
    render(d);
  } catch (e) {
    document.getElementById('out').innerHTML =
      `<p class="muted">error: ${esc(e.message)}</p>`;
  }
}

document.getElementById('range').addEventListener('change', load);
document.getElementById('table-toggle').addEventListener('click', ev => {
  showTable = !showTable;
  ev.target.setAttribute('aria-expanded', String(showTable));
  document.getElementById('tablecard')?.classList.toggle('hidden', !showTable);
});
load();
// History changes slowly; a 3-second poll would be pointless load.
setInterval(load, 60000);
