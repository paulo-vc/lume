// Lume na web: a mesma interface do app (ui/app.js), sem instalar nada. Serve para assistir e
// entrar em salas pelo navegador, no celular ou em qualquer PC. Transmitir continua no app.
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

  const commands = {
    join_room: joinRoom,
    cancel_join: () => { if (cancel) cancel(); },
    app_version: () => 'web',
  };
  window.LumeWeb = {
    invoke: async (cmd, args) => (commands[cmd] ? commands[cmd](args || {}) : undefined),
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
  load(); // começa a baixar as bibliotecas já
})();
