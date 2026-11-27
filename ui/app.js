const { invoke, convertFileSrc } = window.__TAURI__.core;
const { listen } = window.__TAURI__.event;
const $ = (s, el = document) => el.querySelector(s);
const $$ = (s, el = document) => [...el.querySelectorAll(s)];
// Everything from YouTube or an AI is untrusted: escape before it touches innerHTML.
const esc = (s) => String(s ?? '').replace(/[&<>"']/g, (c) => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' })[c]);
const num = (n) => new Intl.NumberFormat('en', { notation: 'compact' }).format(n || 0);
const fmt = (d) => d.toLocaleString([], { weekday: 'short', month: 'short', day: 'numeric', hour: 'numeric', minute: '2-digit' });
const splitTags = (s) => s.split(',').map((t) => t.trim()).filter(Boolean);
const toLocalInput = (d) => new Date(d.getTime() - d.getTimezoneOffset() * 60000).toISOString().slice(0, 16);
// AI JSON is not guaranteed to have the shape we asked for.
const arr = (x) => (Array.isArray(x) ? x : []);
const mmss = (s) => `${Math.floor(s / 60)}:${String(Math.round(s % 60)).padStart(2, '0')}`;

const WRITERS = [
  { v: 'gemini', name: 'Gemini API', sub: 'Uses your Gemini key' },
  { v: 'claude', name: 'Claude', sub: 'Pro / Max subscription', tool: 'claude' },
  { v: 'codex', name: 'ChatGPT', sub: 'Plus / Pro via Codex', tool: 'codex' },
  { v: 'opencode', name: 'OpenCode', sub: 'Zen / Go or linked accounts', tool: 'opencode' },
  { v: 'ollama', name: 'Ollama', sub: 'Free, runs on this PC', tool: 'ollama' },
];
const VISUALS = [['pexels', 'Pexels stock video'], ['pixabay', 'Pixabay stock video'], ['ai', 'AI images (Gemini, paid key)'], ['gradient', 'Animated gradient']];

let settings = {}, info = { tools: {} }, jobs = [], selected = new Set(), filter = 'all', busy = false, comments = [], modalJob = null;

/* ---------- basics ---------- */
function toast(msg, isErr) {
  const t = document.createElement('div');
  t.className = 'toast' + (isErr ? ' err' : '');
  t.textContent = msg;
  $('#toasts').append(t);
  setTimeout(() => t.remove(), isErr ? 9000 : 4000);
}

// Making/uploading shorts uses the CPU, GPU and your upload quota, so only one of those runs at a time.
// Everything else (research, trends, analytics, downloads, previews…) runs alongside.
const HEAVY = new Set(['generate', 'autopilot', 'upload', 'make_clips', 'retry_job', 'auto_run_now', 'story_make']);
let heavyRunning = 0;
async function call(cmd, args = {}, { quiet } = {}) {
  const heavy = HEAVY.has(cmd);
  if (heavy && heavyRunning) {
    toast('Shorts are still being made or uploaded. This can start when that finishes (or press Cancel).', true);
    throw new Error('busy');
  }
  if (heavy) {
    heavyRunning++;
    updateCancel();
  }
  try {
    return await invoke(cmd, args);
  } catch (e) {
    if (!quiet && !(cancelling && String(e).includes('cancelled'))) toast(String(e), true);
    throw e;
  } finally {
    if (heavy) {
      heavyRunning--;
      updateCancel();
    }
  }
}

function log(m) {
  const p = $('#log');
  p.textContent += m + '\n';
  p.scrollTop = p.scrollHeight;
  $('#lastLog').textContent = m;
}

/** Background work: spinner while anything runs; only the clicked button waits, the rest of the app stays usable. */
let cancelling = false, running = 0;
function updateCancel() {
  // Cancel stops the short-making/uploading job (the one that can take a long time).
  const show = heavyRunning > 0;
  $('#btnCancel').classList.toggle('hidden', !show);
  if (show && !cancelling) $('#btnCancel').disabled = false;
}
async function task(label, fn) {
  const btn = document.activeElement?.tagName === 'BUTTON' ? document.activeElement : null;
  if (btn?.dataset.running) return toast('That is already running', true);
  if (btn) {
    btn.dataset.running = '1';
    btn.disabled = true;
  }
  running++;
  busy = true;
  if (!heavyRunning) cancelling = false;
  $('#spinner').classList.remove('hidden');
  $('#activity').classList.add('open');
  log(label);
  try {
    await flushSave();
    return await fn();
  } catch (e) {
    if (!cancelling && String(e) !== 'Error: busy') log('✖ ' + e);
  } finally {
    running--;
    busy = running > 0;
    if (btn) {
      delete btn.dataset.running;
      btn.disabled = false;
    }
    if (cancelling && !heavyRunning) {
      log('■ Cancelled. Unfinished shorts are in the Library: press Retry to continue or Delete to start over.');
      cancelling = false;
      loadJobs();
    }
    if (!heavyRunning) {
      queue = { index: 0, total: 0, id: null };
      renderQueue();
    }
    loadUsage(false);
    $('#spinner').classList.toggle('hidden', running === 0);
    updateCancel();
  }
}

$('#btnCancel').onclick = (e) => {
  e.stopPropagation(); // don't toggle the activity panel
  cancelling = true;
  e.currentTarget.disabled = true;
  log('Cancelling…');
  invoke('cancel_task');
};

listen('log', (e) => {
  log(e.payload);
  if (busy) refreshSoon(); // step progress lives in each job's file
});
listen('usage-changed', () => loadUsage(false));
let refreshTimer, queue = { index: 0, total: 0, id: null };
function refreshSoon() {
  clearTimeout(refreshTimer);
  refreshTimer = setTimeout(async () => {
    await loadJobs();
    renderQueue();
  }, 400);
}
listen('queue', (e) => {
  queue = e.payload;
  renderQueue();
});
/** Overall bar: finished items plus the current item's step fraction. */
function renderQueue() {
  const show = busy && queue.total > 0 && queue.index < queue.total;
  $('#qbar').classList.toggle('hidden', !show);
  $('#qtext').classList.toggle('hidden', !show);
  if (!show) return;
  const cur = jobs.find((j) => j.id === queue.id);
  const frac = cur && cur.steps ? cur.step / cur.steps : 0;
  $('#qfill').style.width = `${Math.round((100 * (queue.index + frac)) / queue.total)}%`;
  $('#qtext').textContent = `${queue.index + 1} of ${queue.total}${cur && cur.steps ? ` · step ${cur.step}/${cur.steps}` : ''}`;
}
listen('jobs-changed', () => loadJobs());

function show(page) {
  $$('nav button').forEach((b) => b.classList.toggle('active', b.dataset.page === page));
  $$('.page').forEach((p) => p.classList.toggle('active', p.id === 'page-' + page));
  if (page === 'library' || page === 'published') loadJobs();
  if (page === 'published') loadYtState();
  if (page === 'analytics') loadLive().then(renderMonetization);
  if (page === 'stories') loadStories();
}
$$('nav button').forEach((b) => (b.onclick = () => show(b.dataset.page)));
$('#channel').onclick = () => show('settings');
$('#activityBar').onclick = () => $('#activity').classList.toggle('open');
document.addEventListener('click', (e) => {
  const a = e.target.closest('[data-link]');
  if (a) {
    e.preventDefault();
    call('open_link', { url: a.dataset.link });
  }
});

/* ---------- settings (autosave) ---------- */
let saveTimer;
function saveSoon() {
  clearTimeout(saveTimer);
  saveTimer = setTimeout(() => call('save_settings', { settings }), 400);
}
async function flushSave() {
  clearTimeout(saveTimer);
  await call('save_settings', { settings });
}

function syncSetting(k) {
  $$(`[data-setting="${k}"]`).forEach((el) => {
    if (el !== document.activeElement) el.value = String(settings[k] ?? '');
  });
  if (k === 'writer') {
    renderWriters();
    if (typeof renderSwap === 'function' && usage) renderSwap();
  }
  if (k === 'seconds') $('#secondsOut').textContent = `${settings.seconds}s`;
}

function bindSettings() {
  $$('.writer-select').forEach((s) => (s.innerHTML = WRITERS.map((w) => `<option value="${w.v}">${w.name}</option>`).join('')));
  $$('.visuals-select').forEach((s) => (s.innerHTML = VISUALS.map(([v, n]) => `<option value="${v}">${n}</option>`).join('')));
  $$('[data-setting]').forEach((el) => {
    const k = el.dataset.setting;
    el.value = String(settings[k] ?? '');
    const update = () => {
      const cur = settings[k]; // keep the type serde expects (number, bool or text)
      settings[k] = typeof cur === 'number' || el.type === 'range' ? Number(el.value) : typeof cur === 'boolean' ? el.value === 'true' : el.value;
      syncSetting(k);
      saveSoon();
    };
    el.addEventListener('input', update);
    el.addEventListener('change', update);
  });
  syncSetting('seconds');
}

function renderWriters() {
  $('#writerCards').innerHTML = WRITERS.map((w) => {
    const ok = !w.tool || info.tools[w.tool];
    const state = w.tool ? (ok ? '● installed' : '○ not installed') : '● API';
    return `<button data-w="${w.v}" class="${settings.writer === w.v ? 'active' : ''}"><b>${w.name}</b><small>${w.sub}</small>
      <small style="color:${ok ? 'var(--ok)' : 'var(--warn)'}">${state}</small></button>`;
  }).join('');
  $$('#writerCards button').forEach((b) => (b.onclick = () => {
    settings.writer = b.dataset.w;
    syncSetting('writer');
    saveSoon();
  }));
}

async function refreshChannel() {
  info = await call('app_info');
  let name = 'YouTube not connected';
  if (info.connected) name = await call('yt_channel', {}, { quiet: true }).catch(() => 'Reconnect YouTube');
  $('#channel').classList.toggle('on', info.connected);
  $('#channelName').textContent = name;
  $('#ytStatus').textContent = info.connected ? `Connected: ${name}` : 'Not connected';
  $('#btnDisconnect').classList.toggle('hidden', !info.connected);
  $('#btnConnect').textContent = info.connected ? 'Reconnect' : 'Connect YouTube';
  $('#outDir').textContent = `Shorts are saved in ${info.out_dir}`;
  renderWriters();
  if (!info.tools.ffmpeg) toast('FFmpeg was not found. Install it with: winget install Gyan.FFmpeg', true);
}

$('#btnConnect').onclick = async () => {
  await flushSave();
  toast('Finish signing in in your browser…');
  try {
    toast(`Connected to ${await call('yt_connect')}`);
  } catch {}
  refreshChannel().then(loadLive);
};
$('#btnDisconnect').onclick = async () => {
  await call('yt_disconnect');
  refreshChannel().then(loadLive);
};

/* ---------- scheduling helpers ---------- */
function slots(startStr, perDay, n) {
  const start = new Date(startStr);
  if (isNaN(start)) return [];
  const gap = (24 / perDay) * 3600e3;
  return Array.from({ length: n }, (_, i) => new Date(start.getTime() + i * gap));
}
function validSlots(d) {
  if (!d.length || d[0] < Date.now() + 20 * 60e3) {
    toast('Pick a first post time at least 20 minutes from now', true);
    return false;
  }
  return true;
}
function defaultStart() {
  const d = new Date(Date.now() + 2 * 3600e3);
  d.setMinutes(0, 0, 0);
  return toLocalInput(d);
}

/* ---------- create ---------- */
$('#btnGenerate').onclick = () => {
  const topics = $('#topics').value.split('\n').map((s) => s.trim()).filter(Boolean);
  if (!topics.length) return toast('Write at least one topic', true);
  task(`Generating ${topics.length} short(s)…`, async () => {
    const ids = await call('generate', { topics, tags: splitTags($('#createTags').value) });
    toast(`Finished ${ids.length} short(s). Open the Library to review.`);
  });
};

/* ---------- autopilot ---------- */
let apMode = 'schedule';
$$('#apMode button').forEach((b) => (b.onclick = () => {
  apMode = b.dataset.mode;
  $$('#apMode button').forEach((x) => x.classList.toggle('active', x === b));
  $('#apSchedule').classList.toggle('hidden', apMode !== 'schedule');
  $('#apNow').classList.toggle('hidden', apMode !== 'now');
  renderSlots();
}));
function renderSlots() {
  const n = Math.min(30, Math.max(0, +$('#apCount').value || 0));
  const s = apMode === 'schedule' ? slots($('#apStart').value, +$('#apPerDay').value, n) : [];
  $('#apSlots').innerHTML = s.map((d, i) => `<span>#${i + 1} · ${esc(fmt(d))}</span>`).join('');
}
['#apCount', '#apStart', '#apPerDay'].forEach((s) => ['input', 'change'].forEach((ev) => $(s).addEventListener(ev, renderSlots)));

$('#btnAutopilot').onclick = () => {
  const niche = $('#apNiche').value.trim();
  if (!niche) return toast('Enter a niche (or find one on the Niche research page)', true);
  const count = Math.max(1, Math.min(30, +$('#apCount').value || 1));
  let sl = [], privacy = '';
  if (apMode !== 'none' && !info.connected) return toast('Connect YouTube in Settings first', true);
  if (apMode === 'schedule') {
    const d = slots($('#apStart').value, +$('#apPerDay').value, count);
    if (!validSlots(d)) return;
    sl = d.map((x) => x.toISOString());
  }
  if (apMode === 'now') privacy = $('#apPrivacy').value;
  task(`Autopilot: ${count} shorts about “${niche}”`, async () => {
    const n = await call('autopilot', { niche, count, slots: sl, privacy });
    toast(`Autopilot finished: ${n}/${count} done`);
  });
};

/* ---------- research ---------- */
const good = { high: 'good', medium: 'mid', low: 'bad' }, easy = { low: 'good', medium: 'mid', high: 'bad' };
$('#btnSuggest').onclick = () => task('Asking the AI for niches…', async () => {
  const r = await call('suggest_niches');
  const list = arr(r.niches);
  $('#niches').innerHTML = list.map((n, i) => `<div class="niche"><b>${esc(n.name)}</b><p>${esc(n.why)}</p>
    <div class="row"><span class="badge ${good[n.rpm] || ''}">RPM ${esc(n.rpm)}</span><span class="badge ${easy[n.competition] || ''}">Competition ${esc(n.competition)}</span></div>
    <ul>${arr(n.examples).map((x) => `<li>${esc(x)}</li>`).join('')}</ul><button data-i="${i}">Research this niche</button></div>`).join('');
  $$('#niches button').forEach((b) => (b.onclick = () => {
    $('#rsNiche').value = list[b.dataset.i].name;
    $('#btnResearch').click();
  }));
  log('Niche ideas ready');
});

$('#btnResearch').onclick = () => {
  const niche = $('#rsNiche').value.trim();
  if (!niche) return toast('Enter a niche', true);
  const count = Math.max(3, Math.min(30, +$('#rsCount').value || 10));
  task(`Researching “${niche}”…`, async () => {
    const r = await call('research', { niche, count });
    renderResearch(r, niche);
    log('Research ready');
  });
};

function renderResearch(r, niche) {
  const tags = arr(r.tags), ideas = arr(r.ideas), vids = arr(r.top_videos), top = arr(r.top_tags);
  $('#rsResult').innerHTML = `
    <div class="grid2"><div class="card"><h3>Verdict</h3><p>${esc(r.verdict)}</p></div><div class="card"><h3>Angle to stand out</h3><p>${esc(r.angle)}</p></div></div>
    <div class="card"><div class="row between"><h3>Video ideas</h3><div class="row"><button class="ghost" id="rsAll">Select all</button>
      <button id="rsAuto">Autopilot this niche</button><button class="primary" id="rsCreate">Create selected</button></div></div>
      <div class="ideas">${ideas.map((x, i) => `<label><input type="checkbox" value="${i}" checked><span>${esc(x)}</span></label>`).join('')}</div></div>
    <div class="card"><div class="row between"><h3>Recommended tags</h3><button class="ghost" id="rsCopy">Copy all</button></div>
      <div id="rsTags">${tags.map((t) => `<span class="chip">${esc(t)}</span>`).join('')}</div><p class="note">${arr(r.hashtags).map(esc).join(' ')}</p></div>
    ${top.length ? `<div class="card"><h3>Tags today's top Shorts use</h3><div id="rsTop">${top.map((t) => `<span class="chip" title="${t.count} videos · ${num(t.views)} views">${esc(t.tag)}</span>`).join('')}</div></div>` : ''}
    ${vids.length ? `<div class="card"><h3>Top Shorts this month</h3><table>${vids.slice(0, 15).map((v) => `<tr><td><a data-link="${esc(v.url)}">${esc(v.title)}</a> <span class="muted small">· ${esc(v.channel)}</span></td><td>${num(v.views)} views</td></tr>`).join('')}</table></div>` : `<p class="note">Connect YouTube in Settings to ground research in live data from this month's top Shorts.</p>`}`;
  $('#rsCopy').onclick = () => navigator.clipboard.writeText(tags.join(', ')).then(() => toast('Tags copied'));
  $$('#rsResult .chip').forEach((c) => (c.onclick = () => navigator.clipboard.writeText(c.textContent).then(() => toast(`Copied “${c.textContent}”`))));
  $('#rsAll').onclick = () => $$('#rsResult .ideas input').forEach((c) => (c.checked = true));
  $('#rsAuto').onclick = () => {
    $('#apNiche').value = niche;
    show('autopilot');
  };
  $('#rsCreate').onclick = () => {
    const picked = $$('#rsResult .ideas input:checked').map((c) => ideas[c.value]);
    if (!picked.length) return toast('Select at least one idea', true);
    task(`Generating ${picked.length} short(s)…`, async () => {
      await call('generate', { topics: picked, tags });
      toast('Done. Open the Library to review.');
    });
  };
}

/* ---------- library ---------- */
async function loadJobs() {
  jobs = await call('list_jobs', {}, { quiet: true }).catch(() => []);
  renderJobs();
  renderCap();
}
const uploadable = (j) => j.video && !j.video_id && !['generating', 'queued'].includes(j.status);
const canPin = (j) => j.video_id && j.comment_id && j.comment_id !== 'unavailable';
function subline(j) {
  if (j.pending_upload && !j.video_id) return '⏳ Waiting for YouTube’s daily upload limit; uploads automatically';
  if (j.status === 'scheduled') return `Goes live ${fmt(new Date(j.publish_at))}`;
  if (j.status === 'uploaded') return `On YouTube · ${j.privacy}`;
  if (j.status === 'failed') return j.error;
  if (j.status === 'generating') return j.stage || 'Generating…';
  if (j.status === 'queued') return 'Waiting in the queue';
  return `Made ${fmt(new Date(j.created * 1000))}`;
}
const PUBLISHED = ['uploaded', 'scheduled'];
let pubFilter = 'all';
function jobCard(j) {
  return `<div class="job ${selected.has(j.id) ? 'sel' : ''}" data-id="${esc(j.id)}">
    <div class="thumb" data-act="open">${j.thumb ? `<img src="${esc(convertFileSrc(j.thumb))}" loading="lazy" alt="">` : j.status === 'generating' ? '<span class="spinner"></span>' : 'No preview'}
      ${uploadable(j) ? `<input type="checkbox" data-act="sel" title="Select" ${selected.has(j.id) ? 'checked' : ''}>` : ''}
      <span class="badge ${esc(j.status)}">${esc(j.status === 'uploaded' ? 'live' : j.status === 'draft' ? 'ready' : j.status)}</span></div>
    <div class="meta"><div class="title">${esc(j.title)}</div>
      ${j.status === 'generating' && j.steps ? `<div class="prog"><div class="meter"><i style="width:${Math.round((100 * j.step) / j.steps)}%"></i></div>Step ${j.step}/${j.steps} · ${esc(j.stage)}</div>` : `<div class="sub">${esc(subline(j))}</div>`}
      ${ytLine(j)}
      ${j.tokens ? `<div class="tok" title="${esc(j.ai)} · ${j.ai_calls} AI calls">🧠 ${num(j.tokens)} tokens</div>` : ''}
      <div class="acts">${j.status === 'failed' ? '<button data-act="retry">Retry</button>' : ''}${j.video_id ? '<button data-act="yt">YouTube</button>' : ''}${canPin(j) ? '<button data-act="pin" title="YouTube does not let apps pin comments: this opens the comment in YouTube Studio">📌 Pin</button>' : ''}<button data-act="folder">Folder</button>${j.video_id ? '' : '<button data-act="del" class="danger">Delete</button>'}</div></div></div>`;
}
function renderJobs() {
  for (const id of [...selected]) if (!jobs.some((j) => j.id === id && uploadable(j))) selected.delete(id);
  $('#selCount').textContent = `${selected.size} selected`;
  // Library = work in progress and ready-to-post; Published = on YouTube (kept apart so nothing gets posted twice).
  const lib = jobs.filter((j) => !PUBLISHED.includes(j.status) && (filter === 'all' || j.status === filter || (filter === 'progress' && ['queued', 'generating'].includes(j.status))));
  const pub = jobs.filter((j) => PUBLISHED.includes(j.status) && (pubFilter === 'all' || j.status === pubFilter));
  $('#jobs').innerHTML = lib.length ? lib.map(jobCard).join('') : `<div class="empty">${jobs.length ? 'Nothing here. Published shorts are on the Published page.' : 'No shorts yet. Make one on the Create page or start Autopilot.'}</div>`;
  $('#pubJobs').innerHTML = pub.length ? pub.map(jobCard).join('') : '<div class="empty">Nothing published yet. Upload or schedule shorts from the Library.</div>';
}
$$('#pubFilter button').forEach((b) => (b.onclick = () => {
  pubFilter = b.dataset.f;
  $$('#pubFilter button').forEach((x) => x.classList.toggle('active', x === b));
  renderJobs();
}));
$$('#libFilter button').forEach((b) => (b.onclick = () => {
  filter = b.dataset.f;
  $$('#libFilter button').forEach((x) => x.classList.toggle('active', x === b));
  renderJobs();
}));
$('#jobs').onclick = $('#pubJobs').onclick = async (e) => {
  const el = e.target.closest('[data-act]'), card = e.target.closest('.job');
  if (!el || !card) return;
  const id = card.dataset.id, j = jobs.find((x) => x.id === id), act = el.dataset.act;
  if (act === 'sel') {
    el.checked ? selected.add(id) : selected.delete(id);
    return renderJobs();
  }
  if (act === 'open') return openModal(j);
  if (act === 'folder') return call('open_job_folder', { id });
  if (act === 'yt') return call('open_link', { url: `https://youtube.com/shorts/${j.video_id}` });
  if (act === 'pin') {
    toast('In YouTube Studio: find the comment from your channel, click ⋮ and choose Pin');
    return call('open_link', { url: `https://studio.youtube.com/video/${j.video_id}/comments` });
  }
  if (act === 'retry') return task(`Retrying “${j.title}”…`, () => call('retry_job', { id }));
  if (act === 'del') {
    if (!el.dataset.armed) {
      el.dataset.armed = '1';
      el.textContent = 'Sure?';
      return setTimeout(() => { delete el.dataset.armed; el.textContent = 'Delete'; }, 3000);
    }
    await call('delete_job', { id });
    loadJobs();
  }
};
$('#btnOpenOut').onclick = () => call('open_job_folder', { id: '' });

function selectedJobs() {
  return jobs.filter((j) => selected.has(j.id) && uploadable(j)).sort((a, b) => a.created - b.created);
}
$('#btnUploadNow').onclick = () => {
  const js = selectedJobs();
  if (!js.length) return toast('Tick the shorts you want to upload first', true);
  if (!info.connected) return toast('Connect YouTube in Settings first', true);
  const privacy = $('#libPrivacy').value;
  task(`Uploading ${js.length} short(s)…`, async () => {
    const n = await call('upload', { items: js.map((j) => ({ id: j.id, privacy, publish_at: null })) });
    toast(`Uploaded ${n}/${js.length}`);
    selected.clear();
  });
};
$('#btnSchedule').onclick = () => {
  const js = selectedJobs();
  if (!js.length) return toast('Tick the shorts you want to schedule first', true);
  if (!info.connected) return toast('Connect YouTube in Settings first', true);
  const d = slots($('#libStart').value, +$('#libPerDay').value, js.length);
  if (!validSlots(d)) return;
  task(`Scheduling ${js.length} short(s)…`, async () => {
    const n = await call('upload', { items: js.map((j, i) => ({ id: j.id, privacy: 'private', publish_at: d[i].toISOString() })) });
    toast(`Scheduled ${n}/${js.length}. YouTube will publish them on time.`);
    selected.clear();
  });
};

/* ---------- preview / edit modal ---------- */
function openModal(j) {
  modalJob = j;
  const v = $('#mVideo');
  j.video ? (v.src = convertFileSrc(j.video)) : v.removeAttribute('src');
  $('#mTitle').value = j.title || '';
  $('#mDesc').value = j.description || '';
  $('#mTags').value = (j.tags || []).join(', ');
  $('#mStatus').className = `badge ${j.status}`;
  $('#mStatus').textContent = j.status;
  $('#mError').textContent = j.error || (j.video_id ? 'Already on YouTube: edit it in YouTube Studio.' : '');
  $('#mYoutube').classList.toggle('hidden', !j.video_id);
  $('#mSave').classList.toggle('hidden', !!j.video_id || j.description == null);
  $('#modal').classList.remove('hidden');
}
function closeModal() {
  const v = $('#mVideo');
  v.pause();
  v.removeAttribute('src');
  v.load();
  $('#modal').classList.add('hidden');
}
$('#mClose').onclick = closeModal;
$('#modal').onclick = (e) => e.target.id === 'modal' && closeModal();
document.addEventListener('keydown', (e) => e.key === 'Escape' && closeModal());
$('#mSave').onclick = async () => {
  await call('save_job_meta', { id: modalJob.id, title: $('#mTitle').value, description: $('#mDesc').value, tags: splitTags($('#mTags').value) });
  toast('Saved');
  closeModal();
  loadJobs();
};
$('#mYoutube').onclick = () => call('open_link', { url: `https://youtube.com/shorts/${modalJob.video_id}` });

/* ---------- comments ---------- */
function renderComments() {
  $('#btnPostAll').classList.toggle('hidden', !comments.some((c) => c.draft));
  $('#commentList').innerHTML = comments.length
    ? comments.map((c, i) => `<div class="card comment" data-i="${i}"><div><div class="who">${esc(c.author)}</div><div class="vid">on “${esc(c.video_title)}”</div><div class="text">${esc(c.text)}</div></div>
      <div><textarea rows="3" placeholder="No reply suggested (spam or nothing to say). Write one or skip.">${esc(c.draft)}</textarea>
      <div class="row end"><button class="ghost" data-act="skip">Skip</button><button class="primary" data-act="post">Post reply</button></div></div></div>`).join('')
    : '<div class="empty">No comments waiting. Press “Fetch &amp; draft replies”.</div>';
}
$('#btnComments').onclick = () => {
  if (!info.connected) return toast('Connect YouTube in Settings first', true);
  task('Fetching comments…', async () => {
    comments = await call('fetch_comments');
    renderComments();
    log(comments.length ? `${comments.length} comments need a reply` : 'No new comments');
  });
};
$('#commentList').onclick = async (e) => {
  const act = e.target.dataset.act, card = e.target.closest('.comment');
  if (!act || !card) return;
  const c = comments[card.dataset.i];
  e.target.disabled = true;
  try {
    if (act === 'post') {
      await call('reply_comment', { id: c.id, text: $('textarea', card).value });
      toast('Reply posted');
    } else await call('skip_comment', { id: c.id });
    comments = comments.filter((x) => x !== c);
    renderComments();
  } catch {
    e.target.disabled = false;
  }
};
$('#btnPostAll').onclick = async () => {
  let n = 0;
  for (const card of $$('#commentList .comment')) {
    const c = comments[card.dataset.i], text = $('textarea', card).value.trim();
    if (!text) continue;
    try {
      await call('reply_comment', { id: c.id, text });
      c.done = true;
      n++;
    } catch {
      break;
    }
  }
  comments = comments.filter((c) => !c.done);
  renderComments();
  toast(`Posted ${n} replies`);
};

/* ---------- clips ---------- */
let clipSrc = 'youtube', clipList = [], clipPicked = new Set();
const CLIP_NOTES = {
  youtube: 'Only videos marked Creative Commons (CC BY) are shown. They may be reused, even commercially, with credit, which is added to the description automatically. YouTube still expects real changes before reused footage can earn: every clip gets a new hook, AI intro, subtitles and framing. Review each one before posting.',
  twitch: 'Twitch clips belong to the streamer. Only use them with permission (many streamers welcome clippers) or from your own channel. Needs a free Twitch app in Settings.',
  link: 'Paste Twitch or Kick clip links (or YouTube videos you have rights to), one per line. Only use clips you have permission to use.',
};
const linkItems = () => $('#clipLinks').value.split('\n').map((s) => s.trim()).filter(Boolean).map((url) => ({ url, title: '', channel: '' }));
function updateClipSel() {
  $('#clipSel').textContent = `${clipSrc === 'link' ? linkItems().length : clipPicked.size} selected`;
}
function setClipSrc(src) {
  clipSrc = src;
  $$('#clipSrc button').forEach((b) => b.classList.toggle('active', b.dataset.src === src));
  $('#srcYoutube').classList.toggle('hidden', src !== 'youtube');
  $('#srcTwitch').classList.toggle('hidden', src !== 'twitch');
  $('#srcLink').classList.toggle('hidden', src !== 'link');
  $('#permRow').classList.toggle('hidden', src === 'youtube');
  $('#clipResults').classList.toggle('hidden', src === 'link');
  $('#clipFilters').classList.toggle('hidden', src === 'link');
  $('#fLen').disabled = src !== 'youtube';
  clipNext = '';
  $('#clipNote').textContent = CLIP_NOTES[src];
  clipList = [];
  clipPicked.clear();
  renderClips();
  updateClipSel();
}
$$('#clipSrc button').forEach((b) => (b.onclick = () => setClipSrc(b.dataset.src)));
$('#clipLinks').addEventListener('input', updateClipSel);

let clipQuery = '', clipNext = '';
// YouTube's language field is often empty: fall back to "title is plain English characters".
const isEnglish = (c) => (c.language ? c.language.startsWith('en') : /^[\x20-\x7E’‘“”–—…|•]*$/.test(c.title || ''));
const likeRate = (c) => (c.views ? (c.likes || 0) / c.views : 0);
const SORTS = {
  smart: (a, b) => isEnglish(b) - isEnglish(a) || b.views - a.views || (b.likes || 0) - (a.likes || 0) || (b.comments || 0) - (a.comments || 0),
  views: (a, b) => b.views - a.views,
  likes: (a, b) => (b.likes || 0) - (a.likes || 0),
  ratio: (a, b) => likeRate(b) - likeRate(a),
  comments: (a, b) => (b.comments || 0) - (a.comments || 0),
  new: (a, b) => new Date(b.published || 0) - new Date(a.published || 0),
  long: (a, b) => b.duration - a.duration,
  short: (a, b) => a.duration - b.duration,
};
/** Filtered + sorted view; keeps each video's index in clipList so selections survive re-sorting. */
function visibleClips() {
  const min = +$('#fMin').value || 0, english = $('#fEnglish').checked, fresh = $('#fFresh').checked;
  return clipList
    .map((c, i) => ({ c, i }))
    .filter(({ c }) => (!english || isEnglish(c)) && c.views >= min && (!fresh || !c.used_secs))
    .sort((x, y) => (SORTS[$('#fSort').value] || SORTS.smart)(x.c, y.c));
}
function renderClips() {
  const vis = visibleClips();
  $('#clipResults').innerHTML = vis.length
    ? vis.map(({ c, i }) => `<div class="job clip ${clipPicked.has(i) ? 'sel' : ''}" data-i="${i}">
      <div class="thumb" data-act="pick">${c.thumb ? `<img src="${esc(c.thumb)}" loading="lazy" alt="">` : ''}
        <input type="checkbox" data-act="pick" ${clipPicked.has(i) ? 'checked' : ''}><span class="badge ${c.source === 'youtube' ? 'uploaded' : 'scheduled'}">${esc(c.license)}</span>
        ${c.language ? `<span class="lang">${esc(c.language.slice(0, 2))}</span>` : ''}<span class="dur">${mmss(c.duration || 0)}</span></div>
      <div class="meta"><div class="title">${esc(c.title)}</div>
        <div class="sub">${esc(c.channel)}${c.published ? ` · ${esc(new Date(c.published).toLocaleDateString())}` : ''}</div>
        <div class="stats"><span><b>${num(c.views)}</b> views</span>${c.source === 'youtube' ? `<span><b>${num(c.likes)}</b> likes</span><span><b>${num(c.comments)}</b> comments</span>` : ''}
          ${c.used_secs ? `<span class="usedchip" title="Already made into shorts; new shorts from this video use other parts">Used ${mmss(c.used_secs)} of ${mmss(c.duration || 0)}</span>` : ''}</div>
        <div class="acts"><button data-act="watch">Watch</button></div></div></div>`).join('')
    : `<div class="empty">${clipList.length ? `None of the ${clipList.length} videos match these filters. Loosen them above.` : 'Search above to find videos you can clip.'}</div>`;
  $('#btnMore').classList.toggle('hidden', !(clipSrc === 'youtube' && clipNext));
}
['#fSort', '#fMin', '#fEnglish', '#fFresh'].forEach((s) => ['input', 'change'].forEach((ev) => $(s).addEventListener(ev, renderClips)));
$('#fLen').onchange = () => clipQuery && $('#btnCc').click();
$('#clipResults').onclick = (e) => {
  const el = e.target.closest('[data-act]'), card = e.target.closest('.clip');
  if (!el || !card) return;
  const i = +card.dataset.i;
  if (el.dataset.act === 'watch') return call('open_link', { url: clipList[i].url });
  clipPicked.has(i) ? clipPicked.delete(i) : clipPicked.add(i);
  renderClips();
  updateClipSel();
};
function showClips(list, next = '') {
  clipList = arr(list);
  clipNext = next;
  clipPicked = new Set();
  renderClips();
  updateClipSel();
  log(clipList.length ? `Found ${clipList.length} videos (${visibleClips().length} match your filters)` : 'No matching videos found');
}
const searchOpts = (page = '') => ({ order: 'viewCount', duration: $('#fLen').value, english: $('#fEnglish').checked, page });
const needYoutube = () => (info.connected ? true : (toast('Connect YouTube in Settings first (search uses your channel login)', true), false));
$('#btnCc').onclick = () => {
  const q = $('#ccQuery').value.trim();
  if (!q) return toast('Enter a topic', true);
  clipQuery = q;
  if (needYoutube()) task(`Searching Creative Commons videos: ${q}`, async () => {
    const r = await call('find_clips', { query: q, opts: searchOpts() });
    showClips(r.items, r.next || '');
  });
};
$('#btnMore').onclick = () => task('Loading 50 more videos…', async () => {
  const r = await call('find_clips', { query: clipQuery, opts: searchOpts(clipNext) });
  const seen = new Set(clipList.map((c) => c.id));
  clipList = clipList.concat(arr(r.items).filter((c) => !seen.has(c.id))); // appended, so selections keep their index
  clipNext = r.next || '';
  renderClips();
  updateClipSel();
  log(`Now ${clipList.length} videos`);
});
$('#btnCcChannel').onclick = () => {
  if (needYoutube()) task('Finding videos that fit your channel…', async () => {
    const r = await call('channel_clips');
    log(`Searched: ${arr(r.queries).join(' · ')}`);
    clipQuery = '';
    showClips(r.results);
  });
};
$('#btnTwitch').onclick = () => {
  const login = $('#twLogin').value.trim();
  if (!login) return toast('Enter a Twitch username', true);
  task(`Finding top Twitch clips for ${login}…`, async () => showClips(await call('twitch_clips', { login, days: +$('#twDays').value })));
};
$('#btnMakeClips').onclick = () => {
  const items = clipSrc === 'link' ? linkItems() : [...clipPicked].map((i) => ({ url: clipList[i].url, title: clipList[i].title, channel: clipList[i].channel }));
  if (!items.length) return toast(clipSrc === 'link' ? 'Paste at least one link' : 'Select at least one video', true);
  if (clipSrc !== 'youtube' && !$('#permOk').checked) return toast('Confirm you have permission to use these clips', true);
  task(`Making ${items.length} clip short(s)…`, async () => {
    const n = await call('make_clips', { items, tags: splitTags($('#clipTags').value), per: +$('#clipPer').value });
    toast(`${n}/${items.length} clips ready. Review them in the Library before posting.`);
  });
};

/* ---------- rebrand ---------- */
let brand = null;
const hexOr = (c, d) => (/^#[0-9a-fA-F]{6}$/.test(c || '') ? c : d);
const artSrc = (p) => `${convertFileSrc(p)}?v=${Date.now()}`; // bust the cache after re-rendering
$('#btnRebrand').onclick = () => {
  const niche = $('#rbNiche').value.trim();
  if (!niche) return toast('Enter the niche you want to win', true);
  task(`Designing a channel for “${niche}”…`, async () => {
    brand = await call('rebrand', { niche, writer: $('#wzWriter').value, imageModel: $('#wzImage').value, pending: false });
    renderBrand();
  });
};
function renderBrand() {
  $('#wzLink').classList.toggle('hidden', !brand.pending);
  const b = brand, st = b.style || {};
  $('#rbResult').innerHTML = `
    <div class="card"><h3>Channel art</h3>
      <div class="brand-art"><img class="banner" id="rbBanner" src="${esc(artSrc(b.banner))}" alt="Banner"><img class="avatar" id="rbAvatar" src="${esc(artSrc(b.avatar))}" alt="Profile picture"></div>
      <div class="grid4">
        <div><label class="lbl">Name</label><select id="rbName">${arr(b.names).map((n) => `<option>${esc(n)}</option>`).join('')}</select></div>
        <div><label class="lbl">Tagline</label><input id="rbTagline" value="${esc(b.tagline)}"></div>
        <div><label class="lbl">Colours</label><div class="row"><input type="color" id="rbC0" value="${hexOr(st.primary, '#ff2d55')}"><input type="color" id="rbC1" value="${hexOr(st.secondary, '#7c3aed')}"></div></div>
        <div><label class="lbl">Initials</label><input id="rbInit" maxlength="3" value="${esc(b.initials)}"></div>
      </div>
      <p class="note">${esc(st.vibe)}</p>
      <p class="note" id="rbArtNote">${artNote(b.art)}</p>
      <div class="card" style="margin:12px 0 0"><h3>Finish in YouTube Studio <span class="hint">YouTube only lets you change these yourself</span></h3>
        <ol class="steps">
          <li><b>Name:</b> ${arr(b.names).slice(0, 3).map((n) => `<span class="chip" data-copy="${esc(n)}" title="Copy">${esc(n)}</span>`).join('')}
            <a data-link="https://studio.youtube.com/channel/${esc(settings.active_channel)}/editing/details">open Basic info</a></li>
          <li><b>Handle:</b> ${arr(b.handles).map((h) => `<span class="chip" data-copy="${esc(String(h).replace(/^@?/, '@'))}" title="Copy">${esc(String(h).replace(/^@?/, '@'))}</span>`).join('') || '<span class="muted">pick one close to the name</span>'}
            <a data-link="https://studio.youtube.com/channel/${esc(settings.active_channel)}/editing/details">open Basic info</a></li>
          <li><b>Profile picture:</b> upload <code>avatar.png</code> from the art folder.
            <a data-link="https://studio.youtube.com/channel/${esc(settings.active_channel)}/editing/images">open Branding</a></li>
          <li><b>Banner, description, keywords, playlists:</b> the <b>Apply to my channel</b> button below does these for you.</li>
        </ol>
        <label class="row" style="margin-top:6px"><input type="checkbox" id="rbPlaylists" checked><span>Also create a playlist for each content pillar</span></label>
      </div>
      <div class="row end"><button class="ghost" id="rbFolder">Open art folder</button><button id="rbRedraw">Update text &amp; colours</button><button class="primary" id="rbAi">✨ Regenerate AI art</button></div></div>
    <div class="card"><h3>Description &amp; keywords</h3>
      <textarea id="rbDesc" rows="7">${esc(b.description)}</textarea>
      <label class="lbl">Keywords <span class="hint">comma separated</span></label><textarea id="rbKw" rows="2">${esc(arr(b.keywords).join(', '))}</textarea>
      <label class="row" style="margin-top:12px"><input type="checkbox" id="rbBannerOk" checked><span>Also upload the banner</span></label>
      <p class="note">Apply changes your live channel's description, keywords and banner. Change the name and profile picture yourself in
        <a data-link="https://studio.youtube.com/">YouTube Studio</a> → Customization, because YouTube does not let apps change them. The profile picture is saved as avatar.png in the art folder.</p>
      <div class="row end"><button class="primary" id="rbApply">Apply to my channel</button></div></div>
    <div class="grid2">
      <div class="card"><h3>Content pillars</h3>${arr(b.pillars).map((p) => `<p><b>${esc(p.name)}</b><br><span class="muted">${esc(p.why)}</span></p><ul>${arr(p.examples).map((x) => `<li>${esc(x)}</li>`).join('')}</ul>`).join('')}</div>
      <div class="card"><h3>First videos</h3><div class="ideas">${arr(b.first_videos).map((x, i) => `<label><input type="checkbox" value="${i}" checked><span>${esc(x)}</span></label>`).join('')}</div>
        <p class="note">Posting plan: ${esc(b.schedule)}</p><div class="row end"><button class="primary" id="rbCreate">Create these shorts</button></div></div>
    </div>`;
  $('#rbFolder').onclick = () => call('open_job_folder', { id: '_brand' });
  $('#rbRedraw').onclick = async () => {
    await call('brand_art', { name: $('#rbName').value, tagline: $('#rbTagline').value, initials: $('#rbInit').value, primary: $('#rbC0').value, secondary: $('#rbC1').value, ai: false, niche: '', vibe: '', pending: !!b.pending });
    $('#rbBanner').src = artSrc(b.banner);
    $('#rbAvatar').src = artSrc(b.avatar);
  };
  $('#rbName').onchange = () => $('#rbRedraw').click();
  $('#rbAi').onclick = () => task('Designing channel art with AI…', async () => {
    const r = await call('brand_art', { name: $('#rbName').value, tagline: $('#rbTagline').value, initials: $('#rbInit').value, primary: $('#rbC0').value, secondary: $('#rbC1').value, ai: true, niche: $('#rbNiche').value, vibe: st.vibe || '', pending: !!b.pending });
    $('#rbBanner').src = artSrc(b.banner);
    $('#rbAvatar').src = artSrc(b.avatar);
    b.art = r;
    $('#rbArtNote').innerHTML = artNote(r);
  });
  $$('#rbResult [data-copy]').forEach((el) => (el.onclick = () => navigator.clipboard.writeText(el.dataset.copy).then(() => toast('Copied ' + el.dataset.copy))));
  $('#rbApply').onclick = async (e) => {
    const btn = e.currentTarget;
    if (!info.connected) return toast('Connect YouTube in Settings first', true);
    if (!btn.dataset.armed) {
      btn.dataset.armed = '1';
      btn.textContent = 'Click again to update your live channel';
      return setTimeout(() => { delete btn.dataset.armed; btn.textContent = 'Apply to my channel'; }, 5000);
    }
    btn.disabled = true;
    try {
      const msg = await call('apply_brand', { description: $('#rbDesc').value, keywords: splitTags($('#rbKw').value), banner: $('#rbBannerOk').checked,
        playlists: $('#rbPlaylists') && $('#rbPlaylists').checked ? arr(b.pillars) : [], about: `${$('#rbName').value}: ${$('#rbNiche').value}. ${b.tagline || ''} Pillars: ${arr(b.pillars).map((p) => p.name).join(', ')}` });
      settings = await call('get_settings');
      toast(`${msg}. Last step: set the name, handle and profile picture in YouTube Studio (see the checklist).`);
    } catch {}
    btn.disabled = false;
    delete btn.dataset.armed;
    btn.textContent = 'Apply to my channel';
  };
  $('#rbCreate').onclick = () => {
    const ideas = arr(b.first_videos), picked = $$('#rbResult .ideas input:checked').map((c) => ideas[c.value]);
    if (!picked.length) return toast('Select at least one idea', true);
    task(`Generating ${picked.length} short(s)…`, async () => {
      await call('generate', { topics: picked, tags: arr(b.keywords) });
      toast('Done. Open the Library to review.');
    });
  };
}

/* ---------- create: ideas for the signed-in channel ---------- */
$('#btnChannelIdeas').onclick = () => {
  if (!info.connected) return toast('Connect YouTube in Settings first', true);
  const count = Math.max(1, Math.min(20, +$('#ideaCount').value || 5));
  task('Reading your channel and planning shorts…', async () => {
    const r = await call('channel_ideas', { count });
    const ideas = arr(r.ideas);
    if (ideas.length) $('#topics').value = ideas.join('\n');
    if (arr(r.tags).length) $('#createTags').value = arr(r.tags).join(', ');
    settings.channel_about = r.about || settings.channel_about;
    showAbout();
    toast(`${ideas.length} ideas for your channel. Edit them, then press Generate.`);
  });
};
function showAbout() {
  $('#channelAbout').textContent = settings.channel_about ? `Scripts are written for your channel: ${settings.channel_about}` : '';
}

/* ---------- settings: models per job ---------- */
const TASKS = [['script', 'Scripts'], ['research', 'Research & rebrand'], ['clips', 'Clip moments'], ['replies', 'Comment replies']];
const modelCache = {};
async function fillModels(writer) {
  if (!writer || modelCache[writer]) return;
  modelCache[writer] = await call('list_models', { writer }, { quiet: true }).catch(() => []);
  $(`#modelList-${writer}`).innerHTML = arr(modelCache[writer]).map((m) => `<option value="${esc(m)}">`).join('');
}
function renderTaskModels() {
  const tm = (settings.task_models ||= {});
  $('#taskModels').innerHTML = TASKS.map(([k, label]) => {
    const t = tm[k] || { writer: '', model: '' };
    return `<div class="task-row" data-task="${k}"><span>${label}</span>
      <select data-f="writer"><option value="">Default AI</option>${WRITERS.map((w) => `<option value="${w.v}" ${t.writer === w.v ? 'selected' : ''}>${w.name}</option>`).join('')}</select>
      <input data-f="model" value="${esc(t.model)}" list="modelList-${esc(t.writer || settings.writer)}" placeholder="Default model" ${t.writer ? '' : 'disabled'}></div>`;
  }).join('');
  TASKS.forEach(([k]) => fillModels(tm[k]?.writer || settings.writer));
  $$('#taskModels .task-row').forEach((row) => {
    const k = row.dataset.task, sel = $('select', row), inp = $('input', row);
    const save = () => {
      tm[k] = { writer: sel.value, model: sel.value ? inp.value.trim() : '' };
      saveSoon();
      loadUsage(false);
    };
    sel.onchange = () => {
      inp.disabled = !sel.value;
      inp.value = '';
      inp.setAttribute('list', `modelList-${sel.value || settings.writer}`);
      fillModels(sel.value);
      save();
    };
    inp.oninput = save;
  });
}

/* ---------- usage (sidebar + details) ---------- */
let usage = [];
const fmtTok = (n) => (n ? num(n) : '0');
const meterCls = (u) => (u >= 90 ? 'full' : u >= 70 ? 'warn' : '');
async function loadUsage(fresh) {
  usage = arr(await call('usage_summary', { fresh }, { quiet: true }).catch(() => []));
  const shown = usage.filter((p) => p.active || (p.installed && p.windows.length));
  $('#usage').innerHTML = shown.length
    ? shown.map((p) => {
        const u = p.used;
        const right = u != null ? `${Math.round(u)}% used` : p.id === 'ollama' ? 'local' : `${p.calls_day} calls today`;
        return `<div class="u-row"><div class="u-top"><b>${esc(p.name)}</b><span>${esc(right)}</span></div>${u != null ? `<div class="meter ${meterCls(u)}"><i style="width:${Math.min(100, u)}%"></i></div>` : ''}</div>`;
      }).join('')
    : '<div class="u-top">AI usage</div>';
  renderSwap();
  if (!$('#usageModal').classList.contains('hidden')) renderUsageModal();
}
function renderUsageModal() {
  const jobsFor = (id) => TASKS.filter(([k]) => ((settings.task_models || {})[k]?.writer || settings.writer) === id).map(([, l]) => l);
  $('#usageBody').innerHTML = usage
    .filter((p) => p.installed || p.active)
    .map((p) => {
      const est = typeof p.estimate === 'number' ? `≈ ${p.estimate} more shorts` : p.estimate;
      const used = jobsFor(p.id);
      return `<div class="u-card"><div class="row between"><h3 style="margin:0">${esc(p.name)}</h3><span class="muted small">${used.length ? `Used for: ${esc(used.join(', '))}` : 'Not used by any job'}</span></div>
        ${arr(p.windows).map((w) => `<div class="u-win"><div class="row between"><span>${esc(w.label)}</span><span>${Math.round(w.used)}% used${w.resets ? ` · resets ${esc(w.resets)}` : w.resets_at ? ` · resets ${esc(fmt(new Date(w.resets_at * 1000)))}` : ''}</span></div><div class="meter ${meterCls(w.used)}"><i style="width:${Math.min(100, w.used)}%"></i></div></div>`).join('')}
        <div class="row between" style="margin-top:10px"><div><div class="est" style="${typeof p.estimate === 'number' ? '' : 'font-size:15px'}">${esc(est)}</div><div class="muted small">${p.per_short ? `about ${p.per_short.toFixed(1)}% of the limit per short (learned from your batches)` : typeof p.estimate === 'number' ? '' : ''}</div></div>
        <div class="muted small" style="text-align:right">${p.avg_tokens ? `<b style="color:var(--text)">≈ ${num(p.avg_tokens)} tokens per short</b><br>` : ""}This app, last 24 h: ${p.calls_day} calls · ${fmtTok(p.tokens_day)} tokens<br>Last 7 days: ${p.calls_week} calls · ${fmtTok(p.tokens_week)} tokens</div></div></div>`;
    })
    .join('') + '<p class="note">Claude and ChatGPT report how much of your plan is used; the estimate divides what is left by what one short has cost so far. Gemini and OpenCode do not report remaining limits, so only this app\'s own usage is shown.</p>';
}
$('#usage').onclick = async () => {
  $('#usageModal').classList.remove('hidden');
  renderUsageModal();
};
$('#usageRefresh').onclick = () => loadUsage(true).then(() => toast('Usage refreshed'));
$('#usageClose').onclick = () => $('#usageModal').classList.add('hidden');
$('#usageModal').onclick = (e) => e.target.id === 'usageModal' && $('#usageModal').classList.add('hidden');

/* ---------- analytics ---------- */
const money = (n) => `$${n < 10 ? n.toFixed(2) : Math.round(n).toLocaleString()}`;
$('#btnAnalytics').onclick = () => {
  if (!info.connected) return toast('Connect YouTube in Settings first', true);
  task('Loading channel analytics…', async () => {
    const a = await call('analytics');
    renderAnalytics(a);
    log('Analytics updated');
  });
};
function renderAnalytics(a) {
  const rpm = a.rpm || 0, list = arr(a.list), hist = arr(a.history);
  const week = hist.find((h) => h.day <= hist[hist.length - 1].day - 7) || hist[0];
  const delta = (k) => (week && week !== hist[hist.length - 1] ? a[k] - week[k] : null);
  const since = (d) => (d == null ? 'Growth shows after a few days of refreshes' : `${d >= 0 ? '+' : ''}${num(d)} in the last 7 days`);
  const totalViews = Math.max(a.views, list.reduce((s, v) => s + v.views, 0)); // YouTube's channel total lags behind per-video counts
  const recent = list.filter((v) => Date.now() - new Date(v.published).getTime() < 28 * 864e5);
  const recentViews = recent.reduce((s, v) => s + v.views, 0);
  const shorts = list.filter((v) => v.duration && v.duration <= 180);
  const tiles = [
    ['Subscribers', a.hidden_subs ? 'Hidden' : num(a.subs), since(delta('subs'))],
    ['Total views', num(totalViews), since(delta('views'))],
    ['Views, last 28 days', num(recentViews), `${recent.length} uploads published in that time`],
    ['Est. revenue, last 28 days', money((recentViews / 1000) * rpm), `at $${rpm} per 1,000 views (set in Settings)`],
    ['Est. revenue, all time', money((totalViews / 1000) * rpm), `${num(a.videos)} videos · ${shorts.length} recent Shorts`],
  ];
  $('#anTiles').innerHTML = tiles.map(([k, v, d]) => `<div class="tile"><div class="k">${k}</div><div class="v">${esc(v)}</div><div class="d">${esc(d)}</div></div>`).join('');
  const max = Math.max(1, ...list.map((v) => v.views));
  $('#anNote').textContent = 'Shorts only earn once the channel is in the YouTube Partner Program';
  $('#anTable').innerHTML = list.length
    ? `<table class="an-table"><tr><th>Video</th><th class="num">Views</th><th class="num">Likes</th><th class="num">Comments</th><th class="num">Est. revenue</th></tr>
      ${list.map((v) => `<tr><td><img src="${esc(v.thumb)}" alt=""><a data-link="https://youtube.com/shorts/${esc(v.id)}">${esc(v.title)}</a>
        <span class="muted small"> · ${v.privacy === 'private' && v.publish_at ? `goes live ${esc(fmt(new Date(v.publish_at)))}` : esc(new Date(v.published).toLocaleDateString())}${v.by_app ? ' · made here' : ''}</span></td>
        <td class="num" title="${v.views.toLocaleString()} views">${num(v.views)}<span class="databar" style="width:${Math.round((60 * v.views) / max)}px"></span></td>
        <td class="num">${num(v.likes)}</td><td class="num">${num(v.comments)}</td><td class="num">${money((v.views / 1000) * rpm)}</td></tr>`).join('')}</table>`
    : '<div class="empty">No uploads yet.</div>';
}

/* ---------- quick AI swap (sidebar) ---------- */
function renderSwap() {
  const byId = Object.fromEntries(usage.map((p) => [p.id, p]));
  const mixed = Object.values(settings.task_models || {}).some((t) => t.writer && t.writer !== settings.writer);
  $('#aiSwap').innerHTML = (mixed ? '<option value="">Mixed (per job)</option>' : '') + WRITERS.map((w) => {
    const p = byId[w.v] || {};
    const note = p.used != null ? ` · ${Math.round(p.used)}% used` : w.tool && !info.tools[w.tool] ? ' · not installed' : '';
    return `<option value="${w.v}">${esc(w.name + note)}</option>`;
  }).join('');
  $('#aiSwap').value = mixed ? '' : settings.writer;
}
$('#aiSwap').onchange = async (e) => {
  const v = e.target.value;
  if (!v) return;
  settings.writer = v;
  settings.writer_model = '';
  settings.task_models = {}; // every job now uses the swapped-in AI
  await flushSave();
  syncSetting('writer');
  renderTaskModels();
  renderSwap();
  toast(`All jobs now use ${WRITERS.find((w) => w.v === v).name}`);
};
// A job stopped by a usage limit: point at the swap.
listen('log', (e) => {
  if (/usage limit|rate limit|limit reached|quota|HTTP 429/i.test(e.payload)) toast('Out of AI limit? Pick another AI in the sidebar (top left) and press Retry.', true);
});

/* ---------- live subs, views & likes (sidebar) + milestones ---------- */
let live = null, mon = null;
const SUB_STEPS = [10, 25, 50, 100, 250, 500, 1000, 2500, 5000, 10000, 25000, 50000, 100000, 250000, 500000, 1000000];
const milestoneOf = {
  views: (n) => Math.floor(n / 100) * 100, // every 100 views
  subs: (n) => SUB_STEPS.filter((s) => n >= s).pop() || 0,
  likes: (n) => Math.floor(n / 50) * 50, // every 50 likes
};
const LABEL = { views: 'views', subs: 'subscribers', likes: 'likes' };

/** Celebrate milestones crossed since last time (remembered across restarts; the first run just records). */
function checkMilestones(stats) {
  const seen = JSON.parse(localStorage.getItem('aiyt-milestones') || 'null');
  const now = Object.fromEntries(Object.keys(milestoneOf).map((k) => [k, milestoneOf[k](stats[k] || 0)]));
  if (seen) {
    const hits = Object.keys(now).filter((k) => now[k] > (seen[k] || 0) && now[k] > 0);
    // Subscribers matter most, then likes, then views: show the biggest news.
    const order = ['subs', 'likes', 'views'].filter((k) => hits.includes(k));
    if (order.length) celebrate(`${num(now[order[0]])} ${LABEL[order[0]]}!`, order.slice(1).map((k) => `${num(now[k])} ${LABEL[k]}`).join(' · '));
  }
  localStorage.setItem('aiyt-milestones', JSON.stringify(seen ? Object.fromEntries(Object.keys(now).map((k) => [k, Math.max(now[k], seen[k] || 0)])) : now));
}

function celebrate(title, sub) {
  log(`🎉 Milestone: ${title}${sub ? ` (${sub})` : ''}`);
  const box = document.createElement('div');
  box.className = 'celebrate';
  box.innerHTML = `<div class="cele-card"><div class="cele-emoji">🎉</div><div class="cele-title">${esc(title)}</div>${sub ? `<div class="cele-sub">${esc(sub)}</div>` : ''}</div>`;
  // Confetti unless the user chose to follow Windows' "reduce motion" setting (Settings → Advanced).
  const confetti = (localStorage.getItem('aiyt-confetti') || 'on') === 'on' || !matchMedia('(prefers-reduced-motion: reduce)').matches;
  if (confetti) {
    const colors = ['#ff2d55', '#7c3aed', '#f59e0b', '#22c55e', '#3b82f6', '#ffffff'];
    for (let i = 0; i < 90; i++) {
      const c = document.createElement('i');
      c.style.left = `${Math.random() * 100}%`;
      c.style.background = colors[i % colors.length];
      c.style.animationDelay = `${Math.random() * 0.6}s`;
      c.style.animationDuration = `${2 + Math.random() * 1.5}s`;
      c.style.transform = `rotate(${Math.random() * 360}deg)`;
      box.append(c);
    }
  }
  box.onclick = () => box.remove();
  document.body.append(box);
  setTimeout(() => box.remove(), 4500);
}

/** Progress (0-1) towards a tier: subscribers plus the better of Shorts views or watch hours. */
function tierProgress(m, t) {
  const subs = Math.min(1, (m.subs || 0) / t.subs);
  const reach = Math.max(Math.min(1, (m.shorts_views_90 || 0) / t.shorts_views), Math.min(1, (m.watch_hours_365 || 0) / t.watch_hours));
  const ups = t.uploads ? Math.min(1, (m.uploads_90 || 0) / t.uploads) : 1;
  return { subs, reach, ups, all: Math.min(subs, reach, ups) };
}
const eta = (left, perDay) => (left <= 0 ? 'reached ✓' : perDay > 0 ? `≈ ${fmtDays(left / perDay)} at your current pace` : 'needs a few days of data');
const fmtDays = (d) => (d < 1 ? 'under a day' : d < 60 ? `${Math.ceil(d)} days` : d < 730 ? `${Math.round(d / 30)} months` : `${(d / 365).toFixed(1)} years`);

async function loadLive() {
  if (!info.connected || document.hidden) return $('#liveStats').classList.toggle('hidden', !info.connected);
  const m = await call('monetization', {}, { quiet: true }).catch(() => null);
  if (!m) return;
  mon = m;
  const views = Math.max(m.views || 0, 0);
  const stats = { subs: m.subs, views, likes: m.likes };
  const bump = (el, now, before) => {
    el.textContent = now == null ? 'Hidden' : Number(now).toLocaleString();
    if (before != null && now > before) {
      el.classList.add('up');
      setTimeout(() => el.classList.remove('up'), 4000);
    }
  };
  bump($('#liveSubs'), stats.subs, live?.subs);
  bump($('#liveViews'), stats.views, live?.views);
  bump($('#liveLikes'), stats.likes, live?.likes);
  live = stats;
  checkMilestones(stats);
  const first = arr(m.tiers)[0];
  if (first) {
    const p = tierProgress(m, first);
    $('#liveMon').innerHTML = `<div class="u-top"><span>Monetization</span><span>${(p.all * 100).toFixed(p.all < 0.01 ? 2 : p.all < 0.1 ? 1 : 0)}%</span></div><div class="meter"><i style="width:${Math.max(1, p.all * 100)}%"></i></div>`;
  }
  $('#liveStats').classList.remove('hidden');
  if ($('#page-analytics').classList.contains('active')) renderMonetization();
}
$('#liveStats').onclick = () => loadLive().then(() => toast('Live stats refreshed'));
// Every 3 minutes while the app is on screen, and on returning to it.
setInterval(loadLive, 180000);
document.addEventListener('visibilitychange', () => !document.hidden && loadLive());

/* ---------- road to monetization (Analytics page) ---------- */
function renderMonetization() {
  const m = mon;
  if (!m) return;
  const bar = (label, have, need, perDay, unit = '') => {
    const pct = Math.min(100, (100 * have) / need);
    return `<div class="mon-row"><div class="row between"><span>${label}</span><span><b>${num(have)}</b> / ${num(need)}${unit}</span></div>
      <div class="meter big ${pct >= 100 ? 'done' : ''}"><i style="width:${Math.max(0.8, pct)}%"></i></div><div class="muted small">${esc(eta(need - have, perDay))}</div></div>`;
  };
  const viewsPerDay = m.views_per_day || 0;
  $('#monCard').innerHTML = `<div class="row between"><h3 style="margin:0">Road to monetization</h3><span class="muted small">${m.exact ? 'Exact numbers from YouTube Analytics' : 'Shorts views are estimated from your recent uploads (reconnect YouTube for exact numbers)'}</span></div>
    <div class="grid2" style="margin-top:10px">${arr(m.tiers).map((t) => {
      const p = tierProgress(m, t);
      return `<div class="tier ${p.all >= 1 ? 'done' : ''}"><div class="row between"><b>${esc(t.name)}</b><span class="badge ${p.all >= 1 ? 'good' : 'mid'}">${(p.all * 100).toFixed(p.all < 0.01 ? 2 : p.all < 0.1 ? 1 : 0)}%</span></div>
        <div class="muted small" style="margin:2px 0 8px">${esc(t.unlocks)}</div>
        ${bar('Subscribers', m.subs || 0, t.subs, m.subs_per_day || 0)}
        ${bar('Shorts views (last 90 days)', m.shorts_views_90 || 0, t.shorts_views, viewsPerDay)}
        ${m.exact ? `<div class="muted small">or ${num(m.watch_hours_365)} / ${num(t.watch_hours)} long-form watch hours (12 months)</div>` : ''}
        ${t.uploads ? `<div class="muted small">${m.uploads_90 >= t.uploads ? '✓' : '○'} ${m.uploads_90}/${t.uploads} public uploads in the last 90 days</div>` : ''}</div>`;
    }).join('')}</div>
    <p class="note">Growth rates come from the app's daily snapshots (${m.history_days} day${m.history_days === 1 ? '' : 's'} so far), so the estimates sharpen over time. YouTube's thresholds can change and vary by country: check YouTube Studio → Earn.</p>`;
}

/* ---------- Kokoro voices + voice preview ---------- */
const VOICE_KIND = { af: 'US female', am: 'US male', bf: 'UK female', bm: 'UK male' };
async function loadKokoroVoices() {
  const list = arr(await call('kokoro_voices', {}, { quiet: true }).catch(() => []));
  const label = (v) => `${v.split('_')[1].replace(/^./, (c) => c.toUpperCase())} · ${VOICE_KIND[v.slice(0, 2)] || ''}${v === 'af_heart' ? ' (best)' : ''}`;
  $('#kokoroVoices').innerHTML = list.map((v) => `<option value="${esc(v)}">${esc(label(v))}</option>`).join('');
  $('#kokoroVoices').value = settings.kokoro_voice || 'af_heart';
  $('#stNarrator').innerHTML = $('#kokoroVoices').innerHTML;
  $('#stNarrator').value = (story.prefs && story.prefs.narrator_voice) || 'bm_george';
}
let previewAudio = null;
$$('.btnPreview').forEach((b) => (b.onclick = () => task('Making a voice preview (the first Kokoro run downloads it once)…', async () => {
  const path = await call('preview_voice', { voice: b.dataset.voice || null });
  if (previewAudio) previewAudio.pause();
  previewAudio = new Audio(`${convertFileSrc(path)}?v=${Date.now()}`);
  await previewAudio.play();
  log('▶ Playing voice preview');
})));

/* ---------- Piper voices + GPU pack ---------- */
async function loadPiperVoices() {
  const list = arr(await call('piper_voices', {}, { quiet: true }).catch(() => []));
  const label = (v) => {
    const [lang, name] = v.split('-');
    return `${name.replace('_', ' ').replace(/^./, (c) => c.toUpperCase())} · ${lang === 'en_GB' ? 'UK' : 'US'}${v === 'en_US-lessac-medium' ? ' (best)' : ''}`;
  };
  $('#piperVoices').innerHTML = list.map((v) => `<option value="${esc(v)}">${esc(label(v))}</option>`).join('');
  $('#piperVoices').value = settings.piper_voice || 'en_US-lessac-medium';
}
async function renderGpu() {
  const g = await call('gpu_status', {}, { quiet: true }).catch(() => ({}));
  $('#gpuState').textContent = !g.nvidia ? 'no NVIDIA GPU found' : g.cuda ? '✓ ready: local voices use your GPU' : 'NVIDIA GPU found, pack not installed';
  $('#btnGpuSetup').disabled = !g.nvidia || g.cuda;
  if (g.cuda) $('#btnGpuSetup').textContent = 'GPU voices installed';
}
$('#btnGpuSetup').onclick = () => task('Setting up GPU voices: downloading NVIDIA CUDA + cuDNN (~2.6 GB)…', async () => {
  await call('setup_gpu_voice');
  toast('GPU voices ready. Kokoro and Piper now run on your GPU.');
  renderGpu();
});

/* ---------- trends (Google + YouTube) ---------- */
let trendData = { google: [], youtube: [] };
const addTopic = (t) => {
  const box = $('#topics');
  box.value = (box.value.trim() ? box.value.trim() + '\n' : '') + t;
  toast('Added to Create. Open Create and press Generate.');
};
function renderTrends() {
  $('#trGoogle').innerHTML = trendData.google.length
    ? trendData.google.slice(0, 25).map((t) => `<div class="tr-row"><div><div class="t">${esc(t.title)}</div>
        <div class="n">${arr(t.news).slice(0, 1).map((n) => `<a data-link="${esc(n.url)}">${esc(n.title)}</a> · ${esc(n.source)}`).join('')}</div></div>
        <div class="acts"><span class="badge good">${esc(t.traffic)}</span></div></div>`).join('')
    : 'No data.';
  const yt = trendData.youtube.filter((v) => !$('#trShorts').checked || v.duration <= 180);
  $('#trYoutube').innerHTML = yt.length
    ? yt.slice(0, 20).map((v) => `<div class="tr-row"><div class="mini"><img src="${esc(v.thumb)}" alt=""><div><div class="t">${esc(v.title)}</div>
        <div class="n">${esc(v.channel)} · ${num(v.views)} views · ${num(v.likes)} likes · ${mmss(v.duration)}</div></div></div>
        <div class="acts"><button data-act="why" data-url="${esc(v.url)}">Why it works</button></div></div>`).join('')
    : `<div class="muted small">${trendData.youtube.length ? 'No Shorts-length videos in this chart; untick "Shorts only".' : ''}</div>`;
}
$('#trShorts').onchange = renderTrends;
$('#btnTrends').onclick = () => task('Loading what is trending…', async () => {
  trendData = await call('trends_now', { geo: $('#trGeo').value, category: $('#trCat').value });
  trendData.google = arr(trendData.google);
  trendData.youtube = arr(trendData.youtube);
  $('#trNote').textContent = trendData.note || '';
  renderTrends();
  renderNiche();
  log(`Trends: ${trendData.google.length} rising searches, ${trendData.youtube.length} trending videos`);
});
['#trGeo', '#trCat'].forEach((s) => ($(s).onchange = () => $('#btnTrends').click()));
$('#btnPicks').onclick = () => task('Finding trends that fit your channel…', async () => {
  if (!trendData.google.length && !trendData.youtube.length) {
    trendData = await call('trends_now', { geo: $('#trGeo').value, category: $('#trCat').value });
    trendData.google = arr(trendData.google);
    trendData.youtube = arr(trendData.youtube);
    renderTrends();
  }
  const r = await call('trend_picks', { google: trendData.google, youtube: trendData.youtube, niche: arr(trendData.niche_videos) });
  const picks = arr(r.picks);
  $('#trPicks').innerHTML = `<div class="card"><div class="row between"><h3>AI picks for your channel</h3><button class="primary" id="btnPickAll">Make all ${picks.length}</button></div>
    ${picks.map((p, i) => `<div class="tr-row"><div><div class="t">${esc(p.topic)}</div><div class="n">${esc(p.trend)} · ${esc(p.why)}</div></div>
      <div class="acts"><span class="badge ${p.urgency === 'today' ? 'bad' : 'mid'}">${esc(p.urgency)}</span><button data-pick="${i}">Make</button></div></div>`).join('')}</div>`;
  $$('#trPicks [data-pick]').forEach((b) => (b.onclick = () => addTopic(picks[b.dataset.pick].topic)));
  $('#btnPickAll').onclick = () => task(`Generating ${picks.length} trend shorts…`, async () => {
    await call('generate', { topics: picks.map((p) => p.topic), tags: [] });
    toast('Done. Review them in the Library.');
  });
});
$('#trYoutube').onclick = (e) => {
  const b = e.target.closest('[data-act="why"]');
  if (!b) return;
  $('#wUrl').value = b.dataset.url;
  $('#btnWatch').click();
  $('#wUrl').scrollIntoView({ behavior: 'smooth' });
};

/* ---------- why does this video work? ---------- */
$('#btnWatch').onclick = () => {
  const url = $('#wUrl').value.trim();
  if (!url) return toast('Paste a video link', true);
  task('Watching the video for you…', async () => {
    const r = await call('watch_video', { url });
    const m = r.metrics || {};
    $('#wReport').innerHTML = `<h3 style="margin-top:18px">${esc(r.title)} <span class="muted small">· ${esc(r.creator)}</span></h3>
      <div class="tiles">${[
        ['Cuts per minute', m.cuts_per_minute], ['Avg shot', `${m.avg_shot_seconds}s`], ['Words per minute', m.words_per_minute],
        ['First cut at', m.first_cut_at != null ? `${Number(m.first_cut_at).toFixed(1)}s` : 'none'], ['Views', m.views != null ? num(m.views) : '–'],
      ].map(([k, v]) => `<div class="tile"><div class="k">${k}</div><div class="v">${esc(v)}</div></div>`).join('')}</div>
      <div class="frames">${arr(r.frames).map((f) => `<img src="${esc(convertFileSrc(f))}" alt="">`).join('')}</div>
      <p><b>Verdict:</b> ${esc(r.verdict)}</p>
      <p><b>Hook (first 3 s):</b> ${esc(r.hook?.what)} <span class="muted">${esc(r.hook?.why)}</span></p>
      <div class="grid2"><div><h3>Structure</h3><ul class="beats">${arr(r.structure).map((b) => `<li><b>${esc(b.time)}</b>${esc(b.beat)}</li>`).join('')}</ul>
        <p><b>Pacing:</b> ${esc(r.pacing)}</p><p><b>Emotion:</b> ${esc(r.emotion)}</p><p><b>Call to action:</b> ${esc(r.cta)}</p></div>
        <div><h3>Retention tricks</h3><ul>${arr(r.retention).map((x) => `<li>${esc(x)}</li>`).join('')}</ul>
        <h3>Copy this</h3><ul>${arr(r.copy_this).map((x) => `<li>${esc(x)}</li>`).join('')}</ul></div></div>
      <h3>Shorts using this formula</h3>${arr(r.ideas).map((x, i) => `<div class="tr-row"><div class="t">${esc(x)}</div><div class="acts"><button data-idea="${i}">Make</button></div></div>`).join('')}`;
    $$('#wReport [data-idea]').forEach((b) => (b.onclick = () => addTopic(arr(r.ideas)[b.dataset.idea])));
    log('Analysis ready');
  });
};

/* ---------- studio stats (YouTube Analytics) ---------- */
$('#btnStudio').onclick = () => {
  if (!info.connected) return toast('Connect YouTube in Settings first', true);
  task('Loading Studio stats…', async () => {
    const r = await call('studio_insights', { days: +$('#stDays').value, advise: $('#stAdvice').checked });
    const t = r.totals || {}, pct = (x) => (x == null ? '–' : `${Math.round(x)}%`);
    const tiles = [
      ['Views', num(t.views)], ['Watch time', `${num(Math.round((t.estimatedMinutesWatched || 0) / 60))} h`],
      ['Avg view duration', mmss(t.averageViewDuration || 0)], ['Avg % viewed', pct(t.averageViewPercentage)],
      ['Subscribers', `${(t.subscribersGained || 0) - (t.subscribersLost || 0) >= 0 ? '+' : ''}${(t.subscribersGained || 0) - (t.subscribersLost || 0)}`],
      ['Shares', num(t.shares)],
    ];
    const traffic = arr(r.traffic), tmax = Math.max(1, ...traffic.map((x) => x.views || 0));
    const label = (k) => String(k || '').toLowerCase().replace(/_/g, ' ').replace(/^./, (c) => c.toUpperCase());
    const a = r.advice;
    $('#stBody').innerHTML = `<p class="note" style="margin:0 0 10px">${esc(r.start)} → ${esc(r.end)}</p>
      <div class="tiles">${tiles.map(([k, v]) => `<div class="tile"><div class="k">${k}</div><div class="v">${esc(v)}</div></div>`).join('')}</div>
      ${a ? `<div class="card"><h3>What performs</h3><p>${esc(a.summary)}</p><div class="grid2"><div><b>Working</b><ul>${arr(a.working).map((x) => `<li>${esc(x)}</li>`).join('')}</ul>
        <b>Fix</b><ul>${arr(a.fix).map((x) => `<li>${esc(x)}</li>`).join('')}</ul></div><div><b>Make next</b><ul>${arr(a.next).map((x) => `<li>${esc(x)}</li>`).join('')}</ul></div></div></div>` : ''}
      <div class="grid2" style="align-items:start"><div><h3>Where views come from</h3>${traffic.map((x) => `<div class="bar-row" title="${num(x.views)} views"><span>${esc(label(x.insightTrafficSourceType))}</span>
        <span class="bar" style="width:${Math.round((100 * (x.views || 0)) / tmax)}%"></span><span class="v">${num(x.views)}</span></div>`).join('') || '<p class="muted">No traffic yet.</p>'}</div>
      <div><h3>Retention by video</h3><table class="an-table"><tr><th>Video</th><th class="num">Views</th><th class="num">Avg % viewed</th></tr>
        ${arr(r.videos).map((v) => `<tr><td><a data-link="https://youtube.com/shorts/${esc(v.video)}">${esc(v.title || v.video)}</a></td><td class="num">${num(v.views)}</td><td class="num">${pct(v.averageViewPercentage)}</td></tr>`).join('')}</table></div></div>`;
    log('Studio stats loaded');
  });
};

/* ---------- smart picks (a short is blowing up → follow-up) ---------- */
async function loadSmartPicks() {
  const picks = arr(await call('smart_picks', {}, { quiet: true }).catch(() => []));
  const html = picks.length
    ? picks.slice(0, 3).map((p, i) => `<div class="card smart"><div class="row between"><div>
        <div class="muted small">${p.kind === 'starter' ? '🚀 Launch idea for your new channel' : ''}</div><div class="muted small ${p.kind === 'starter' ? 'hidden' : ''}">🔥 Your short “${esc(p.video_title)}” is taking off: ${num(p.rate)} views/hour (${esc(p.ratio)}× your normal)${p.retention != null ? `, ${Math.round(p.retention)}% viewed` : ''}, ${num(p.comments)} comments</div>
        <h3 style="margin:6px 0 2px">Top pick: ${esc(p.topic)}</h3><div class="muted small">${esc(p.why)}</div></div>
        <div class="row"><button class="ghost" data-dismiss="${i}">Dismiss</button><button class="primary" data-make="${i}">Create &amp; review</button></div></div></div>`).join('')
    : '';
  $$('.smartBox').forEach((box) => {
    box.innerHTML = html;
    $$('[data-make]', box).forEach((b) => (b.onclick = () => {
      const p = picks[b.dataset.make];
      task(`Making the follow-up to “${p.video_title}”…`, async () => {
        await call('generate', { topics: [p.topic], tags: [] });
        await call('dismiss_pick', { videoId: p.video_id });
        loadSmartPicks();
        toast('Follow-up ready. Review it in the Library, then publish.');
        show('library');
      });
    }));
    $$('[data-dismiss]', box).forEach((b) => (b.onclick = async () => {
      await call('dismiss_pick', { videoId: picks[b.dataset.dismiss].video_id });
      loadSmartPicks();
    }));
  });
  $('nav [data-page="create"]').classList.toggle('hot', picks.length > 0);
}
listen('smart-pick', () => {
  loadSmartPicks();
  toast('🔥 One of your shorts is blowing up. A follow-up pick is waiting on the Create page.');
});

/* ---------- trends: hot in your niche ---------- */
function renderNiche() {
  const list = arr(trendData.niche_videos);
  $('#trNicheQ').textContent = trendData.niche ? `“${trendData.niche}”, top Shorts this month` : '';
  $('#trNiche').innerHTML = list.length
    ? list.slice(0, 12).map((v) => `<div class="tr-row"><div><div class="t"><a data-link="${esc(v.url)}">${esc(v.title)}</a></div><div class="n">${esc(v.channel)} · ${num(v.views)} views</div></div>
        <div class="acts"><button data-act="why" data-url="${esc(v.url)}">Why it works</button></div></div>`).join('')
    : `<span class="muted small">${info.connected ? 'Nothing found for your niche yet.' : 'Connect YouTube to see what is hot in your niche.'}</span>`;
}
$('#trNiche').onclick = (e) => $('#trYoutube').onclick(e);

/* ---------- daily automation ---------- */
let autoPlan = null;
const dayNum = (dateStr) => {
  const [y, m, d] = dateStr.split('-').map(Number);
  return Math.floor(Date.UTC(y, m - 1, d) / 864e5);
};
const dayStr = (n) => new Date(n * 864e5).toISOString().slice(0, 10);
function renderAutoTimes() {
  const n = Math.max(1, Math.min(10, +$('#auPer').value || 1));
  const t = arr(autoPlan.times);
  $('#auTimes').innerHTML = Array.from({ length: n }, (_, i) => `<input type="time" class="w90" style="width:120px" data-i="${i}" value="${esc(t[i] || '')}">`).join('');
  $$('#auTimes input').forEach((el) => (el.onchange = saveAuto));
}
async function loadAuto() {
  const r = await call('auto_status', {}, { quiet: true }).catch(() => null);
  if (!r) return;
  autoPlan = r.plan;
  $('#auOn').checked = autoPlan.enabled;
  $('#auPer').value = autoPlan.per_day;
  $('#auDays').value = autoPlan.days;
  $('#auStart').value = autoPlan.start_day ? dayStr(autoPlan.start_day) : toLocalInput(new Date()).slice(0, 10);
  $('#auMake').value = autoPlan.make_at;
  renderAutoTimes();
  const last = arr(r.history)[0];
  $('#auStatus').textContent = !autoPlan.enabled
    ? 'Off'
    : r.days_left <= 0
      ? 'Plan finished. Pick a new start date to run again.'
      : r.day_of_plan < 1
        ? `Starts ${dayStr(autoPlan.start_day)} at ${autoPlan.make_at}`
        : `Day ${r.day_of_plan} of ${autoPlan.days} · ${r.done_today ? 'today’s batch is done' : `today’s batch starts at ${autoPlan.make_at}`}${last ? ` · last run: ${last.made} made, ${last.uploaded} published${last.note ? ` (${last.note})` : ''}` : ''}`;
}
async function saveAuto() {
  const times = $$('#auTimes input').map((el) => el.value).filter(Boolean);
  settings.auto = {
    enabled: $('#auOn').checked, per_day: Math.max(1, Math.min(10, +$('#auPer').value || 1)), days: Math.max(1, +$('#auDays').value || 1),
    start_day: dayNum($('#auStart').value || toLocalInput(new Date()).slice(0, 10)), times, make_at: $('#auMake').value || '08:00',
    tz_offset_min: new Date().getTimezoneOffset(),
  };
  if (settings.auto.enabled && !info.connected) {
    $('#auOn').checked = settings.auto.enabled = false;
    toast('Connect YouTube in Settings first', true);
  }
  await flushSave();
  loadAuto();
  ensureCtaOption();
}
['#auOn', '#auDays', '#auStart', '#auMake'].forEach((s) => ($(s).onchange = saveAuto));
$('#auPer').onchange = () => {
  autoPlan.times = $$('#auTimes input').map((el) => el.value);
  renderAutoTimes();
  saveAuto();
};
$('#btnAutoNow').onclick = () => {
  if (!info.connected) return toast('Connect YouTube in Settings first', true);
  task('Running today’s automation batch…', async () => {
    await call('auto_run_now');
    loadAuto();
  ensureCtaOption();
  });
};
listen('auto-changed', () => loadAuto());

$('#confettiPref').value = localStorage.getItem('aiyt-confetti') || 'on';
$('#confettiPref').onchange = (e) => localStorage.setItem('aiyt-confetti', e.target.value);

/* ---------- library: select all / half / none ---------- */
function readyToPost() {
  return jobs.filter((j) => !PUBLISHED.includes(j.status) && uploadable(j)).sort((a, b) => a.created - b.created);
}
$('#selAll').onclick = () => {
  readyToPost().forEach((j) => selected.add(j.id));
  renderJobs();
};
$('#selHalf').onclick = () => {
  // The older half: post those now, keep the newer half for later.
  const ready = readyToPost();
  selected.clear();
  ready.slice(0, Math.ceil(ready.length / 2)).forEach((j) => selected.add(j.id));
  renderJobs();
};
$('#selNone').onclick = () => {
  selected.clear();
  renderJobs();
};

/* ---------- published: YouTube's processing status ---------- */
let ytState = {};
async function loadYtState() {
  if (!info.connected) return;
  ytState = (await call('published_status', {}, { quiet: true }).catch(() => null)) || {};
  renderJobs();
}
const YT_STATE = {
  processing: ['mid', 'YouTube is still processing this video'],
  failed: ['bad', 'YouTube processing failed'],
  rejected: ['bad', 'YouTube rejected this video'],
  deleted: ['bad', 'Deleted on YouTube'],
  missing: ['bad', 'Not found on YouTube'],
};
function ytLine(j) {
  const y = j.video_id && ytState[j.video_id];
  if (!y || !YT_STATE[y.state]) return '';
  const [cls, text] = YT_STATE[y.state];
  const why = { duplicate: 'a duplicate of another upload', copyright: 'copyright claim', uploaderAccountSuspended: 'account suspended' }[y.reason] || y.reason || '';
  return `<div class="sub"><span class="badge ${cls}">${esc(y.state)}</span> ${esc(text)}${why ? `: ${esc(why)}` : ''}</div>`;
}

/* ---------- call-to-action rate: show auto-tuned values too ---------- */
function ensureCtaOption() {
  const sel = $('[data-setting="cta_rate"]');
  const v = String(settings.cta_rate ?? 33);
  if (![...sel.options].some((o) => o.value === v)) {
    const o = document.createElement('option');
    o.value = v;
    o.textContent = `${v}% of shorts (auto-tuned)`;
    sel.append(o);
  }
  sel.value = v;
}

/* ---------- channels: switcher + new-channel setup ---------- */
function artNote(r) {
  if (!r) return '';
  if (r === 'ai') return '✨ Banner and profile picture designed by AI, with your channel name as the logo.';
  return '🎨 Simple styled art for now: AI-designed art needs an image model (a Gemini key with billing, or a local image model). Press “✨ Regenerate AI art” once one is set up.';
}
async function loadChannels() {
  const r = await call('list_channels', {}, { quiet: true }).catch(() => ({ channels: [], active: '' }));
  const list = arr(r.channels);
  $('#chanSel').innerHTML = (list.length ? '' : '<option value="">No channel linked</option>') +
    list.map((c) => `<option value="${esc(c.id)}">📺 ${esc(c.title || c.id)}</option>`).join('') +
    list.map((c) => `<option value="quick:${esc(c.id)}">⚡ Make 3 now: ${esc(c.title || c.id)}</option>`).join('') + '<option value="__add">＋ Add a channel…</option>';
  $('#chanSel').value = r.active || '';
}
$('#chanSel').onchange = async (e) => {
  const v = e.target.value;
  if (v.startsWith('quick:')) {
    const id = v.slice(6);
    if (id === settings.active_channel) {
      loadChannels();
      return openQuick();
    }
    sessionStorage.setItem('aiyt-quick', '1'); // reopen the quick window after switching channel
    try {
      await call('switch_channel', { id });
      location.reload();
    } catch {
      loadChannels();
    }
    return;
  }
  if (v === '__add') {
    if (!settings.google_client_id || !settings.google_client_secret) {
      toast('Add your Google OAuth client ID and secret in Settings first', true);
      loadChannels();
      return show('settings');
    }
    toast('Sign in in your browser and pick the channel to add…');
    try {
      const name = await call('yt_connect');
      toast(`Linked ${name}`);
      location.reload(); // everything reloads for the new channel
    } catch {
      loadChannels();
    }
    return;
  }
  if (!v) return;
  try {
    await call('switch_channel', { id: v });
    location.reload();
  } catch {
    loadChannels();
  }
};
function showSetupPrompt() {
  // A freshly linked channel has no profile yet: offer the full setup.
  const fresh = info.connected && !settings.channel_about;
  $('#setupBox').innerHTML = fresh
    ? `<div class="card smart"><div class="row between"><div><h3 style="margin:0 0 4px">👋 New channel: let's set it up</h3>
        <div class="muted small">The AI designs the banner and profile picture, and writes the description, keywords, handle ideas and content pillars, then applies them to YouTube for you.</div></div>
        <button class="primary" id="btnSetupNow">Set up this channel</button></div></div>`
    : '';
  if (fresh) $('#btnSetupNow').onclick = () => {
    show('rebrand');
    $('#rbNiche').focus();
  };
}

/* ---------- stories ---------- */
let story = { prefs: {}, series: [], tools: {} };
async function loadStories() {
  story = await call('story_status', {}, { quiet: true }).catch(() => story);
  const p = story.prefs || {};
  $('#stEnabled').value = String(!!p.enabled);
  const fastOpt = $('#stStyle option[value="fast"]'), fastOk = !!(story.tools || {}).fast;
  fastOpt.disabled = !fastOk;
  fastOpt.textContent = fastOk ? 'Fast: drawn with code' : 'Fast: drawn with code (next update)';
  $('#stStyle').value = fastOk ? p.style || 'fast' : 'slow';
  $('#stSecs').value = p.seconds || 40;
  $('#stSecsOut').textContent = `${$('#stSecs').value}s`;
  $('#stParts').value = p.parts || 4;
  $('#stImages').value = p.images || 'local';
  if ($('#stNarrator').options.length) $('#stNarrator').value = p.narrator_voice || 'bm_george';
  renderStoryTools();
  if (story.research) renderStoryResearch(story.research);
  renderSeries();
}
function storyPrefs() {
  return { enabled: $('#stEnabled').value === 'true', style: $('#stStyle').value, seconds: +$('#stSecs').value, images: $('#stImages').value,
    narrator_voice: $('#stNarrator').value || 'bm_george', parts: Math.max(1, Math.min(12, +$('#stParts').value || 4)) };
}
async function saveStoryPrefs() {
  $('#stSecsOut').textContent = `${$('#stSecs').value}s`;
  await call('story_prefs', { prefs: storyPrefs() });
  story.prefs = storyPrefs();
  renderStoryTools();
}
['#stEnabled', '#stStyle', '#stSecs', '#stParts', '#stImages', '#stNarrator'].forEach((s) => ($(s).onchange = saveStoryPrefs));
$('#stSecs').oninput = () => ($('#stSecsOut').textContent = `${$('#stSecs').value}s`);

function renderStoryTools() {
  const t = story.tools || {}, slowLocal = $('#stStyle').value === 'slow' && $('#stImages').value === 'local';
  $('#stStyleNote').textContent = $('#stStyle').value === 'fast'
    ? 'Fast: characters and places are drawn with code. About 1-2 minutes a part, no picture AI needed.'
    : `Slow: one AI picture per scene (${$('#stImages').value === 'local' ? 'about 9 seconds each on your GPU' : 'Gemini, paid key'}). About 3-4 minutes a part.`;
  let html = '';
  if (!t.node) html = 'Story videos are animated with GiveMotion, which needs <b>Node.js</b>. Install it (<code>winget install OpenJS.NodeJS.LTS</code>), then restart AIYT Studio.';
  else if (!t.animator || (slowLocal && !t.pictures)) {
    const parts = [!t.animator && 'the story animator (~150 MB)', slowLocal && !t.pictures && 'local pictures for the Slow style (~8.5 GB)'].filter(Boolean);
    html = `One-time setup: ${parts.join(' and ')}. <button class="primary" id="btnStSetup">Set up now</button>`;
  }
  $('#stSetup').innerHTML = html ? `<div class="card smart"><div class="small">${html}</div></div>` : '';
  if ($('#btnStSetup')) $('#btnStSetup').onclick = () => {
    toast('Setting up the story tools in the background. You can keep working; progress is in the activity bar.');
    task('Setting up the story tools…', async () => {
      await call('setup_story_tools', { pictures: slowLocal && !t.pictures });
      toast('Story tools ready');
      loadStories();
    });
  };
}

function renderStoryResearch(r) {
  const ideas = arr(r.ideas);
  $('#stResearch').innerHTML = `${arr(r.insights).length ? `<b>What works now${r.niche ? ` in ${esc(r.niche)}` : ''}</b><ul>${arr(r.insights).map((x) => `<li>${esc(x)}</li>`).join('')}</ul>` : ''}
    <div class="niche-grid">${ideas.map((i, k) => `<div class="niche"><b>${esc(i.title)}</b><p>${esc(i.premise)}</p>
      <p class="muted small">“${esc(i.hook)}”</p><p class="muted small">${esc(i.why)} · ${num(i.parts)} parts</p>
      <button data-idea="${k}">Start this series</button></div>`).join('')}</div>
    ${arr(r.top_videos).length ? `<details><summary>The most-viewed stories it studied</summary><ul>${arr(r.top_videos).map((v) => `<li>${esc(v.title)} · ${num(v.views)} views</li>`).join('')}</ul></details>` : ''}`;
  $$('#stResearch [data-idea]').forEach((b) => (b.onclick = () => {
    const i = ideas[b.dataset.idea];
    newSeries(`${i.title}: ${i.premise} First line: ${i.hook}`);
  }));
}
$('#btnStResearch').onclick = () => task('Finding the most popular stories in your niche…', async () => {
  renderStoryResearch(await call('story_research'));
  log('Story research ready');
});

function newSeries(idea) {
  return task('Creating the series and its characters…', async () => {
    const se = await call('story_new', { idea, style: $('#stStyle').value });
    toast(`Series “${se.title}” created. Press Make next part.`);
    $('#stIdea').value = '';
    await loadStories();
  });
}
$('#btnStNew').onclick = () => {
  const idea = $('#stIdea').value.trim();
  if (!idea) return toast('Describe the story idea first, or pick one from Find popular stories', true);
  newSeries(idea);
};

function partState(p) {
  const j = jobs.find((x) => x.id === p.job_id);
  if (!j) return '<span class="badge">deleted</span>';
  return `<span class="badge ${esc(j.status)}">${esc(j.status === 'uploaded' ? 'live' : j.status === 'draft' ? 'ready' : j.status)}</span>`;
}
function renderSeries() {
  const list = arr(story.series);
  $('#stSeries').innerHTML = list.length ? list.map((se) => {
    const done = arr(se.parts).length >= (se.planned || 1);
    return `<div class="card series" data-id="${esc(se.id)}">
      <div class="row between"><div><h3 style="margin:0">${esc(se.title)}</h3>
        <div class="muted small">${esc(se.style === 'slow' ? 'Slow · AI pictures' : 'Fast · drawn with code')} · ${arr(se.parts).length} of ${num(se.planned)} parts</div></div>
        <div class="row">${done ? '<span class="badge good">Story finished</span>' : `<button class="primary" data-sa="next">Make next part</button><button data-sa="rest">Make all ${se.planned - arr(se.parts).length} left</button>`}
        <button class="ghost danger" data-sa="del">Delete</button></div></div>
      <p>${esc(se.premise)}</p>
      <div class="row wrap">${arr(se.characters).map((c) => `<span class="badge" style="border-left:6px solid ${esc(c.caption)}">${esc(c.name)} · ${esc(se.style === 'slow' ? 'AI picture' : c.kind.replace(/_/g, ' '))} · ${esc(c.voice)}</span>`).join('')}</div>
      ${arr(se.parts).length ? `<ol class="parts">${arr(se.parts).map((p) => `<li>${partState(p)} <b>${esc(p.title)}</b><div class="muted small">${esc(p.summary)}</div></li>`).join('')}</ol>` : ''}
    </div>`;
  }).join('') : '<div class="empty">No series yet. Find popular stories or type your own idea above.</div>';
}
$('#stSeries').onclick = async (e) => {
  const b = e.target.closest('[data-sa]'), card = e.target.closest('.series');
  if (!b || !card) return;
  const se = arr(story.series).find((x) => x.id === card.dataset.id);
  if (b.dataset.sa === 'del') {
    if (!b.dataset.armed) {
      b.dataset.armed = '1';
      b.textContent = 'Sure? Parts stay in the Library';
      return setTimeout(() => { delete b.dataset.armed; b.textContent = 'Delete'; }, 3000);
    }
    await call('story_delete', { id: se.id });
    return loadStories();
  }
  const count = b.dataset.sa === 'next' ? 1 : se.planned - arr(se.parts).length;
  task(`Making ${count} part(s) of “${se.title}”…`, async () => {
    const made = await call('story_make', { series: se.id, count, autoUpload: $('#stAuto').checked });
    toast(`${made} part(s) made. Review them in the Library.`);
    await loadJobs();
    loadStories();
  });
};

/* ---------- new channel wizard ---------- */
let wzPicked = null;
async function initWizard() {
  $('#wzWriter').innerHTML = '<option value="">My current AI (sidebar)</option>' + WRITERS.map((w) => `<option value="${w.v}">${w.name}</option>`).join('');
  $('#wzImage').value = settings.image_model || 'flux';
  await renderImageState();
  const kit = await call('pending_kit', {}, { quiet: true }).catch(() => null);
  if (kit) {
    // A designed channel is waiting to be created and linked.
    brand = kit;
    brand.pending = true;
    $('#rbNiche').value = kit.niche || '';
    renderBrand();
  }
}
async function renderImageState() {
  const st = await call('flux_status', {}, { quiet: true }).catch(() => ({}));
  const txt = st.ready ? '✓ FLUX installed' : 'FLUX not installed yet';
  $('#wzImgState').textContent = txt;
  $('#imgState').textContent = txt;
  $('#btnFluxSetup').classList.toggle('hidden', !!st.ready || $('#wzImage').value !== 'flux');
  $('#btnFluxSetup2').disabled = !!st.ready;
  if (st.ready) $('#btnFluxSetup2').textContent = 'FLUX installed ✓';
}
$('#wzImage').onchange = renderImageState;
const setupFlux = () => {
  toast('Downloading FLUX (~10 GB) in the background. You can keep working; progress is in the activity bar.');
  task('Setting up local FLUX…', async () => {
    await call('setup_flux');
    toast('FLUX is ready: AI images are now free and made on your GPU.');
    renderImageState();
  });
};
$('#btnFluxSetup').onclick = setupFlux;
$('#btnFluxSetup2').onclick = setupFlux;

$('#btnNiches').onclick = () => task('Finding the best niches…', async () => {
  const r = await call('niche_finder', { writer: $('#wzWriter').value, story: $('#wzKind').value === 'story' });
  const list = arr(r.niches);
  const lvl = { low: 'good', medium: 'mid', high: 'bad' }, rpmLvl = { high: 'good', medium: 'mid', low: 'bad' };
  $('#wzNiches').innerHTML = list.map((n, i) => {
    const e = n.evidence || {};
    return `<div class="niche ${i === 0 ? 'picked' : ''}" data-i="${i}"><div class="row between"><b>${esc(n.name)}</b>${i === 0 ? '<span class="badge good">Top pick</span>' : ''}</div>
      <div class="score"><b>${num(n.score)}</b><div class="meter big"><i style="width:${Math.min(100, n.score || 0)}%"></i></div><span class="muted small">success score</span></div>
      <p><b>${esc(n.verdict)}</b> ${esc(n.why)}</p>
      <div class="row wrap"><span class="badge ${lvl[n.competition] || ''}">competition ${esc(n.competition)}</span><span class="badge ${rpmLvl[n.rpm] || ''}">RPM ${esc(n.rpm)}</span><span class="badge ${n.trend === 'rising' ? 'good' : n.trend === 'fading' ? 'bad' : 'mid'}">${esc(n.trend)}</span></div>
      ${e.niche ? `<div class="muted small">This month: ${num(e.median_top10_views)} median views on top Shorts · best ${num(e.best_views)} · ${num(e.over_100k)} over 100K</div>` : ''}
      <ul>${arr(n.first_videos).map((x) => `<li>${esc(x)}</li>`).join('')}</ul>
      <button data-pick="${i}">Choose this niche</button></div>`;
  }).join('') + (r.live_data ? '' : '<p class="note">Scored without live YouTube data (link a channel for real numbers).</p>');
  if (list[0]) $('#rbNiche').value = list[0].name;
  $$('#wzNiches [data-pick]').forEach((b) => (b.onclick = () => {
    $('#rbNiche').value = list[b.dataset.pick].name;
    $$('#wzNiches .niche').forEach((c) => c.classList.toggle('picked', c.dataset.i === b.dataset.pick));
    toast(`Niche: ${list[b.dataset.pick].name}. Now press Design my new channel.`);
  }));
  log('Niche scores ready');
});

$('#btnDesignNew').onclick = () => {
  const niche = $('#rbNiche').value.trim();
  if (!niche) return toast('Pick or type a niche first', true);
  task(`Designing your new “${niche}” channel…`, async () => {
    brand = await call('rebrand', { niche, writer: $('#wzWriter').value, imageModel: $('#wzImage').value, pending: true, story: $('#wzKind').value === 'story' });
    brand.pending = true;
    renderBrand();
  });
};
$('#btnLinkNew').onclick = async () => {
  if (!settings.google_client_id || !settings.google_client_secret) {
    toast('Add your Google OAuth client ID and secret in Settings first (one time)', true);
    return show('settings');
  }
  toast('Sign in in your browser and pick your NEW channel…');
  try {
    const name = await call('yt_connect');
    await task(`Setting up ${name}…`, async () => {
      const msg = await call('launch_channel');
      toast(msg);
    });
    location.reload();
  } catch {}
};

/* ---------- ⚡ make 3 now (per channel) ---------- */
let quickIds = [];
function openQuick() {
  const ch = [...$('#chanSel').options].find((o) => o.value === settings.active_channel);
  $('#qTitle').textContent = `⚡ Make 3 shorts now${ch && ch.value ? ` for ${ch.textContent.replace('📺 ', '')}` : ''}`;
  $('#qStart').classList.remove('hidden');
  $('#qReview').classList.add('hidden');
  $('#quickModal').classList.remove('hidden');
}
$('#qClose').onclick = () => $('#quickModal').classList.add('hidden');
$('#qLater').onclick = () => {
  $('#quickModal').classList.add('hidden');
  toast('Kept as drafts in the Library.');
};
$('#qGo').onclick = () => {
  const auto = $('#qAuto').checked;
  $('#quickModal').classList.add('hidden');
  task(`⚡ Making 3 shorts${auto ? ' and uploading them' : ''}…`, async () => {
    quickIds = arr(await call('quick_batch', { count: 3, autoUpload: auto }));
    await loadJobs();
    if (auto) return toast(`Done: ${quickIds.length} shorts made and uploaded.`);
    renderQuickReview();
  });
};
function renderQuickReview() {
  const items = quickIds.map((id) => jobs.find((j) => j.id === id)).filter(Boolean);
  $('#qNote').textContent = items.length ? 'Review them, then publish the ones you like.' : 'Nothing was made (see the activity log).';
  $('#qItems').innerHTML = items.map((j) => `<div class="qi" data-id="${esc(j.id)}">${j.video ? `<video src="${esc(convertFileSrc(j.video))}" controls playsinline></video>` : `<div class="muted small">${esc(j.error || j.status)}</div>`}
    <b>${esc(j.title)}</b><div class="row">${j.video_id ? '<span class="badge uploaded">uploaded</span>' : `${j.video ? '<button class="primary" data-q="pub">Publish</button>' : ''}<button class="danger" data-q="del">Discard</button>`}</div></div>`).join('');
  $('#qStart').classList.add('hidden');
  $('#qReview').classList.remove('hidden');
  $('#quickModal').classList.remove('hidden');
}
$('#qItems').onclick = async (e) => {
  const b = e.target.closest('[data-q]'), card = e.target.closest('.qi');
  if (!b || !card) return;
  const id = card.dataset.id;
  if (b.dataset.q === 'pub') await call('upload', { items: [{ id, privacy: 'public', publish_at: null }] }).catch(() => {});
  else await call('delete_job', { id }).catch(() => {});
  if (b.dataset.q === 'del') quickIds = quickIds.filter((x) => x !== id);
  await loadJobs();
  renderQuickReview();
};
$('#qPublishAll').onclick = async () => {
  const items = quickIds.map((id) => jobs.find((j) => j.id === id)).filter((j) => j && j.video && !j.video_id);
  if (!items.length) return toast('Nothing left to publish');
  await call('upload', { items: items.map((j) => ({ id: j.id, privacy: 'public', publish_at: null })) }).catch(() => {});
  await loadJobs();
  renderQuickReview();
  toast('Published');
};

/* ---------- update notice (GitHub releases) ---------- */
async function checkUpdate() {
  const u = await call('check_update', {}, { quiet: true }).catch(() => null);
  if (!u || !u.newer) return;
  const bar = document.createElement('div');
  bar.className = 'toast';
  bar.innerHTML = `⬆️ AIYT Studio ${esc(u.latest)} is out (you have ${esc(u.current)}). <a data-link="${esc(u.url)}">Download it</a>`;
  $('#toasts').append(bar);
  setTimeout(() => bar.remove(), 20000);
}

/* ---------- upload limit status (Library) ---------- */
async function renderCap() {
  const u = await call('upload_status', {}, { quiet: true }).catch(() => null);
  if (!u) return;
  const capTxt = u.cap ? ` of ~${u.cap}` : '';
  $('#capInfo').textContent = u.blocked_until > u.now
    ? `⏸ Daily upload limit reached${u.cap ? ` (~${u.cap}/day)` : ''}: ${u.queued} queued, uploads resume ${fmt(new Date(u.blocked_until * 1000))} · `
    : `Uploads today: ${u.today}${capTxt}${u.queued ? ` · ${u.queued} queued` : ''} · `;
}

/* ---------- start ---------- */
(async () => {
  settings = await call('get_settings');
  bindSettings();
  $('#apStart').value = $('#libStart').value = defaultStart();
  renderSlots();
  renderComments();
  setClipSrc('youtube');
  loadKokoroVoices();
  loadPiperVoices();
  loadAuto();
  ensureCtaOption();
  loadSmartPicks();
  renderGpu();
  renderTaskModels();
  showAbout();
  await refreshChannel();
  loadChannels();
  showSetupPrompt();
  initWizard();
  checkUpdate();
  if (sessionStorage.getItem('aiyt-quick')) {
    sessionStorage.removeItem('aiyt-quick');
    openQuick();
  }
  renderSwap(); // percentages fill in once usage loads
  loadLive();
  loadJobs();
  loadUsage(false);
  setInterval(() => loadUsage(false), 180000);
})();
