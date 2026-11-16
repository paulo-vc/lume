// Lume - interface. Captura, encoder e envio são nativos (Rust); aqui ficam a sala e quem assiste.
// Sala: cada convidado tem uma conexão com o anfitrião (vídeo do anfitrião + canal de controle).
// Quando outro participante transmite, quem assiste abre uma conexão direta com ele; a oferta
// e a resposta dessa conexão viajam pelo canal de controle, com o anfitrião repassando.
'use strict';

const invoke = window.__TAURI__ ? window.__TAURI__.core.invoke : async () => { throw 'Rode dentro do app' };
const listen = window.__TAURI__ ? window.__TAURI__.event.listen : () => {};
const $ = id => document.getElementById(id);
const ICE = [{ urls: ['stun:stun.l.google.com:19302', 'stun:stun.cloudflare.com:3478'] }];
const HOST = '@host', SELF = '@me';

const S = {
  role: null,            // 'host' | 'guest'
  room: null,            // {name, code} (o convidado só conhece o nome)
  me: null,              // meu id na sala ('@host' para o anfitrião)
  peers: new Map(),      // anfitrião: id -> {state, live, open, av, q}
  members: [],           // convidado: lista que o anfitrião manda [{id, name, host, live}]
  avs: new Map(),        // id -> foto
  reqs: new Map(),       // anfitrião: pedidos pendentes id -> {id, name, av}
  streams: new Map(),    // telas na grade: id -> {pc, ms, el, ...}
  failed: new Map(),     // id -> quando a conexão com essa tela falhou (para tentar de novo depois)
  focus: null,
  sharing: null,         // título do que eu estou transmitindo
  guestPc: null, guestDc: null, hostMs: null,
  q: { res: 1080, fps: 60, pri: 'motion', br: 8 },
  enc: null,
};

// ---------- utilidades ----------
function toast(msg, ms = 2600) { const t = $('toast'); t.textContent = msg; t.classList.add('show'); clearTimeout(t._h); t._h = setTimeout(() => t.classList.remove('show'), ms); }
function initials(n) { return (n || '?').trim().split(/\s+/).map(p => p[0]).slice(0, 2).join('').toUpperCase() || '?'; }
function myName() { return $('meName').value.trim() || 'Convidado'; }
async function copy(text) { try { await navigator.clipboard.writeText(text); toast('Copiado'); } catch { toast('Não foi possível copiar'); } }
function esc(s) { return String(s).replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c])); }
// Foto de perfil: só aceita imagem pequena embutida (nada de endereço externo).
function okAv(a) { return typeof a === 'string' && a.length <= 24000 && /^data:image\/(jpeg|png|webp);base64,[A-Za-z0-9+/=]+$/.test(a) ? a : ''; }
function avInner(name, av) { return okAv(av) ? `<img src="${av}" alt="">` : esc(initials(name)); }

function waitIce(pc, ms = 4000) {
  if (pc.iceGatheringState === 'complete') return Promise.resolve();
  return new Promise(res => {
    const t = setTimeout(res, ms);
    pc.addEventListener('icegatheringstatechange', () => { if (pc.iceGatheringState === 'complete') { clearTimeout(t); res(); } });
  });
}

function preferH264(transceiver) {
  const caps = RTCRtpReceiver.getCapabilities && RTCRtpReceiver.getCapabilities('video');
  if (!caps || !transceiver.setCodecPreferences) return;
  const h264 = caps.codecs.filter(c => /h264/i.test(c.mimeType));
  const rest = caps.codecs.filter(c => !/h264/i.test(c.mimeType));
  if (h264.length) transceiver.setCodecPreferences([...h264, ...rest]);
}

// Conexão só para assistir (vídeo + áudio).
function recvPc() {
  const pc = new RTCPeerConnection({ iceServers: ICE });
  preferH264(pc.addTransceiver('video', { direction: 'recvonly' }));
  pc.addTransceiver('audio', { direction: 'recvonly' });
  return pc;
}
function tuneReceiver(r) { if ('jitterBufferTarget' in r) r.jitterBufferTarget = 40; } // ms, buffer curto

