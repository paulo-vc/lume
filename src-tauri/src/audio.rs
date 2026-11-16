//! Áudio por aplicativo (Windows 10 2004+ / 11): Process Loopback do WASAPI -> Opus 48 kHz estéreo.
//! - "app": só o áudio do processo escolhido (e dos filhos dele).
//! - "except": o PC inteiro menos um processo (ex.: o Discord, para a call não voltar como eco).

use bytes::Bytes;
use serde::{Deserialize, Serialize};
use std::collections::{HashMap, VecDeque};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::broadcast;
use unsafe_libopus as opus;
use wasapi::{AudioClient, Direction, SampleType, StreamMode, WaveFormat};

pub const RATE: usize = 48_000;
pub const CH: usize = 2;
/// 10 ms por pacote: latência menor que os 20 ms padrão.
pub const FRAME: usize = RATE / 100;

#[derive(Clone, Debug, Deserialize, Serialize, PartialEq)]
pub struct AudioSel {
    /// "none" | "app" | "except" | "all"
    pub mode: String,
    #[serde(default)]
    pub pid: u32,
}

#[derive(Clone)]
pub struct EncodedAudio {
    pub data: Bytes,
    pub duration: Duration,
}

#[derive(Serialize)]
pub struct Proc {
    pub pid: u32,
    pub name: String,
}

/// (pid, pid do pai, nome do executável) de tudo que está rodando.
fn snapshot() -> Vec<(u32, u32, String)> {
    use windows::Win32::Foundation::CloseHandle;
    use windows::Win32::System::Diagnostics::ToolHelp::*;
    let mut all: Vec<(u32, u32, String)> = vec![];
    unsafe {
        let Ok(snap) = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0) else { return vec![] };
        let mut e = PROCESSENTRY32W { dwSize: std::mem::size_of::<PROCESSENTRY32W>() as u32, ..Default::default() };
        if Process32FirstW(snap, &mut e).is_ok() {
            loop {
                let len = e.szExeFile.iter().position(|&c| c == 0).unwrap_or(e.szExeFile.len());
                all.push((e.th32ProcessID, e.th32ParentProcessID, String::from_utf16_lossy(&e.szExeFile[..len])));
                if Process32NextW(snap, &mut e).is_err() {
                    break;
                }
            }
        }
        let _ = CloseHandle(snap);
    }
    all
}

/// Processos "raiz" de cada programa (pai não tem o mesmo nome), um por nome de executável.
pub fn list_processes() -> Vec<Proc> {
    let all = snapshot();
    let names: HashMap<u32, String> = all.iter().map(|(p, _, n)| (*p, n.to_lowercase())).collect();
    let me = std::process::id();
    let mut seen = std::collections::HashSet::new();
    let mut out = vec![];
    for (pid, ppid, name) in &all {
        let lower = name.to_lowercase();
        if *pid == me || *pid <= 4 || names.get(ppid) == Some(&lower) {
            continue; // processo filho do mesmo programa
        }
        if seen.insert(lower) {
            out.push(Proc { pid: *pid, name: name.clone() });
        }
    }
    out.sort_by_key(|p| p.name.to_lowercase());
    out
}

/// Inicia a captura numa thread própria. Devolve a flag para parar.
pub fn start(sel: &AudioSel, out: broadcast::Sender<EncodedAudio>) -> Result<Arc<AtomicBool>, String> {
    let me = std::process::id();
    let (pid, include) = match sel.mode.as_str() {
        "app" => (sel.pid, true),
        "except" => (sel.pid, false),
        // PC inteiro: exclui o próprio Lume, senão as telas que você assiste voltariam para a sala.
        "all" => (me, false),
        _ => return Err("sem áudio".into()),
    };
    if pid == 0 {
        return Err("Escolha um aplicativo para o áudio".into());
    }
    let mix = sel.mode == "except";
    let stop = Arc::new(AtomicBool::new(false));
    let s2 = stop.clone();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();
    std::thread::Builder::new()
        .name("lume-audio".into())
        .spawn(move || {
            // O Windows só deixa excluir UM programa por captura. Para tirar a call E o próprio
            // Lume, captura-se cada programa que está tocando e mistura-se o resto.
            let r = if mix { run_mix(vec![pid, me], out, s2, &ready_tx) } else { run(pid, include, out, s2, &ready_tx) };
            if let Err(e) = r {
                let _ = ready_tx.send(Err(e));
            }
        })
        .map_err(|e| e.to_string())?;
    ready_rx.recv_timeout(Duration::from_secs(5)).map_err(|_| "O áudio não respondeu".to_string())??;
    Ok(stop)
}

