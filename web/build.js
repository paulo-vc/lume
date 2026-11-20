// Monta o site (GitHub Pages, pasta docs/): a landing page já está em docs/index.html; aqui a
// versão web do app vai para docs/app/, copiada da interface do app (ui/) mais web/lume-web.js.
// Rode depois de mudar a interface:  node web/build.js
const fs = require('fs');
const path = require('path');
const root = path.join(__dirname, '..');
const out = path.join(root, 'docs', 'app');
fs.mkdirSync(out, { recursive: true });
for (const f of ['app.js', 'style.css']) fs.copyFileSync(path.join(root, 'ui', f), path.join(out, f));
fs.copyFileSync(path.join(__dirname, 'lume-web.js'), path.join(out, 'lume-web.js'));
let html = fs.readFileSync(path.join(root, 'ui', 'index.html'), 'utf8');
html = html
  .replace('<title>Lume</title>', '<title>Lume na web</title>\n<meta name="description" content="Assista telas compartilhadas no Lume pelo navegador, sem instalar nada.">\n<link rel="icon" href="../icon.png">')
  .replace('<script src="app.js"></script>', '<script src="lume-web.js"></script>\n<script src="app.js"></script>');
if (!html.includes('lume-web.js')) throw new Error('index.html mudou: não achei onde colocar o lume-web.js');
fs.writeFileSync(path.join(out, 'index.html'), html);
fs.copyFileSync(path.join(root, 'src-tauri', 'icons', 'icon.png'), path.join(root, 'docs', 'icon.png'));
console.log('docs/app pronto');
