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

## Rede

- **Ponto de encontro:** os dois lados trocam os dados de conexão por relays MQTT públicos (três ao
  mesmo tempo; basta um no ar, `signal.rs`). O relay só vê bytes cifrados e é usado só no aperto de
  mão; vídeo e áudio nunca passam por ele.
- **Conexão direta:** IPv4 e IPv6, STUN público para descobrir o endereço externo e **UPnP**: o Lume
  pede ao roteador para abrir uma porta só para cada conexão e a fecha quando a pessoa sai (`net.rs`).
  Com a porta aberta de um dos lados, a conexão passa mesmo que o outro esteja em rede restritiva.
- **Limite:** não há servidor de retransmissão. Se as duas internets usarem CGNAT e nenhuma tiver
  IPv6, a conexão direta não é possível. O convite mostra se o seu roteador abriu a porta, se o UPnP
  está desligado ou se a sua internet usa CGNAT.

## Segurança

- **Código da sala** (12 caracteres, ~60 bits, `room.rs`): dele saem o endereço do encontro e a chave
  ChaCha20-Poly1305 que cifra e autentica o pedido de entrada e a resposta. Sem o código não se acha
  a sala nem se forja um pedido. Mesmo com o código, cada pessoa precisa ser aceita na primeira vez.
- **Vídeo, áudio e mensagens da sala:** WebRTC cifrado (DTLS-SRTP). As impressões digitais DTLS
  viajam dentro da mensagem cifrada pelo código, então ninguém no meio consegue se passar por um dos lados.
- **Entrada direta de quem já foi aceito:** cada PC tem um identificador aleatório guardado só nele,
  enviado cifrado no pedido. Ele nunca é repassado aos outros participantes.
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