// ---------- perfil (nome e foto ficam só neste PC) ----------
let myAv = '';
try {
  $('meName').value = localStorage.getItem('sl.name') || '';
  myAv = okAv(localStorage.getItem('sl.av') || '');
  document.documentElement.dataset.theme = localStorage.getItem('sl.theme') || 'light';
} catch {}
function renderMe() { $('meAv').innerHTML = avInner($('meName').value, myAv); $('avDelWrap').hidden = !myAv; }
function setAv(a) {
  myAv = okAv(a);
  try { myAv ? localStorage.setItem('sl.av', myAv) : localStorage.removeItem('sl.av'); } catch {}
  renderMe();
  if (S.role === 'host') { S.avs.set(HOST, myAv); broadcast({ t: 'av', id: HOST, av: myAv }); }
  if (S.role === 'guest') sendHost({ t: 'av', av: myAv });
  renderPeers(); labelTiles();
}
$('meName').oninput = () => { renderMe(); try { localStorage.setItem('sl.name', $('meName').value); } catch {} };
$('meAv').onclick = $('avPick').onclick = () => $('avFile').click();
$('avDel').onclick = () => setAv('');
$('avFile').onchange = async () => {
  const f = $('avFile').files[0]; $('avFile').value = '';
  if (!f) return;
  try {
    // Recorta um quadrado no centro e reduz para 96 px: fica com poucos kB e viaja junto do pedido de entrada.
    const img = await createImageBitmap(f);
    const side = Math.min(img.width, img.height);
    const c = document.createElement('canvas'); c.width = c.height = 96;
    c.getContext('2d').drawImage(img, (img.width - side) / 2, (img.height - side) / 2, side, side, 0, 0, 96, 96);
    let out = c.toDataURL('image/jpeg', 0.82);
    if (out.length > 24000) out = c.toDataURL('image/jpeg', 0.5);
    if (!okAv(out)) throw 0;
    setAv(out);
  } catch { toast('Não consegui abrir essa imagem'); }
};
$('theme').onclick = () => { const d = document.documentElement; d.dataset.theme = d.dataset.theme === 'dark' ? 'light' : 'dark'; try { localStorage.setItem('sl.theme', d.dataset.theme); } catch {} };
renderMe();

// ---------- quem está na sala ----------
function roster() {
  if (S.role === 'host') {
    const list = [{ id: HOST, name: myName(), host: true, live: !!S.sharing }];
    for (const [id, p] of S.peers) list.push({ id, name: id, live: !!p.live, state: p.state, open: p.open });
    return list;
  }
  return S.role === 'guest' ? S.members : [];
}
function nameOf(id) { if (id === SELF || id === S.me) return 'Você'; const m = roster().find(m => m.id === id); return m ? m.name : id; }
function avOf(id) { return id === SELF || id === S.me ? myAv : S.avs.get(id) || ''; }

// ---------- estado visual ----------
function render() {
  document.body.className = S.role || '';
  const inRoom = !!S.role;
  $('empty').hidden = inRoom; $('roomView').hidden = !inRoom;
  $('roomDot').className = 'dot' + (inRoom ? ' live' : '');
  if (S.role === 'host') {
    $('title').textContent = S.room.name;
    $('subtitle').textContent = `Você é o anfitrião · código da sala: ${S.room.code}`;
    $('roomLabel').textContent = S.room.name;
  } else if (S.role === 'guest') {
    $('title').textContent = S.room ? S.room.name : 'Sala';
    $('subtitle').textContent = 'Conexões diretas entre os participantes · criptografado';
    $('roomLabel').textContent = S.room ? S.room.name : 'Sala';
  } else {
    $('title').textContent = 'Lume'; $('subtitle').textContent = 'Crie uma sala ou entre com um código';
    $('roomLabel').textContent = 'Nenhuma sala';
  }
  renderPeers(); renderReqs();
}

const STATE_PT = { connected: '', connecting: 'conectando', new: 'aguardando', disconnected: 'instável', failed: 'falhou', closed: 'saiu' };
function renderPeers() {
  const list = roster();
  $('peers').innerHTML = list.map(m => {
    const mine = m.id === S.me;
    const tag = m.host ? 'anfitrião' : m.state ? (STATE_PT[m.state] ?? m.state) : '';
    return `<div class="peer"><div class="av">${avInner(m.name, avOf(m.id))}</div><span class="pn">${esc(m.name)}${mine ? ' (você)' : ''}</span>
      ${m.live ? '<span class="live">● AO VIVO</span>' : ''}<small>${esc(tag)}</small>
      ${S.role === 'host' && !mine ? `<button class="kick" data-kick="${esc(m.id)}" title="Remover da sala">✕</button>` : ''}</div>`;
  }).join('');
  $('pCount').textContent = `${list.length} / 8`; $('roomCount').textContent = list.length;
}

// Anfitrião remove alguém da sala (libera a vaga; a pessoa precisa pedir para entrar de novo).
$('peers').onclick = e => {
  const b = e.target.closest('[data-kick]'); if (!b) return;
  dropPeer(b.dataset.kick, 'foi removido da sala');
};
function dropPeer(id, why) {
  if (!S.peers.delete(id)) return;
  S.avs.delete(id);
  invoke('remove_peer', { name: id }).catch(() => {});
  if (why) toast(`${id} ${why}`);
  pushRoster();
}

// ---------- abas Participantes / Pedidos ----------
function showTab(reqs) {
  $('tabPeers').classList.toggle('on', !reqs); $('tabReqs').classList.toggle('on', reqs);
  $('peers').hidden = reqs; $('reqList').hidden = !reqs;
}
$('tabPeers').onclick = () => showTab(false);
$('tabReqs').onclick = $('reqItem').onclick = () => showTab(true);

