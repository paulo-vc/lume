// Lume - interface. Captura, encoder e envio são nativos (Rust); aqui ficam a sala e quem assiste.
// Sala: cada convidado tem uma conexão com o anfitrião (vídeo do anfitrião + canal de controle).
// Quando outro participante transmite, quem assiste abre uma conexão direta com ele; a oferta
// e a resposta dessa conexão viajam pelo canal de controle, com o anfitrião repassando.
'use strict';

// No app, os comandos vão para o Rust; na versão web (web/lume-web.js), para o navegador.
const WEB = !window.__TAURI__ && !!window.LumeWeb;
const invoke = window.__TAURI__ ? window.__TAURI__.core.invoke : WEB ? window.LumeWeb.invoke : async () => { throw 'Rode dentro do app' };
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
function normCode(c) { const s = String(c).toUpperCase().replace(/[^0-9A-Z]/g, '').replace(/O/g, '0').replace(/[IL]/g, '1'); return s.length === 12 ? `${s.slice(0, 4)}-${s.slice(4, 8)}-${s.slice(8)}` : s; }
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
// Id fixo deste PC: o anfitrião reconhece quem já convidou e deixa entrar de novo sem perguntar.
let myUid = '';
try { myUid = localStorage.getItem('sl.uid') || ''; } catch {}
if (!/^[0-9a-f]{32}$/.test(myUid)) { myUid = [...crypto.getRandomValues(new Uint8Array(16))].map(b => b.toString(16).padStart(2, '0')).join(''); try { localStorage.setItem('sl.uid', myUid); } catch {} }

// ---------- salas salvas ----------
// { code, name, role: 'host'|'guest', members: [{uid, name, av}], last }
let rooms = [];
try { rooms = JSON.parse(localStorage.getItem('sl.rooms') || '[]'); if (!Array.isArray(rooms)) rooms = []; } catch { rooms = []; }
function saveRooms() { try { localStorage.setItem('sl.rooms', JSON.stringify(rooms)); } catch {} renderRooms(); }
function roomByCode(code) { return rooms.find(r => r.code === code); }
function rememberRoom(code, name, role) {
  let r = roomByCode(code);
  if (!r) { r = { code, name, role, members: [] }; rooms.unshift(r); }
  r.name = name || r.name; r.role = role; r.last = Date.now();
  saveRooms(); return r;
}
function curSaved() { return S.room && S.room.code ? roomByCode(S.room.code) : null; }
function rememberMember(uid, name, av) {
  const r = curSaved(); if (!r || S.role !== 'host' || !/^[0-9a-f]{32}$/.test(uid || '')) return;
  const m = r.members.find(m => m.uid === uid);
  if (m) { m.name = name; m.av = okAv(av); } else r.members.push({ uid, name, av: okAv(av) });
  saveRooms();
}
function renderRooms() {
  $('roomCount').textContent = rooms.length;
  $('rooms').innerHTML = rooms.length ? rooms.map(r => {
    const cur = S.room && S.room.code === r.code;
    const who = r.role === 'host' ? (r.members.length ? r.members.map(m => m.name).join(', ') : 'Você é o anfitrião') : 'Convidado';
    return `<div class="room${cur ? ' on' : ''}" data-code="${esc(r.code)}" title="${esc(r.code)}"><div class="rav">${esc(initials(r.name))}${cur ? '<span class="dot live"></span>' : ''}</div>
      <div class="rt"><b>${esc(r.name)}</b><small>${esc(who)}</small></div><button class="kick" data-del title="Esquecer esta sala">✕</button></div>`;
  }).join('') : '<div class="roomsEmpty">As salas que você criar ou entrar ficam aqui. Clique numa para voltar a ela.</div>';
}
$('rooms').onclick = e => {
  const el = e.target.closest('.room'); if (!el) return;
  const r = roomByCode(el.dataset.code); if (!r) return;
  if (e.target.closest('[data-del]')) {
    if (S.room && S.room.code === r.code) return toast('Saia da sala antes de esquecê-la');
    rooms = rooms.filter(x => x !== r); saveRooms(); return;
  }
  if (S.room && S.room.code === r.code) return;
  if (S.role) leave();
  if (r.role === 'host') openRoom(r.name, r.code);
  else { openModal('join'); $('jCode').value = r.code; guestStart(); }
};
async function openRoom(name, code) {
  try { S.room = await invoke('create_room', { name, code: code || null }); }
  catch (err) { return toast(String(err)); }
  S.role = 'host'; S.me = HOST; S.avs.set(HOST, myAv);
  rememberRoom(S.room.code, S.room.name, 'host');
  render();
  if (!code) openModal('invite'); else toast('Sala aberta. Quem você já convidou pode entrar com o mesmo código.', 3500);
}
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
$('meName').oninput = () => { renderMe(); if (S.role === 'host') renderPeers(); try { localStorage.setItem('sl.name', $('meName').value); } catch {} };
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
    for (const [id, p] of S.peers) list.push({ id, name: id, live: !!p.live, state: p.state, open: p.open, uid: p.uid });
    const r = curSaved();
    if (r) for (const m of r.members) if (!list.some(x => x.uid === m.uid || x.name === m.name)) list.push({ id: offId(r, m), name: m.name, off: true, uid: m.uid });
    return list;
  }
  return S.role === 'guest' ? S.members : [];
}
function nameOf(id) { if (id === SELF || id === S.me) return 'Você'; const m = roster().find(m => m.id === id); return m ? m.name : id; }
// Id de quem está fora da sala na lista que vai para os convidados: não expõe o uid (que libera a entrada direta).
function offId(r, m) { return 'off:' + r.members.indexOf(m); }
function avOf(id) {
  if (id === SELF || id === S.me) return myAv;
  if (S.avs.get(id)) return S.avs.get(id);
  const r = curSaved(), m = r && String(id).startsWith('off:') && r.members.find(m => offId(r, m) === id);
  return m ? m.av : '';
}

