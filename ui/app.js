// Lume - interface + WebRTC (o WebView2 usa o encoder H.264 por hardware da GPU).
// A criptografia dos convites e a validação de quem entra ficam no Rust (src-tauri/src/room.rs).
'use strict';

const invoke = window.__TAURI__ ? window.__TAURI__.core.invoke : async () => { throw 'Rode dentro do app' };
const $ = id => document.getElementById(id);
const ICE = [{ urls: ['stun:stun.l.google.com:19302', 'stun:stun.cloudflare.com:3478'] }];

const S = {
  role: null,            // 'host' | 'guest'
  room: null,            // RoomInfo (host)
  stream: null,          // tela + áudio (host)
  micStream: null,
  peers: new Map(),      // host: name -> {pc, dc, video, audio}
  guestPc: null, guestDc: null,
  q: { res: 1080, fps: 60, pri: 'motion', br: 8 },
};

// ---------- utilidades ----------
function toast(msg, ms = 2600) { const t = $('toast'); t.textContent = msg; t.classList.add('show'); clearTimeout(t._h); t._h = setTimeout(() => t.classList.remove('show'), ms); }
function initials(n) { return (n || '?').trim().split(/\s+/).map(p => p[0]).slice(0, 2).join('').toUpperCase() || '?'; }
function myName() { return $('meName').value.trim() || 'Convidado'; }
async function copy(text) { try { await navigator.clipboard.writeText(text); toast('Copiado'); } catch { toast('Não foi possível copiar'); } }
function esc(s) { return String(s).replace(/[&<>"']/g, c => ({ '&': '&amp;', '<': '&lt;', '>': '&gt;', '"': '&quot;', "'": '&#39;' }[c])); }

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

// ---------- persistência leve ----------
try { $('meName').value = localStorage.getItem('sl.name') || ''; document.documentElement.dataset.theme = localStorage.getItem('sl.theme') || 'light'; } catch {}
$('meAv').textContent = initials($('meName').value);
$('meName').oninput = () => { $('meAv').textContent = initials($('meName').value); try { localStorage.setItem('sl.name', $('meName').value); } catch {} };
$('theme').onclick = () => { const d = document.documentElement; d.dataset.theme = d.dataset.theme === 'dark' ? 'light' : 'dark'; try { localStorage.setItem('sl.theme', d.dataset.theme); } catch {} };

// ---------- estado visual ----------
function render() {
  document.body.className = S.role || '';
  const inRoom = !!S.role;
  $('empty').hidden = inRoom; $('roomView').hidden = !inRoom;
  $('roomDot').className = 'dot' + (inRoom ? ' live' : '');
  if (S.role === 'host') {
    $('title').textContent = S.room.name;
    $('subtitle').textContent = 'Você é o anfitrião · sala privada, só por convite';
    $('roomLabel').textContent = S.room.name;
    $('qWho').textContent = 'aplica a todos';
  } else if (S.role === 'guest') {
    $('title').textContent = 'Assistindo';
    $('subtitle').textContent = 'Conectado direto ao anfitrião · criptografado';
    $('roomLabel').textContent = 'Sala do anfitrião';
    $('qWho').textContent = 'pede ao anfitrião';
  } else {
    $('title').textContent = 'Lume'; $('subtitle').textContent = 'Crie uma sala ou entre com um convite';
    $('roomLabel').textContent = 'Nenhuma sala';
  }
  renderPeers();
}

function renderPeers() {
  const list = [];
  if (S.role === 'host') {
    list.push({ name: myName() + ' (você)', tag: 'anfitrião' });
    for (const [n, p] of S.peers) list.push({ name: n, tag: p.rtt != null ? p.rtt + ' ms' : p.pc.connectionState });
  } else if (S.role === 'guest') {
    list.push({ name: 'Anfitrião', tag: S.guestPc ? S.guestPc.connectionState : '' }, { name: myName() + ' (você)', tag: '' });
  }
  $('peers').innerHTML = list.map(p => `<div class="peer"><div class="av">${esc(initials(p.name))}</div>${esc(p.name)}<small>${esc(p.tag)}</small></div>`).join('');
  const n = S.role === 'host' ? S.peers.size + 1 : S.role ? 2 : 0;
  $('pCount').textContent = `${n} / 8`; $('roomCount').textContent = n;
}

// ---------- modais ----------
const modal = $('modal'), sheet = $('sheet');
modal.onclick = e => { if (e.target === modal) closeModal(); };
function closeModal() { modal.classList.remove('show'); }
document.addEventListener('click', e => { const o = e.target.closest('[data-open]'); if (o) openModal(o.dataset.open); });
document.addEventListener('keydown', e => { if (e.key === 'Escape') closeModal(); });

function openModal(kind) {
  if (kind === 'new') {
    if (S.role) return toast('Saia da sala atual primeiro');
    sheet.innerHTML = `<h2>Nova sala</h2><p>Você será o anfitrião. A sala existe só enquanto este app estiver aberto e ninguém entra sem um convite seu.</p>
      <label>Nome da sala</label><input class="field" id="fRoom" value="Sala de ${esc(myName())}" maxlength="40">
      <div class="act"><button class="btn ghost" id="x">Cancelar</button><button class="btn pri" id="ok">Criar sala</button></div>`;
    $('x').onclick = closeModal;
    $('ok').onclick = async () => { S.room = await invoke('create_room', { name: $('fRoom').value.trim() || 'Sala' }); S.role = 'host'; render(); openModal('invite'); };
  }
  if (kind === 'invite') {
    if (S.role !== 'host') return;
    const mins = Math.max(0, Math.round((S.room.expires_at * 1000 - Date.now()) / 60000));
    sheet.innerHTML = `<h2>Convidar e aprovar</h2>
      <div class="step"><b>1</b>Envie este convite (expira em ${mins} min)</div>
      <div class="code" id="inv">${esc(S.room.invite)}</div>
      <div class="act" style="margin-top:8px;justify-content:flex-start"><button class="btn ghost" id="cp">Copiar convite</button><button class="btn ghost" id="rot">Gerar novo (invalida o atual)</button></div>
      <div class="step"><b>2</b>Cole o pedido de entrada que a pessoa te devolver</div>
      <textarea class="field" id="req" placeholder="SLQ-…"></textarea>
      <div id="reqOut"></div>
      <div class="act"><button class="btn ghost" id="x">Fechar</button><button class="btn pri" id="chk">Verificar pedido</button></div>`;
    $('x').onclick = closeModal;
    $('cp').onclick = () => copy(S.room.invite);
    $('rot').onclick = async () => { S.room = await invoke('new_invite'); openModal('invite'); toast('Convite antigo invalidado'); };
    $('chk').onclick = async () => {
      const code = $('req').value.trim();
      try {
        const g = await invoke('inspect_request', { code });
        if (S.peers.has(g.name)) throw 'Já existe alguém com esse nome na sala';
        $('reqOut').innerHTML = `<div class="req"><div class="av">${esc(initials(g.name))}</div><div><b style="font-weight:500">${esc(g.name)}</b><div class="mute">convite válido · quer entrar</div></div>
          <div class="sp"></div><button class="btn ghost" id="no">Recusar</button><button class="btn pri" id="yes">Aprovar</button></div>`;
        $('no').onclick = () => { $('reqOut').innerHTML = ''; $('req').value = ''; };
        $('yes').onclick = () => approveGuest(code, g);
      } catch (err) { toast(String(err)); }
    };
  }
  if (kind === 'join') {
    if (S.role) return toast('Saia da sala atual primeiro');
    sheet.innerHTML = `<h2>Entrar com convite</h2><p>São dois códigos trocados com o anfitrião (por qualquer chat). Isso substitui um servidor.</p>
      <label>Seu nome</label><input class="field" id="jName" value="${esc(myName())}" maxlength="32">
      <div class="step"><b>1</b>Cole o convite</div><input class="field" id="jInv" placeholder="sl://join/…">
      <div class="act" style="justify-content:flex-start;margin-top:8px"><button class="btn pri" id="gen">Gerar pedido de entrada</button></div>
      <div id="jStep2"></div>
      <div class="act"><button class="btn ghost" id="x">Cancelar</button></div>`;
    $('x').onclick = () => { if (S.guestPc && S.role !== 'guest') { S.guestPc.close(); S.guestPc = null; } closeModal(); };
    $('gen').onclick = guestStart;
  }
  modal.classList.add('show');
}

// ---------- ANFITRIÃO ----------
async function approveGuest(code, g) {
  const pc = new RTCPeerConnection({ iceServers: ICE });
  const peer = { pc, dc: null, rtt: null, last: null };
  pc.ondatachannel = e => { peer.dc = e.channel; peer.dc.onmessage = m => onHostMsg(g.name, m.data); };
  await pc.setRemoteDescription({ type: 'offer', sdp: g.sdp });
  // O convidado já ofereceu transceivers recvonly de vídeo e áudio: só ligamos as faixas, sem renegociar.
  for (const t of pc.getTransceivers()) {
    t.direction = 'sendonly';
    if (t.receiver.track.kind === 'video') peer.video = t.sender; else peer.audio = t.sender;
  }
  attachTracks(peer);
  await pc.setLocalDescription(await pc.createAnswer());
  await waitIce(pc);
  try {
    const reply = await invoke('approve_request', { code, answerSdp: pc.localDescription.sdp });
    S.peers.set(g.name, peer);
    pc.onconnectionstatechange = () => {
      if (['failed', 'closed'].includes(pc.connectionState)) dropPeer(g.name, 'saiu');
      if (pc.connectionState === 'connected') { toast(`${g.name} entrou`); applyQuality(peer); }
      renderPeers();
    };
    $('reqOut').innerHTML = `<div class="step"><b>3</b>Envie esta resposta para ${esc(g.name)}</div><div class="code">${esc(reply)}</div>
      <div class="act" style="justify-content:flex-start;margin-top:8px"><button class="btn pri" id="cpr">Copiar resposta</button></div>`;
    $('cpr').onclick = () => copy(reply);
    $('req').value = '';
    renderPeers();
  } catch (err) { pc.close(); toast(String(err)); }
}

function dropPeer(name, why) {
  const p = S.peers.get(name); if (!p) return;
  p.pc.close(); S.peers.delete(name); invoke('member_left', { name }).catch(() => {});
  toast(`${name} ${why}`); renderPeers();
}

function attachTracks(peer) {
  const v = S.stream && S.stream.getVideoTracks()[0];
  const a = mixedAudio();
  if (peer.video) peer.video.replaceTrack(v || null);
  if (peer.audio) peer.audio.replaceTrack(a || null);
}

let audioCtx, audioDest;
function mixedAudio() {
  const sys = S.stream && S.stream.getAudioTracks()[0];
  const mic = S.micStream && S.micStream.getAudioTracks()[0];
  if (!sys && !mic) return null;
  if (sys && !mic) return sys;
  if (!sys && mic) return mic;
  audioCtx = audioCtx || new AudioContext({ latencyHint: 'interactive' });
  audioDest = audioCtx.createMediaStreamDestination();
  audioCtx.createMediaStreamSource(new MediaStream([sys])).connect(audioDest);
  audioCtx.createMediaStreamSource(new MediaStream([mic])).connect(audioDest);
  return audioDest.stream.getAudioTracks()[0];
}

async function startShare() {
  try {
    S.stream = await navigator.mediaDevices.getDisplayMedia({
      video: { frameRate: { ideal: S.q.fps, max: 60 }, cursor: 'always' },
      audio: { echoCancellation: false, noiseSuppression: false, autoGainControl: false },
      systemAudio: 'include', selfBrowserSurface: 'exclude',
    });
  } catch { return toast('Compartilhamento cancelado'); }
  const vt = S.stream.getVideoTracks()[0];
  vt.contentHint = S.q.pri;
  vt.onended = stopShare;
  $('video').srcObject = new MediaStream([vt]); $('video').muted = true; $('ph').hidden = true;
  for (const p of S.peers.values()) { attachTracks(p); applyQuality(p); }
  $('share').textContent = '■ Parar'; $('share').className = 'btn red hostOnly';
  if (!S.stream.getAudioTracks().length) toast('Sem áudio do sistema: marque "Compartilhar áudio" na janela de seleção', 4500);
}

function stopShare() {
  if (S.stream) S.stream.getTracks().forEach(t => t.stop());
  S.stream = null; $('video').srcObject = null; $('ph').hidden = false;
  for (const p of S.peers.values()) attachTracks(p);
  $('share').textContent = '▶ Compartilhar tela'; $('share').className = 'btn pri hostOnly';
}
$('share').onclick = () => S.stream ? stopShare() : startShare();

$('mic').onclick = async () => {
  if (S.micStream) { S.micStream.getTracks().forEach(t => t.stop()); S.micStream = null; $('mic').classList.add('off'); }
  else {
    try { S.micStream = await navigator.mediaDevices.getUserMedia({ audio: { echoCancellation: true, noiseSuppression: true } }); $('mic').classList.remove('off'); }
    catch { return toast('Microfone indisponível'); }
  }
  for (const p of S.peers.values()) attachTracks(p);
};

// Qualidade: aplicada no encoder de cada conexão, ao vivo, sem renegociar.
async function applyQuality(peer, q = peer.q || S.q) {
  const sender = peer.video; if (!sender) return;
  const vt = S.stream && S.stream.getVideoTracks()[0];
  if (vt) {
    vt.contentHint = q.pri;
    vt.applyConstraints({ frameRate: { ideal: q.fps, max: q.fps } }).catch(() => {});
  }
  const p = sender.getParameters();
  if (!p.encodings || !p.encodings.length) p.encodings = [{}];
  const srcH = vt ? (vt.getSettings().height || 1080) : 1080;
  const e = p.encodings[0];
  e.maxBitrate = q.br * 1_000_000;
  e.maxFramerate = q.fps;
  e.scaleResolutionDownBy = q.res && srcH > q.res ? srcH / q.res : 1;
  e.networkPriority = e.priority = 'high';
  // Fluidez: segura o FPS e baixa a resolução sob pressão. Nitidez: o contrário.
  p.degradationPreference = q.pri === 'motion' ? 'maintain-framerate' : 'maintain-resolution';
  try { await sender.setParameters(p); } catch (err) { console.warn('setParameters', err); }
}

function onHostMsg(name, data) {
  let m; try { m = JSON.parse(data); } catch { return; }
  const peer = S.peers.get(name); if (!peer) return;
  if (m.t === 'quality') {
    const q = { res: +m.res, fps: Math.min(60, +m.fps), pri: m.pri === 'detail' ? 'detail' : 'motion', br: Math.min(40, Math.max(1, +m.br)) };
    peer.q = q; applyQuality(peer, q); toast(`${name} ajustou a qualidade`);
  }
}

// ---------- CONVIDADO ----------
async function guestStart() {
  const invite = $('jInv').value.trim();
  const name = $('jName').value.trim() || 'Convidado';
  $('meName').value = name; $('meName').oninput();
  const pc = new RTCPeerConnection({ iceServers: ICE });
  const vt = pc.addTransceiver('video', { direction: 'recvonly' });
  pc.addTransceiver('audio', { direction: 'recvonly' });
  preferH264(vt);
  const dc = pc.createDataChannel('ctl');
  const remote = new MediaStream();
  pc.ontrack = e => {
    remote.addTrack(e.track);
    if ('jitterBufferTarget' in e.receiver) e.receiver.jitterBufferTarget = 40; // ms, buffer curto
    $('video').srcObject = remote; $('video').muted = false;
    e.track.onunmute = () => { $('ph').hidden = remote.getVideoTracks().some(t => !t.muted) ? true : $('ph').hidden; };
  };
  pc.onconnectionstatechange = () => {
    if (pc.connectionState === 'connected') { toast('Conectado'); closeModal(); }
    if (pc.connectionState === 'failed') toast('Não foi possível conectar direto (rede com NAT restritivo)', 5000);
    if (['failed', 'closed', 'disconnected'].includes(pc.connectionState) && S.role === 'guest' && pc.connectionState !== 'disconnected') leave();
    renderPeers();
  };
  await pc.setLocalDescription(await pc.createOffer());
  await waitIce(pc);
  let r;
  try { r = await invoke('make_join_request', { invite, name, offerSdp: pc.localDescription.sdp }); }
  catch (err) { pc.close(); return toast(String(err)); }
  S.guestPc = pc; S.guestDc = dc;
  $('gen').disabled = true;
  $('jStep2').innerHTML = `<div class="step"><b>2</b>Envie este pedido ao anfitrião</div><div class="code">${esc(r.request)}</div>
    <div class="act" style="justify-content:flex-start;margin-top:8px"><button class="btn ghost" id="cpq">Copiar pedido</button></div>
    <div class="step"><b>3</b>Cole a resposta do anfitrião</div><textarea class="field" id="jRep" placeholder="SLR-…"></textarea>
    <div class="act" style="justify-content:flex-start;margin-top:8px"><button class="btn pri" id="con">Conectar</button></div>`;
  $('cpq').onclick = () => copy(r.request);
  $('con').onclick = async () => {
    try {
      const sdp = await invoke('open_reply', { code: $('jRep').value.trim() });
      await pc.setRemoteDescription({ type: 'answer', sdp });
      S.role = 'guest'; render(); toast('Conectando…');
    } catch (err) { toast(String(err)); }
  };
}

$('mute').onclick = () => { const v = $('video'); v.muted = !v.muted; $('mute').classList.toggle('off', v.muted); };

// ---------- painel de qualidade ----------
document.querySelectorAll('.seg').forEach(seg => seg.onclick = e => {
  const b = e.target.closest('button'); if (!b) return;
  seg.querySelectorAll('button').forEach(x => x.classList.toggle('on', x === b));
  const k = seg.dataset.k;
  S.q[k] = k === 'pri' ? b.dataset.v : +b.dataset.v;
  qualityChanged();
});
$('br').oninput = () => { S.q.br = +$('br').value; $('brv').textContent = S.q.br + ' Mbps'; };
$('br').onchange = qualityChanged;

function qualityChanged() {
  if (S.role === 'host') for (const p of S.peers.values()) { p.q = null; applyQuality(p); }
  if (S.role === 'guest' && S.guestDc && S.guestDc.readyState === 'open') S.guestDc.send(JSON.stringify({ t: 'quality', ...S.q }));
}

// ---------- sair / tela cheia ----------
function leave() {
  stopShare();
  if (S.micStream) { S.micStream.getTracks().forEach(t => t.stop()); S.micStream = null; }
  for (const p of S.peers.values()) p.pc.close();
  S.peers.clear();
  if (S.guestPc) S.guestPc.close();
  S.guestPc = S.guestDc = null; S.role = null; S.room = null;
  $('video').srcObject = null; $('ph').hidden = false;
  ['hres', 'hfps', 'hlat'].forEach(id => $(id).textContent = '—');
  render();
}
$('leave').onclick = leave;
$('fs').onclick = () => document.fullscreenElement ? document.exitFullscreen() : $('stage').requestFullscreen();

// ---------- estatísticas (1x por segundo) ----------
async function readStats(pc, prev) {
  const r = await pc.getStats(); const out = { t: performance.now() };
  r.forEach(s => {
    if (s.type === 'candidate-pair' && s.nominated && s.state === 'succeeded') { out.rtt = s.currentRoundTripTime; out.relay = false; }
    if (s.type === 'outbound-rtp' && s.kind === 'video') { out.bytes = s.bytesSent; out.fps = s.framesPerSecond; out.w = s.frameWidth; out.h = s.frameHeight; out.enc = s.encoderImplementation; }
    if (s.type === 'inbound-rtp' && s.kind === 'video') { out.bytes = s.bytesReceived; out.fps = s.framesPerSecond; out.w = s.frameWidth; out.h = s.frameHeight; out.lost = s.packetsLost; out.recv = s.packetsReceived; out.dec = s.decoderImplementation; }
    if (s.type === 'remote-inbound-rtp' && s.kind === 'video') { out.lossFrac = s.fractionLost; }
  });
  if (prev && out.bytes != null && prev.bytes != null) out.kbps = (out.bytes - prev.bytes) * 8 / (out.t - prev.t);
  return out;
}

setInterval(async () => {
  let s = null;
  if (S.role === 'guest' && S.guestPc) { s = S.guestPc._st = await readStats(S.guestPc, S.guestPc._st); }
  if (S.role === 'host') {
    for (const p of S.peers.values()) { p.last = await readStats(p.pc, p.last); if (p.last.rtt != null) p.rtt = Math.round(p.last.rtt * 1000); }
    s = [...S.peers.values()].map(p => p.last).sort((a, b) => (b.rtt || 0) - (a.rtt || 0))[0] || null; // pior conexão
    renderPeers();
  }
  const pc = S.role === 'guest' ? S.guestPc : [...S.peers.values()][0]?.pc;
  $('connState').textContent = pc ? ({ connected: '● direta', connecting: 'conectando…', new: 'aguardando', failed: 'falhou', disconnected: 'instável' }[pc.connectionState] || pc.connectionState) : '—';
  $('connState').style.color = pc && pc.connectionState === 'connected' ? 'var(--good)' : '';
  if (!s) return;
  const lat = s.rtt != null ? Math.round(s.rtt * 1000 / 2 + 30) : null; // ~ rede (meio RTT) + captura/encode/decode
  $('sLat').textContent = lat != null ? lat + ' ms' : '—';
  $('hlat').textContent = lat != null ? lat + ' ms' : '— ms';
  $('sBr').textContent = s.kbps != null ? (s.kbps / 1000).toFixed(1) + ' M' : '—';
  const loss = s.lossFrac != null ? s.lossFrac * 100 : s.recv ? 100 * s.lost / (s.lost + s.recv) : null;
  $('sLoss').textContent = loss != null ? loss.toFixed(1) + '%' : '—';
  if (s.w) $('hres').textContent = `${s.w}×${s.h}` + ((s.enc || s.dec) ? ' · ' + (s.enc || s.dec) : '');
  if (s.fps != null) $('hfps').textContent = Math.round(s.fps) + ' fps';
}, 1000);

window.addEventListener('beforeunload', leave);
render();
