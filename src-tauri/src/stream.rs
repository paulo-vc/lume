//! Transmissão nativa: captura -> H.264 -> WebRTC (uma conexão por convidado).
//! O convidado continua recebendo pelo WebRTC do WebView2 (decodificação por hardware).

use crate::audio::{self, AudioSel, EncodedAudio};
use crate::capture::{self, RawFrame};
use bytes::Bytes;
use openh264::encoder::{BitRate, Encoder, EncoderConfig, FrameRate, IntraFramePeriod, RateControlMode, UsageType};
use openh264::formats::{BgraSliceU8, YUVBuffer};
use openh264::OpenH264API;
use rtc::interceptor::Registry;
use rtc::media::Sample;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCIceServer;
use rtc::rtp_transceiver::rtp_sender::{
    RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind,
};
use serde::{Deserialize, Serialize};
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::{mpsc, Arc, Mutex};
use std::time::{Duration, Instant};
pub type Emit = Arc<dyn Fn(&str, serde_json::Value) + Send + Sync>;
use tokio::sync::{broadcast, mpsc as tmpsc};
use webrtc::data_channel::{DataChannel, DataChannelEvent};
use webrtc::media_stream::track_local::static_sample::TrackLocalStaticSample;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::Track;
use webrtc::peer_connection::{
    PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState, RTCPeerConnectionState,
};

#[derive(Clone, Debug, Serialize, Deserialize, PartialEq)]
pub struct Quality {
    /// altura máxima; 0 = nativa
    pub res: u32,
    pub fps: u32,
    /// "motion" (fluidez) ou "detail" (nitidez)
    pub pri: String,
    /// Mbps
    pub br: u32,
}

impl Default for Quality {
    fn default() -> Self {
        Self { res: 1080, fps: 60, pri: "motion".into(), br: 8 }
    }
}

#[derive(Clone)]
pub struct EncodedFrame {
    pub data: Bytes,
    pub duration: Duration,
}

pub struct Engine {
    app: Emit,
    pub quality: Arc<Mutex<Quality>>,
    q_version: Arc<AtomicU64>,
    force_idr: Arc<AtomicBool>,
    frames: broadcast::Sender<EncodedFrame>,
    capture: Mutex<Option<(capture::Control, Arc<AtomicBool>)>>,
    pub source_title: Mutex<Option<String>>,
    audio_out: broadcast::Sender<EncodedAudio>,
    audio_stop: Mutex<Option<Arc<AtomicBool>>>,
    /// canal de controle de cada convidado (anfitrião): recados da sala passam por aqui
    ctl: Mutex<std::collections::HashMap<String, Arc<dyn DataChannel>>>,
}

impl Engine {
    pub fn new(app: Emit) -> Self {
        let (frames, _) = broadcast::channel(8);
        let (audio_out, _) = broadcast::channel(64);
        Self {
            app,
            quality: Arc::new(Mutex::new(Quality::default())),
            q_version: Arc::new(AtomicU64::new(0)),
            force_idr: Arc::new(AtomicBool::new(false)),
            frames,
            capture: Mutex::new(None),
            source_title: Mutex::new(None),
            audio_out,
            audio_stop: Mutex::new(None),
            ctl: Mutex::new(Default::default()),
        }
    }

    /// Manda um recado pelo canal de controle de um convidado.
    pub async fn ctl_send(&self, name: &str, text: &str) -> Result<(), String> {
        let dc = self.ctl.lock().unwrap().get(name).cloned().ok_or("Essa pessoa não está conectada")?;
        dc.send_text(text).await.map_err(|e| e.to_string())
    }

    pub fn ctl_forget(&self, name: &str) {
        self.ctl.lock().unwrap().remove(name);
    }

    pub fn ctl_clear(&self) {
        self.ctl.lock().unwrap().clear();
    }

    /// Qualidade automática: muda o bitrate do encoder na hora, sem reiniciar (sem quadro-chave extra).
    pub fn set_bitrate(&self, kbps: u32) {
        AUTO_KBPS.store(kbps, Ordering::SeqCst);
    }

    pub fn set_quality(&self, q: Quality) {
        let mut cur = self.quality.lock().unwrap();
        if *cur != q {
            *cur = q;
            self.q_version.fetch_add(1, Ordering::SeqCst);
        }
    }

    pub fn request_keyframe(&self) {
        self.force_idr.store(true, Ordering::SeqCst);
    }

