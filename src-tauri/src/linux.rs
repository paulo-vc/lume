//! O Lume no Linux. A interface é a da versão web (`docs/app`). O jeito principal é abrir o Lume
//! numa janela de app do Chrome/Chromium/Edge/Brave da pessoa (`--app=`): o WebRTC e o
//! compartilhamento de tela deles funcionam bem. O navegador interno do sistema (WebKitGTK) fecha
//! sozinho ao compartilhar a tela em algumas distros (falha do WebKitWebProcess), então só é usado
//! se não houver nenhum desses navegadores — ou com `LUME_WEBKIT=1`.

use std::process::{Command, Stdio};
use tauri::Manager;

const URL: &str = "https://paulo-vc.github.io/lume/app/";

/// Navegadores da família do Chrome, do jeito que cada distro instala (pacote, snap ou flatpak).
const BROWSERS: &[&[&str]] = &[
    &["google-chrome-stable"],
    &["google-chrome"],
    &["chromium"],
    &["chromium-browser"],
    &["microsoft-edge-stable"],
    &["microsoft-edge"],
    &["brave-browser"],
    &["vivaldi-stable"],
    &["/snap/bin/chromium"],
    &["flatpak", "run", "com.google.Chrome"],
    &["flatpak", "run", "org.chromium.Chromium"],
    &["flatpak", "run", "com.microsoft.Edge"],
    &["flatpak", "run", "com.brave.Browser"],
];

fn in_path(bin: &str) -> bool {
    if bin.contains('/') {
        return std::path::Path::new(bin).exists();
    }
    std::env::var_os("PATH").map_or(false, |p| std::env::split_paths(&p).any(|d| d.join(bin).is_file()))
}

fn flatpak_has(app: &str) -> bool {
    Command::new("flatpak").args(["info", app]).stdout(Stdio::null()).stderr(Stdio::null()).status().map_or(false, |s| s.success())
}

/// Abre o Lume numa janela de app de um navegador Chromium. Devolve false se não achou nenhum.
fn open_in_browser() -> bool {
    let app_arg = format!("--app={URL}");
    for cmd in BROWSERS {
        let ok = if cmd[0] == "flatpak" { in_path("flatpak") && flatpak_has(cmd[2]) } else { in_path(cmd[0]) };
        if !ok {
            continue;
        }
        let spawned = Command::new(cmd[0])
            .args(&cmd[1..])
            .arg(&app_arg)
            .stdin(Stdio::null())
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .spawn();
        if spawned.is_ok() {
            return true;
        }
    }
    false
}

pub fn run() {
    if std::env::var("LUME_WEBKIT").ok().as_deref() != Some("1") && open_in_browser() {
        return;
    }
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
