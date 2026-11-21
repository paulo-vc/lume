# Lume

Compartilhamento de tela entre amigos, direto de PC para PC. Um único `.exe` portátil, sem conta,
sem instalação e sem servidor próprio. Só entra na sala quem tem o código e foi aceito.

## O que faz

- **Todos transmitem:** qualquer pessoa da sala compartilha a tela ou uma janela. As telas ficam numa
  grade: clique para destacar uma, duplo clique para tela cheia, volume e mudo por tela.
- **Prévia da sua tela:** você vê exatamente o que os outros recebem. O botão 👁 desliga a prévia.
- **Sem barra de "está compartilhando":** captura nativa do Windows (no Windows 11, nem a borda amarela).
- **Áudio por programa:** só o som da janela escolhida, o PC inteiro menos a call (Discord, Teams…)
  para ninguém ouvir a própria voz de volta, ou o PC inteiro.
- **Leve:** a imagem vai da captura ao encoder sem sair da placa de vídeo.
- **Salas salvas:** ficam na barra lateral. Um clique reabre a sala com o mesmo código, e quem você
  já aceitou entra direto. Quem não está na sala aparece apagado em "Fora da sala".
- **Reconecta sozinho** se a conexão cair.
- **A sala continua sem o anfitrião:** se quem criou a sala sai ou cai, quem está há mais tempo
  assume com o mesmo código, e os outros reconectam nele sozinhos.
- **Versão web:** abre no navegador do celular ou de qualquer PC, sem instalar, para assistir e entrar
  em salas (`web/`, publicada em `docs/app/`). Criar salas e transmitir continua no app.
- **Aviso de versão nova** na barra lateral.

## Entrar numa sala

1. Quem cria a sala manda o **código** (ex.: `K7QM-2XPD-9RTA`).
2. O convidado digita o código e clica em **Entrar**.
3. Na primeira vez, quem criou a sala clica em **Aceitar**. Nas próximas, a pessoa entra direto.

## Como funciona

- **Interface:** HTML/CSS em `ui/`, exibida pelo WebView2 (já vem no Windows 10/11).
- **Captura:** Windows Graphics Capture (`src-tauri/src/capture.rs`).
- **Vídeo:** o processador de vídeo da GPU reduz e converte a textura para NV12 (`gpu.rs`) e o
  encoder H.264 da placa (NVENC, Quick Sync ou AMF, via Media Foundation, `mf.rs`) lê a textura
  direto. Se algo falhar, volta para a conversão na CPU; sem GPU compatível, usa o OpenH264.
- **Áudio:** Process Loopback do WASAPI, Opus em pacotes de 10 ms (`audio.rs`). O Windows só exclui
  um programa por captura; no modo "menos a call" o Lume captura cada programa que está tocando e
  mistura, deixando de fora a call e o próprio Lume.
- **Envio:** WebRTC nativo (`webrtc-rs`, `stream.rs`), uma conexão por pessoa que assiste. Quem
  assiste recebe pelo WebRTC do WebView2, com decodificação pela placa e buffer curto (40 ms).
- **Sala:** cada convidado tem uma conexão com quem criou a sala (tela + canal de controle). Para
  ver a tela de outro convidado, abre-se uma conexão direta entre os dois; a negociação passa pelo
  canal de controle.
- **Troca de anfitrião:** o anfitrião manda ao herdeiro (quem está há mais tempo), só pelo canal
  cifrado, a lista de quem já foi aceito. Se o anfitrião sai, avisa quem assume; se cai sem avisar,
  o herdeiro primeiro tenta voltar a ele e só assume se ninguém responder.
- **Versão web:** `web/lume-web.js` faz no navegador o mesmo que o Rust (scrypt, ChaCha20-Poly1305,
  MQTT por WebSocket) e a interface é a mesma do app. `node web/build.js` monta `docs/app/`.
- **Site:** `docs/index.html` (landing page) e `docs/app/` (versão web), para o GitHub Pages.

## Rede