// ---------- modais ----------
const modal = $('modal'), sheet = $('sheet');
modal.onclick = e => { if (e.target === modal) closeModal(); };
function closeModal() { modal.classList.remove('show'); }
document.addEventListener('click', e => { const o = e.target.closest('[data-open]'); if (o) openModal(o.dataset.open); });
document.addEventListener('keydown', e => { if (e.key === 'Escape') closeModal(); });

function openModal(kind) {
  if (kind === 'new') {
    if (S.role) return toast('Saia da sala atual primeiro');
    sheet.innerHTML = `<h2>Nova sala</h2><p>Você será o anfitrião. A sala existe só enquanto este app estiver aberto e ninguém entra sem você aceitar.</p>
      <label>Nome da sala</label><input class="field" id="fRoom" value="Sala de ${esc(myName())}" maxlength="40">
      <div class="act"><button class="btn ghost" id="x">Cancelar</button><button class="btn pri" id="ok">Criar sala</button></div>`;
    $('x').onclick = closeModal;
    $('ok').onclick = async () => {
      try { S.room = await invoke('create_room', { name: $('fRoom').value.trim() || 'Sala' }); }
      catch (err) { return toast(String(err)); }
      S.role = 'host'; S.me = HOST; S.avs.set(HOST, myAv); render(); openModal('invite');
    };
  }
  if (kind === 'invite') {
    if (S.role !== 'host') return;
    sheet.innerHTML = `<h2>Código da sala</h2><p>Mande este código para quem você quer convidar. A pessoa digita no Lume dela e você só clica em Aceitar.</p>
      <div class="bigcode" id="inv">${esc(S.room.code)}</div>
      <div class="mute" id="relay" style="text-align:center;font-size:12px;margin-top:8px"></div>
      <div class="act"><button class="btn ghost" id="rot">Trocar código</button><button class="btn ghost" id="x">Fechar</button><button class="btn pri" id="cp">Copiar código</button></div>`;
    $('x').onclick = closeModal;
    $('cp').onclick = () => copy(S.room.code);
    $('rot').onclick = async () => { S.room = await invoke('new_code'); S.reqs.clear(); $('reqs').innerHTML = ''; render(); openModal('invite'); toast('O código antigo parou de funcionar'); };
    const check = async () => {
      if (!$('relay')) return;
      const n = await invoke('relays_online').catch(() => 0);
      $('relay').textContent = n ? '● Sala aberta para pedidos de entrada' : 'Conectando aos servidores de encontro…';
      $('relay').style.color = n ? 'var(--good)' : '';
      if (!n) setTimeout(check, 1000);
    };
    check();
  }
  if (kind === 'join') {
    if (S.role) return toast('Saia da sala atual primeiro');
    sheet.innerHTML = `<h2>Entrar numa sala</h2><p>Digite o código que o anfitrião te mandou.</p>
      <label>Seu nome</label><input class="field" id="jName" value="${esc(myName())}" maxlength="32">
      <label>Código da sala</label><input class="field codein" id="jCode" placeholder="K7QM-2XPD-9RTA" maxlength="20" spellcheck="false" autocomplete="off">
      <div class="mute" id="jStatus" style="font-size:12px;margin-top:8px;min-height:16px"></div>
      <div class="act"><button class="btn ghost" id="x">Cancelar</button><button class="btn pri" id="go">Entrar</button></div>`;
    $('x').onclick = () => { invoke('cancel_join').catch(() => {}); if (S.guestPc && S.role !== 'guest') { S.guestPc.close(); S.guestPc = null; } closeModal(); };
    $('go').onclick = guestStart;
    $('jCode').onkeydown = e => { if (e.key === 'Enter') guestStart(); };
    setTimeout(() => $('jCode') && $('jCode').focus(), 50);
  }
  modal.classList.add('show');
}

