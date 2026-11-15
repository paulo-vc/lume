# Lume

Compartilhamento de tela P2P, portátil (um `.exe`), sem conta e só entra quem tem o código da sala.

## Como funciona

- **Interface:** HTML/CSS em `ui/`, exibida pelo WebView2 (já vem no Windows 10/11).
- **Transmissão (nativa, em Rust):**
  - Captura de tela ou de uma janela específica pelo Windows Graphics Capture (`src-tauri/src/capture.rs`).
    Não aparece a barra "está compartilhando", e no Windows 11 nem a borda amarela.
  - Vídeo codificado em H.264 pela placa de vídeo via Media Foundation (`src-tauri/src/mf.rs`:
    NVENC, Quick Sync ou AMF, modo de baixa latência, CBR). Sem GPU compatível, cai para o OpenH264 na CPU.
  - Áudio por aplicativo pelo Process Loopback do Windows (`src-tauri/src/audio.rs`), codificado em Opus
    (pacotes de 10 ms). Três modos: só o som da janela transmitida, PC inteiro menos um programa
    (ex.: Discord, para a call não voltar como eco) ou PC inteiro. Requer Windows 10 2004+.
  - Envio por WebRTC nativo (`webrtc-rs`, `src-tauri/src/stream.rs`), uma conexão por convidado.
- **Quem assiste:** recebe pelo WebRTC do WebView2, com decodificação pela placa de vídeo e buffer curto (40 ms).
- **Salas por código:** `src-tauri/src/room.rs`. Cada sala tem um código de 12 caracteres
  (ex.: `K7QM-2XPD-9RTA`, ~60 bits). Do código saem o endereço do ponto de encontro e a chave
  ChaCha20-Poly1305 que cifra e autentica o pedido de entrada e a resposta. Garante:
  - sem o código não se acha a sala nem se gera um pedido válido;
  - o convidado sabe que a resposta veio de quem tem o código;
  - o anfitrião aceita ou recusa cada pessoa.
- **Ponto de encontro:** `src-tauri/src/signal.rs`. Os dois lados trocam os dados de conexão por
  relays MQTT públicos (três ao mesmo tempo; basta um no ar). O relay só vê bytes cifrados e é usado
  apenas no aperto de mão; vídeo e áudio nunca passam por ele.
- **Rede:** conexão direta; STUN público só para descobrir o endereço externo. Sem TURN, então
  NAT simétrico/CGNAT pode impedir a conexão (use IPv6, encaminhamento de porta ou Tailscale/ZeroTier).

## Entrar numa sala

1. O anfitrião cria a sala e manda o **código** para quem quiser convidar.
2. O convidado digita o código e clica em **Entrar**.
3. No anfitrião aparece "Fulano quer entrar": **Aceitar** e pronto.

## Compilar

Requer Rust (MSVC). Se o caminho da pasta for muito longo, use uma pasta de saída curta:

```powershell
cd src-tauri
$env:CARGO_TARGET_DIR="C:\sltarget"
cargo build --release
# resultado: C:\sltarget\release\lume.exe  (portátil)
```

Testes do protocolo de convite: `cargo test`.

Testes nativos (capturam a tela de verdade): `cargo test -- --ignored`.

## Próximos passos

- Passar os quadros direto na GPU (textura D3D11 -> encoder), sem a cópia pela CPU.

- Pipeline nativo opcional (captura WGC + Media Foundation em Rust) para reduzir ainda mais o uso de CPU e RAM.
- Modo IP:porta direto para redes onde o STUN não basta.
