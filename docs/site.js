// Lume — landing page: abas da vitrine, descrições, entradas ao rolar e o código ao vivo.
'use strict';
document.documentElement.classList.add('js');

// Entrada das partes de cada seção, uma vez, quando aparecem.
const io = 'IntersectionObserver' in window ? new IntersectionObserver(es => {
  for (const e of es) if (e.isIntersecting) { e.target.classList.add('in'); io.unobserve(e.target); }
}, { rootMargin: '0px 0px -10% 0px' }) : null;
document.querySelectorAll('[data-r]').forEach(el => io ? io.observe(el) : el.classList.add('in'));

// Vitrine: as abas trocam a imagem e a frase.
const tabs = document.querySelectorAll('.tabs [role=tab]');
tabs.forEach(t => t.addEventListener('click', () => {
  tabs.forEach(x => x.setAttribute('aria-selected', String(x === t)));
  const img = document.getElementById('plateImg');
  img.style.opacity = '0';
  setTimeout(() => { img.src = t.dataset.img; img.alt = t.dataset.alt; img.onload = () => { img.style.opacity = '1'; }; }, 180);
  document.getElementById('plateText').textContent = t.dataset.t;
}));

// O que vem junto: passar o mouse (ou focar) mostra o que o item faz.
const desc = document.getElementById('featDesc');
document.querySelectorAll('.group li').forEach(li => {
  const show = () => { document.querySelectorAll('.group li.on').forEach(x => x.classList.remove('on')); li.classList.add('on'); desc.textContent = li.dataset.d; };
  li.addEventListener('mouseenter', show);
  li.addEventListener('focus', show);
});

// O código ao vivo: mesma conta do app (scrypt -> endereço e chave), feita pelo lume-web.js.
const input = document.getElementById('code'), outTopic = document.getElementById('outTopic'), outKey = document.getElementById('outKey'), hint = document.getElementById('labHint');
let last = null, busy = 0;
function paint(el, now, before) {
  // Destaca o que ficou igual ao valor anterior (quase nada, quando muda uma letra).
  el.textContent = '';
  for (let i = 0; i < now.length; i++) {
    const s = document.createElement('span');
    s.className = before && before[i] === now[i] ? 'same' : 'diff';
    s.textContent = now[i];
    el.appendChild(s);
  }
  el.classList.remove('flash'); void el.offsetWidth; el.classList.add('flash');
}
async function update() {
  if (!window.LumeWeb || !window.LumeWeb.derive) return;
  const mine = ++busy;
  let r;
  try { r = await window.LumeWeb.derive(input.value); }
  catch (e) { if (mine === busy) hint.textContent = typeof e === 'string' ? e : 'Não consegui calcular aqui. Tente de novo.'; return; }
  if (mine !== busy) return;
  const prev = last;
  paint(outTopic, r.topic, prev && prev.topic);
  paint(outKey, r.key, prev && prev.key);
  if (prev && prev.code !== r.code) {
    const same = [...r.key].filter((c, i) => c === prev.key[i]).length;
    hint.textContent = `${prev.code} virou ${r.code}: dos 64 caracteres da chave, ${same} ficaram iguais por acaso. É outra sala.`;
  }
  last = r;
}
let t;
input.addEventListener('input', () => {
  clearTimeout(t);
  t = setTimeout(() => {
    if (input.value.replace(/[^0-9a-z]/gi, '').length === 12) update();
    else hint.textContent = 'O código tem 12 letras e números.';
  }, 250);
});
document.getElementById('dice').addEventListener('click', () => {
  if (!window.LumeWeb) return;
  input.value = window.LumeWeb.newCode();
  update();
});
update();
