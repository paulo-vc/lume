#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod capture;
mod room;
mod stream;

use room::{GuestCtx, HostRoom, Invite};
use serde::Serialize;
use std::collections::HashMap;
use std::sync::{Arc, Mutex};
use stream::{Engine, Quality};
use tauri::{Emitter, Manager, State};
use webrtc::peer_connection::PeerConnection;

struct AppState {
    host: Mutex<Option<HostRoom>>,
    guest: Mutex<Option<GuestCtx>>,
    engine: Arc<Engine>,
    peers: tokio::sync::Mutex<HashMap<String, Arc<dyn PeerConnection>>>,
}

#[derive(Serialize)]
struct RoomInfo {
    name: String,
    invite: String,
    expires_at: u64,
    members: Vec<String>,
}

fn info(r: &HostRoom) -> RoomInfo {
    RoomInfo {
        name: r.name.clone(),
        invite: r.invite.to_link(),
        expires_at: r.invite.expires_at,
        members: r.members.clone(),
    }
}

// ---------- sala (anfitrião) ----------

#[tauri::command]
fn create_room(st: State<AppState>, name: String) -> RoomInfo {
    let r = HostRoom::new(name, 15);
    let i = info(&r);
    *st.host.lock().unwrap() = Some(r);
    i
}

#[tauri::command]
fn new_invite(st: State<AppState>) -> Result<RoomInfo, String> {
    let mut g = st.host.lock().unwrap();
    let r = g.as_mut().ok_or("Nenhuma sala aberta")?;
    r.rotate_invite(15);
    Ok(info(r))
}

/// Valida o pedido colado e mostra quem quer entrar (ainda não aprovado).
#[tauri::command]
fn inspect_request(st: State<AppState>, code: String) -> Result<String, String> {
    let g = st.host.lock().unwrap();
    let r = g.as_ref().ok_or("Nenhuma sala aberta")?;
    Ok(r.check_request(&code)?.name)
}

/// Aprova: cria a conexão nativa para esse convidado e devolve o código de resposta.
#[tauri::command]
async fn approve_request(st: State<'_, AppState>, code: String) -> Result<String, String> {
    let req = {
        let g = st.host.lock().unwrap();
        g.as_ref().ok_or("Nenhuma sala aberta")?.check_request(&code)?
    };
    if st.peers.lock().await.contains_key(&req.name) {
        return Err("Já existe alguém com esse nome na sala".into());
    }
    let (pc, answer) = st.engine.accept_guest(req.name.clone(), req.sdp.clone()).await?;
    let reply = {
        let mut g = st.host.lock().unwrap();
        g.as_mut().ok_or("Nenhuma sala aberta")?.approve(&code, answer)
    };
    match reply {
        Ok(reply) => {
            st.peers.lock().await.insert(req.name, pc);
            Ok(reply)
        }
        Err(e) => {
            let _ = pc.close().await;
            Err(e)
        }
    }
}

#[tauri::command]
async fn remove_peer(st: State<'_, AppState>, name: String) -> Result<(), String> {
    if let Some(pc) = st.peers.lock().await.remove(&name) {
        let _ = pc.close().await;
    }
    if let Some(r) = st.host.lock().unwrap().as_mut() {
        r.members.retain(|m| *m != name);
    }
    Ok(())
}

#[tauri::command]
async fn close_room(st: State<'_, AppState>) -> Result<(), String> {
    st.engine.stop_share();
    let peers: Vec<_> = st.peers.lock().await.drain().map(|(_, p)| p).collect();
    for p in peers {
        let _ = p.close().await;
    }
    *st.host.lock().unwrap() = None;
    *st.guest.lock().unwrap() = None;
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
fn stop_share(st: State<AppState>) {
    st.engine.stop_share();
}

#[tauri::command]
fn set_quality(st: State<AppState>, quality: Quality) {
    st.engine.set_quality(quality);
}

// ---------- convidado ----------

#[derive(Serialize)]
struct JoinStart {
    room_id: String,
    request: String,
}

#[tauri::command]
fn make_join_request(st: State<AppState>, invite: String, name: String, offer_sdp: String) -> Result<JoinStart, String> {
    let inv = Invite::parse(&invite)?;
    let room_id = inv.room_id.clone();
    let (ctx, request) = GuestCtx::make_request(inv, &name, &offer_sdp)?;
    *st.guest.lock().unwrap() = Some(ctx);
    Ok(JoinStart { room_id, request })
}

#[tauri::command]
fn open_reply(st: State<AppState>, code: String) -> Result<String, String> {
    let g = st.guest.lock().unwrap();
    g.as_ref().ok_or("Nenhum pedido em andamento")?.open_reply(&code)
}

fn main() {
    tauri::Builder::default()
        .setup(|app| {
            app.manage(AppState {
                host: Mutex::new(None),
                guest: Mutex::new(None),
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
            new_invite,
            inspect_request,
            approve_request,
            remove_peer,
            close_room,
            list_sources,
            start_share,
            stop_share,
            set_quality,
            make_join_request,
            open_reply
        ])
        .run(tauri::generate_context!())
        .expect("falha ao iniciar o Lume");
}