// ---------- estado visual ----------
function render() {
  document.body.className = S.role || '';
  const inRoom = !!S.role;
  $('empty').hidden = inRoom; $('roomView').hidden = !inRoom;
  if (S.role === 'host') {
    $('title').textContent = S.room.name;
    $('subtitle').textContent = `Você é o anfitrião · código ${S.room.code}`;
  } else if (S.role === 'guest') {
    $('title').textContent = S.room && S.room.name ? S.room.name : 'Sala';
    $('subtitle').textContent = 'Conexões diretas entre os participantes · criptografado';
  } else {
    $('title').textContent = 'Lume'; $('subtitle').textContent = 'Crie uma sala ou entre com um código';
  }
  renderPeers(); renderReqs(); renderRooms();
}

const STATE_PT = { connecting: 'conectando', new: 'conectando', disconnected: 'instável', failed: 'falhou', closed: 'saiu' };
function renderPeers() {
  const list = roster();
  const on = list.filter(m => !m.off), off = list.filter(m => m.off);
  const row = m => {
    const mine = m.id === S.me;
    const tag = m.live ? '<span class="tag live">● ao vivo</span>'
      : m.off ? '<span class="tag wait">fora</span>'
      : m.host ? '<span class="tag">anfitrião</span>'
      : STATE_PT[m.state] ? `<span class="tag wait">${STATE_PT[m.state]}</span>` : '<span></span>';
    const act = S.role === 'host' && !mine ? `<button class="kick" data-kick="${esc(m.id)}" title="${m.off ? 'Esquecer esta pessoa' : 'Remover da sala'}">✕</button>` : '<span></span>';
    return `<div class="peer${m.off ? ' off' : ''}"><div class="av">${avInner(m.name, avOf(m.id))}</div><span class="pn">${esc(m.name)}${mine ? ' <i>(você)</i>' : ''}</span>${tag}${act}</div>`;
  };
  $('peers').innerHTML = on.map(row).join('') + (off.length ? '<div class="sep">Fora da sala</div>' + off.map(row).join('') : '');
  $('pCount').textContent = $('gCount').textContent = `${on.length} / 8`;
}

// Anfitrião remove alguém da sala (libera a vaga; a pessoa precisa pedir para entrar de novo).
$('peers').onclick = e => {
  const b = e.target.closest('[data-kick]'); if (!b) return;
  const id = b.dataset.kick;
  if (id.startsWith('off:')) { const r = curSaved(); if (r) { r.members = r.members.filter(m => offId(r, m) !== id); saveRooms(); pushRoster(); } return; }
  // Remover tira da sala e da lista de convidados (precisa pedir de novo para entrar).
  const p = S.peers.get(id), r = curSaved();
  if (p && r && p.uid) { r.members = r.members.filter(m => m.uid !== p.uid); saveRooms(); }
  sendPeer(id, { t: 'kick' });
  setTimeout(() => dropPeer(id, 'foi removido da sala'), 300); // dá tempo do aviso chegar
};
function dropPeer(id, why) {
  if (!S.peers.delete(id)) return Promise.resolve();
  S.avs.delete(id);
  stopPull(id);
  invoke('relay_drop_viewer', { viewer: id }).catch(() => {});
  const done = invoke('remove_peer', { name: id }).catch(() => {});
  if (why) toast(`${id} ${why}`);
  pushRoster();
  return done;
}

// ---------- abas Participantes / Pedidos ----------
function showTab(reqs) {
  $('tabPeers').classList.toggle('on', !reqs); $('tabReqs').classList.toggle('on', reqs);
  $('peers').hidden = reqs; $('reqList').hidden = !reqs;
}
$('tabPeers').onclick = () => showTab(false);
$('tabReqs').onclick = () => showTab(true);

// ---------- modais ----------
const modal = $('modal'), sheet = $('sheet');
modal.onclick = e => { if (e.target === modal) closeModal(); };
function closeModal() { modal.classList.remove('show'); }
document.addEventListener('click', e => { const o = e.target.closest('[data-open]'); if (o) openModal(o.dataset.open); });
document.addEventListener('keydown', e => { if (e.key === 'Escape') closeModal(); });