struct Enc(*mut opus::OpusEncoder);
impl Drop for Enc {
    fn drop(&mut self) {
        unsafe { opus::opus_encoder_destroy(self.0) }
    }
}

fn new_encoder() -> Result<Enc, String> {
    unsafe {
        let mut err = 0i32;
        let st = opus::opus_encoder_create(RATE as i32, CH as i32, opus::OPUS_APPLICATION_AUDIO, &mut err);
        if st.is_null() || err != opus::OPUS_OK {
            return Err(format!("Opus: erro {err}"));
        }
        opus::opus_encoder_ctl!(st, opus::OPUS_SET_BITRATE_REQUEST, 128_000i32);
        opus::opus_encoder_ctl!(st, opus::OPUS_SET_COMPLEXITY_REQUEST, 5i32);
        opus::opus_encoder_ctl!(st, opus::OPUS_SET_INBAND_FEC_REQUEST, 1i32);
        opus::opus_encoder_ctl!(st, opus::OPUS_SET_PACKET_LOSS_PERC_REQUEST, 5i32);
        Ok(Enc(st))
    }
}

fn run(
    pid: u32,
    include_tree: bool,
    out: broadcast::Sender<EncodedAudio>,
    stop: Arc<AtomicBool>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let e = |x: wasapi::WasapiError| x.to_string();
    let _ = wasapi::initialize_mta();
    let fmt = WaveFormat::new(32, 32, SampleType::Float, RATE, CH, None);
    let mut client = AudioClient::new_application_loopback_client(pid, include_tree).map_err(e)?;
    client
        .initialize_client(&fmt, Direction::Capture, StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 200_000 }, None)
        .map_err(e)?;
    let event = client.set_get_eventhandle().map_err(e)?;
    let cap = client.get_audiocaptureclient().map_err(e)?;
    client.start_stream().map_err(e)?;
    let enc = new_encoder()?;
    let _ = ready.send(Ok(()));

    let bytes_per_frame = 4 * CH;
    let chunk = FRAME * bytes_per_frame;
    let mut q: VecDeque<u8> = VecDeque::with_capacity(chunk * 20);
    let mut pcm = vec![0f32; FRAME * CH];
    let mut pkt = vec![0u8; 4000];
    // Relógio: quando o app fica em silêncio o Windows não entrega nada; completamos com
    // silêncio para o tempo do áudio não "encolher" no convidado.
    let start = Instant::now();
    let mut sent_frames: u64 = 0;
    while !stop.load(Ordering::SeqCst) {
        let _ = event.wait_for_event(10);
        let _ = cap.read_from_device_to_deque(&mut q);
        let due = (start.elapsed().as_micros() as u64 * RATE as u64 / 1_000_000) / FRAME as u64;
        if q.len() < chunk && sent_frames + 3 < due {
            q.extend(std::iter::repeat(0u8).take(chunk));
        }
        // Se acumulou demais (atraso), descarta para manter a latência baixa.
        while q.len() > chunk * 10 {
            q.drain(..chunk);
        }
        while q.len() >= chunk {
            for (i, s) in pcm.iter_mut().enumerate() {
                let b = [q[i * 4], q[i * 4 + 1], q[i * 4 + 2], q[i * 4 + 3]];
                *s = f32::from_le_bytes(b);
            }
            q.drain(..chunk);
            let n = unsafe { opus::opus_encode_float(enc.0, pcm.as_ptr(), FRAME as i32, pkt.as_mut_ptr(), pkt.len() as i32) };
            sent_frames += 1;
            if n > 0 {
                let _ = out.send(EncodedAudio { data: Bytes::copy_from_slice(&pkt[..n as usize]), duration: Duration::from_millis(10) });
            }
        }
        if sent_frames > due + 50 {
            sent_frames = due; // reancora o relógio
        }
    }
    let _ = client.stop_stream();
    Ok(())
}