- **Ponto de encontro:** os dois lados trocam os dados de conexão por relays MQTT públicos (três ao
  mesmo tempo; basta um no ar, `signal.rs`). O relay só vê bytes cifrados e é usado só no aperto de
  mão; vídeo e áudio nunca passam por ele.
- **Conexão direta:** IPv4 e IPv6, STUN público para descobrir o endereço externo e **UPnP**: o Lume
  pede ao roteador para abrir uma porta só para cada conexão e a fecha quando a pessoa sai (`net.rs`).
  Com a porta aberta de um dos lados, a conexão passa mesmo que o outro esteja em rede restritiva.
  Vale também para quem assiste: as portas da conexão de quem assiste são abertas no roteador dele
  e o IP público vai junto na oferta (`net::expose_offer`).
- **Retransmissão pelo anfitrião** (`relay.rs`): quando dois convidados não conseguem conexão direta
  (os dois em rede restritiva), quem assiste passa a ver a tela pelo anfitrião, que já está conectado
  aos dois. O anfitrião puxa a tela de quem transmite uma vez e repassa os mesmos pacotes (sem
  recodificar) para cada pessoa que precisar; os pedidos de quadro-chave são repassados de volta.
  Custa ao anfitrião, no upload, o bitrate da transmissão por pessoa retransmitida.
- **Pedidos de quadro-chave** (`keyreq.rs`): um interceptador entrega ao app os pedidos de imagem
  completa (PLI/FIR) de quem assiste, que a biblioteca de WebRTC descartaria. Assim uma perda de
  pacote é recuperada na hora, em vez de esperar o quadro-chave automático (a cada 10 s).
- **Limite:** não há servidor externo de retransmissão. Se a internet do anfitrião e a de um
  convidado usarem CGNAT e nenhuma tiver IPv6, os dois não se conectam. O convite mostra se o seu
  roteador abriu a porta, se o UPnP está desligado ou se a sua internet usa CGNAT.

## Segurança

- **Código da sala** (12 caracteres, ~60 bits, sorteados pelo gerador seguro do sistema, `room.rs`):
  dele saem, por scrypt (2^15, r=8), o endereço do encontro e a chave ChaCha20-Poly1305 que cifra e
  autentica o pedido de entrada e a resposta. O scrypt custa ~0,1 s e 32 MB por tentativa, então
  testar códigos às cegas (mesmo vendo os endereços no relay público) fica fora de alcance. Uma letra
  trocada dá outro endereço e outra chave. Mesmo com o código, cada pessoa é aceita na primeira vez.
- **Vídeo, áudio e mensagens da sala:** WebRTC cifrado (DTLS-SRTP). As impressões digitais DTLS
  viajam dentro da mensagem cifrada pelo código, então ninguém no meio consegue se passar por um dos lados.
- **Entrada direta de quem já foi aceito:** cada PC tem um identificador aleatório guardado só nele.
  Ele só viaja pelo canal cifrado de ponta a ponta, depois de conectar; nunca pelo relay. O pedido de
  entrada leva apenas uma prova (HMAC do identificador sobre este pedido e esta oferta), que não serve
  para outro pedido. Assim, nem quem tem o código consegue se passar por outra pessoa.
- **Interface:** política de conteúdo (CSP) que só permite os arquivos do próprio app; nomes e fotos
  vindos de outras pessoas são tratados como texto ou imagem embutida validada.
- **Nada é coletado.** A única consulta externa além dos relays e do STUN é a página pública de
  lançamentos do GitHub, para o aviso de versão nova.

## Compilar

Requer Rust (MSVC). Se o caminho da pasta for muito longo, use uma pasta de saída curta:

```powershell
cd src-tauri
$env:CARGO_TARGET_DIR="C:\sltarget"
cargo build --release
# resultado: C:\sltarget\release\lume.exe  (portátil)
```

Testes: `cargo test`. Testes que usam a tela, o áudio e o roteador de verdade: `cargo test -- --ignored`.
`LUME_CPU=1` força o caminho antigo de vídeo (conversão na CPU), para comparar.
