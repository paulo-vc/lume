#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod audio;
mod capture;
mod gpu;
mod mf;
mod net;
mod room;
mod signal;
mod stream;

use room::{JoinReq, JoinRes, Keys};
use serde::Serialize;
use signal::Bus;
use std::collections::HashMap;
use std::sync::atomic::Ordering;
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};
use stream::{Engine, Quality};
use tauri::{AppHandle, Emitter, Manager, State};
use tokio::sync::Notify;
use webrtc::peer_connection::PeerConnection;

struct Host {
    name: String,
    code: String,
    keys: Keys,
    bus: Arc<Bus>,
    listener: tokio::task::JoinHandle<()>,
    /// pedidos esperando o anfitrião aceitar ou recusar
    pending: HashMap<String, (JoinReq, Instant)>,
    /// respostas já dadas (reenviadas se o convidado repetir o pedido)
    answered: HashMap<String, Vec<u8>>,
    /// pedidos com o Aceitar em andamento
    accepting: std::collections::HashSet<String>,
    members: Vec<String>,
}

impl Drop for Host {
    fn drop(&mut self) {
        self.listener.abort();
    }
}

struct AppState {
    host: Mutex<Option<Host>>,
    guest_cancel: Mutex<Option<Arc<Notify>>>,
    engine: Arc<Engine>,
    peers: tokio::sync::Mutex<HashMap<String, Arc<dyn PeerConnection>>>,
}

#[derive(Serialize)]
struct RoomInfo {
    name: String,
    code: String,
}

// ---------- sala (anfitrião) ----------

/// Abre o ponto de encontro da sala e fica ouvindo pedidos de entrada.
fn listen_requests(app: AppHandle, keys: Keys) -> (Arc<Bus>, tokio::task::JoinHandle<()>) {
    let (bus, mut rx) = Bus::start(keys.requests());
    let bus = Arc::new(bus);
    let task = tokio::spawn(async move {
        while let Some(msg) = rx.recv().await {
            // Só abre quem tem o código da sala; o resto é ruído.
            let Some(mut req) = keys.open::<JoinReq>(&msg) else { continue };
            let st = app.state::<AppState>();
            let mut g = st.host.lock().unwrap();
            let Some(h) = g.as_mut() else { break };
            if h.keys.topic != keys.topic {
                break; // código trocado: este ouvinte ficou velho
            }
            if let Some(ans) = h.answered.get(&req.id) {
                h.bus.publish(&keys.answer(&req.id), ans);
                continue;
            }
            if let Some(p) = h.pending.get_mut(&req.id) {
                p.1 = Instant::now(); // o convidado continua esperando
                continue;
            }
            if h.pending.len() >= 16 || req.id.len() > 32 {
                continue;
            }
            if req.av.len() > 24_000 || !req.av.starts_with("data:image/") {
                req.av.clear();
            }
            req.name = req.name.trim().chars().take(32).collect();
            if req.name.is_empty() {
                req.name = "Convidado".into();
            }
            let _ = app.emit("join-request", serde_json::json!({ "id": req.id, "name": req.name, "av": req.av, "proof": !req.proof.is_empty() }));
            h.pending.insert(req.id.clone(), (req, Instant::now()));
        }
    });
    (bus, task)
}

#[tauri::command]
async fn create_room(app: AppHandle, st: State<'_, AppState>, name: String, code: Option<String>) -> Result<RoomInfo, String> {
    // Sala salva: reabre com o mesmo código, e quem já foi convidado entra de novo com ele.
    let code = match code {
        Some(c) if !c.trim().is_empty() => room::normalize(&c)?,
        _ => room::new_code(),
    };
    let keys = Keys::from_code(&code)?;
    let (bus, listener) = listen_requests(app, keys.clone());
    let info = RoomInfo { name: name.clone(), code: code.clone() };
    *st.host.lock().unwrap() =
        Some(Host { name, code, keys, bus, listener, pending: HashMap::new(), answered: HashMap::new(), accepting: Default::default(), members: vec![] });
    Ok(info)
}

