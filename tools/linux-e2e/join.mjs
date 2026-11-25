// Teste de ponta a ponta no Linux: abre a versão web no Chrome e tenta entrar numa sala.
// Uso (no GitHub Actions): node join.mjs <url da versão web> <código> [segundos]
// Mostra onde o processo de entrada para: o status da janela "Entrar", erros do console,
// pedidos de rede que falharam e o estado da conexão.
import { spawn } from 'node:child_process';
import { mkdtempSync } from 'node:fs';
import { tmpdir } from 'node:os';
import { join } from 'node:path';

const [url, code, secs = '120'] = process.argv.slice(2);
const sleep = ms => new Promise(r => setTimeout(r, ms));
const chrome = process.env.CHROME || 'google-chrome';
// (Chromium do snap não enxerga o /tmp de fora: PROFILE_DIR aponta para uma pasta na home)
const dir = process.env.PROFILE_DIR ? mkdtempSync(join(process.env.PROFILE_DIR, 'lume-')) : mkdtempSync(join(tmpdir(), 'lume-'));
const proc = spawn(chrome, ['--headless=new', '--no-first-run', '--no-default-browser-check', `--user-data-dir=${dir}`, '--remote-debugging-port=9333', '--use-fake-ui-for-media-stream', 'about:blank'], { stdio: 'ignore' });

let list;
for (let i = 0; i < 50; i++) { try { list = await (await fetch('http://127.0.0.1:9333/json')).json(); break; } catch { await sleep(200); } }
const page = list.find(t => t.type === 'page');
const ws = new WebSocket(page.webSocketDebuggerUrl);
await new Promise(r => ws.onopen = r);
let n = 0; const wait = new Map();
ws.onmessage = e => {
  const m = JSON.parse(e.data);
  if (wait.has(m.id)) { wait.get(m.id)(m); wait.delete(m.id); return; }
  if (m.method === 'Runtime.consoleAPICalled') console.log('[console]', m.params.type, m.params.args.map(a => a.value ?? a.description).join(' '));
  if (m.method === 'Runtime.exceptionThrown') console.log('[erro]', JSON.stringify(m.params.exceptionDetails.exception?.description || m.params.exceptionDetails.text));
  if (m.method === 'Log.entryAdded') console.log('[log]', m.params.entry.level, m.params.entry.text, m.params.entry.url || '');
  if (m.method === 'Network.loadingFailed') console.log('[rede falhou]', m.params.errorText, m.params.blockedReason || '');
  if (m.method === 'Network.webSocketCreated') console.log('[websocket]', m.params.url);
  if (m.method === 'Network.webSocketClosed') console.log('[websocket fechou]', m.params.requestId);
};
const call = (method, params = {}) => { const id = ++n; const p = new Promise(r => wait.set(id, r)); ws.send(JSON.stringify({ id, method, params })); return p; };
const ev = async expr => { const m = await call('Runtime.evaluate', { expression: `(async()=>{${expr}})()`, awaitPromise: true, returnByValue: true, userGesture: true }); return m.result.exceptionDetails ? 'EXCEÇÃO: ' + JSON.stringify(m.result.exceptionDetails.exception?.description || m.result.exceptionDetails.text) : m.result.result.value; };

await call('Runtime.enable'); await call('Log.enable'); await call('Network.enable');
await call('Page.navigate', { url });
await sleep(4000);
console.log('versão do Chrome:', (await call('Browser.getVersion')).result.product);
console.log('carregou:', await ev(`return { web: document.documentElement.className, temLumeWeb: !!window.LumeWeb, h264: (RTCRtpReceiver.getCapabilities('video')?.codecs || []).filter(c => /h264/i.test(c.mimeType)).length, compressao: typeof CompressionStream }`));
console.log('autoteste da cifra:', await ev(`const t = Date.now(); const r = await LumeWeb.selfTest(); return { ...r, ms: Date.now() - t }`));
await ev(`document.getElementById('meName').value='Linux'; document.querySelector('[data-open=join]').click(); await new Promise(r=>setTimeout(r,300)); document.getElementById('jCode').value=${JSON.stringify(code)}; document.getElementById('go').click(); return 'clicou'`);
const t0 = Date.now(); let last = '';
while (Date.now() - t0 < +secs * 1000) {
  const s = await ev(`return JSON.stringify({ status: document.getElementById('jStatus')?.textContent ?? '(janela fechada)', papel: S.role, conexao: S.guestPc && S.guestPc.connectionState, ice: S.guestPc && S.guestPc.iceGatheringState, toast: document.getElementById('toast').classList.contains('show') ? document.getElementById('toast').textContent : '' })`);
  if (s !== last) { console.log(`[${Math.round((Date.now() - t0) / 1000)}s]`, s); last = s; }
  if (/"papel":"guest".*"conexao":"connected"/.test(s)) {
    await sleep(6000);
    console.log('vídeo do anfitrião:', await ev(`const s=S.streams.get('@host'); const v=s&&s.el.querySelector('video'); return v ? [v.videoWidth, v.videoHeight] : null`));
    break;
  }
  await sleep(1000);
}
proc.kill();
process.exit(0);
