#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod room;

use room::{GuestCtx, HostRoom, Invite};
use serde::Serialize;
use std::sync::Mutex;
use tauri::State;

#[derive(Default)]
struct AppState {
    host: Mutex<Option<HostRoom>>,
    guest: Mutex<Option<GuestCtx>>,
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

#[derive(Serialize)]
struct PendingGuest {
    name: String,
    sdp: String,
}

/// Anfitrião: valida o pedido colado e mostra quem quer entrar (ainda não aprovado).
#[tauri::command]
fn inspect_request(st: State<AppState>, code: String) -> Result<PendingGuest, String> {
    let g = st.host.lock().unwrap();
    let r = g.as_ref().ok_or("Nenhuma sala aberta")?;
    let req = r.check_request(&code)?;
    Ok(PendingGuest { name: req.name, sdp: req.sdp })
}

/// Anfitrião: aprova e devolve o código de resposta para o convidado.
#[tauri::command]
fn approve_request(st: State<AppState>, code: String, answer_sdp: String) -> Result<String, String> {
    let mut g = st.host.lock().unwrap();
    g.as_mut().ok_or("Nenhuma sala aberta")?.approve(&code, answer_sdp)
}

#[tauri::command]
fn member_left(st: State<AppState>, name: String) {
    if let Some(r) = st.host.lock().unwrap().as_mut() {
        if let Some(i) = r.members.iter().position(|m| *m == name) {
            r.members.remove(i);
        }
    }
}

#[derive(Serialize)]
struct JoinStart {
    room_id: String,
    request: String,
}

/// Convidado: a partir do convite e da oferta WebRTC gera o pedido de entrada.
#[tauri::command]
fn make_join_request(st: State<AppState>, invite: String, name: String, offer_sdp: String) -> Result<JoinStart, String> {
    let inv = Invite::parse(&invite)?;
    let room_id = inv.room_id.clone();
    let (ctx, request) = GuestCtx::make_request(inv, &name, &offer_sdp)?;
    *st.guest.lock().unwrap() = Some(ctx);
    Ok(JoinStart { room_id, request })
}

/// Convidado: valida a resposta do anfitrião e devolve o SDP de resposta.
#[tauri::command]
fn open_reply(st: State<AppState>, code: String) -> Result<String, String> {
    let g = st.guest.lock().unwrap();
    g.as_ref().ok_or("Nenhum pedido em andamento")?.open_reply(&code)
}

fn main() {
    tauri::Builder::default()
        .manage(AppState::default())
        .invoke_handler(tauri::generate_handler![
            create_room,
            new_invite,
            inspect_request,
            approve_request,
            member_left,
            make_join_request,
            open_reply
        ])
        .run(tauri::generate_context!())
        .expect("falha ao iniciar o Lume");
}