/// Troca o código da sala (o antigo para de funcionar). Quem já está dentro continua.
#[tauri::command]
async fn new_code(app: AppHandle, st: State<'_, AppState>) -> Result<RoomInfo, String> {
    let code = room::new_code();
    let keys = Keys::from_code(&code)?;
    let (bus, listener) = listen_requests(app, keys.clone());
    let mut g = st.host.lock().unwrap();
    let h = g.as_mut().ok_or("Nenhuma sala aberta")?;
    h.listener.abort();
    (h.code, h.keys, h.bus, h.listener) = (code.clone(), keys, bus, listener);
    h.pending.clear();
    h.answered.clear();
    Ok(RoomInfo { name: h.name.clone(), code })
}

/// Quantos relays estão no ar (0 = ninguém consegue pedir para entrar).
#[tauri::command]
fn relays_online(st: State<AppState>) -> usize {
    st.host.lock().unwrap().as_ref().map(|h| h.bus.online.load(Ordering::SeqCst)).unwrap_or(0)
}

fn answer(h: &mut Host, id: &str, res: &JoinRes) {
    let sealed = h.keys.seal(res);
    h.bus.publish(&h.keys.answer(id), &sealed);
    h.answered.insert(id.to_string(), sealed);
}

/// Aceita: cria a conexão nativa para esse convidado e manda a resposta pelo relay.
#[tauri::command]
async fn approve_request(st: State<'_, AppState>, id: String) -> Result<String, String> {
    let req = {
        let mut g = st.host.lock().unwrap();
        let h = g.as_mut().ok_or("Nenhuma sala aberta")?;
        // O pedido fica em `pending` até a resposta sair: o convidado repete o pedido a cada
        // poucos segundos e, se ele sumisse daqui antes, a repetição viraria um segundo cartão.
        let mut req = h.pending.get(&id).map(|p| p.0.clone()).ok_or("Esse pedido não existe mais")?;
        if !h.accepting.insert(id.clone()) {
            return Err("Esse pedido já está sendo aceito".into());
        }
        if h.members.len() >= room::MAX_MEMBERS {
            h.pending.remove(&id);
            h.accepting.remove(&id);
            answer(h, &id, &JoinRes { ok: false, sdp: String::new(), why: "A sala está cheia".into() });
            return Err("A sala está cheia. Remova alguém para liberar uma vaga.".into());
        }
        // Nome repetido não barra ninguém: vira "Nome (2)".
        let base = req.name.clone();
        let mut n = 2;
        while h.members.contains(&req.name) {
            req.name = format!("{base} ({n})");
            n += 1;
        }
        h.members.push(req.name.clone());
        req
    };
    let made = st.engine.accept_guest(req.name.clone(), req.sdp.clone()).await;
    let pc = {
        let mut g = st.host.lock().unwrap();
        let h = g.as_mut().ok_or("Nenhuma sala aberta")?;
        h.pending.remove(&id);
        h.accepting.remove(&id);
        match made {
            Ok((pc, sdp)) => {
                answer(h, &id, &JoinRes { ok: true, sdp, why: String::new() });
                pc
            }
            Err(e) => {
                h.members.retain(|m| *m != req.name);
                answer(h, &id, &JoinRes { ok: false, sdp: String::new(), why: "O anfitrião não conseguiu abrir a conexão".into() });
                return Err(e);
            }
        }
    };
    st.peers.lock().await.insert(req.name.clone(), pc);
    Ok(req.name)
}

#[tauri::command]
fn deny_request(st: State<AppState>, id: String) {
    if let Some(h) = st.host.lock().unwrap().as_mut() {
        if !h.accepting.contains(&id) && h.pending.remove(&id).is_some() {
            answer(h, &id, &JoinRes { ok: false, sdp: String::new(), why: "O anfitrião recusou a entrada".into() });
        }
    }
}

/// Pedidos que ainda estão valendo. Quem parou de repetir o pedido (desistiu) sai da lista.
#[tauri::command]
fn list_requests(st: State<AppState>) -> Vec<String> {
    let mut g = st.host.lock().unwrap();
    let Some(h) = g.as_mut() else { return vec![] };
    let accepting = h.accepting.clone();
    h.pending.retain(|id, p| accepting.contains(id) || p.1.elapsed() < Duration::from_secs(15));
    h.pending.keys().cloned().collect()
}