// ---------- pedidos de entrada (anfitrião) ----------
// Cada pedido aparece como aviso no canto e fica na aba Pedidos até ser aceito, recusado ou a pessoa desistir.
function renderReqs() {
  const n = S.reqs.size;
  for (const id of ['rCount', 'rCount2']) { $(id).textContent = n; $(id).classList.toggle('hot', n > 0); }
  $('reqList').innerHTML = n ? [...S.reqs.values()].map(r => `<div class="reqrow" data-id="${esc(r.id)}"><div class="av">${avInner(r.name, r.av)}</div><span class="pn">${esc(r.name)}</span>
    <button class="btn ghost" data-a="no">Recusar</button><button class="btn pri" data-a="yes">Aceitar</button></div>`).join('')
    : '<div class="mute" style="padding:8px 0">Nenhum pedido pendente. Quem digitar o código da sala aparece aqui.</div>';
}
function forgetReq(id) {
  S.reqs.delete(id);
  document.querySelectorAll('.reqcard').forEach(el => { if (el.dataset.id === id) el.remove(); });
  renderReqs();
}
async function answerReq(id, yes, btn) {
  const r = S.reqs.get(id); if (!r) return;
  if (!yes) { invoke('deny_request', { id }).catch(() => {}); return forgetReq(id); }
  if (r.busy) return;
  r.busy = true;
  if (btn) { btn.disabled = true; btn.textContent = 'Conectando…'; }
  try {
    const who = await invoke('approve_request', { id });
    S.peers.set(who, { state: 'connecting', live: false, open: false, q: Promise.resolve() });
    if (okAv(r.av)) S.avs.set(who, r.av);
    renderPeers();
  } catch (err) { toast(String(err)); }
  forgetReq(id);
}
function onReqClick(e) {
  const b = e.target.closest('[data-a]'); if (!b) return;
  const row = b.closest('[data-id]');
  if (b.dataset.a === 'hide') return row.remove(); // só fecha o aviso; o pedido continua na aba
  answerReq(row.dataset.id, b.dataset.a === 'yes', b);
}
$('reqs').onclick = onReqClick; $('reqList').onclick = onReqClick;

listen('join-request', ({ payload: r }) => {
  if (S.role !== 'host' || S.reqs.has(r.id)) return;
  r.av = okAv(r.av); S.reqs.set(r.id, r); renderReqs();
  const el = document.createElement('div');
  el.className = 'reqcard'; el.dataset.id = r.id;
  el.innerHTML = `<div class="av">${avInner(r.name, r.av)}</div><div class="who"><b>${esc(r.name)}</b><small>quer entrar na sala</small></div>
    <button class="btn ghost" data-a="no">Recusar</button><button class="btn pri" data-a="yes">Aceitar</button><button class="x" data-a="hide" title="Decidir depois">✕</button>`;
  $('reqs').appendChild(el);
  if (modal.classList.contains('show') && $('inv')) closeModal();
});
// Quem desistiu de esperar some da lista.
setInterval(async () => {
  if (S.role !== 'host' || !S.reqs.size) return;
  const alive = await invoke('list_requests').catch(() => null);
  if (alive) for (const id of [...S.reqs.keys()]) if (!alive.includes(id) && !S.reqs.get(id).busy) forgetReq(id);
}, 3000);

// ---------- recados da sala ----------
// Anfitrião -> convidado. Os envios de cada pessoa saem em fila, na ordem.
function sendPeer(id, obj) {
  const p = S.peers.get(id); if (!p || !p.open) return;
  const text = JSON.stringify(obj);
  p.q = p.q.then(() => invoke('ctl_send', { name: id, text })).catch(() => {});
}
function broadcast(obj, except) { for (const id of S.peers.keys()) if (id !== except) sendPeer(id, obj); }
function sendHost(obj) { if (S.guestDc && S.guestDc.readyState === 'open') S.guestDc.send(JSON.stringify(obj)); }
function pushRoster() {
  if (S.role !== 'host') return;
  broadcast({ t: 'roster', members: roster().filter(m => m.host || m.open).map(({ id, name, host, live }) => ({ id, name, host: !!host, live })) });
  refresh();
}
function refresh() { renderPeers(); syncStreams(); }

listen('ctl-open', ({ payload: id }) => {
  const p = S.peers.get(id); if (S.role !== 'host' || !p || p.open) return;
  p.open = true;
  sendPeer(id, { t: 'hello', you: id, room: S.room.name });
  for (const [who, av] of S.avs) if (av && who !== id) sendPeer(id, { t: 'av', id: who, av });
  if (S.avs.get(id)) broadcast({ t: 'av', id, av: S.avs.get(id) }, id);
  pushRoster();
});
listen('ctl-msg', ({ payload: [id, text] }) => {
  const p = S.peers.get(id); if (S.role !== 'host' || !p) return;
  let m; try { m = JSON.parse(text); } catch { return; }
  if (m.t === 'live') { p.live = !!m.on; pushRoster(); }
  if (m.t === 'bye') dropPeer(id, 'saiu');
  if (m.t === 'av') { const av = okAv(m.av); S.avs.set(id, av); broadcast({ t: 'av', id, av }, id); renderPeers(); labelTiles(); }
  if (m.t === 'sig') {
    m.from = id; // quem mandou é quem o canal diz que é, não o que veio escrito
    if (m.to === HOST) onSig(m); else if (S.peers.has(m.to)) sendPeer(m.to, m);
  }
});
listen('peer-state', ({ payload: [id, st] }) => {
  if (S.role !== 'host') return; // no convidado esses avisos são de quem assiste a tela dele
  const p = S.peers.get(id); if (!p) return;
  p.state = st;
  if (st === 'connected') toast(`${id} entrou`);
  if (st === 'failed' || st === 'closed') return dropPeer(id, st === 'failed' ? 'não conseguiu conectar' : 'saiu');
  renderPeers();
});