    pub fn start_share(&self, source_id: &str, title: String) -> Result<(), String> {
        self.stop_share();
        let fps = self.quality.lock().unwrap().fps;
        let (tx, rx) = mpsc::sync_channel::<RawFrame>(1);
        let control = capture::start(source_id, fps, tx)?;
        let stop = Arc::new(AtomicBool::new(false));

        let (quality, ver, idr, out, stop2, app) = (
            self.quality.clone(),
            self.q_version.clone(),
            self.force_idr.clone(),
            self.frames.clone(),
            stop.clone(),
            self.app.clone(),
        );
        std::thread::Builder::new()
            .name("lume-encoder".into())
            .spawn(move || encode_loop(rx, quality, ver, idr, out, stop2, app))
            .map_err(|e| e.to_string())?;

        *self.capture.lock().unwrap() = Some((control, stop));
        *self.source_title.lock().unwrap() = Some(title);
        Ok(())
    }

    /// Troca a fonte de áudio (pode ser feito durante a transmissão).
    pub fn set_audio(&self, sel: &AudioSel) -> Result<(), String> {
        self.stop_audio();
        if sel.mode == "none" {
            return Ok(());
        }
        let stop = audio::start(sel, self.audio_out.clone())?;
        *self.audio_stop.lock().unwrap() = Some(stop);
        Ok(())
    }

    pub fn stop_audio(&self) {
        if let Some(s) = self.audio_stop.lock().unwrap().take() {
            s.store(true, Ordering::SeqCst);
        }
    }

    pub fn stop_share(&self) {
        self.stop_audio();
        if let Some((control, stop)) = self.capture.lock().unwrap().take() {
            stop.store(true, Ordering::SeqCst);
            let _ = control.stop();
        }
        *self.source_title.lock().unwrap() = None;
    }

    /// Anfitrião: recebe a oferta do convidado e devolve a resposta SDP já com os candidatos ICE.
    pub async fn accept_guest(self: &Arc<Self>, name: String, offer_sdp: String) -> Result<(Arc<dyn PeerConnection>, String), String> {
        let e = |x: &dyn std::fmt::Display| x.to_string();
        let mut me = MediaEngine::default();
        let codec = RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_H264.to_owned(),
                clock_rate: 90000,
                channels: 0,
                sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 102,
            ..Default::default()
        };
        me.register_codec(codec.clone(), RtpCodecKind::Video).map_err(|x| e(&x))?;
        // O convidado sempre oferece áudio também (usado na etapa de áudio por janela).
        let opus = RTCRtpCodecParameters {
            rtp_codec: RTCRtpCodec {
                mime_type: MIME_TYPE_OPUS.to_owned(),
                clock_rate: 48000,
                channels: 2,
                sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
                rtcp_feedback: vec![],
            },
            payload_type: 111,
            ..Default::default()
        };
        me.register_codec(opus.clone(), RtpCodecKind::Audio).map_err(|x| e(&x))?;
        let registry = register_default_interceptors(Registry::new(), &mut me)
            .map_err(|x| e(&x))?
            .with(rtc::interceptor::Slot::from(crate::keyreq::SLOT), crate::keyreq::KeyframeRequests::default());
        let config = RTCConfigurationBuilder::new()
            .with_ice_servers(vec![RTCIceServer {
                urls: vec!["stun:stun.l.google.com:19302".into(), "stun:stun.cloudflare.com:3478".into()],
                ..Default::default()
            }])
            .build();

        let (gather_tx, mut gather_rx) = tmpsc::channel::<()>(1);
        let (state_tx, mut state_rx) = tmpsc::channel::<RTCPeerConnectionState>(8);
        let (dc_tx, mut dc_rx) = tmpsc::channel::<Arc<dyn DataChannel>>(2);
        let handler = Arc::new(Handler { gather_tx, state_tx, dc_tx });

