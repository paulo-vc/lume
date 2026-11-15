# Lume

Compartilhamento de tela P2P, portátil (um `.exe`), sem servidor e só por convite.

## Como funciona

- **Interface:** HTML/CSS em `ui/`, exibida pelo WebView2 (já vem no Windows 10/11).
- **Transmissão (nativa, em Rust):**
  - Captura de tela ou de uma janela específica pelo Windows Graphics Capture (`src-tauri/src/capture.rs`).
    Não aparece a barra "está compartilhando", e no Windows 11 nem a borda amarela.
  - Vídeo codificado em H.264 (OpenH264 por enquanto; encoder pela placa de vídeo é a próxima etapa).
  - Envio por WebRTC nativo (`webrtc-rs`, `src-tauri/src/stream.rs`), uma conexão por convidado.
- **Quem assiste:** recebe pelo WebRTC do WebView2, com decodificação pela placa de vídeo e buffer curto (40 ms).
- **Salas e convites:** `src-tauri/src/room.rs`. Convite com segredo de 256 bits que expira em 15 min.
  Pedidos e respostas são assinados com HMAC-SHA256, o que garante:
  - sem o convite não se gera um pedido válido;
  - pedidos não podem ser reutilizados;
  - o convidado confere se a resposta veio do anfitrião verdadeiro;
  - o anfitrião aprova cada pessoa.
- **Rede:** conexão direta; STUN público só para descobrir o endereço externo. Sem TURN, então
  NAT simétrico/CGNAT pode impedir a conexão (use IPv6, encaminhamento de porta ou Tailscale/ZeroTier).

## Entrar numa sala

1. O anfitrião cria a sala e envia o **convite** (`sl://join/…`).
2. O convidado cola o convite e devolve o **pedido** (`SLQ-…`).
3. O anfitrião cola o pedido, aprova e devolve a **resposta** (`SLR-…`).
4. O convidado cola a resposta e a conexão é direta.

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

- Áudio por janela (Process Loopback do Windows) e opção "PC inteiro menos a call".
- Encoder H.264 pela placa de vídeo (Media Foundation: NVENC / QuickSync / AMF).

- Troca automática dos códigos por relays Nostr públicos, cifrada com o segredo do convite.
- Pipeline nativo opcional (captura WGC + Media Foundation em Rust) para reduzir ainda mais o uso de CPU e RAM.
- Modo IP:porta direto para redes onde o STUN não basta.