/// Processos com sessão de áudio aberta em qualquer saída de som.
fn session_pids() -> Vec<u32> {
    let mut out = vec![];
    let Ok(en) = wasapi::DeviceEnumerator::new() else { return out };
    let Ok(devs) = en.get_device_collection(Direction::Render) else { return out };
    for d in devs.into_iter().flatten() {
        let Ok(list) = d.get_iaudiosessionmanager().and_then(|m| m.get_audiosessionenumerator()) else { continue };
        for i in 0..list.get_count().unwrap_or(0) {
            if let Ok(pid) = list.get_session(i).and_then(|s| s.get_process_id()) {
                out.push(pid);
            }
        }
    }
    out
}

/// Quais programas capturar: todos que têm áudio, menos os excluídos (e os filhos deles).
/// Se pai e filho têm áudio, fica só o pai (a captura dele já inclui os filhos).
fn mix_targets(excluded: &[u32]) -> Vec<u32> {
    let parent: HashMap<u32, u32> = snapshot().into_iter().map(|(p, pp, _)| (p, pp)).collect();
    let pids: std::collections::HashSet<u32> = session_pids().into_iter().filter(|p| *p > 4 && parent.contains_key(p)).collect();
    let ancestors = |p: u32| {
        let mut v = vec![];
        let mut cur = p;
        while let Some(&pp) = parent.get(&cur) {
            if pp == 0 || pp == cur || v.len() > 32 {
                break;
            }
            v.push(pp);
            cur = pp;
        }
        v
    };
    pids.iter()
        .copied()
        .filter(|&p| {
            let up = ancestors(p);
            !excluded.contains(&p) && !up.iter().any(|a| excluded.contains(a) || pids.contains(a))
        })
        .collect()
}

struct Source {
    pid: u32,
    client: AudioClient,
    cap: wasapi::AudioCaptureClient,
    _event: wasapi::Handle,
    q: VecDeque<u8>,
    primed: bool,
}

fn open_source(pid: u32) -> Result<Source, wasapi::WasapiError> {
    let fmt = WaveFormat::new(32, 32, SampleType::Float, RATE, CH, None);
    let mut client = AudioClient::new_application_loopback_client(pid, true)?;
    client.initialize_client(&fmt, Direction::Capture, StreamMode::EventsShared { autoconvert: true, buffer_duration_hns: 200_000 }, None)?;
    let _event = client.set_get_eventhandle()?;
    let cap = client.get_audiocaptureclient()?;
    client.start_stream()?;
    Ok(Source { pid, client, cap, _event, q: VecDeque::new(), primed: false })
}

