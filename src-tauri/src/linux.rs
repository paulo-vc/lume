//! O Lume no Linux. A interface é a da versão web (`docs/app`). O jeito principal é abrir o Lume
//! numa janela de app do Chrome/Chromium/Edge/Brave da pessoa (`--app=`): o WebRTC e o
//! compartilhamento de tela deles funcionam bem. O navegador interno do sistema (WebKitGTK) fecha
//! sozinho ao compartilhar a tela em algumas distros (falha do WebKitWebProcess), então só é usado
//! se não houver nenhum desses navegadores — ou com `LUME_WEBKIT=1`.

use std::os::unix::fs::PermissionsExt;
use std::path::Path;
use std::process::{Command, Stdio};
use tauri::Manager;

const URL: &str = "https://paulo-vc.github.io/lume/app/";

/// Navegadores da família do Chrome, do jeito que cada distro instala (pacote, snap ou flatpak).
const BROWSERS: &[&[&str]] = &[
    &["google-chrome-stable"],
    &["google-chrome"],
    &["chromium"],
    &["/snap/bin/chromium"],
    &["chromium-browser"],
    &["microsoft-edge-stable"],
    &["microsoft-edge"],
    &["brave-browser"],
    &["brave"],
    &["/snap/bin/brave"],
    &["vivaldi-stable"],
    &["vivaldi"],
    &["flatpak", "run", "com.google.Chrome"],
    &["flatpak", "run", "org.chromium.Chromium"],
    &["flatpak", "run", "com.microsoft.Edge"],
    &["flatpak", "run", "com.brave.Browser"],
    &["flatpak", "run", "com.vivaldi.Vivaldi"],
    &["flatpak", "run", "io.github.ungoogled_software.ungoogled_chromium"],
];

fn is_exec(path: &Path) -> bool {
    path.metadata().map_or(false, |m| m.is_file() && (m.permissions().mode() & 0o111 != 0))
}

fn in_path(bin: &str) -> bool {
    if bin.contains('/') {
        return is_exec(Path::new(bin));
    }
    let appdir = std::env::var_os("APPDIR");
    std::env::var_os("PATH").map_or(false, |p| {
        std::env::split_paths(&p)
            .filter(|d| appdir.as_ref().map_or(true, |a| !d.starts_with(a)))
            .any(|d| is_exec(&d.join(bin)))
    })
}

/// Limpa variáveis colocadas pelo AppImage (LD_LIBRARY_PATH, GTK, GIO, etc.) para que o navegador
/// ou o flatpak do sistema não carreguem bibliotecas de dentro do AppImage — que desmonta assim
/// que este processo sai.
fn host_command(program: &str) -> Command {
    let mut cmd = Command::new(program);
    let Ok(appdir) = std::env::var("APPDIR") else {
        return cmd;
    };
    if appdir.is_empty() {
        return cmd;
    }
    for var in ["LD_LIBRARY_PATH", "XDG_DATA_DIRS", "GI_TYPELIB_PATH", "PATH"] {
        if let Some(val) = std::env::var_os(var) {
            let kept: Vec<_> = std::env::split_paths(&val).filter(|p| !p.starts_with(&appdir)).collect();
            if kept.is_empty() {
                cmd.env_remove(var);
            } else if let Ok(joined) = std::env::join_paths(kept) {
                cmd.env(var, joined);
            }
        }
    }
    for var in [
        "GTK_PATH",
        "GTK_EXE_PREFIX",
        "GTK_DATA_PREFIX",
        "GDK_PIXBUF_MODULE_FILE",
        "GDK_PIXBUF_MODULEDIR",
        "GIO_MODULE_DIR",
        "GSETTINGS_SCHEMA_DIR",
        "GST_PLUGIN_PATH",
        "GST_PLUGIN_SYSTEM_PATH",
        "GST_PLUGIN_SCANNER",
        "QT_PLUGIN_PATH",
        "PYTHONHOME",
        "PYTHONPATH",
    ] {
        if let Ok(val) = std::env::var(var) {
            if val.contains(&appdir) {
                cmd.env_remove(var);
            }
        }
    }
    for var in ["APPDIR", "APPIMAGE", "ARGV0", "OWD"] {
        cmd.env_remove(var);
    }
    if let Ok(cwd) = std::env::current_dir() {
        if cwd.starts_with(&appdir) {
            if let Ok(owd) = std::env::var("OWD") {
                cmd.current_dir(owd);
            } else if let Ok(home) = std::env::var("HOME") {
                cmd.current_dir(home);
            }
        }
    }
    cmd
}

fn flatpak_has(app: &str) -> bool {
    host_command("flatpak")
        .args(["info", app])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .map_or(false, |s| s.success())
}

/// Abre o Lume numa janela de app de um navegador Chromium. Devolve false se não achou nenhum.
fn open_in_browser() -> bool {
    let app_arg = format!("--app={URL}");
    for cmd in BROWSERS {
        let ok = if cmd[0] == "flatpak" { in_path("flatpak") && flatpak_has(cmd[2]) } else { in_path(cmd[0]) };
        if !ok {
            continue;
        }
        let spawned = host_command(cmd[0])
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
