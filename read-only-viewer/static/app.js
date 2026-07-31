const K = n => n == null ? '—' :
  n >= 1e9 ? (n/1e9).toFixed(2)+'B' : n >= 1e6 ? (n/1e6).toFixed(2)+'M' :
  n >= 1e3 ? (n/1e3).toFixed(1)+'k' : String(n);
const AGE = s => s == null ? '—' : s < 60 ? Math.round(s)+'s' :
  s < 3600 ? Math.round(s/60)+'m' : s < 86400 ? (s/3600).toFixed(1)+'h' :
  (s/86400).toFixed(1)+'d';
const esc = t => (t==null?'':String(t)).replace(/[&<>"]/g,
  c => ({'&':'&amp;','<':'&lt;','>':'&gt;','"':'&quot;'}[c]));
const HHMM = at => at && at.length > 18 ? at.slice(11,19)+'Z' : '';

// Collapse state lives outside the render so a 3s refresh cannot reopen what
// you closed. Persisted so it survives a reload too.
const LS = 'alcove.collapsed';
let collapsed = new Set(JSON.parse(localStorage.getItem(LS) || '[]'));
const saveCollapsed = () =>
  localStorage.setItem(LS, JSON.stringify([...collapsed]));

function toggle(id, el){
  if(collapsed.has(id)){ collapsed.delete(id); el.classList.add('open'); }
  else { collapsed.add(id); el.classList.remove('open'); }
  saveCollapsed();
}

// Prefer the operator's own switch count; fall back to changes in served model.
const SW = s => (s.selections && s.selections.length > 1)
  ? s.selections.length - 1 : s.switches;
// Selected one model, but a turn served AFTER that selection used another. The
// ordering check matters: a selection older than the last served turn is just
// chronology, not a discrepancy, and flagging it produces false alarms on any
// session whose tail window predates the switch. `[1m]` is a context variant of
// the same model, not a different one, so strip it before comparing.
const bare = m => (m||'').replace(/\[1m\]$/,'');
function MISMATCH(s){
  const sel = s.selections || [], tl = s.timeline || [];
  if(!sel.length || !tl.length || !s.selected_model || !s.model) return false;
  if(tl[tl.length-1].at < sel[sel.length-1].at) return false;
  return bare(s.selected_model) !== bare(s.model);
}

function timelineHTML(t){
  if(!t || t.length < 2) return '';
  return '<div class="tl">served: ' + t.map(x =>
    '<span class="sw">'+esc(x.model)+'</span>'+(x.at?' <span class="muted">'+HHMM(x.at)+'</span>':'')
  ).join(' <span class="muted">→</span> ') + '</div>';
}

// Effort switches, drawn exactly like model switches because they are the same
// kind of fact: what the harness was actually running at, turn by turn. A
// session that never changed effort has nothing to show here — one entry is a
// setting, not a trace — and one that recorded no effort at all has no entries,
// which must read as silence rather than as a level.
//
// Codex writes effort on `turn_context`, whose timestamp a resume overwrites
// with the file-open time; the server therefore stamps each entry with the
// TURN's own time, so these clocks are comparable with the served timeline.
function effortHTML(t){
  if(!t || t.length < 2) return '';
  return '<div class="tl">effort: ' + t.map(x =>
    '<span class="eff">'+esc(x.effort)+'</span>'
    + (x.at?' <span class="muted">'+HHMM(x.at)+'</span>':'')
  ).join(' <span class="muted">→</span> ')
  + ' <span class="muted">· '+(t.length-1)+' switch'+(t.length>2?'es':'')
  + '</span></div>';
}

// Harness version. Deliberately quiet: the steady state is uninteresting (it is
// whatever happens to be installed) and belongs in the harness chip's tooltip,
// while the SWITCH is the fact worth a line — a session left open across
// upgrades has its turns served by different builds, and "which version was
// running when this broke" is otherwise unanswerable. Claude stamps it on every
// event; Codex writes it once per rollout, so a Codex session has a version and
// no trace, and that absence is the honest render.
function versionHTML(t){
  if(!t || t.length < 2) return '';
  return '<div class="tl">harness: ' + t.map(x =>
    '<span class="ver">'+esc(x.version)+'</span>'
    + (x.at?' <span class="muted">'+HHMM(x.at)+'</span>':'')
  ).join(' <span class="muted">→</span> ')
  + ' <span class="muted">· '+(t.length-1)+' upgrade'+(t.length>2?'s':'')
  + ' inside this session</span></div>';
}

// Operator selections, from the `/model` command record. Shown separately from
// the served timeline because a model can be selected and never serve a turn —
// which is invisible in `message.model` and was why switches looked unlogged.
function selectionHTML(s){
  const sel = s.selections;
  if(!sel || !sel.length) return '';
  const served = new Set((s.timeline||[]).map(x => x.model));
  return '<div class="tl">selected: ' + sel.map(x => {
    const never = !served.has(x.model) && !served.has(bare(x.model));
    return '<span class="'+(never?'nosrv':'sw')+'" title="'
      + (x.requested?'asked for '+esc(x.requested)+'; ':'')
      + (never?'never served a turn':'served')+'">'+esc(x.model)+'</span>'
      + (x.at?' <span class="muted">'+HHMM(x.at)+'</span>':'');
  }).join(' <span class="muted">→</span> ')
  + (sel.length>1?' <span class="muted">· '+(sel.length-1)+' switch'
      +(sel.length>2?'es':'')+'</span>':'') + '</div>';
}

// A transcript on disk is not a session. `running` means a live process owns the
// session id; everything else is weaker evidence and is labelled as such.
const DOT = {running:'live', writing:'warn', ended:'idle', unknown:'unk'};
// A running session that has not written in the live window is still running —
// the process is there — but it is not working. Solid green is reserved for a
// session that is actually moving; quiet ones get a hollow ring, so an
// abandoned terminal open since yesterday stops competing for attention.
const DOTCLS = s => DOT[s.state] + (s.quiet ? ' quiet' : '');
function STATE_WHY(s){
  if(s.state === 'running') return s.quiet
    ? 'process alive (pid '+s.pids.join(', ')+') but ' + (s.quiet_inferred
        ? 'the transcript has not moved in the live window'
        : 'the harness logged this turn ending at '+HHMM(s.turn_stopped_at))
      + ' — open, not working'
    : 'process alive: pid '+s.pids.join(', ');
  if(s.state === 'unknown') return 'pid lookup failed — absence proves nothing';
  if(s.state === 'writing') return s.state_inferred
    ? 'no per-session pid for this harness; transcript written recently'
    : 'no owning process, but the transcript was written recently';
  return 'no process and no recent write — transcript only';
}

// Compaction is the one event that invalidates every token total above it, so
// it gets its own line rather than hiding in a tooltip.
function compactHTML(s){
  const c = s.compactions;
  if(!c || !c.length) return '';
  return '<div class="tl">compacted: ' + c.map(x =>
    '<span class="sw">'+HHMM(x.at)+'</span>'
    + (x.trigger?' <span class="muted">'+esc(x.trigger)+'</span>':'')
    + (x.pre_tokens?' <span class="muted">context was '+K(x.pre_tokens)+'</span>':'')
  ).join(' <span class="muted">→</span> ')
  + ' <span class="muted">· totals below span the boundary</span></div>';
}

// Two sources, and they must not look alike. `state` is folded from the harness's
// own stop events: `stopped` means a SubagentStop was logged and nothing has
// happened since, `running` means a tool call landed after any stop. `inferred`
// means no such event exists for this child — it ran before the hooks did, or it
// aged out of the spool window — so the answer fell back to the transcript's age
// and is marked with the same trailing `?` the session header uses.
//
// `status` is only the parent's launch record. It reads `async_launched` for every
// backgrounded subagent and never flips to completed, so it cannot mean "done";
// it is consulted only when there is nothing better.
function STATE(s){
  if(s.no_transcript) return '<span class="pill warn">no transcript</span>';
  if(s.state === 'stopped') return '<span class="muted" title="SubagentStop '
    + 'logged at '+esc(s.stopped_at)+'; a later event would mean it resumed">'
    + 'stopped '+HHMM(s.stopped_at)+'</span>';
  if(s.state === 'running') return s.inferred
    ? '<span class="run quiet" title="no stop event on record for this child; '
      + 'inferred from a transcript written '+AGE(s.age_s)+' ago">running?</span>'
    : '<span class="run" title="a tool call from this child, with no stop '
      + 'after it">running</span>';
  // No stop event and no recent write. "Finished" and "abandoned" look identical
  // from here, which is exactly why this does not say `stopped`.
  // 'completed' is Claude's spawn record; 'closed'/'open' come from Codex's
  // spawn edge, the only place Codex writes down that a subagent finished.
  if(s.status === 'completed' || s.status === 'closed')
    return '<span class="muted" title="from the parent\'s spawn record, not a '
      + 'stop event">done?</span>';
  if(s.status === 'open') return '<span class="muted" title="Codex still has '
    + 'this spawn open, but the transcript has been idle">open · idle</span>';
  return '<span class="muted" title="no stop event and no recent write; '
    + 'finished and abandoned look the same from here">idle</span>';
}

// A subagent with no transcript has nothing to stream, so it gets no link
// rather than a link to an empty page.
function spillLink(sid, agent){
  const q = '/spill?session='+encodeURIComponent(sid)
          + (agent?'&agent='+encodeURIComponent(agent):'');
  return '<a class="lnk spill" href="'+q+'">spill</a>';
}

function subTable(subs, sid){
  if(!subs.length) return '<div class="empty">no subagents</div>';
  let h = '<table><tr><th>subagent</th><th>role</th><th>model</th><th>effort</th>'
        + '<th>state</th>'
        + '<th class="num">turns</th><th class="num">out</th><th class="num">in</th>'
        + '<th class="num">cache rd</th><th class="num">age</th><th>task</th>'
        + '<th></th></tr>';
  for(const s of subs){
    const mism = s.record_model && s.model && s.record_model !== s.model;
    // Hollow ring for an inferred "running", filled for one the harness
    // confirmed — the same treatment a quiet session already gets.
    const dot = s.state === 'running' ? (s.inferred ? 'live quiet' : 'live') : 'idle';
    h += '<tr>'
      + '<td><span class="dot '+dot+'" '
      +   'style="display:inline-block;margin-right:6px"></span><code'
      +   (s.version?' title="harness '+esc(s.version)+'"':'')+'>'+esc(s.label)+'</code>'
      +   (s.nickname?' <span class="nm">'+esc(s.nickname)+'</span>':'')+'</td>'
      + '<td class="muted">'+esc(s.role||'—')+'</td>'
      + '<td><span class="model sm">'+esc(s.model||'unknown')+'</span>'
      +   (mism?' <span class="pill warn">rec '+esc(s.record_model)+'</span>':'')
      +   (s.timeline&&s.timeline.length>1?' <span class="pill warn">'+(s.timeline.length-1)+' sw</span>':'')+'</td>'
      // A child transcript records its own effort. Em dash where it does not:
      // "not recorded" and "low" must not look the same.
      + '<td>'+(s.effort?'<span class="eff">'+esc(s.effort)+'</span>'
      +   (s.effort_timeline&&s.effort_timeline.length>1
            ?' <span class="pill warn">'+(s.effort_timeline.length-1)+' sw</span>':'')
      : '<span class="muted">—</span>')+'</td>'
      + '<td>'+STATE(s)+'</td>'
      + '<td class="num">'+K(s.turns)+'</td>'
      + '<td class="num">'+K(s.usage.output)+'</td>'
      + '<td class="num">'+K(s.usage.input)+'</td>'
      + '<td class="num muted">'+K(s.usage.cache_read)+'</td>'
      + '<td class="num muted">'+AGE(s.age_s)+'</td>'
      + '<td class="t">'+esc(s.task||'')+'</td>'
      + '<td>'+(s.no_transcript?'':spillLink(sid, s.id))+'</td></tr>';
  }
  return h + '</table>';
}

let last = '';
function render(d){
  const mode = document.getElementById('filter').value;
  let list = d.sessions;
  if(mode === 'running') list = list.filter(s => s.state === 'running');
  if(mode === 'live') list = list.filter(s => s.live || s.state === 'running');
  if(mode === 'active') list = list.filter(s => s.subagents.some(x => x.live));
  if(mode === 'subs') list = list.filter(s => s.subagents.length);

  const liveN = d.sessions.filter(s=>s.live).length;
  const subs = d.sessions.reduce((a,s)=>a+s.subagents.length,0);
  const subsLive = d.sessions.reduce((a,s)=>a+s.subagents.filter(x=>x.live).length,0);
  const running = d.sessions.filter(s=>s.state==='running').length;
  // Headline the ones actually moving; a count that lumps in six day-old
  // terminals is the same lie the solid green dot was telling.
  const busy = d.sessions.filter(s=>s.state==='running'&&!s.quiet).length;
  const quiet = running - busy;
  const bad = d.pid_source && d.pid_source !== 'ok';
  document.getElementById('stat').innerHTML =
    esc(d.generated_at)+' · <b>'+busy+' running</b>'
    +(quiet?' <span class="muted">(+'+quiet+' quiet)</span>':'')+' / '+liveN+' writing / '
    +d.sessions.length+' transcripts · '+subsLive+' active / '+subs+' subagents'
    +(d.codex_processes!=null?' · '+d.codex_processes+' codex proc':'')
    +(bad?' · <span class="warnx">pid lookup '+esc(d.pid_source)+'</span>':'');

  let h = '';
  for(const s of list){
    const models = new Set(s.subagents.filter(x=>x.model).map(x=>x.model));
    const mixed = s.model && [...models].some(m => m !== s.model);
    const act = s.subagents.filter(x=>x.live).length;
    const since = s.usage_since_compact;
    const open = collapsed.has(s.session_id) ? '' : ' open';
    h += '<div class="s'+open+'" data-id="'+esc(s.session_id)+'">'
      + '<div class="shead"><span class="caret"></span>'
      + '<span class="dot '+DOTCLS(s)+'" title="'+STATE_WHY(s)+'"></span>'
      + '<span class="hz"'+(s.version?' title="'+esc(s.harness)+' '+esc(s.version)
          +((s.version_timeline&&s.version_timeline.length>1)
            ?' · '+(s.version_timeline.length-1)+' upgrade'
              +(s.version_timeline.length>2?'s':'')+' inside this session':'')
          +'"':'')+'>'+esc(s.harness)+'</span>'
      + '<span class="st '+DOTCLS(s)+'">'+s.state+(s.quiet?' · quiet':'')
      +   (s.state_inferred&&s.state!=='ended'?'?':'')+'</span>'
      + '<span class="sid">'+esc(s.label)+'</span>'
      + (s.agent_name?'<span class="nm" title="the CLI\'s name for this window'
          +(s.kind?' ('+esc(s.kind)+')':'')+'">'+esc(s.agent_name)+'</span>':'')
      + '<span class="model">'+esc(s.model||'unknown')+'</span>'
      + (SW(s)?'<span class="pill warn" title="'+(s.selections&&s.selections.length
          ?'operator /model switches':'changes in the serving model')+'">'
          +SW(s)+' switch'+(SW(s)>1?'es':'')+'</span>':'')
      + (MISMATCH(s)?'<span class="pill warn" title="selected but the last turn '
          +'was served by a different model">sel '+esc(s.selected_model)+'</span>':'')
      // Absent effort renders as nothing at all — no pill, no placeholder.
      // Most of this corpus predates the field and a default would be a claim.
      + (s.effort?'<span class="pill eff" title="reasoning effort'
          +((s.effort_timeline&&s.effort_timeline.length>1)
            ?'; '+(s.effort_timeline.length-1)+' switch'
              +(s.effort_timeline.length>2?'es':'')+' in the tail window':'')
          +'">'+esc(s.effort)
          +((s.effort_timeline&&s.effort_timeline.length>1)
            ?' <span class="muted">×'+s.effort_timeline.length+'</span>':'')
          +'</span>':'')
      + '<span class="pill">'+esc(s.project)+'</span>'
      + (s.branch?'<span class="pill">'+esc(s.branch)+'</span>':'')
      + '<span class="grow"></span>'
      + (s.subagents.length?'<span class="pill'+(act?' on':mixed?' warn':'')+'">'
          +(act?act+' active / ':'')+s.subagents.length+' sub'
          +(models.size>1?' · '+models.size+' models':'')+'</span>':'')
      + (s.compactions&&s.compactions.length?'<span class="pill cmp" title="context '
          +'compacted; token totals here span the boundary">compacted '
          +HHMM(s.compactions[s.compactions.length-1].at)
          +(s.compactions.length>1?' ×'+s.compactions.length:'')+'</span>':'')
      + '<span class="pill"'+(since?' title="since last compaction / tail total"':'')
          +'>out '+(since?K(since.output)+' / ':'')+K(s.usage.output)+'</span>'
      + '<span class="pill"'+(s.turns_since_compact!=null
          ?' title="since last compaction / tail total"':'')+'>'
          +(s.turns_since_compact!=null?s.turns_since_compact+' / ':'')
          +s.turns+' turns</span>'
      + (s.pids.length?'<span class="pill">pid '+s.pids.join(',')+'</span>':'')
      + '<span class="muted">'+AGE(s.age_s)+'</span>'
      + spillLink(s.session_id,'')
      + '</div><div class="body">'+selectionHTML(s)+timelineHTML(s.timeline)
      + effortHTML(s.effort_timeline)+versionHTML(s.version_timeline)
      + compactHTML(s)+subTable(s.subagents, s.session_id)+'</div></div>';
  }
  h = h || '<p class="muted">no sessions match this filter</p>';
  // Only touch the DOM when something actually changed, so a refresh mid-scroll
  // or mid-click does not yank the page.
  if(h !== last){
    document.getElementById('out').innerHTML = h;
    last = h;
    for(const el of document.querySelectorAll('.s')){
      // The header carries a link; clicking it must navigate, not collapse.
      el.querySelector('.shead').addEventListener('click',
        ev => { if(!ev.target.closest('a')) toggle(el.dataset.id, el); });
    }
  }
}

let data = null, timer = null;
// One fetch at a time. A burst of change events must not stack 1.7 MB requests;
// the last one still runs, so nothing is missed.
let inflight = false, again = false;
async function load(){
  if(inflight){ again = true; return; }
  inflight = true;
  try{
    const r = await fetch('/api/sessions', {cache:'no-store'});
    data = await r.json();
    render(data);
  }catch(e){ document.getElementById('stat').textContent = 'error: '+e; }
  finally{
    inflight = false;
    if(again){ again = false; load(); }
  }
}

// The server pushes a change signal when a transcript or a hook spool line moves,
// so polling becomes the fallback rather than the mechanism. The poll is stretched
// rather than stopped: if the stream dies in a way EventSource does not report,
// a 60 s refresh still beats a page frozen forever.
const PUSH_MS = 60000, POLL_MS = 3000;
let stream = null, pushing = false;
function transport(text, cls){
  const el = document.getElementById('stream');
  if(el){ el.textContent = text; el.className = cls; }
}
function arm(){
  if(timer) clearInterval(timer);
  if(!document.getElementById('auto').checked){ transport('paused','muted'); return; }
  timer = setInterval(load, pushing ? PUSH_MS : POLL_MS);
  transport(pushing ? 'push' : 'poll 3s', pushing ? 'run' : 'muted');
}
function connect(){
  if(!window.EventSource) return;         // no push: the poll above is the answer
  stream = new EventSource('/api/events');
  stream.onopen = () => { pushing = true; arm(); };
  // The signal carries a sequence number, not the payload: refetch through the
  // same cached endpoint the poll uses.
  stream.addEventListener('change', load);
  // EventSource reconnects on its own; until it does, poll. Never silently
  // present a dead stream as a live one.
  stream.onerror = () => { pushing = false; arm(); };
}
document.getElementById('auto').addEventListener('change', arm);
document.getElementById('now').addEventListener('click', load);
document.getElementById('filter').addEventListener('change', () => { last=''; render(data); });
document.getElementById('expand').addEventListener('click', () => {
  collapsed.clear(); saveCollapsed(); last=''; render(data); });
document.getElementById('collapse').addEventListener('click', () => {
  for(const s of data.sessions) collapsed.add(s.session_id);
  saveCollapsed(); last=''; render(data); });
load(); arm(); connect();