function onHostMsg(text) {
  let m; try { m = JSON.parse(text); } catch { return; }
  if (m.t === 'hello') { S.me = String(m.you); S.room = { name: String(m.room || 'Sala') }; render(); }
  if (m.t === 'roster' && Array.isArray(m.members)) S.members = m.members.slice(0, 16).map(x => ({ id: String(x.id), name: String(x.name), host: !!x.host, live: !!x.live }));
  if (m.t === 'av') { S.avs.set(String(m.id), okAv(m.av)); labelTiles(); }
  if (m.t === 'sig') onSig(m);
  if (S.me) refresh();
}

// Oferta/resposta de uma conexão entre dois participantes.
function sendSig(to, kind, sdp) {
  const m = { t: 'sig', to, kind, sdp };
  if (S.role === 'host') sendPeer(to, { ...m, from: HOST }); else sendHost(m);
}
async function onSig(m) {
  if (m.kind === 'offer') {
    // Alguém quer ver a minha tela. (Do anfitrião os convidados já recebem pela conexão de entrada.)
    if (S.role !== 'guest' || !S.sharing) return;
    try { sendSig(m.from, 'answer', await invoke('accept_viewer', { name: String(m.from), offerSdp: String(m.sdp) })); } catch {}
  }
  if (m.kind === 'answer') {
    const st = S.streams.get(m.from);
    if (st && st.pc && st.pc.signalingState === 'have-local-offer') st.pc.setRemoteDescription({ type: 'answer', sdp: String(m.sdp) }).catch(() => {});
  }
}

// ---------- telas (grade) ----------
function makeTile(id) {
  const el = document.createElement('div');
  el.className = 'tile'; el.dataset.id = id;
  el.innerHTML = id === SELF
    ? `<div class="self"><div><b>Você está transmitindo</b><br><span class="what"></span></div></div><div class="tl"><span class="chip"><span class="dot live"></span><span class="res">—</span></span></div>`
    : `<video autoplay playsinline></video>
       <div class="tl"><div class="av"></div><span class="chip"><span class="who"></span><span class="res mute"></span></span></div>
       <div class="tb"><button data-a="mute" title="Silenciar esta tela">🔊</button><input type="range" min="0" max="100" value="100" title="Volume desta tela">
       <button data-a="focus" title="Destacar">⤢</button><button data-a="fs" title="Tela cheia">⛶</button></div>`;
  const v = el.querySelector('video');
  el.onclick = e => {
    const b = e.target.closest('[data-a]');
    if (e.target.closest('input')) return;
    if (b && b.dataset.a === 'mute') { v.muted = !v.muted; b.classList.toggle('off', v.muted); b.textContent = v.muted ? '🔇' : '🔊'; return; }
    if (b && b.dataset.a === 'fs') return void (document.fullscreenElement ? document.exitFullscreen() : el.requestFullscreen());
    // Clique na tela: destaca (as outras vão para a faixa de baixo). Clique de novo: volta para a grade.
    S.focus = S.focus === id ? null : id; layoutTiles();
  };
  el.ondblclick = e => { if (!e.target.closest('.tb')) document.fullscreenElement ? document.exitFullscreen() : el.requestFullscreen(); };
  const vol = el.querySelector('input');
  if (vol) vol.oninput = () => { v.volume = vol.value / 100; };
  return el;
}
function attach(st) {
  const v = st.el.querySelector('video'); if (!v) return;
  if (v.srcObject !== st.ms) v.srcObject = st.ms;
  v.play().catch(() => {});
}
function labelTiles() {
  for (const [id, st] of S.streams) {
    if (id === SELF) { st.el.querySelector('.what').textContent = S.sharing || ''; continue; }
    st.el.querySelector('.who').textContent = nameOf(id);
    st.el.querySelector('.av').innerHTML = avInner(nameOf(id), avOf(id));
  }
}
function layoutTiles() {
  const box = $('tiles');
  if (S.focus && !S.streams.has(S.focus)) S.focus = null;
  [...box.children].forEach(el => { const st = S.streams.get(el.dataset.id); if (!st || st.el !== el) el.remove(); });
  for (const [id, st] of S.streams) { if (st.el.parentNode !== box) box.appendChild(st.el); st.el.classList.toggle('main', id === S.focus); }
  box.dataset.n = S.streams.size; box.classList.toggle('focus', !!S.focus);
  box.style.setProperty('--k', Math.max(1, S.streams.size - 1));
  // Grade: escolhe o número de colunas que deixa as telas (16:9) maiores no espaço que há.
  let cols = 1, best = 0;
  const n = S.streams.size, w = $('stage').clientWidth, h = $('stage').clientHeight;
  for (let c = 1; c <= n; c++) { const a = Math.min(w / c, (h / Math.ceil(n / c)) * 16 / 9); if (a > best) { best = a; cols = c; } }
  box.style.gridTemplateColumns = S.focus ? '' : `repeat(${cols},minmax(0,1fr))`;
  $('ph').hidden = S.streams.size > 0;
  labelTiles();
}