/// Quem pediu para entrar já foi aceito antes? Confere a prova do pedido contra os
/// identificadores guardados desta sala e devolve o que bateu.
#[tauri::command]
fn check_proof(st: State<AppState>, id: String, uids: Vec<String>) -> Option<String> {
    let g = st.host.lock().unwrap();
    let (req, _) = g.as_ref()?.pending.get(&id)?;
    if req.proof.len() != 64 {
        return None;
    }
    uids.into_iter().take(64).find(|u| {
        let p = room::proof(u, &req.id, &req.sdp);
        // comparação em tempo constante
        p.len() == req.proof.len() && p.bytes().zip(req.proof.bytes()).fold(0u8, |a, (x, y)| a | (x ^ y)) == 0
    })
}

/// Recado pelo canal de controle de um convidado (só o anfitrião tem esses canais).
#[tauri::command]
async fn ctl_send(st: State<'_, AppState>, name: String, text: String) -> Result<(), String> {
    st.engine.ctl_send(&name, &text).await
}

/// Outro participante quer assistir a minha tela: abre uma conexão direta só para ele.
#[tauri::command]
async fn accept_viewer(st: State<'_, AppState>, name: String, offer_sdp: String) -> Result<String, String> {
    // Limite de quem assiste a minha tela (evita alguém da sala abrir conexões sem parar).
    if st.peers.lock().await.len() >= room::MAX_MEMBERS + 2 && !st.peers.lock().await.contains_key(&name) {
        return Err("Muita gente assistindo".into());
    }
    if let Some(old) = st.peers.lock().await.remove(&name) {
        let _ = old.close().await;
    }
    let (pc, sdp) = st.engine.accept_guest(name.clone(), offer_sdp).await?;
    st.peers.lock().await.insert(name, pc);
    Ok(sdp)
}

/// Convidado parou de transmitir ou saiu: fecha as conexões de quem assistia.
#[tauri::command]
async fn drop_viewers(st: State<'_, AppState>) -> Result<(), String> {
    let peers: Vec<_> = st.peers.lock().await.drain().map(|(_, p)| p).collect();
    for p in peers {
        let _ = p.close().await;
    }
    Ok(())
}

#[tauri::command]
async fn remove_peer(st: State<'_, AppState>, name: String) -> Result<(), String> {
    st.engine.ctl_forget(&name);
    if let Some(pc) = st.peers.lock().await.remove(&name) {
        let _ = pc.close().await;
    }
    if let Some(h) = st.host.lock().unwrap().as_mut() {
        h.members.retain(|m| *m != name);
    }
    Ok(())
}

#[tauri::command]
async fn close_room(st: State<'_, AppState>) -> Result<(), String> {
    st.engine.stop_share();
    st.engine.ctl_clear();
    let peers: Vec<_> = st.peers.lock().await.drain().map(|(_, p)| p).collect();
    for p in peers {
        let _ = p.close().await;
    }
    *st.host.lock().unwrap() = None;
    if let Some(n) = st.guest_cancel.lock().unwrap().take() {
        n.notify_waiters();
    }
    Ok(())
}

// ---------- captura e qualidade ----------

#[tauri::command]
fn list_sources() -> Vec<capture::Source> {
    capture::list_sources()
}

#[tauri::command]
fn start_share(st: State<AppState>, source_id: String, title: String) -> Result<(), String> {
    st.engine.start_share(&source_id, title)
}

#[tauri::command]
fn list_processes() -> Vec<audio::Proc> {
    audio::list_processes()
}

#[tauri::command]
fn set_audio(st: State<AppState>, sel: audio::AudioSel) -> Result<(), String> {
    st.engine.set_audio(&sel)
}

#[tauri::command]
fn stop_share(st: State<AppState>) {
    st.engine.stop_share();
}

#[tauri::command]
fn set_bitrate(st: State<AppState>, kbps: u32) {
    st.engine.set_bitrate(kbps);
}