        // Porta aberta no roteador (UPnP) só para esta conexão; se não der, segue sem.
        // (A prévia local, "@preview", não precisa.)
        let mapping = if name.starts_with('@') { None } else { crate::net::open_port().await };
        let v4 = match mapping {
            Some(m) => format!("0.0.0.0:{}", m.port),
            None => "0.0.0.0:0".to_string(),
        };
        let runtime = webrtc::runtime::default_runtime().ok_or("runtime indisponível")?;
        let pc: Arc<dyn PeerConnection> = Arc::new(
            PeerConnectionBuilder::new()
                .with_configuration(config)
                .with_media_engine(me)
                .with_interceptor_registry(registry)
                .with_handler(handler)
                .with_runtime(runtime)
                // IPv6 também: muitos provedores (CGNAT no IPv4) dão IPv6 público, que conecta direto.
                .with_udp_addrs(vec![v4, "[::]:0".to_string()])
                .build()
                .await
                .map_err(|x| e(&x))?,
        );

        let track = Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    "lume".into(),
                    "lume-video".into(),
                    "lume-video".into(),
                    RtpCodecKind::Video,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(rand::random::<u32>()), ..Default::default() },
                        codec: codec.rtp_codec.clone(),
                        ..Default::default()
                    }],
                ),
            )
            .map_err(|x| e(&x))?,
        );

        let atrack = Arc::new(
            TrackLocalStaticSample::new(
                Instant::now(),
                MediaStreamTrack::new(
                    "lume".into(),
                    "lume-audio".into(),
                    "lume-audio".into(),
                    RtpCodecKind::Audio,
                    vec![RTCRtpEncodingParameters {
                        rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(rand::random::<u32>()), ..Default::default() },
                        codec: opus.rtp_codec.clone(),
                        ..Default::default()
                    }],
                ),
            )
            .map_err(|x| e(&x))?,
        );

        let offer = RTCSessionDescription::offer(offer_sdp).map_err(|x| e(&x))?;
        pc.set_remote_description(offer).await.map_err(|x| e(&x))?;
        let sender = pc.add_track(track.clone() as Arc<dyn TrackLocal>).await.map_err(|x| e(&x))?;
        let asender = pc.add_track(atrack.clone() as Arc<dyn TrackLocal>).await.map_err(|x| e(&x))?;
        // Pedidos de quadro-chave de quem assiste (PLI/FIR): quando a imagem estraga por pacote
        // perdido, o navegador pede uma imagem completa. Sem atender, a tela ficava parada até o
        // próximo quadro-chave automático. Limite de um a cada 0,5 s (vários pedidos viram um).
        for (t, video) in [(track.clone(), true), (atrack.clone(), false)] {
            let eng = self.clone();
            tokio::spawn(async move {
                let mut last: Option<Instant> = None;
                while let Some(TrackLocalEvent::OnRtcpPacket(pkts)) = t.poll().await {
                    let wants_key = pkts.iter().any(|p| {
                        let a = p.as_any();
                        a.is::<rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication>()
                            || a.is::<rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest>()
                    });
                    if video && wants_key {
                        KEY_REQUESTS.fetch_add(1, Ordering::Relaxed);
                        if last.is_none_or(|l| l.elapsed() >= Duration::from_millis(500)) {
                            last = Some(Instant::now());
                            eng.request_keyframe();
                        }
                    }
                }
            });
        }
        let answer = pc.create_answer(None).await.map_err(|x| e(&x))?;
        pc.set_local_description(answer).await.map_err(|x| e(&x))?;
        let _ = tokio::time::timeout(Duration::from_secs(5), gather_rx.recv()).await;
        let mut sdp = pc.local_description().await.ok_or("Falha ao gerar resposta")?.sdp;
        if let Some(m) = mapping {
            sdp = crate::net::add_public_candidate(&sdp, m);
        }

        // Estado da conexão -> interface; ao conectar começa a enviar os quadros.
        let (engine, app, n) = (self.clone(), self.app.clone(), name.clone());
        let mut frames = self.frames.subscribe();
        let mut sounds = self.audio_out.subscribe();
        tokio::spawn(async move {
            let mut sending = false;
            // Quem não conecta em 30 s sai da lista (ex.: o convidado desistiu ou a rede bloqueou).
            let deadline = tokio::time::sleep(Duration::from_secs(30));
            tokio::pin!(deadline);
            loop {
                tokio::select! {
                    _ = &mut deadline, if !sending => {
                        (app)("peer-state", serde_json::json!([n, "failed"]));
                        break;
                    }
                    st = state_rx.recv() => {
                        let Some(st) = st else { break };
                        (app)("peer-state", serde_json::json!([n, st.to_string()]));
                        match st {
                            RTCPeerConnectionState::Connected => { sending = true; engine.request_keyframe(); }
                            RTCPeerConnectionState::Failed | RTCPeerConnectionState::Closed => break,
                            _ => {}
                        }
                    }
                    f = frames.recv() => {
                        match f {
                            Ok(f) if sending => {
                                let pt = match sender.get_parameters().await.ok().and_then(|p| p.rtp_parameters.codecs.first().map(|c| c.payload_type)) {
                                    Some(pt) => pt, None => continue,
                                };
                                let Some(ssrc) = track.ssrcs().await.first().copied() else { continue };
                                let _ = track.sample_writer(ssrc, pt)
                                    .write_sample(&Sample { data: f.data, duration: f.duration, ..Sample::new(Instant::now()) })
                                    .await;
                            }
                            Err(broadcast::error::RecvError::Lagged(_)) => engine.request_keyframe(),
                            Err(broadcast::error::RecvError::Closed) => break,
                            _ => {}
                        }
                    }
                    a = sounds.recv() => {
                        match a {
                            Ok(a) if sending => {
                                let pt = asender.get_parameters().await.ok().and_then(|p| p.rtp_parameters.codecs.first().map(|c| c.payload_type)).unwrap_or(111);
                                let Some(ssrc) = atrack.ssrcs().await.first().copied() else { continue };
                                let _ = atrack.sample_writer(ssrc, pt)
                                    .write_sample(&Sample { data: a.data, duration: a.duration, ..Sample::new(Instant::now()) })
                                    .await;
                            }
                            Err(broadcast::error::RecvError::Closed) => break,
                            _ => {}
                        }
                    }
                }
            }
            // Conexão acabou: fecha a porta no roteador.
            if let Some(m) = mapping {
                crate::net::close_port(m.port).await;
            }
        });

        // Canal de controle: os recados da sala (quem está transmitindo, ofertas entre
        // participantes) sobem para a interface, que decide o que fazer com eles.
        let (engine, app, n) = (self.clone(), self.app.clone(), name);
        tokio::spawn(async move {
            while let Some(dc) = dc_rx.recv().await {
                engine.ctl.lock().unwrap().insert(n.clone(), dc.clone());
                let mut open = false;
                while let Some(ev) = dc.poll().await {
                    match ev {
                        DataChannelEvent::OnOpen if !open => {
                            open = true;
                            (app)("ctl-open", serde_json::json!(n));
                        }
                        DataChannelEvent::OnMessage(msg) => {
                            if !open {
                                open = true;
                                (app)("ctl-open", serde_json::json!(n));
                            }
                            if msg.data.len() <= 64 * 1024 {
                                (app)("ctl-msg", serde_json::json!([n, String::from_utf8_lossy(&msg.data)]));
                            }
                        }
                        DataChannelEvent::OnClose => break,
                        _ => {}
                    }
                }
                let mut g = engine.ctl.lock().unwrap();
                if g.get(&n).is_some_and(|cur| Arc::ptr_eq(cur, &dc)) {
                    g.remove(&n);
                }
            }
        });

        Ok((pc, sdp))
    }
}