function closeStream(id) {
  const st = S.streams.get(id); if (!st) return;
  S.streams.delete(id); clearTimeout(st.timer);
  if (st.pc) st.pc.close();
  const v = st.el.querySelector('video'); if (v) v.srcObject = null;
}
function failStream(id, st) {
  if (S.streams.get(id) !== st) return;
  closeStream(id); layoutTiles();
  if (!S.failed.has(id)) toast(`Não foi possível conectar direto com ${nameOf(id)} (rede com NAT restritivo). Tentando de novo…`, 5000);
  S.failed.set(id, Date.now());
}
async function openStream(id) {
  const st = { id, ms: new MediaStream(), pc: null, el: makeTile(id) };
  S.streams.set(id, st);
  // A tela do anfitrião já chega pela conexão de entrada do convidado.
  if (S.role === 'guest' && id === HOST) { st.ms = S.hostMs; st.viaHost = true; attach(st); return; }
  const pc = st.pc = recvPc();
  pc.ontrack = e => { st.ms.addTrack(e.track); tuneReceiver(e.receiver); attach(st); };
  pc.onconnectionstatechange = () => {
    if (pc.connectionState === 'connected') { clearTimeout(st.timer); S.failed.delete(id); }
    if (pc.connectionState === 'failed') failStream(id, st);
  };
  st.timer = setTimeout(() => { if (pc.connectionState !== 'connected') failStream(id, st); }, 25000);
  try {
    await pc.setLocalDescription(await pc.createOffer());
    await waitIce(pc);
    if (S.streams.get(id) === st) sendSig(id, 'offer', pc.localDescription.sdp);
  } catch { failStream(id, st); }
}
// Deixa a grade igual à sala: uma tela para cada pessoa que está transmitindo.
function syncStreams() {
  if (!S.role || !S.me) return;
  const want = new Set(roster().filter(m => m.live && m.id !== S.me && (S.role !== 'host' || m.open)).map(m => m.id));
  for (const id of [...S.streams.keys()]) if (id !== SELF && !want.has(id)) closeStream(id);
  for (const id of [...S.failed.keys()]) if (!want.has(id)) S.failed.delete(id);
  for (const id of want) {
    if (S.streams.has(id)) continue;
    if (Date.now() - (S.failed.get(id) || 0) < 10000) continue; // falhou há pouco: espera antes de tentar de novo
    openStream(id);
  }
  if (S.sharing && !S.streams.has(SELF)) S.streams.set(SELF, { el: makeTile(SELF) });
  if (!S.sharing && S.streams.has(SELF)) closeStream(SELF);
  layoutTiles();
}

// ---------- transmitir a minha tela ----------
listen('enc-stats', ({ payload: s }) => { S.enc = s; });
listen('enc-fallback', () => toast('A placa de vídeo não aceitou codificar; usando a CPU', 4000));