#[tauri::command]
fn set_quality(st: State<AppState>, quality: Quality) {
    st.engine.set_quality(quality);
}

// ---------- convidado ----------

/// Manda o pedido de entrada pelo relay e espera o anfitrião aceitar. Devolve a resposta SDP.
#[tauri::command]
async fn join_room(st: State<'_, AppState>, code: String, name: String, av: String, uid: String, offer_sdp: String) -> Result<String, String> {
    let keys = Keys::from_code(&code)?;
    let id = room::new_id();
    let proof = if uid.is_empty() { String::new() } else { room::proof(&uid, &id, &offer_sdp) };
    let req = JoinReq { id, name: name.chars().take(32).collect(), sdp: offer_sdp, av, proof };
    let (bus, mut rx) = Bus::start(keys.answer(&req.id));
    let cancel = Arc::new(Notify::new());
    *st.guest_cancel.lock().unwrap() = Some(cancel.clone());
    let sealed = keys.seal(&req);
    let started = Instant::now();
    let mut tick = tokio::time::interval(Duration::from_secs(4));
    loop {
        tokio::select! {
            _ = cancel.notified() => return Err("Cancelado".into()),
            _ = tick.tick() => {
                if started.elapsed() > Duration::from_secs(10) && bus.online.load(Ordering::SeqCst) == 0 {
                    return Err("Sem acesso aos servidores de encontro. Verifique a internet ou o firewall.".into());
                }
                if started.elapsed() > Duration::from_secs(180) {
                    return Err("O anfitrião não respondeu. Confira o código e se a sala está aberta.".into());
                }
                // Repete o pedido: cobre relay que caiu ou anfitrião que abriu a sala depois.
                bus.publish(&keys.requests(), &sealed);
            }
            msg = rx.recv() => {
                let Some(msg) = msg else { return Err("Conexão com os servidores de encontro caiu".into()) };
                let Some(res) = keys.open::<JoinRes>(&msg) else { continue };
                return if res.ok { Ok(res.sdp) } else { Err(if res.why.is_empty() { "O anfitrião recusou a entrada".into() } else { res.why }) };
            }
        }
    }
}

#[tauri::command]
fn app_version() -> &'static str {
    env!("CARGO_PKG_VERSION")
}

/// Abre no navegador o download da versão mais nova (endereço fixo; a página não escolhe o link).
#[tauri::command]
fn open_update() {
    use windows::core::w;
    use windows::Win32::UI::Shell::ShellExecuteW;
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;
    unsafe {
        ShellExecuteW(None, w!("open"), w!("https://github.com/paulo-vc/lume/releases/latest/download/Lume.exe"), None, None, SW_SHOWNORMAL);
    }
}

/// Se o roteador abre portas sozinho (UPnP) e se a internet usa CGNAT.
#[tauri::command]
async fn net_status() -> net::Status {
    net::status().await
}

#[tauri::command]
fn cancel_join(st: State<AppState>) {
    if let Some(n) = st.guest_cancel.lock().unwrap().take() {
        n.notify_waiters();
    }
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            tauri::async_runtime::spawn(net::probe());
            app.manage(AppState {
                host: Mutex::new(None),
                guest_cancel: Mutex::new(None),
                engine: Arc::new(Engine::new({
                    let h = app.handle().clone();
                    Arc::new(move |ev: &str, v: serde_json::Value| {
                        let _ = h.emit(ev, v);
                    })
                })),
                peers: tokio::sync::Mutex::new(HashMap::new()),
            });
            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            create_room,
            new_code,
            relays_online,
            approve_request,
            deny_request,
            remove_peer,
            list_requests,
            check_proof,
            ctl_send,
            accept_viewer,
            drop_viewers,
            close_room,
            list_sources,
            start_share,
            stop_share,
            list_processes,
            set_audio,
            set_quality,
            set_bitrate,
            join_room,
            cancel_join,
            net_status,
            app_version,
            open_update
        ])
        .run(tauri::generate_context!())
        .expect("falha ao iniciar o Lume");
}