#[derive(Clone)]
struct Handler {
    gather_tx: tmpsc::Sender<()>,
    state_tx: tmpsc::Sender<RTCPeerConnectionState>,
    dc_tx: tmpsc::Sender<Arc<dyn DataChannel>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gather_tx.try_send(());
        }
    }
    async fn on_connection_state_change(&self, state: RTCPeerConnectionState) {
        let _ = self.state_tx.try_send(state);
    }
    async fn on_data_channel(&self, dc: Arc<dyn DataChannel>) {
        let _ = self.dc_tx.try_send(dc);
    }
}

#[derive(Serialize, Clone)]
struct EncStats {
    width: usize,
    height: usize,
    fps: f32,
    kbps: f32,
    encoder: String,
    /// quadros que chegaram da captura e quantos o encoder devolveu vazios (diagnóstico)
    src_fps: f32,
    skipped: u32,
    /// pedidos de quadro-chave recebidos desde o início da transmissão
    key_requests: u32,
}

/// Se o encoder de hardware falhar durante o uso, não tenta de novo nesta sessão.
static HW_BROKEN: AtomicBool = AtomicBool::new(false);

/// Placa de vídeo (Media Foundation) quando possível; senão OpenH264 na CPU.
/// Com o conversor, o quadro vai da captura ao encoder sem sair da placa de vídeo.
enum VideoEnc {
    Hw(crate::mf::HwEncoder, Option<crate::gpu::Converter>),
    Sw(Encoder),
}