async function openPicker() {
  const [list, procs] = await Promise.all([invoke('list_sources'), invoke('list_processes').catch(() => [])]);
  const item = s => `<div class="src" data-id="${esc(s.id)}" data-title="${esc(s.title)}" data-pid="${s.pid || 0}" data-proc="${esc(s.process || '')}"><span class="ico">${s.kind === 'monitor' ? '🖥' : '▢'}</span>
    <div><b>${esc(s.title)}</b>${s.process ? `<small>${esc(s.process)}</small>` : ''}</div></div>`;
  const call = procs.find(p => /^(discord|discordptb|discordcanary|teamspeak|ts3client_win64|skype|ms-teams|teams|zoom|whatsapp)\.exe$/i.test(p.name));
  sheet.innerHTML = `<h2>O que você quer transmitir?</h2><p>Captura nativa do Windows, sem barra de compartilhamento na tela.</p>
    <label>Telas</label><div class="srcs">${list.filter(s => s.kind === 'monitor').map(item).join('')}</div>
    <label>Janelas</label><div class="srcs">${list.filter(s => s.kind === 'window').map(item).join('') || '<div class="mute">Nenhuma janela aberta</div>'}</div>
    <label>Áudio</label>
    <div class="audio">
      <select id="aMode">
        <option value="app" id="aApp">Só o áudio da janela escolhida</option>
        <option value="except">PC inteiro, menos um programa (a call)</option>
        <option value="all">PC inteiro</option>
        <option value="none">Sem áudio</option>
      </select>
      <select id="aEx">${procs.map(p => `<option value="${p.pid}" ${call && p.pid === call.pid ? 'selected' : ''}>${esc(p.name)}</option>`).join('')}</select>
    </div>
    <div class="mute" id="aHint"></div>
    <div class="act"><button class="btn ghost" id="x">Cancelar</button><button class="btn pri" id="go" disabled>Transmitir</button></div>`;
  $('x').onclick = closeModal;
  let pick = null;
  const sync = () => {
    const win = pick && pick.dataset.id.startsWith('w:');
    $('aApp').disabled = !win;
    if (!win && $('aMode').value === 'app') $('aMode').value = call ? 'except' : 'all';
    $('aEx').hidden = $('aMode').value !== 'except';
    const m = $('aMode').value;
    $('aHint').textContent = m === 'app' ? `Só o som de ${pick.dataset.proc || 'este programa'} vai junto. A call não volta como eco.`
      : m === 'except' ? 'Tudo que toca no PC vai junto, menos o programa escolhido e as telas que você assiste no Lume. Se a voz da call ainda vazar, escolha aqui o programa por onde ela sai (ex.: um mixer como Sonar ou Voicemeeter).'
      : m === 'all' ? 'Atenção: se você estiver numa call, as pessoas vão ouvir a própria voz de volta.' : '';
  };
  sheet.querySelectorAll('.src').forEach(el => el.onclick = () => {
    sheet.querySelectorAll('.src').forEach(x => x.classList.toggle('sel', x === el));
    pick = el; $('go').disabled = false;
    $('aMode').value = el.dataset.id.startsWith('w:') ? 'app' : (call ? 'except' : 'all');
    sync();
  });
  $('aMode').onchange = sync;
  $('go').onclick = async () => {
    const mode = $('aMode').value;
    const pid = mode === 'app' ? Number(pick.dataset.pid) : mode === 'except' ? Number($('aEx').value) : 0;
    try {
      await invoke('set_quality', { quality: S.q });
      await invoke('start_share', { sourceId: pick.dataset.id, title: pick.dataset.title });
      try { await invoke('set_audio', { sel: { mode, pid } }); }
      catch (err) { toast('Vídeo ok, mas o áudio falhou: ' + err); }
      S.sharing = pick.dataset.title; closeModal();
      $('share').textContent = '■ Parar'; $('share').className = 'btn red inRoom';
      announceLive();
    } catch (err) { toast(String(err)); }
  };
  sync();
  modal.classList.add('show');
}
function announceLive() {
  if (S.role === 'host') pushRoster();
  else { sendHost({ t: 'live', on: !!S.sharing }); refresh(); }
}
function stopShare() {
  if (S.sharing) {
    invoke('stop_share').catch(() => {});
    if (S.role === 'guest') invoke('drop_viewers').catch(() => {});
  }
  const was = S.sharing; S.sharing = null; S.enc = null;
  $('share').textContent = '▶ Compartilhar tela'; $('share').className = 'btn pri inRoom';
  if (was) announceLive();
}
$('share').onclick = () => S.sharing ? stopShare() : openPicker();

// ---------- entrar como convidado ----------
async function guestStart() {
  const code = $('jCode').value.trim();
  const name = $('jName').value.trim() || 'Convidado';
  if (code.replace(/[^a-z0-9]/gi, '').length !== 12) return toast('O código tem 12 letras e números');
  $('meName').value = name; $('meName').oninput();
  $('go').disabled = true; $('jCode').disabled = true; $('jName').disabled = true;
  $('jStatus').textContent = 'Preparando a conexão…';
  const pc = recvPc();
  const dc = pc.createDataChannel('ctl');
  const ms = new MediaStream();
  dc.onopen = () => dc.send(JSON.stringify({ t: 'hi' }));
  dc.onmessage = e => { if (S.guestPc === pc && typeof e.data === 'string') onHostMsg(e.data); };
  // O canal fecha na hora quando o anfitrião remove a pessoa ou fecha a sala.
  dc.onclose = () => { if (S.role === 'guest' && S.guestPc === pc) { toast('O anfitrião encerrou a sua conexão', 5000); leave(); } };
  pc.ontrack = e => { ms.addTrack(e.track); tuneReceiver(e.receiver); const st = S.streams.get(HOST); if (st) attach(st); };
  pc.onconnectionstatechange = () => {
    if (pc.connectionState === 'connected') { toast('Conectado'); closeModal(); }
    if (pc.connectionState === 'failed') toast('Não foi possível conectar direto (rede com NAT restritivo)', 5000);
    if (['failed', 'closed'].includes(pc.connectionState) && S.role === 'guest' && S.guestPc === pc) leave();
  };
  await pc.setLocalDescription(await pc.createOffer());
  await waitIce(pc);
  S.guestPc = pc; S.guestDc = dc; S.hostMs = ms;
  if ($('jStatus')) $('jStatus').textContent = 'Pedido enviado. Aguardando o anfitrião aceitar…';
  try {
    const sdp = await invoke('join_room', { code, name, av: myAv, offerSdp: pc.localDescription.sdp });
    await pc.setRemoteDescription({ type: 'answer', sdp });
    S.role = 'guest'; render(); closeModal(); toast('Aceito! Conectando…');
  } catch (err) {
    pc.close(); if (S.guestPc === pc) S.guestPc = S.guestDc = S.hostMs = null;
    if (String(err) !== 'Cancelado') toast(String(err), 5000);
    if ($('jStatus')) { $('jStatus').textContent = ''; $('go').disabled = false; $('jCode').disabled = false; $('jName').disabled = false; }
  }
}