/// "PC inteiro menos X": mistura o áudio de cada programa que não está na lista de excluídos.
fn run_mix(
    excluded: Vec<u32>,
    out: broadcast::Sender<EncodedAudio>,
    stop: Arc<AtomicBool>,
    ready: &std::sync::mpsc::Sender<Result<(), String>>,
) -> Result<(), String> {
    let _ = wasapi::initialize_mta();
    let enc = new_encoder()?;
    let _ = ready.send(Ok(()));

    let chunk = FRAME * 4 * CH;
    let mut srcs: Vec<Source> = vec![];
    let mut pcm = vec![0f32; FRAME * CH];
    let mut pkt = vec![0u8; 4000];
    let start = Instant::now();
    let mut sent: u64 = 0;
    let mut scanned: Option<Instant> = None;
    while !stop.load(Ordering::SeqCst) {
        // Programas abrem e fecham: confere a lista uma vez por segundo.
        if scanned.map_or(true, |t| t.elapsed() > Duration::from_secs(1)) {
            scanned = Some(Instant::now());
            let want = mix_targets(&excluded);
            srcs.retain(|s| want.contains(&s.pid));
            for pid in want {
                if srcs.len() < 24 && !srcs.iter().any(|s| s.pid == pid) {
                    if let Ok(s) = open_source(pid) {
                        srcs.push(s);
                    }
                }
            }
        }
        for s in srcs.iter_mut() {
            for _ in 0..8 {
                let before = s.q.len();
                if s.cap.read_from_device_to_deque(&mut s.q).is_err() || s.q.len() == before {
                    break;
                }
            }
            while s.q.len() > chunk * 8 {
                s.q.drain(..chunk); // acumulou atraso: descarta
            }
        }
        let due = (start.elapsed().as_micros() as u64 * RATE as u64 / 1_000_000) / FRAME as u64;
        if sent + 6 < due {
            sent = due - 1; // a thread ficou parada: não tenta recuperar o tempo perdido
        }
        while sent < due {
            pcm.fill(0.0);
            for s in srcs.iter_mut() {
                // Espera juntar 20 ms antes de começar a tocar uma fonte, para ela não picotar.
                if !s.primed && s.q.len() >= chunk * 2 {
                    s.primed = true;
                }
                if !s.primed {
                    continue;
                }
                if s.q.len() < chunk {
                    s.primed = false;
                    continue;
                }
                for (i, o) in pcm.iter_mut().enumerate() {
                    *o += f32::from_le_bytes([s.q[i * 4], s.q[i * 4 + 1], s.q[i * 4 + 2], s.q[i * 4 + 3]]);
                }
                s.q.drain(..chunk);
            }
            for o in pcm.iter_mut() {
                *o = o.clamp(-1.0, 1.0);
            }
            let n = unsafe { opus::opus_encode_float(enc.0, pcm.as_ptr(), FRAME as i32, pkt.as_mut_ptr(), pkt.len() as i32) };
            sent += 1;
            if n > 0 {
                let _ = out.send(EncodedAudio { data: Bytes::copy_from_slice(&pkt[..n as usize]), duration: Duration::from_millis(10) });
            }
        }
        std::thread::sleep(Duration::from_millis(3));
    }
    for s in &srcs {
        let _ = s.client.stop_stream();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captura o PC inteiro por 2 s. `cargo test audio -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn capture_all_audio() {
        let (tx, mut rx) = broadcast::channel(256);
        // LUME_MODE=app|except e LUME_PID=<pid> para testar um processo específico.
        let mode = std::env::var("LUME_MODE").unwrap_or("all".into());
        let pid = std::env::var("LUME_PID").ok().and_then(|p| p.parse().ok()).unwrap_or(0);
        let stop = start(&AudioSel { mode, pid }, tx).expect("áudio");
        std::thread::sleep(Duration::from_secs(2));
        stop.store(true, Ordering::SeqCst);
        let (mut n, mut big) = (0, 0);
        while let Ok(p) = rx.try_recv() {
            n += 1;
            if p.data.len() > 20 {
                big += 1; // pacote com som (silêncio vira ~3 bytes)
            }
        }
        println!("{n} pacotes Opus em 2 s, {big} com som");
        assert!(n > 100, "poucos pacotes: {n}");
    }

    /// Mostra quais programas a mistura capturaria agora. `cargo test mix_targets -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn mix_targets_now() {
        let _ = wasapi::initialize_mta();
        let names: HashMap<u32, String> = snapshot().into_iter().map(|(p, _, n)| (p, n)).collect();
        let show = |v: Vec<u32>| v.iter().map(|p| format!("{p}:{}", names.get(p).cloned().unwrap_or_default())).collect::<Vec<_>>();
        println!("sessões: {:?}", show(session_pids()));
        println!("alvos:   {:?}", show(mix_targets(&[std::process::id()])));
        for p in mix_targets(&[std::process::id()]) {
            println!("abrir {p}: {:?}", open_source(p).map(|_| ()).map_err(|e| e.to_string()));
        }
    }

    #[test]
    fn lists_processes() {
        assert!(list_processes().iter().any(|p| p.name.eq_ignore_ascii_case("explorer.exe")));
    }
}