impl VideoEnc {
    /// `src` é o quadro que chegou da captura (diz se ele está na GPU e o tamanho original).
    fn new(q: &Quality, bps: u32, w: usize, h: usize, src: &RawFrame, app: &Emit) -> Option<Self> {
        // Quadro-chave automático a cada 10 s só por segurança: quem assiste pede um na hora quando precisa.
        let (fps, br, gop) = (q.fps, bps, q.fps * 10);
        if let Some(g) = &src.gpu {
            // Quadro só na GPU: precisa do encoder de hardware lendo texturas.
            if !HW_BROKEN.load(Ordering::SeqCst) {
                let conv = crate::gpu::Converter::new(&g.dev, (src.width as u32, src.height as u32), (w as u32, h as u32));
                let enc = conv.as_ref().map_err(|e| e.message()).and_then(|_| crate::mf::HwEncoder::new(w, h, fps, br, gop, Some(&g.dev)));
                match (conv, enc) {
                    (Ok(c), Ok(e)) => return Some(Self::Hw(e, Some(c))),
                    (_, Err(err)) =>(app)("gpu-fallback", serde_json::json!(err)),
                    (Err(err), _) => (app)("gpu-fallback", serde_json::json!(err.message())),
                }
            }
            // Daqui em diante a captura manda os pixels pela memória.
            crate::gpu::BROKEN.store(true, Ordering::SeqCst);
            return None;
        }
        if !HW_BROKEN.load(Ordering::SeqCst) {
            match crate::mf::HwEncoder::new(w, h, fps, br, gop, None) {
                Ok(e) => return Some(Self::Hw(e, None)),
                Err(err) => {
                    HW_BROKEN.store(true, Ordering::SeqCst);
                    (app)("enc-fallback", serde_json::json!(err));
                }
            }
        }
        make_encoder(q, bps).map(Self::Sw)
    }
    /// Troca o bitrate em andamento. false = este encoder não deixa (aí é recriado).
    fn set_bitrate(&mut self, bps: u32) -> bool {
        match self {
            Self::Hw(e, _) => e.set_bitrate(bps),
            Self::Sw(_) => false,
        }
    }
    fn name(&self) -> String {
        match self {
            Self::Hw(e, c) => format!(
                "{} ({})",
                e.name.trim_end_matches("Encoder").trim_end_matches("h264").trim(),
                if c.is_some() && e.uses_gpu() { "GPU direto" } else { "GPU" }
            ),
            Self::Sw(_) => "OpenH264 (CPU)".into(),
        }
    }
    fn force_idr(&mut self) {
        match self {
            Self::Hw(e, _) => e.force_keyframe(),
            Self::Sw(e) => e.force_intra_frame(),
        }
    }
    fn encode(&mut self, f: &RawFrame) -> Result<Vec<u8>, String> {
        match (self, &f.gpu) {
            (Self::Hw(e, Some(c)), Some(g)) => {
                if !c.fits((f.width as u32, f.height as u32), c.dst) {
                    return Err("tamanho mudou".into()); // recria conversor e encoder
                }
                let r = c.convert(&g.tex).map_err(|x| x.message()).and_then(|t| e.encode_texture(&t));
                r.inspect_err(|_| crate::gpu::BROKEN.store(true, Ordering::SeqCst))
            }
            (Self::Hw(e, None), None) => e.encode(&f.bgra).inspect_err(|_| HW_BROKEN.store(true, Ordering::SeqCst)),
            (Self::Sw(e), None) => {
                let yuv = YUVBuffer::from_bgra8_source(BgraSliceU8::new(&f.bgra, (f.width, f.height)));
                e.encode(&yuv).map(|bs| bs.to_vec()).map_err(|x| x.to_string())
            }
            // Quadro e encoder de caminhos diferentes (a GPU acabou de ser desligada): recria.
            _ => Err("caminho trocado".into()),
        }
    }
}

