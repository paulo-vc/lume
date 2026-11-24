//! O Lume no Linux: a interface da versão web (`docs/app`, montada por `web/build.js`) numa janela
//! própria. Tudo acontece no navegador interno (WebKitGTK): sala cifrada pelos relays, conexões
//! WebRTC e compartilhamento de tela pelo PipeWire (xdg-desktop-portal). Aqui só ligamos o WebRTC
//! e a captura no WebKitGTK, que vêm desligados, e deixamos o pedido de tela chegar ao portal.

use tauri::Manager;

pub fn run() {
    tauri::Builder::default()
        .setup(|app| {
            if let Some(win) = app.get_webview_window("main") {
                let _ = win.with_webview(|wv| {
                    use webkit2gtk::{PermissionRequestExt, SettingsExt, WebViewExt};
                    let view = wv.inner();
                    if let Some(s) = view.settings() {
                        s.set_enable_webrtc(true);
                        s.set_enable_media_stream(true);
                        s.set_enable_mediasource(true);
                        s.set_enable_media_capabilities(true);
                    }
                    // Pedidos de câmera/tela vindos da própria interface do Lume: aceita. A escolha
                    // do que compartilhar continua sendo do sistema (janela do portal).
                    view.connect_permission_request(|_, req| {
                        req.allow();
                        true
                    });
                });
            }
            Ok(())
        })
        .run(tauri::generate_context!())
        .expect("falha ao iniciar o Lume");
}
