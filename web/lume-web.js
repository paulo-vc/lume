// Lume na web: a mesma interface do app (ui/app.js), sem instalar nada. Pelo navegador dá para
// entrar em salas e assistir (celular ou PC) e, no computador, criar salas e transmitir a tela
// (Linux, Mac, Chromebook). O que é exclusivo do app: áudio por programa, transmitir sem a barra
// do navegador, codificador próprio da placa e retransmissão pelo anfitrião.
//
// Faz no navegador o que o Rust faz no app, com o mesmo protocolo (src-tauri/src/room.rs e
// signal.rs): código da sala -> scrypt -> endereço no relay + chave ChaCha20-Poly1305;
// pedido de entrada cifrado e repetido a cada 4 s em três relays MQTT públicos (por WebSocket).
'use strict';

(() => {
  const LIBS = {
    mqtt: 'https://cdn.jsdelivr.net/npm/mqtt@5.10.1/dist/mqtt.esm.js',
    chacha: 'https://cdn.jsdelivr.net/npm/@noble/ciphers@1.0.0/chacha.js/+esm',
    scrypt: 'https://cdn.jsdelivr.net/npm/@noble/hashes@1.5.0/scrypt.js/+esm',
  };
  const RELAYS = ['wss://broker.emqx.io:8084/mqtt', 'wss://broker.hivemq.com:8884/mqtt', 'wss://test.mosquitto.org:8081/mqtt'];
  const ALPHABET = '0123456789ABCDEFGHJKMNPQRSTVWXYZ';
  const enc = new TextEncoder(), dec = new TextDecoder();
  const hex = b => [...b].map(x => x.toString(16).padStart(2, '0')).join('');
  const rand = n => crypto.getRandomValues(new Uint8Array(n));
  let libs = null;
  const load = () => libs || (libs = Promise.all([import(LIBS.mqtt), import(LIBS.chacha), import(LIBS.scrypt)])
    .then(([m, c, s]) => ({ mqtt: m.default || m, chacha: c.chacha20poly1305, scrypt: s.scryptAsync })));

  function normalize(code) {
    const s = String(code).toUpperCase().replace(/[^0-9A-Z]/g, '').replace(/O/g, '0').replace(/[IL]/g, '1');
    if (s.length !== 12 || [...s].some(c => !ALPHABET.includes(c))) throw 'Código inválido: são 12 letras e números, como K7QM-2XPD-9RTA';
    return `${s.slice(0, 4)}-${s.slice(4, 8)}-${s.slice(8)}`;
  }

  async function keys(code) {
    const { scrypt } = await load();
    const out = await scrypt(enc.encode(normalize(code)), enc.encode('lume-v2-room'), { N: 1 << 15, r: 8, p: 1, dkLen: 44 });
    return { key: out.slice(0, 32), topic: 'lume2/' + hex(out.slice(32)) };
  }

  async function pipe(bytes, stream) {
    return new Uint8Array(await new Response(new Blob([bytes]).stream().pipeThrough(stream)).arrayBuffer());
  }
  async function seal(k, obj) {
    const { chacha } = await load();
    const plain = await pipe(enc.encode(JSON.stringify(obj)), new CompressionStream('deflate-raw'));
    const nonce = rand(12);
    const ct = chacha(k.key, nonce).encrypt(plain);
    const out = new Uint8Array(12 + ct.length); out.set(nonce); out.set(ct, 12);
    return out;
  }
  async function open(k, data) {
    try {
      const { chacha } = await load();
      if (data.length < 28) return null;
      const plain = chacha(k.key, data.slice(0, 12)).decrypt(data.slice(12));
      const raw = await pipe(plain, new DecompressionStream('deflate-raw'));
      if (raw.length > 256 * 1024) return null;
      return JSON.parse(dec.decode(raw));
    } catch { return null; }
  }
  async function proof(uid, id, sdp) {
    const key = await crypto.subtle.importKey('raw', enc.encode(uid), { name: 'HMAC', hash: 'SHA-256' }, false, ['sign']);
    return hex(new Uint8Array(await crypto.subtle.sign('HMAC', key, enc.encode(id + '\n' + sdp))));
  }

  let cancel = null;
  async function joinRoom({ code, name, av, uid, offerSdp }) {
    const k = await keys(code);
    const id = hex(rand(8));
    const req = { id, name: String(name).slice(0, 32), sdp: offerSdp, av: av || '', proof: uid ? await proof(uid, id, offerSdp) : '' };
    const sealed = await seal(k, req);
    const { mqtt } = await load();
    const answerTopic = `${k.topic}/a/${id}`;
    const clients = [];
    let online = 0;
    try {
      return await new Promise((resolve, reject) => {
        cancel = () => reject('Cancelado');
        for (const url of RELAYS) {
          const c = mqtt.connect(url, { clientId: 'lume-' + hex(rand(8)), keepalive: 20, reconnectPeriod: 2000, connectTimeout: 8000, clean: true });
          let up = false;
          c.on('connect', () => { if (!up) { up = true; online++; } c.subscribe(answerTopic, { qos: 1 }); c.publish(`${k.topic}/q`, sealed, { qos: 1 }); });
          c.on('close', () => { if (up) { up = false; online--; } });
          c.on('error', () => {});
          c.on('message', async (_, payload) => {
            const res = await open(k, new Uint8Array(payload));
            if (!res) return;
            if (res.ok) resolve(String(res.sdp));
            else reject(res.why || 'O anfitrião recusou a entrada');
          });
          clients.push(c);
        }
        const started = Date.now();
        const tick = setInterval(() => {
          if (Date.now() - started > 10000 && online === 0) { clearInterval(tick); return reject('Sem acesso aos servidores de encontro. Verifique a internet.'); }
          if (Date.now() - started > 180000) { clearInterval(tick); return reject('O anfitrião não respondeu. Confira o código e se a sala está aberta.'); }
          // Repete o pedido: cobre relay que caiu ou anfitrião que abriu a sala depois.
          for (const c of clients) if (c.connected) c.publish(`${k.topic}/q`, sealed, { qos: 1 });
        }, 4000);
        const done = () => clearInterval(tick);
        cancel = () => { done(); reject('Cancelado'); };
        const r0 = resolve, j0 = reject;
        resolve = v => { done(); r0(v); }; reject = e => { done(); j0(e); };
      });
    } finally {
      cancel = null;
      for (const c of clients) c.end(true);
    }
  }

  // ---------- eventos (o que o Rust manda para a interface no app) ----------
  const handlers = new Map();
  const emit = (ev, payload) => (handlers.get(ev) || []).forEach(cb => { try { cb({ payload }); } catch {} });
  const listen = (ev, cb) => { if (!handlers.has(ev)) handlers.set(ev, []); handlers.get(ev).push(cb); return Promise.resolve(() => {}); };

  const ICE = [{ urls: ['stun:stun.l.google.com:19302', 'stun:stun.cloudflare.com:3478'] }];
  const gather = (pc, ms = 4000) => pc.iceGatheringState === 'complete' ? Promise.resolve() : new Promise(res => {
    const t = setTimeout(res, ms);
    pc.addEventListener('icegatheringstatechange', () => { if (pc.iceGatheringState === 'complete') { clearTimeout(t); res(); } });
  });

  // ---------- transmissão pelo navegador ----------
  // Compartilhamento de tela do próprio navegador (Chrome, Edge, Firefox; no Linux pelo PipeWire).
  // Diferente do app: o navegador mostra a barra "compartilhando" e o som é o da aba ou do sistema.
  let share = null; // MediaStream
  let quality = { br: 8, fps: 60 };
  let capKbps = 0;
  const senders = () => [...peers.values(), ...viewers.values()].flatMap(p => p.pc.getSenders());
  function applyTracks(pc) {
    for (const t of pc.getTransceivers()) {
      const kind = t.receiver.track && t.receiver.track.kind;
      const track = share && share.getTracks().find(x => x.kind === kind) || null;
      try { t.direction = 'sendonly'; } catch {}
      t.sender.replaceTrack(track).catch(() => {});
    }
  }
  async function applyBitrate() {
    const max = (capKbps > 0 ? Math.min(capKbps, quality.br * 1000) : quality.br * 1000) * 1000;
    for (const s of senders()) {
      if (!s.track || s.track.kind !== 'video') continue;
      const p = s.getParameters();
      if (!p.encodings || !p.encodings.length) p.encodings = [{}];
      p.encodings[0].maxBitrate = max;
      p.encodings[0].maxFramerate = quality.fps;
      s.setParameters(p).catch(() => {});
    }
  }
  async function startShareWeb() {
    if (!navigator.mediaDevices || !navigator.mediaDevices.getDisplayMedia) throw 'Este navegador não deixa compartilhar a tela';
    const stream = await navigator.mediaDevices.getDisplayMedia({ video: { frameRate: quality.fps }, audio: true });
    stopShareWeb();
    share = stream;
    const v = stream.getVideoTracks()[0];
    if (v) { v.contentHint = 'motion'; v.onended = () => emit('share-ended', null); }
    for (const p of [...peers.values(), ...viewers.values()]) applyTracks(p.pc);
    applyBitrate();
    // O rótulo do navegador às vezes é um identificador interno; só usa quando parece um nome.
    const label = v && v.label || '';
    return /^(screen|window|web-contents-media-stream):|^[A-Za-z0-9+/=]{24,}$/.test(label) || !label ? 'Tela' : label.slice(0, 60);
  }
  function stopShareWeb() {
    if (!share) return;
    share.getTracks().forEach(t => t.stop());
    share = null;
    for (const s of senders()) s.replaceTrack(null).catch(() => {});
  }
  // Estatística da minha transmissão (o app mostra isso na minha tela da grade).
  setInterval(() => {
    if (!share) return;
    const s = share.getVideoTracks()[0] && share.getVideoTracks()[0].getSettings();
    if (s) emit('enc-stats', { width: s.width || 0, height: s.height || 0, fps: s.frameRate || 0, kbps: 0, encoder: 'navegador', src_fps: s.frameRate || 0, skipped: 0, key_requests: 0 });
  }, 1000);

  // ---------- anfitrião no navegador ----------
  // O mesmo que main.rs faz no app: escuta pedidos cifrados nos relays, aceita, responde.
  let host = null;
  const peers = new Map();   // nome -> { pc, dc } (convidados, no anfitrião)
  const viewers = new Map(); // nome -> { pc } (quem assiste a minha tela, sendo convidado)
  const MAX = 8;

  async function startBus(code) {
    const { mqtt } = await load();
    const k = await keys(code);
    const h = { code: normalize(code), keys: k, clients: [], pending: new Map(), answered: new Map(), accepting: new Set(), members: [] };
    for (const url of RELAYS) {
      const c = mqtt.connect(url, { clientId: 'lume-' + hex(rand(8)), keepalive: 20, reconnectPeriod: 2000, connectTimeout: 8000, clean: true });
      c.on('connect', () => c.subscribe(`${k.topic}/q`, { qos: 1 }));
      c.on('error', () => {});
      c.on('message', async (_, payload) => {
        if (host !== h) return;
        const req = await open(k, new Uint8Array(payload));
        if (!req || typeof req.id !== 'string' || req.id.length > 32) return;
        if (h.answered.has(req.id)) return publish(h, req.id, h.answered.get(req.id));
        const p = h.pending.get(req.id);
        if (p) { p.at = Date.now(); return; }
        if (h.pending.size >= 16) return;
        req.name = String(req.name || '').trim().slice(0, 32) || 'Convidado';
        req.av = typeof req.av === 'string' && req.av.length <= 24000 && req.av.startsWith('data:image/') ? req.av : '';
        h.pending.set(req.id, { req, at: Date.now() });
        emit('join-request', { id: req.id, name: req.name, av: req.av, proof: !!req.proof });
      });
      h.clients.push(c);
    }
    return h;
  }
  function stopBus(h) { if (h) for (const c of h.clients) c.end(true); }
  function publish(h, id, sealed) { for (const c of h.clients) if (c.connected) c.publish(`${h.keys.topic}/a/${id}`, sealed, { qos: 1 }); }
  async function answer(h, id, res) { const s = await seal(h.keys, res); h.answered.set(id, s); publish(h, id, s); }

  function wirePeer(name, pc) {
    const entry = { pc, dc: null };
    pc.ondatachannel = e => {
      const dc = e.channel; entry.dc = dc;
      let opened = false;
      const open = () => { if (!opened) { opened = true; emit('ctl-open', name); } };
      dc.onopen = open;
      dc.onmessage = m => { open(); if (typeof m.data === 'string' && m.data.length <= 65536) emit('ctl-msg', [name, m.data]); };
      if (dc.readyState === 'open') open();
    };
    let sent = false;
    const deadline = setTimeout(() => { if (pc.connectionState !== 'connected') emit('peer-state', [name, 'failed']); }, 30000);
    pc.onconnectionstatechange = () => {
      const st = pc.connectionState;
      if (st === 'connected') clearTimeout(deadline);
      if (st === 'connected' && !sent) { sent = true; }
      emit('peer-state', [name, st]);
    };
    return entry;
  }

  // Responde a uma oferta "só recebe" com a minha tela (se houver).
  async function answerOffer(sdp) {
    const pc = new RTCPeerConnection({ iceServers: ICE });
    await pc.setRemoteDescription({ type: 'offer', sdp });
    applyTracks(pc);
    await pc.setLocalDescription(await pc.createAnswer());
    await gather(pc);
    return pc;
  }

  const commands = {
    join_room: joinRoom,
    cancel_join: () => { if (cancel) cancel(); },
    app_version: () => 'web',

    create_room: async ({ name, code }) => {
      if (host) { stopBus(host); host = null; }
      const h = await startBus(code || window.LumeWeb.newCode());
      h.name = String(name || 'Sala').slice(0, 40);
      host = h;
      return { name: h.name, code: h.code };
    },
    new_code: async () => {
      if (!host) throw 'Nenhuma sala aberta';
      const old = host;
      const h = await startBus(window.LumeWeb.newCode());
      h.name = old.name; h.members = old.members;
      stopBus(old); host = h;
      return { name: h.name, code: h.code };
    },
    relays_online: () => host ? host.clients.filter(c => c.connected).length : 0,
    list_requests: () => {
      if (!host) return [];
      for (const [id, p] of host.pending) if (!host.accepting.has(id) && Date.now() - p.at > 15000) host.pending.delete(id);
      return [...host.pending.keys()];
    },
    check_proof: async ({ id, uids }) => {
      const p = host && host.pending.get(id);
      if (!p || !p.req.proof || p.req.proof.length !== 64) return null;
      for (const u of (uids || []).slice(0, 64)) if (await proof(u, p.req.id, p.req.sdp) === p.req.proof) return u;
      return null;
    },
    approve_request: async ({ id }) => {
      const h = host; if (!h) throw 'Nenhuma sala aberta';
      const p = h.pending.get(id); if (!p) throw 'Esse pedido não existe mais';
      if (h.accepting.has(id)) throw 'Esse pedido já está sendo aceito';
      if (h.members.length >= MAX) { h.pending.delete(id); await answer(h, id, { ok: false, sdp: '', why: 'A sala está cheia' }); throw 'A sala está cheia. Remova alguém para liberar uma vaga.'; }
      h.accepting.add(id);
      let name = p.req.name; const base = name; let n = 2;
      while (h.members.includes(name)) name = `${base} (${n++})`;
      h.members.push(name);
      try {
        const pc = await answerOffer(p.req.sdp);
        peers.set(name, wirePeer(name, pc));
        // o canal de controle chega pelo ondatachannel, ligado agora (antes de a conexão abrir)
        await answer(h, id, { ok: true, sdp: pc.localDescription.sdp, why: '' });
        applyBitrate();
        return name;
      } catch (e) {
        h.members = h.members.filter(m => m !== name);
        await answer(h, id, { ok: false, sdp: '', why: 'O anfitrião não conseguiu abrir a conexão' });
        throw String(e);
      } finally { h.pending.delete(id); h.accepting.delete(id); }
    },
    deny_request: async ({ id }) => {
      if (host && !host.accepting.has(id) && host.pending.delete(id)) await answer(host, id, { ok: false, sdp: '', why: 'O anfitrião recusou a entrada' });
    },
    ctl_send: ({ name, text }) => { const p = peers.get(name); if (!p || !p.dc || p.dc.readyState !== 'open') throw 'Essa pessoa não está conectada'; p.dc.send(text); },
    remove_peer: ({ name }) => {
      for (const m of [peers, viewers]) { const p = m.get(name); if (p) { p.pc.close(); m.delete(name); } }
      if (host) host.members = host.members.filter(m => m !== name);
    },
    close_room: ({ keepShare } = {}) => {
      if (!keepShare) stopShareWeb();
      for (const m of [peers, viewers]) { for (const p of m.values()) p.pc.close(); m.clear(); }
      stopBus(host); host = null;
      if (cancel) cancel();
    },

    // transmitir
    start_share_web: startShareWeb,
    stop_share: stopShareWeb,
    set_quality: ({ quality: q }) => { if (q) { quality = { br: +q.br || 8, fps: +q.fps || 60 }; applyBitrate(); } },
    set_bitrate: ({ kbps }) => { capKbps = +kbps || 0; applyBitrate(); },
    accept_viewer: async ({ name, offerSdp }) => {
      const old = viewers.get(name); if (old) old.pc.close();
      if (viewers.size >= MAX + 2 && !old) throw 'Muita gente assistindo';
      const pc = await answerOffer(offerSdp);
      viewers.set(name, { pc });
      applyBitrate();
      return pc.localDescription.sdp;
    },
    drop_viewers: () => { for (const p of viewers.values()) p.pc.close(); viewers.clear(); },
    // a retransmissão pelo anfitrião só existe no app
    relay_pull_offer: () => { throw 'A retransmissão só funciona quando o anfitrião usa o app'; },
  };
  window.LumeWeb = {
    invoke: async (cmd, args) => (commands[cmd] ? commands[cmd](args || {}) : undefined),
    listen,
    // Para a landing page: o que sai de um código (endereço no relay e chave). Mesma conta do app.
    derive: async code => { const k = await keys(code); return { code: normalize(code), topic: k.topic, key: hex(k.key) }; },
    newCode: () => { const b = rand(12); const s = [...b].map(x => ALPHABET[x & 31]).join(''); return `${s.slice(0, 4)}-${s.slice(4, 8)}-${s.slice(8)}`; },
    // Para conferir contra os valores de referência do Rust (room.rs: reference_vectors).
    selfTest: async () => {
      const k = await keys('k7qm 2xpd 9rta');
      return { topic: k.topic, key: hex(k.key), proof: await proof('0123456789abcdef0123456789abcdef', 'a1b2c3d4e5f60718', 'v=0\r\n') };
    },
  };
  document.documentElement.classList.add('web');
  // Celular: o navegador não deixa compartilhar a tela, então só dá para assistir.
  if (!navigator.mediaDevices || !navigator.mediaDevices.getDisplayMedia) document.documentElement.classList.add('noshare');
  load(); // começa a baixar as bibliotecas já
})();