function openModal(kind) {
  if (kind === 'new') {
    if (S.role) return toast('Saia da sala atual primeiro');
    sheet.innerHTML = `<h2>Nova sala</h2><p>Você será o anfitrião. A sala fica salva na barra lateral: depois é só clicar nela para abrir de novo, com o mesmo código, e quem você já aceitou entra direto.</p>
      <label>Nome da sala</label><input class="field" id="fRoom" value="Sala de ${esc(myName())}" maxlength="40">
      <div class="act"><button class="btn ghost" id="x">Cancelar</button><button class="btn pri" id="ok">Criar sala</button></div>`;
    $('x').onclick = closeModal;
    $('ok').onclick = () => openRoom($('fRoom').value.trim() || 'Sala');
    $('fRoom').onkeydown = e => { if (e.key === 'Enter') $('ok').click(); };
  }
  if (kind === 'invite') {
    if (S.role !== 'host') return;
    sheet.innerHTML = `<h2>Código da sala</h2><p>Mande este código para quem você quer convidar. A pessoa digita no Lume dela e você clica em Aceitar só na primeira vez; depois ela entra direto sempre que a sala estiver aberta.</p>
      <div class="bigcode" id="inv">${esc(S.room.code)}</div>
      <div class="mute" id="relay" style="text-align:center;font-size:12px;margin-top:8px"></div>
      <div class="mute" id="netst" style="text-align:center;font-size:12px;margin-top:4px;line-height:1.45"></div>
      <div class="act"><button class="btn ghost" id="rot" title="O código antigo para de funcionar para todos">Trocar código</button><button class="btn ghost" id="x">Fechar</button><button class="btn pri" id="cp">Copiar código</button></div>`;
    $('x').onclick = closeModal;
    $('cp').onclick = () => copy(S.room.code);
    $('rot').onclick = async () => { const old = curSaved(); S.room = await invoke('new_code'); if (old) { old.code = S.room.code; saveRooms(); } S.reqs.clear(); $('reqs').innerHTML = ''; render(); openModal('invite'); toast('O código antigo parou de funcionar'); };
    const check = async () => {
      if (!$('relay')) return;
      const n = await invoke('relays_online').catch(() => 0);
      $('relay').textContent = n ? '● Sala aberta para pedidos de entrada' : 'Conectando aos servidores de encontro…';
      $('relay').style.color = n ? 'var(--good)' : '';
      if (!n) setTimeout(check, 1000);
    };
    check();
    showNet();
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

// Como está a rede deste PC para receber conexões (porta aberta no roteador ou CGNAT).
async function showNet() {
  const el = $('netst'); if (!el) return;
  const n = await invoke('net_status').catch(() => null);
  if (!n || !$('netst')) return;
  if (n.checking) { el.textContent = 'Verificando o roteador…'; return setTimeout(showNet, 1000); }
  el.style.color = n.upnp ? 'var(--good)' : '';
  el.textContent = n.upnp ? '● Roteador abre a porta sozinho: convidados em qualquer rede conseguem conectar'
    : n.cgnat ? 'Sua internet usa CGNAT. Se o convidado também estiver numa rede restritiva, a conexão direta pode falhar; nesse caso, peça para ele criar a sala.'
    : 'O roteador não abriu a porta (UPnP desligado). Se algum convidado não conectar, ative o UPnP no roteador ou peça para ele criar a sala.';
}

// ---------- pedidos de entrada (anfitrião) ----------
// Cada pedido aparece como aviso no canto e fica na aba Pedidos até ser aceito, recusado ou a pessoa desistir.
function renderReqs() {
  const n = S.reqs.size;
  $('rCount').textContent = n; $('rCount').classList.toggle('hot', n > 0);
  $('reqList').innerHTML = n ? [...S.reqs.values()].map(r => `<div class="peer reqrow" data-id="${esc(r.id)}"><div class="av">${avInner(r.name, r.av)}</div><span class="pn">${esc(r.name)}</span>
    <button class="btn ghost sm" data-a="no">Recusar</button><button class="btn pri sm" data-a="yes">Aceitar</button></div>`).join('')
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
    S.peers.set(who, { state: 'connecting', live: false, open: false, q: Promise.resolve(), uid: r.uid, av: r.av });
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

listen('join-request', async ({ payload: r }) => {
  if (S.role !== 'host' || S.reqs.has(r.id)) return;
  r.av = okAv(r.av); r.uid = ''; S.reqs.set(r.id, r);
  // Já foi aceito antes nesta sala? O pedido traz só uma prova presa a esta oferta; quem confere
  // é o Rust, contra os identificadores guardados (que nunca passam pelo relay).
  const saved = curSaved();
  if (r.proof && saved && saved.members.length) {
    r.uid = (await invoke('check_proof', { id: r.id, uids: saved.members.map(m => m.uid) }).catch(() => null)) || '';
  }
  if (r.uid) {
    // Caiu e voltou: a conexão velha dele pode ainda constar aqui; tira antes, para não virar "Nome (2)".
    const old = [...S.peers].filter(([, p]) => p.uid === r.uid).map(([pid]) => dropPeer(pid));
    toast(`${r.name} voltou para a sala`);
    return Promise.all(old).then(() => answerReq(r.id, true));
  }
  renderReqs();
  const el = document.createElement('div');
  el.className = 'reqcard'; el.dataset.id = r.id;
  el.innerHTML = `<div class="av">${avInner(r.name, r.av)}</div><div><b>${esc(r.name)}</b><small>quer entrar na sala</small></div>
    <button class="btn ghost sm" data-a="no">Recusar</button><button class="btn pri sm" data-a="yes">Aceitar</button><button class="kick" data-a="hide" title="Decidir depois (fica na aba Pedidos)">✕</button>`;
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
// Herdeiro: quem está há mais tempo na sala assume se o anfitrião sair ou cair.
function heirId() { const e = [...S.peers].find(([, p]) => p.open && p.state === 'connected'); return e ? e[0] : null; }
function pushRoster() {
  if (S.role !== 'host') return;
  const heir = heirId();
  broadcast({ t: 'roster', members: roster().filter(m => m.host || m.open || m.off).map(({ id, name, host, live, off }) => ({ id, name, host: !!host, live: !!live, off: !!off, heir: id === heir })) });
  // Só o herdeiro recebe a lista de quem já foi aceito (com os identificadores), pelo canal cifrado,
  // para que todos voltem direto quando ele assumir. Inclui o próprio anfitrião.
  if (heir) {
    const r = curSaved();
    const members = [...(r ? r.members : []), { uid: myUid, name: myName(), av: myAv }].filter(m => m.uid !== S.peers.get(heir).uid);
    sendPeer(heir, { t: 'heir', name: S.room.name, members });
  }
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
  // O identificador do PC chega só por aqui (canal cifrado de ponta a ponta), nunca pelo relay.
  if (m.t === 'hi' && /^[0-9a-f]{32}$/.test(m.uid || '') && !p.uid) { p.uid = m.uid; if (p.state === 'connected') { rememberMember(p.uid, id, S.avs.get(id) || p.av); pushRoster(); } }
  if (m.t === 'live') { p.live = !!m.on; if (!p.live) stopPull(id); pushRoster(); }
  if (m.t === 'bye') dropPeer(id, 'saiu');
  if (m.t === 'av') { const av = okAv(m.av); S.avs.set(id, av); rememberMember(p.uid, id, av); broadcast({ t: 'av', id, av }, id); renderPeers(); labelTiles(); }
  if (m.t === 'fb') {
    const r = { t: 'fb', from: id, loss: +m.loss || 0, fz: +m.fz || 0, jb: +m.jb || 0 };
    if (m.to === HOST) onFb(r); else if (S.peers.has(m.to)) sendPeer(m.to, r);
  }
  if (m.t === 'sig') {
    m.from = id; // quem mandou é quem o canal diz que é, não o que veio escrito
    if (m.to === HOST) onSig(m);
    else if (m.to === '@relay') { if (m.kind === 'answer' && S.pulls.has(id)) invoke('relay_pull_answer', { source: id, sdp: String(m.sdp) }).catch(() => {}); }
    else if (S.peers.has(m.to)) sendPeer(m.to, m);
  }
  // Um convidado não conseguiu ver outro direto: repasso a tela pelo meu Lume.
  if (m.t === 'relayreq') {
    const of = String(m.of);
    if (of !== id && S.peers.has(of) && S.peers.get(of).live) {
      relayFor(of, id, String(m.sdp)).then(ans => sendPeer(id, { t: 'sig', kind: 'answer', from: of, sdp: ans })).catch(() => {});
    }
  }
});
listen('peer-state', ({ payload: [id, st] }) => {
  if (S.role !== 'host') return; // no convidado esses avisos são de quem assiste a tela dele
  const p = S.peers.get(id); if (!p) return;
  p.state = st;
  // (pushRoster também atualiza a lista que o herdeiro guarda, com quem acabou de entrar)
  if (st === 'connected') { toast(`${id} entrou`); rememberMember(p.uid, id, S.avs.get(id) || p.av); pushRoster(); }
  if (st === 'failed' || st === 'closed') return dropPeer(id, st === 'failed' ? 'não conseguiu conectar' : 'saiu');
  renderPeers();
});

function onHostMsg(text) {
  let m; try { m = JSON.parse(text); } catch { return; }
  if (m.t === 'hello') {
    S.me = String(m.you); S.room = { name: String(m.room || 'Sala').slice(0, 40), code: S.joinCode }; rememberRoom(S.joinCode, S.room.name, 'guest'); render();
    if (S.sharing) sendHost({ t: 'live', on: true }); // voltei transmitindo (queda ou troca de anfitrião)
  }
  if (m.t === 'roster' && Array.isArray(m.members)) S.members = m.members.slice(0, 16).map(x => ({ id: String(x.id), name: String(x.name), host: !!x.host, live: !!x.live && !x.off, off: !!x.off, heir: !!x.heir }));
  if (m.t === 'heir' && Array.isArray(m.members)) S.heirPkg = { name: String(m.name || 'Sala').slice(0, 40), members: m.members.slice(0, 32).filter(x => x && /^[0-9a-f]{32}$/.test(x.uid)).map(x => ({ uid: x.uid, name: String(x.name || '').slice(0, 32), av: okAv(x.av) })) };
  if (m.t === 'av') { S.avs.set(String(m.id), okAv(m.av)); labelTiles(); }
  if (m.t === 'sig') onSig(m);
  if (m.t === 'fb') onFb({ from: String(m.from), loss: +m.loss || 0, fz: +m.fz || 0, jb: +m.jb || 0 });
  if (m.t === 'kick' || m.t === 'closed') { S.bye = m.t; S.byeHeir = m.heir ? String(m.heir) : null; }
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
    ? `<video autoplay playsinline muted></video><div class="self"><div><b>Você está transmitindo</b><br><span class="what"></span></div></div>
       <div class="tl"><span class="chip"><span class="dot live"></span><span class="res">—</span></span></div>
       <div class="tb"><button data-a="preview" title="Mostrar/ocultar a prévia da sua tela">👁</button><button data-a="focus" title="Destacar">⤢</button><button data-a="fs" title="Tela cheia">⛶</button></div>`
    : `<video autoplay playsinline></video>
       <div class="tl"><div class="av"></div><span class="chip"><span class="who"></span><span class="res mute"></span></span></div>
       <div class="tb"><button data-a="mute" title="Silenciar esta tela">🔊</button><input type="range" min="0" max="100" value="100" title="Volume desta tela">
       <button data-a="focus" title="Destacar">⤢</button><button data-a="fs" title="Tela cheia">⛶</button></div>`;
  const v = el.querySelector('video');
  el.onclick = e => {
    const b = e.target.closest('[data-a]');
    if (e.target.closest('input')) return;
    if (b && b.dataset.a === 'preview') { setPreview(!S.preview); return; }
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
  if (id === SELF) stopPreview(st);
  if (st.pc) st.pc.close();
  const v = st.el.querySelector('video'); if (v) v.srcObject = null;
}
// Telas que eu vejo pelo anfitrião porque a conexão direta com quem transmite falhou.
S.relayed = new Set();
function failStream(id, st) {
  if (S.streams.get(id) !== st) return;
  closeStream(id); layoutTiles();
  // Primeira falha da conexão direta: passa a ver essa tela pelo anfitrião (que já fala com os dois).
  if (!st.relay) {
    S.relayed.add(id);
    toast(`Sem conexão direta com ${nameOf(id)}. Vendo pela retransmissão do anfitrião…`, 5000);
    return void setTimeout(() => { if (!S.streams.has(id)) syncStreams(); }, 300);
  }
  if (!S.failed.has(id)) toast(`Não foi possível ver a tela de ${nameOf(id)}. Tentando de novo…`, 5000);
  S.failed.set(id, Date.now());
}
async function openStream(id) {
  const st = { id, ms: new MediaStream(), pc: null, el: makeTile(id) };
  S.streams.set(id, st);
  // A tela do anfitrião já chega pela conexão de entrada do convidado.
  if (S.role === 'guest' && id === HOST) { st.ms = S.hostMs; st.viaHost = true; attach(st); return; }
  let forced = false; try { forced = localStorage.getItem('sl.forceRelay') === '1'; } catch {}
  st.relay = S.relayed.has(id) || (forced && id !== HOST);
  const pc = st.pc = recvPc();
  pc.ontrack = e => { st.ms.addTrack(e.track); tuneReceiver(e.receiver); attach(st); };
  pc.onconnectionstatechange = () => {
    if (pc.connectionState === 'connected') { clearTimeout(st.timer); S.failed.delete(id); }
    if (pc.connectionState === 'failed') failStream(id, st);
  };
  st.timer = setTimeout(() => { if (pc.connectionState !== 'connected') failStream(id, st); }, st.relay ? 30000 : 20000);
  try {
    await pc.setLocalDescription(await pc.createOffer());
    await waitIce(pc);
    let sdp = pc.localDescription.sdp;
    if (S.streams.get(id) !== st) return;
    if (st.relay) {
      // Pelo anfitrião: ele puxa a tela de quem transmite e repassa para mim.
      if (S.role === 'host') {
        const ans = await relayFor(id, HOST, sdp);
        if (S.streams.get(id) === st) await pc.setRemoteDescription({ type: 'answer', sdp: ans });
      } else sendHost({ t: 'relayreq', of: id, sdp });
      return;
    }
    // Direto: abre no meu roteador as portas desta conexão, para quem transmite conseguir chegar aqui.
    if (!WEB) sdp = (await invoke('expose_offer', { sdp }).catch(() => null)) || sdp;
    if (S.streams.get(id) === st) sendSig(id, 'offer', sdp);
  } catch { failStream(id, st); }
}

// ---------- retransmissão (anfitrião) ----------
// O Lume nativo do anfitrião puxa a tela de quem transmite uma vez só e repassa os mesmos pacotes
// para cada pessoa que não conseguiu conexão direta (relay.rs).
S.pulls = new Map();
function ensurePull(of) {
  if (!S.pulls.has(of)) {
    const p = invoke('relay_pull_offer', { source: of }).then(offer => sendPeer(of, { t: 'sig', kind: 'offer', from: '@relay', sdp: offer }));
    p.catch(() => S.pulls.delete(of));
    S.pulls.set(of, p);
  }
  return S.pulls.get(of);
}
async function relayFor(of, viewer, sdp) {
  await ensurePull(of);
  return invoke('relay_viewer', { source: of, viewer, offerSdp: sdp });
}
function stopPull(of) { if (S.pulls.delete(of)) invoke('relay_stop', { source: of }).catch(() => {}); }
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
  if (S.sharing && !S.streams.has(SELF)) { const st = { el: makeTile(SELF) }; S.streams.set(SELF, st); if (S.preview) startPreview(st); paintPreview(st); }
  if (!S.sharing && S.streams.has(SELF)) closeStream(SELF);
  layoutTiles();
}

// ---------- prévia da minha tela ----------
// A prévia é uma conexão local com o próprio Lume: mostra exatamente o que os outros recebem
// (mesma imagem codificada), decodificada pela placa de vídeo. Desligada, fica só o quadro preto.
try { S.preview = localStorage.getItem('sl.preview') !== '0'; } catch { S.preview = true; }
const PREVIEW = '@preview';
async function startPreview(st) {
  if (st.pc || st.opening) return;
  st.opening = true;
  await S.prevStop; // parou e voltou a transmitir rápido: espera a prévia antiga fechar
  st.opening = false;
  if (S.streams.get(SELF) !== st || !S.preview || st.pc) return;
  const pc = st.pc = recvPc();
  st.ms = new MediaStream();
  pc.ontrack = e => { if (e.track.kind === 'video') { st.ms.addTrack(e.track); tuneReceiver(e.receiver); attach(st); } };
  try {
    await pc.setLocalDescription(await pc.createOffer());
    await waitIce(pc, 1500);
    const sdp = await invoke('accept_viewer', { name: PREVIEW, offerSdp: pc.localDescription.sdp });
    if (st.pc === pc) await pc.setRemoteDescription({ type: 'answer', sdp });
  } catch { if (st.pc === pc) { pc.close(); st.pc = null; } }
}
function stopPreview(st) {
  if (!st.pc) return;
  st.pc.close(); st.pc = null;
  const v = st.el.querySelector('video'); if (v) v.srcObject = null;
  S.prevStop = invoke('remove_peer', { name: PREVIEW }).catch(() => {});
}
function paintPreview(st) {
  st.el.querySelector('.self').hidden = S.preview;
  const b = st.el.querySelector('[data-a=preview]');
  b.classList.toggle('off', !S.preview); b.title = S.preview ? 'Ocultar a prévia (economiza um pouco de placa de vídeo)' : 'Mostrar a prévia da sua tela';
}
function setPreview(on) {
  S.preview = on;
  try { localStorage.setItem('sl.preview', on ? '1' : '0'); } catch {}
  const st = S.streams.get(SELF); if (!st) return;
  on ? startPreview(st) : stopPreview(st);
  paintPreview(st);
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
  const was = S.sharing; S.sharing = null; S.enc = null; S.fb.clear(); S.rate = 0;
  $('share').textContent = '▶ Compartilhar tela'; $('share').className = 'btn pri inRoom';
  if (was) announceLive();
}
$('share').onclick = () => S.sharing ? stopShare() : openPicker();

// ---------- entrar como convidado ----------
async function guestStart() {
  const code = $('jCode').value.trim();
  const name = $('jName').value.trim() || 'Convidado';
  if (code.replace(/[^a-z0-9]/gi, '').length !== 12) return toast('O código tem 12 letras e números');
  if ($('go').disabled) return;
  const saved = roomByCode(normCode(code));
  if ($('jStatus')) $('jStatus').dataset.saved = saved ? '1' : '';
  $('meName').value = name; $('meName').oninput();
  $('go').disabled = true; $('jCode').disabled = true; $('jName').disabled = true;
  $('jStatus').textContent = 'Preparando a conexão…';
  const pc = recvPc();
  const dc = pc.createDataChannel('ctl');
  const ms = new MediaStream();
  dc.onopen = () => dc.send(JSON.stringify({ t: 'hi', uid: myUid }));
  dc.onmessage = e => { if (S.guestPc === pc && typeof e.data === 'string') onHostMsg(e.data); };
  // O canal fecha na hora quando o anfitrião remove a pessoa ou fecha a sala.
  dc.onclose = () => { if (S.role === 'guest' && S.guestPc === pc) lost(); };
  pc.ontrack = e => { ms.addTrack(e.track); tuneReceiver(e.receiver); const st = S.streams.get(HOST); if (st) attach(st); };
  pc.onconnectionstatechange = () => {
    if (pc.connectionState === 'connected') { toast('Conectado'); closeModal(); }
    if (pc.connectionState === 'failed') toast('Não foi possível conectar direto (rede com NAT restritivo)', 5000);
    if (['failed', 'closed'].includes(pc.connectionState) && S.role === 'guest' && S.guestPc === pc) lost();
  };
  await pc.setLocalDescription(await pc.createOffer());
  await waitIce(pc);
  S.guestPc = pc; S.guestDc = dc; S.hostMs = ms;
  if ($('jStatus')) $('jStatus').textContent = saved ? `Chamando ${saved.name}… Se o anfitrião ainda não abriu a sala, fica esperando.` : 'Pedido enviado. Aguardando o anfitrião aceitar…';
  try {
    const sdp = await invoke('join_room', { code, name, av: myAv, uid: myUid, offerSdp: pc.localDescription.sdp });
    await pc.setRemoteDescription({ type: 'answer', sdp });
    S.joinCode = normCode(code);
    S.room = { name: (roomByCode(S.joinCode) || {}).name || '', code: S.joinCode };
    S.role = 'guest'; render(); closeModal(); toast('Aceito! Conectando…');
  } catch (err) {
    pc.close(); if (S.guestPc === pc) S.guestPc = S.guestDc = S.hostMs = null;
    if (String(err) !== 'Cancelado') toast(String(err), 5000);
    // Não consegui voltar para a sala: a transmissão que tinha ficado ligada para a volta para.
    if (S.sharing && !S.role && String(err) !== 'Cancelado') stopShare();
    if ($('jStatus')) { $('jStatus').textContent = ''; $('go').disabled = false; $('jCode').disabled = false; $('jName').disabled = false; }
  }
}

// Saí da sala sem querer: se foi expulso ou a sala fechou, avisa; se a conexão caiu, volta sozinho
// (quem já foi aceito entra direto, então o anfitrião nem precisa clicar).
function lost() {
  const bye = S.bye, code = S.joinCode, me = S.me, pkg = S.heirPkg;
  // Quem assume: o que o anfitrião indicou ao sair ou, se ele caiu, o herdeiro da última lista.
  const heir = bye === 'closed' ? S.byeHeir : bye ? null : (S.members.find(m => m.heir) || {}).id;
  const heirName = heir && (S.members.find(m => m.id === heir) || {}).name;
  // Se vou voltar para a sala (queda ou troca de anfitrião), minha transmissão continua.
  leave(bye !== 'kick' && !!code && !(bye === 'closed' && !heir));
  if (bye === 'kick') return toast('O anfitrião removeu você da sala', 5000);
  if (!code) return toast('A conexão com a sala caiu', 5000);
  if (heir && heir === me && pkg) {
    const becomeHost = () => {
      // Eu viro o anfitrião: mesmo código, e quem já estava na sala entra de volta sem pedir.
      const r = rememberRoom(code, pkg.name, 'host');
      for (const m of pkg.members) if (m.uid !== myUid && !r.members.some(x => x.uid === m.uid)) r.members.push(m);
      saveRooms();
      toast('O anfitrião saiu. Agora você é o anfitrião da sala.', 5000);
      openRoom(pkg.name, code);
    };
    // (espera o leave() terminar de fechar a conexão antiga antes de abrir a sala de novo)
    if (bye === 'closed') return setTimeout(() => { if (!S.role) becomeHost(); }, 600);
    // Caiu sem aviso: pode ter sido só a MINHA conexão. Tenta voltar ao anfitrião de antes;
    // se ninguém responder em alguns segundos, ele caiu mesmo e eu assumo.
    toast('A conexão caiu. Reconectando…', 4000);
    setTimeout(() => {
      if (S.role) return;
      openModal('join'); $('jCode').value = code; guestStart();
      setTimeout(() => {
        if (S.role) return; // o anfitrião antigo respondeu
        invoke('cancel_join').catch(() => {}); closeModal();
        setTimeout(() => { if (!S.role) becomeHost(); }, 800);
      }, 9000);
    }, 1000);
    return;
  }
  if (bye === 'closed' && !heir) return toast('O anfitrião fechou a sala', 5000);
  toast(heir ? `${heirName || 'Outra pessoa'} agora é o anfitrião. Reconectando…` : 'A conexão caiu. Reconectando…', 4000);
  // Quem assume leva uns segundos para reabrir a sala; o pedido de entrada fica se repetindo até lá.
  setTimeout(() => { if (S.role) return; openModal('join'); $('jCode').value = code; guestStart(); }, heir ? 2500 : 1500);
}

// ---------- painel de qualidade (da minha transmissão) ----------
document.querySelectorAll('.seg').forEach(seg => seg.onclick = e => {
  const b = e.target.closest('button'); if (!b) return;
  seg.querySelectorAll('button').forEach(x => x.classList.toggle('on', x === b));
  const k = seg.dataset.k;
  S.q[k] = k === 'pri' ? b.dataset.v : +b.dataset.v;
  qualityChanged();
});
$('br').oninput = () => { S.q.br = +$('br').value; paintRate(); };
$('br').onchange = qualityChanged;
function qualityChanged() { invoke('set_quality', { quality: S.q }).catch(err => toast(String(err))); S.rate = 0; adapt(); }

// ---------- qualidade automática ----------
// Quem assiste manda a cada 2 s como a imagem está chegando (perda de pacotes e atraso do buffer).
// Quem transmite ajusta o bitrate pelo pior relato: baixa quando a rede de alguém continua ruim e
// sobe devagar quando todos estão bem, sem passar do máximo escolhido. O encoder muda na hora.
S.fb = new Map(); S.rate = 0; S.hold = 0; S.sent = -1;
try { S.auto = localStorage.getItem('sl.auto') !== '0'; } catch { S.auto = true; }
function feedback(id, a, b) {
  if (!a || !b || id === SELF || b.recv == null || a.recv == null) return;
  const recv = b.recv - a.recv, lost = Math.max(0, b.lost - a.lost);
  if (recv + lost < 20) return; // tela parada: pouco para medir
  const r = { t: 'fb', to: id, loss: +(lost / (recv + lost)).toFixed(3), fz: Math.max(0, b.fz - a.fz), jb: b.jbe > a.jbe ? Math.round((b.jbd - a.jbd) / (b.jbe - a.jbe) * 1000) : 0 };
  if (S.role === 'host') sendPeer(id, { ...r, from: HOST });
  else sendHost(r);
}
function onFb(r) { if (S.sharing) S.fb.set(r.from, { ...r, at: Date.now() }); }
function adapt() {
  const max = S.q.br * 1000;
  // Nunca abaixo disto: menos que isso a imagem vira borrão e não compensa.
  const floor = Math.min(max, Math.max(1500, max * 0.3));
  let target = 0;
  if (S.auto && S.sharing) {
    const now = Date.now();
    // Cada relato conta uma vez só (antes, o mesmo relato ruim cortava o bitrate várias vezes).
    const news = [...S.fb.values()].filter(f => !f.used && now - f.at < 7000);
    news.forEach(f => { f.used = true; });
    const fresh = [...S.fb.values()].filter(f => now - f.at < 7000);
    let cur = S.rate || max;
    // Só perda de pacotes e atraso da rede contam. "Travadas" ficam de fora: vídeo e tela parada
    // têm pausas naturais entre quadros, e elas eram lidas como rede ruim.
    const bad = news.some(f => f.loss > 0.04 || f.jb > 350);
    S.badRun = bad ? (S.badRun || 0) + 1 : news.length ? 0 : (S.badRun || 0);
    if (S.badRun >= 2) { cur = Math.max(floor, cur * 0.8); S.hold = now; S.badRun = 0; } // ruim de novo: aí sim baixa
    else if (!bad && fresh.every(f => f.loss < 0.02 && f.jb < 200) && now - S.hold > 4000) cur = Math.min(max, cur * 1.12);
    S.rate = Math.round(Math.min(cur, max));
    target = S.rate >= max ? 0 : S.rate;
  } else S.rate = 0;
  if (target !== S.sent) { S.sent = target; invoke('set_bitrate', { kbps: target }).catch(() => {}); }
  paintRate();
}
function paintRate() {
  $('autoSeg').querySelectorAll('button').forEach(b => b.classList.toggle('on', (b.dataset.v === '1') === S.auto));
  $('brl').textContent = S.auto ? 'Bitrate máx.' : 'Bitrate';
  const now = S.auto && S.sharing && S.rate && S.rate < S.q.br * 1000;
  $('brv').textContent = now ? `${(S.rate / 1000).toFixed(1)} de ${S.q.br} Mbps` : `${S.q.br} Mbps`;
  $('brv').title = now ? 'Reduzido automaticamente porque a conexão de alguém está engasgando' : '';
}
$('autoSeg').onclick = e => {
  const b = e.target.closest('button'); if (!b) return;
  S.auto = b.dataset.v === '1';
  try { localStorage.setItem('sl.auto', S.auto ? '1' : '0'); } catch {}
  S.rate = 0; adapt();
};
setInterval(adapt, 2000);
paintRate();

// ---------- sair ----------
// keepShare: saída por queda ou troca de anfitrião, em que a pessoa volta sozinha para a sala.
// A transmissão dela continua; quem assiste reconecta quando ela voltar.
function leave(keepShare = false) {
  if (keepShare !== true) keepShare = false; // (leave também é usado direto como evento de clique)
  const was = S.role;
  if (was === 'guest') sendHost({ t: 'bye' });
  if (was === 'host') {
    // Sair passa a sala para quem está há mais tempo nela (se houver alguém).
    const heir = heirId();
    broadcast({ t: 'closed', heir });
    if (heir) { const r = curSaved(); if (r) { r.role = 'guest'; saveRooms(); } toast(`${heir} agora é o anfitrião da sala`); }
  }
  if (!keepShare) stopShare();
  for (const id of [...S.streams.keys()]) closeStream(id);
  // O anfitrião espera o aviso "sala fechada" sair antes de derrubar as conexões.
  if (was) setTimeout(() => { if (!S.role) invoke('close_room', { keepShare: !!(keepShare && S.sharing) }).catch(() => {}); }, was === 'host' ? 400 : 0);
  $('reqs').innerHTML = '';
  S.peers.clear(); S.members = []; S.avs.clear(); S.reqs.clear(); S.failed.clear(); S.relayed.clear(); S.pulls.clear();
  if (S.guestPc) S.guestPc.close();
  S.guestPc = S.guestDc = S.hostMs = null; S.role = null; S.room = null; S.joinCode = null; S.bye = null; S.byeHeir = null; S.heirPkg = null; S.me = null; S.focus = null;
  showTab(false); layoutTiles(); render();
}
$('leave').onclick = leave;

// ---------- estatísticas (1x por segundo) ----------
async function readStats(pc, prev) {
  const r = await pc.getStats(); const out = { t: performance.now() };
  r.forEach(s => {
    if (s.type === 'candidate-pair' && s.nominated && s.state === 'succeeded') out.rtt = s.currentRoundTripTime;
    if (s.type === 'inbound-rtp' && s.kind === 'video') { out.bytes = s.bytesReceived; out.fps = s.framesPerSecond; out.w = s.frameWidth; out.h = s.frameHeight; out.lost = s.packetsLost; out.recv = s.packetsReceived; out.fz = s.freezeCount || 0; out.jbd = s.jitterBufferDelay || 0; out.jbe = s.jitterBufferEmittedCount || 0; }
  });
  if (prev && out.bytes != null && prev.bytes != null) out.kbps = (out.bytes - prev.bytes) * 8 / (out.t - prev.t);
  return out;
}

setInterval(async () => {
  if (!S.role) return;
  syncStreams(); // também tenta de novo as telas que falharam
  for (const [id, st] of S.streams) {
    const res = st.el.querySelector('.res');
    if (id === SELF) { res.textContent = S.enc ? `${S.enc.width}×${S.enc.height} · ${S.enc.fps >= 1 ? Math.round(S.enc.fps) + ' fps' : 'tela parada'} · ${S.enc.encoder}` : 'iniciando…'; continue; }
    const pc = st.viaHost ? S.guestPc : st.pc; if (!pc) continue;
    const prev = st.stats;
    st.stats = await readStats(pc, prev).catch(() => prev);
    feedback(id, prev, st.stats);
    const s = st.stats;
    // Tela parada não manda quadros novos (o vídeo segue certo): diz isso em vez de "0 fps".
    res.textContent = s && s.w ? ` · ${s.w}×${s.h} · ${s.fps >= 1 ? Math.round(s.fps) + ' fps' : 'tela parada'}` : ' · conectando…';
  }
  // O painel Conexão mostra a tela destacada (ou a primeira que estou assistindo).
  const watched = [...S.streams].filter(([id]) => id !== SELF);
  const cur = (watched.find(([id]) => id === S.focus) || watched[0] || [])[1];
  const s = cur && cur.stats;
  const n = S.role === 'host' ? [...S.peers.values()].filter(p => p.state === 'connected').length : S.members.filter(m => !m.off).length - 1;
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

// ---------- aviso de versão nova ----------
// Só consulta a página pública de lançamentos do projeto; nada sobre você é enviado.
function newer(a, b) {
  const p = v => String(v).replace(/^v/, '').split(/[.-]/).map(x => parseInt(x, 10) || 0);
  const [x, y] = [p(a), p(b)];
  for (let i = 0; i < 3; i++) if ((x[i] || 0) !== (y[i] || 0)) return (x[i] || 0) > (y[i] || 0);
  return false;
}
async function checkUpdate() {
  try {
    const cur = await invoke('app_version');
    const r = await fetch('https://api.github.com/repos/paulo-vc/lume/releases/latest', { headers: { Accept: 'application/vnd.github+json' }, credentials: 'omit', referrerPolicy: 'no-referrer' });
    if (!r.ok) return;
    const tag = String((await r.json()).tag_name || '');
    if (!/^v?\d+\.\d+\.\d+$/.test(tag) || !newer(tag, cur)) return;
    $('updVer').textContent = `${tag.replace(/^v/, '')} (você tem ${cur})`;
    $('update').hidden = false;
  } catch {}
}
$('updGo').onclick = () => { invoke('open_update').catch(() => {}); toast('Baixando no navegador. Feche o Lume e abra o arquivo novo.', 5000); };
if (!WEB) setTimeout(checkUpdate, 3000);
setInterval(checkUpdate, 6 * 3600 * 1000);

window.addEventListener('resize', layoutTiles);
window.addEventListener('beforeunload', leave);
render();