fn make_encoder(q: &Quality, bps: u32) -> Option<Encoder> {
    let cfg = EncoderConfig::new()
        .usage_type(UsageType::ScreenContentRealTime)
        .bitrate(BitRate::from_bps(bps))
        .max_frame_rate(FrameRate::from_hz(q.fps as f32))
        .rate_control_mode(if q.pri == "detail" { RateControlMode::Quality } else { RateControlMode::Bitrate })
        .skip_frames(q.pri != "detail")
        .intra_frame_period(IntraFramePeriod::from_num_frames(q.fps * 10))
        .num_threads(4);
    Encoder::with_api_config(OpenH264API::from_source(), cfg).ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Captura a tela principal por ~2 s e codifica em H.264. `cargo test -- --ignored`
    #[test]
    #[ignore]
    fn capture_and_encode_primary_monitor() {
        let (tx, rx) = mpsc::sync_channel(1);
        let ctl = capture::start("m:1", 30, tx).expect("captura");
        let q = Quality::default();
        let (mut enc, mut frames, mut bytes, mut dims) = (None, 0, 0, (0, 0));
        let t = Instant::now();
        while t.elapsed() < Duration::from_secs(2) {
            let Ok(raw) = rx.recv_timeout(Duration::from_millis(500)) else { continue };
            let f = capture::downscale(&raw, q.res as usize);
            if (f.width, f.height) != dims {
                dims = (f.width, f.height);
                enc = make_encoder(&q, q.br * 1_000_000);
            }
            let yuv = YUVBuffer::from_bgra8_source(BgraSliceU8::new(&f.bgra, (f.width, f.height)));
            let out = enc.as_mut().unwrap().encode(&yuv).unwrap().to_vec();
            if !out.is_empty() {
                frames += 1;
                bytes += out.len();
            }
        }
        let _ = ctl.stop();
        println!("{frames} quadros codificados em 2 s, {dims:?}, {} kB", bytes / 1024);
        assert!(frames > 5, "poucos quadros: {frames}");
    }

    /// Mede quanto de CPU a transmissão gasta (captura + conversão + encoder), sem rede.
    /// `LUME_RES=0 cargo test cpu_cost -- --ignored --nocapture` (LUME_CPU=1 força o caminho antigo)
    #[test]
    #[ignore]
    fn cpu_cost() {
        use windows::Win32::Foundation::FILETIME;
        use windows::Win32::System::Threading::{GetCurrentProcess, GetProcessTimes};
        let cpu = || unsafe {
            let (mut a, mut b, mut k, mut u) = (FILETIME::default(), FILETIME::default(), FILETIME::default(), FILETIME::default());
            let _ = GetProcessTimes(GetCurrentProcess(), &mut a, &mut b, &mut k, &mut u);
            let t = |f: FILETIME| ((f.dwHighDateTime as u64) << 32 | f.dwLowDateTime as u64) as f64 / 1e7;
            t(k) + t(u)
        };
        let res: u32 = std::env::var("LUME_RES").ok().and_then(|v| v.parse().ok()).unwrap_or(1080);
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let engine = Arc::new(Engine::new(Arc::new(|ev: &str, v: serde_json::Value| {
                if ev != "enc-stats" || std::env::var_os("LUME_VERBOSE").is_some() {
                    println!("[{ev}] {v}")
                }
            })));
            engine.set_quality(Quality { res, fps: 60, pri: "motion".into(), br: 12 });
            let mut rx = engine.frames.subscribe();
            let src = std::env::var("LUME_SRC").unwrap_or("m:1".into());
            engine.start_share(&src, "Teste".into()).unwrap();
            tokio::time::sleep(Duration::from_secs(2)).await; // aquece
            while rx.try_recv().is_ok() {}
            let (c0, t0) = (cpu(), Instant::now());
            let (mut n, mut bytes) = (0, 0);
            while t0.elapsed() < Duration::from_secs(10) {
                if let Ok(Ok(f)) = tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
                    n += 1;
                    bytes += f.data.len();
                }
            }
            let (c1, el) = (cpu(), t0.elapsed().as_secs_f64());
            engine.stop_share();
            let cores = std::thread::available_parallelism().map(|n| n.get()).unwrap_or(1) as f64;
            println!(
                "caminho {}: {:.0} quadros/s, {:.1} Mbps, CPU {:.2} núcleo ({:.1}% do PC)",
                if crate::gpu::disabled() { "CPU" } else { "GPU direto" },
                n as f64 / el,
                bytes as f64 * 8.0 / el / 1e6,
                (c1 - c0) / el,
                (c1 - c0) / el / cores * 100.0
            );
        });
    }

    /// Teste de ponta a ponta com um navegador real como convidado:
    /// abra http://127.0.0.1:8765/ no navegador; a página manda a oferta e recebe a resposta.
    /// Transmite a tela principal + áudio do PC por 40 s.
    #[test]
    #[ignore]
    fn e2e_with_browser() {
        use std::io::{Read, Write};
        const PAGE: &str = r#"<!doctype html><video id=v autoplay playsinline muted></video><script>
(async()=>{const pc=new RTCPeerConnection({iceServers:[{urls:'stun:stun.l.google.com:19302'}]});window.pc=pc;
pc.addTransceiver('video',{direction:'recvonly'});pc.addTransceiver('audio',{direction:'recvonly'});pc.createDataChannel('ctl');
const ms=new MediaStream();pc.ontrack=e=>{ms.addTrack(e.track);v.srcObject=ms};
await pc.setLocalDescription(await pc.createOffer());
await new Promise(r=>{pc.onicegatheringstatechange=()=>pc.iceGatheringState==='complete'&&r();setTimeout(r,4000)});
const a=await (await fetch('/offer',{method:'POST',body:pc.localDescription.sdp})).text();
await pc.setRemoteDescription({type:'answer',sdp:a});})()</script>"#;
        let srv = std::net::TcpListener::bind("127.0.0.1:8765").unwrap();
        let read_req = |s: &mut std::net::TcpStream| -> (String, String) {
            let mut buf = vec![];
            let mut tmp = [0u8; 8192];
            loop {
                let n = s.read(&mut tmp).unwrap_or(0);
                if n == 0 && !String::from_utf8_lossy(&buf).contains("\r\n\r\n") {
                    return (String::new(), String::new()); // conexão vazia (pré-conexão do navegador)
                }
                buf.extend_from_slice(&tmp[..n]);
                let txt = String::from_utf8_lossy(&buf).to_string();
                if let Some(h) = txt.find("\r\n\r\n") {
                    let len = txt[..h].lines().find_map(|l| l.to_lowercase().strip_prefix("content-length:").map(|v| v.trim().parse::<usize>().unwrap())).unwrap_or(0);
                    if buf.len() >= h + 4 + len || n == 0 {
                        return (txt[..h].lines().next().unwrap_or("").to_string(), txt[h + 4..].to_string());
                    }
                }
            }
        };
        let (mut conn, offer) = loop {
            let (mut c, _) = srv.accept().unwrap();
            let (line, body) = read_req(&mut c);
            if line.starts_with("POST /offer") {
                break (c, body);
            }
            let _ = write!(c, "HTTP/1.1 200 OK\r\nContent-Type: text/html\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{PAGE}", PAGE.len());
        };
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async move {
            let engine = Arc::new(Engine::new(Arc::new(|ev: &str, v: serde_json::Value| println!("[{ev}] {v}"))));
            engine.start_share("m:1", "Tela 1".into()).unwrap();
            engine.set_audio(&AudioSel { mode: "all".into(), pid: 0 }).unwrap();
            let (pc, answer) = engine.accept_guest("Teste".into(), offer).await.unwrap();
            let _ = write!(conn, "HTTP/1.1 200 OK\r\nContent-Length: {}\r\nConnection: close\r\n\r\n{answer}", answer.len());
            drop(conn);
            tokio::time::sleep(Duration::from_secs(40)).await;
            engine.stop_share();
            let _ = pc.close().await;
        });
    }
}