// ---------- painel de qualidade (da minha transmissão) ----------
document.querySelectorAll('.seg').forEach(seg => seg.onclick = e => {
  const b = e.target.closest('button'); if (!b) return;
  seg.querySelectorAll('button').forEach(x => x.classList.toggle('on', x === b));
  const k = seg.dataset.k;
  S.q[k] = k === 'pri' ? b.dataset.v : +b.dataset.v;
  qualityChanged();
});
$('br').oninput = () => { S.q.br = +$('br').value; $('brv').textContent = S.q.br + ' Mbps'; };
$('br').onchange = qualityChanged;
function qualityChanged() { invoke('set_quality', { quality: S.q }).catch(err => toast(String(err))); }

// ---------- sair ----------
function leave() {
  const was = S.role;
  if (was === 'guest') sendHost({ t: 'bye' });
  stopShare();
  for (const id of [...S.streams.keys()]) closeStream(id);
  if (was) invoke('close_room').catch(() => {});
  $('reqs').innerHTML = '';
  S.peers.clear(); S.members = []; S.avs.clear(); S.reqs.clear(); S.failed.clear();
  if (S.guestPc) S.guestPc.close();
  S.guestPc = S.guestDc = S.hostMs = null; S.role = null; S.room = null; S.me = null; S.focus = null;
  showTab(false); layoutTiles(); render();
}
$('leave').onclick = leave;

// ---------- estatísticas (1x por segundo) ----------
async function readStats(pc, prev) {
  const r = await pc.getStats(); const out = { t: performance.now() };
  r.forEach(s => {
    if (s.type === 'candidate-pair' && s.nominated && s.state === 'succeeded') out.rtt = s.currentRoundTripTime;
    if (s.type === 'inbound-rtp' && s.kind === 'video') { out.bytes = s.bytesReceived; out.fps = s.framesPerSecond; out.w = s.frameWidth; out.h = s.frameHeight; out.lost = s.packetsLost; out.recv = s.packetsReceived; }
  });
  if (prev && out.bytes != null && prev.bytes != null) out.kbps = (out.bytes - prev.bytes) * 8 / (out.t - prev.t);
  return out;
}

setInterval(async () => {
  if (!S.role) return;
  syncStreams(); // também tenta de novo as telas que falharam
  for (const [id, st] of S.streams) {
    const res = st.el.querySelector('.res');
    if (id === SELF) { res.textContent = S.enc ? `${S.enc.width}×${S.enc.height} · ${Math.round(S.enc.fps)} fps · ${S.enc.encoder}` : 'iniciando…'; continue; }
    const pc = st.viaHost ? S.guestPc : st.pc; if (!pc) continue;
    st.stats = await readStats(pc, st.stats).catch(() => st.stats);
    const s = st.stats;
    res.textContent = s && s.w ? ` · ${s.w}×${s.h} · ${Math.round(s.fps || 0)} fps` : ' · conectando…';
  }
  // O painel Conexão mostra a tela destacada (ou a primeira que estou assistindo).
  const watched = [...S.streams].filter(([id]) => id !== SELF);
  const cur = (watched.find(([id]) => id === S.focus) || watched[0] || [])[1];
  const s = cur && cur.stats;
  const n = S.role === 'host' ? [...S.peers.values()].filter(p => p.state === 'connected').length : S.members.length - 1;
  const ok = S.role === 'host' ? n > 0 : S.guestPc && S.guestPc.connectionState === 'connected';
  $('connState').textContent = S.role === 'host' ? (n ? `● ${n} conectado${n > 1 ? 's' : ''}` : 'ninguém conectado') : ok ? '● direta' : 'conectando…';
  $('connState').style.color = ok ? 'var(--good)' : '';
  const lat = s && s.rtt != null ? Math.round(s.rtt * 1000 / 2 + 30) : null; // ~ rede (meio RTT) + captura/encode/decode
  $('sLat').textContent = lat != null ? lat + ' ms' : '—';
  const kbps = s && s.kbps != null ? s.kbps : S.enc ? S.enc.kbps : null;
  $('sBr').textContent = kbps != null ? (kbps / 1000).toFixed(1) + ' M' : '—';
  const loss = s && s.recv ? 100 * s.lost / (s.lost + s.recv) : null;
  $('sLoss').textContent = loss != null ? loss.toFixed(1) + '%' : '—';
}, 1000);

window.addEventListener('resize', layoutTiles);
window.addEventListener('beforeunload', leave);
render();
