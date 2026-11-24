#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]
//! Lume. No Windows, o app completo (captura, codificador, áudio e conexões nativas, `win.rs`).
//! No Linux, a versão web dentro de uma janela própria (`linux.rs`): sala, transmissão e conexões
//! pelo WebRTC do navegador interno, com a captura de tela pelo PipeWire.

#[cfg(windows)]
mod audio;
#[cfg(windows)]
mod capture;
#[cfg(windows)]
mod gpu;
#[cfg(windows)]
mod keyreq;
#[cfg(windows)]
mod mf;
#[cfg(windows)]
mod net;
#[cfg(windows)]
mod pcp;
#[cfg(windows)]
mod relay;
#[cfg(windows)]
mod room;
#[cfg(windows)]
mod signal;
#[cfg(windows)]
mod stream;
#[cfg(windows)]
mod win;

#[cfg(not(windows))]
mod linux;

fn main() {
    #[cfg(windows)]
    win::run();
    #[cfg(not(windows))]
    linux::run();
}