/// Bitrate ajustado pela qualidade automática (kbps; 0 = usa o máximo escolhido).
static AUTO_KBPS: AtomicU32 = AtomicU32::new(0);
/// Pedidos de quadro-chave recebidos de quem assiste (diagnóstico, vai nas estatísticas).
static KEY_REQUESTS: AtomicU32 = AtomicU32::new(0);

/// Bitrate que o encoder deve usar agora: o automático, sem passar do máximo escolhido.
fn target_bps(q: &Quality) -> u32 {
    let max = q.br * 1_000_000;
    match AUTO_KBPS.load(Ordering::SeqCst) {
        0 => max,
        k => (k * 1000).clamp(300_000, max),
    }
}

fn encode_loop(
    rx: mpsc::Receiver<RawFrame>,
    quality: Arc<Mutex<Quality>>,
    ver: Arc<AtomicU64>,
    idr: Arc<AtomicBool>,
    out: broadcast::Sender<EncodedFrame>,
    stop: Arc<AtomicBool>,
    app: Emit,
) {
    let mut q = quality.lock().unwrap().clone();
    let mut seen = ver.load(Ordering::SeqCst);
    let mut enc: Option<VideoEnc> = None;
    let mut applied = 0u32;
    let mut enc_name = String::new();
    let mut last = Instant::now();
    let (mut n, mut bytes, mut window) = (0u32, 0usize, Instant::now());
    let (mut got, mut skipped) = (0u32, 0u32);
    let mut dims = ((0, 0), (0, 0));

    let mut last_raw: Option<RawFrame> = None;
    while !stop.load(Ordering::SeqCst) {
        let v = ver.load(Ordering::SeqCst);
        // Tela parada => o Windows não manda quadros. Se alguém entrou ou a qualidade mudou,
        // reaproveita o último quadro para o convidado não ficar com a tela preta.
        let raw = match rx.recv_timeout(Duration::from_millis(150)) {
            Ok(r) => {
                got += 1;
                r
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => break,
            Err(_) => match &last_raw {
                Some(r) if idr.load(Ordering::SeqCst) || v != seen => r.clone(),
                _ => {
                    // Tela parada: avisa mesmo assim (0 fps), para a interface não ficar em "iniciando…".
                    if window.elapsed() >= Duration::from_secs(2) && enc.is_some() {
                        let s = window.elapsed().as_secs_f32();
                        let (w, h) = dims.0;
                        (app)("enc-stats", serde_json::to_value(EncStats { width: w, height: h, fps: n as f32 / s, kbps: bytes as f32 * 8.0 / 1000.0 / s, encoder: enc_name.clone(), src_fps: got as f32 / s, skipped, key_requests: KEY_REQUESTS.load(Ordering::Relaxed) }).unwrap());
                        n = 0;
                        bytes = 0;
                        got = 0;
                        skipped = 0;
                        window = Instant::now();
                    }
                    continue;
                }
            },
        };
        if v != seen {
            seen = v;
            q = quality.lock().unwrap().clone();
            enc = None;
        }
        // Na GPU o conversor reduz junto com a conversão de cor; na CPU reduz aqui.
        let f = if raw.gpu.is_some() { raw.clone() } else { capture::downscale(&raw, q.res as usize) };
        let out_dims = crate::gpu::target_dims(raw.width, raw.height, q.res as usize);
        let src_dims = (raw.width, raw.height);
        last_raw = Some(raw);
        if (out_dims, src_dims) != dims {
            dims = (out_dims, src_dims);
            enc = None; // mudança de tamanho (ex.: janela redimensionada)
        }
        if enc.is_none() {
            applied = target_bps(&q);
            enc = VideoEnc::new(&q, applied, out_dims.0, out_dims.1, &f, &app);
            enc_name = enc.as_ref().map(|e| e.name()).unwrap_or_default();
            idr.store(true, Ordering::SeqCst); // encoder novo começa com quadro-chave
        }
        let Some(e) = enc.as_mut() else { continue };
        let want = target_bps(&q);
        if want != applied {
            applied = want;
            if !e.set_bitrate(want) {
                enc = None; // encoder na CPU: recria com o bitrate novo no próximo quadro
                continue;
            }
        }
        if idr.swap(false, Ordering::SeqCst) {
            e.force_idr();
        }
        let data = match e.encode(&f) {
            Ok(d) => d,
            Err(_) => {
                enc = None; // tenta de novo (já cai para a CPU se a GPU falhou)
                continue;
            }
        };
        if data.is_empty() {
            skipped += 1;
            continue; // quadro pulado pelo controle de taxa
        }
        let now = Instant::now();
        let duration = now - last;
        last = now;
        bytes += data.len();
        n += 1;
        let _ = out.send(EncodedFrame { data: Bytes::from(data), duration });

        if window.elapsed() >= Duration::from_secs(1) {
            let s = window.elapsed().as_secs_f32();
            (app)("enc-stats", serde_json::to_value(EncStats { width: out_dims.0, height: out_dims.1, fps: n as f32 / s, kbps: bytes as f32 * 8.0 / 1000.0 / s, encoder: enc_name.clone(), src_fps: got as f32 / s, skipped, key_requests: KEY_REQUESTS.load(Ordering::Relaxed) }).unwrap());
            n = 0;
            bytes = 0;
            got = 0;
            skipped = 0;
            window = Instant::now();
        }
    }
}
