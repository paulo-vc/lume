//! Retransmissão pelo anfitrião: quando dois convidados não conseguem conexão direta (os dois
//! atrás de rede que não deixa abrir porta), o anfitrião, que já está conectado aos dois, puxa a
//! tela de quem transmite e repassa os mesmos pacotes para quem assiste. Nada é decodificado nem
//! recodificado: os pacotes RTP passam direto, cifrados de novo em cada conexão.

use crate::net::Mapping;
use rtc::interceptor::Registry;
use rtc::media_stream::MediaStreamTrack;
use rtc::peer_connection::configuration::interceptor_registry::register_default_interceptors;
use rtc::peer_connection::configuration::media_engine::{MediaEngine, MIME_TYPE_H264, MIME_TYPE_OPUS};
use rtc::peer_connection::configuration::RTCConfigurationBuilder;
use rtc::peer_connection::sdp::RTCSessionDescription;
use rtc::peer_connection::transport::RTCIceServer;
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::rtp_transceiver::rtp_sender::{RTCRtpCodec, RTCRtpCodecParameters, RTCRtpCodingParameters, RTCRtpEncodingParameters, RtpCodecKind};
use rtc::rtp_transceiver::{RTCRtpTransceiverDirection, RTCRtpTransceiverInit};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::{Duration, Instant};
use tokio::sync::{broadcast, mpsc, Mutex};
use webrtc::media_stream::track_local::static_rtp::TrackLocalStaticRTP;
use webrtc::media_stream::track_local::{TrackLocal, TrackLocalEvent};
use webrtc::media_stream::track_remote::{TrackRemote, TrackRemoteEvent};
use webrtc::media_stream::Track;
use webrtc::peer_connection::{PeerConnection, PeerConnectionBuilder, PeerConnectionEventHandler, RTCIceGatheringState};

fn h264() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_H264.to_owned(),
            clock_rate: 90000,
            channels: 0,
            sdp_fmtp_line: "level-asymmetry-allowed=1;packetization-mode=1;profile-level-id=42e01f".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 102,
        ..Default::default()
    }
}

fn opus() -> RTCRtpCodecParameters {
    RTCRtpCodecParameters {
        rtp_codec: RTCRtpCodec {
            mime_type: MIME_TYPE_OPUS.to_owned(),
            clock_rate: 48000,
            channels: 2,
            sdp_fmtp_line: "minptime=10;useinbandfec=1".to_owned(),
            rtcp_feedback: vec![],
        },
        payload_type: 111,
        ..Default::default()
    }
}

struct Handler {
    gather_tx: mpsc::Sender<()>,
    track_tx: Option<mpsc::UnboundedSender<Arc<dyn TrackRemote>>>,
}

#[async_trait::async_trait]
impl PeerConnectionEventHandler for Handler {
    async fn on_ice_gathering_state_change(&self, state: RTCIceGatheringState) {
        if state == RTCIceGatheringState::Complete {
            let _ = self.gather_tx.try_send(());
        }
    }
    async fn on_track(&self, track: Arc<dyn TrackRemote>) {
        if let Some(tx) = &self.track_tx {
            let _ = tx.send(track);
        }
    }
}

/// Uma conexão nova, com porta aberta no roteador (UPnP) quando der.
async fn build(track_tx: Option<mpsc::UnboundedSender<Arc<dyn TrackRemote>>>, expose: bool) -> Result<(Arc<dyn PeerConnection>, Option<Mapping>, mpsc::Receiver<()>), String> {
    let e = |x: &dyn std::fmt::Display| x.to_string();
    let mut me = MediaEngine::default();
    me.register_codec(h264(), RtpCodecKind::Video).map_err(|x| e(&x))?;
    me.register_codec(opus(), RtpCodecKind::Audio).map_err(|x| e(&x))?;
    let registry = register_default_interceptors(Registry::new(), &mut me)
            .map_err(|x| e(&x))?
            .with(rtc::interceptor::Slot::from(crate::keyreq::SLOT), crate::keyreq::KeyframeRequests::default());
    let config = RTCConfigurationBuilder::new()
        .with_ice_servers(vec![RTCIceServer { urls: vec!["stun:stun.l.google.com:19302".into(), "stun:stun.cloudflare.com:3478".into()], ..Default::default() }])
        .build();
    let (gather_tx, gather_rx) = mpsc::channel(1);
    let mapping = if expose { crate::net::open_port().await } else { None };
    let addrs = crate::net::udp_addrs(mapping);
    let runtime = webrtc::runtime::default_runtime().ok_or("runtime indisponível")?;
    let pc: Arc<dyn PeerConnection> = Arc::new(
        PeerConnectionBuilder::new()
            .with_configuration(config)
            .with_media_engine(me)
            .with_interceptor_registry(registry)
            .with_handler(Arc::new(Handler { gather_tx, track_tx }))
            .with_runtime(runtime)
            .with_udp_addrs(addrs)
            .build()
            .await
            .map_err(|x| e(&x))?,
    );
    Ok((pc, mapping, gather_rx))
}

async fn local_sdp(pc: &Arc<dyn PeerConnection>, gather: &mut mpsc::Receiver<()>, mapping: Option<Mapping>) -> Result<String, String> {
    let _ = tokio::time::timeout(Duration::from_secs(5), gather.recv()).await;
    let mut sdp = pc.local_description().await.ok_or("Falha ao gerar a descrição da conexão")?.sdp;
    if let Some(m) = mapping {
        sdp = crate::net::add_public_candidate(&sdp, m);
    }
    Ok(sdp)
}

/// O último pacote de vídeo da fonte diz o SSRC a quem pedir quadro-chave.
type Upstream = Arc<std::sync::Mutex<Option<(Arc<dyn TrackRemote>, u32)>>>;

fn ask_keyframe(up: &Upstream) {
    let target = up.lock().unwrap().clone();
    if let Some((t, ssrc)) = target {
        tokio::spawn(async move {
            let _ = t.write_rtcp(vec![Box::new(PictureLossIndication { sender_ssrc: 0, media_ssrc: ssrc })]).await;
        });
    }
}

struct Source {
    pc: Arc<dyn PeerConnection>,
    mapping: Option<Mapping>,
    video: broadcast::Sender<rtc::rtp::Packet>,
    audio: broadcast::Sender<rtc::rtp::Packet>,
    upstream: Upstream,
    viewers: HashMap<String, (Arc<dyn PeerConnection>, Option<Mapping>)>,
}

#[derive(Default)]
pub struct Relay {
    sources: Mutex<HashMap<String, Source>>,
}

impl Relay {
    /// Começa a puxar a tela de `source`: devolve a oferta (só recebe) para mandar a quem transmite.
    pub async fn pull_offer(&self, source: &str) -> Result<String, String> {
        self.stop(source).await;
        let (track_tx, mut track_rx) = mpsc::unbounded_channel::<Arc<dyn TrackRemote>>();
        let (pc, mapping, mut gather) = build(Some(track_tx), true).await?;
        let recv = || Some(RTCRtpTransceiverInit { direction: RTCRtpTransceiverDirection::Recvonly, streams: vec![], send_encodings: vec![] });
        pc.add_transceiver_from_kind(RtpCodecKind::Video, recv()).await.map_err(|x| x.to_string())?;
        pc.add_transceiver_from_kind(RtpCodecKind::Audio, recv()).await.map_err(|x| x.to_string())?;
        let offer = pc.create_offer(None).await.map_err(|x| x.to_string())?;
        pc.set_local_description(offer).await.map_err(|x| x.to_string())?;
        let sdp = local_sdp(&pc, &mut gather, mapping).await?;

        let (video, _) = broadcast::channel(512);
        let (audio, _) = broadcast::channel(256);
        let upstream: Upstream = Default::default();
        let (v, a, up) = (video.clone(), audio.clone(), upstream.clone());
        tokio::spawn(async move {
            while let Some(t) = track_rx.recv().await {
                let is_video = t.kind().await == RtpCodecKind::Video;
                let (tx, up) = (if is_video { v.clone() } else { a.clone() }, up.clone());
                tokio::spawn(async move {
                    while let Some(ev) = t.poll().await {
                        match ev {
                            TrackRemoteEvent::OnRtpPacket(p) => {
                                if is_video {
                                    let mut g = up.lock().unwrap();
                                    if g.as_ref().is_none_or(|(_, s)| *s != p.header.ssrc) {
                                        *g = Some((t.clone(), p.header.ssrc));
                                    }
                                }
                                let _ = tx.send(p);
                            }
                            TrackRemoteEvent::OnEnded => break,
                            _ => {}
                        }
                    }
                });
            }
        });
        self.sources.lock().await.insert(source.to_string(), Source { pc, mapping, video, audio, upstream, viewers: HashMap::new() });
        Ok(sdp)
    }

    pub async fn pull_answer(&self, source: &str, sdp: String) -> Result<(), String> {
        let pc = self.sources.lock().await.get(source).map(|s| s.pc.clone()).ok_or("Retransmissão não iniciada")?;
        let ans = RTCSessionDescription::answer(sdp).map_err(|x| x.to_string())?;
        pc.set_remote_description(ans).await.map_err(|x| x.to_string())
    }

    /// Alguém quer assistir `source` pelo anfitrião: recebe a oferta dele e devolve a resposta.
    pub async fn add_viewer(&self, source: &str, viewer: &str, offer_sdp: String) -> Result<String, String> {
        let (vrx, arx, upstream) = {
            let g = self.sources.lock().await;
            let s = g.get(source).ok_or("Retransmissão não iniciada")?;
            (s.video.subscribe(), s.audio.subscribe(), s.upstream.clone())
        };
        let (pc, mapping, mut gather) = build(None, !viewer.starts_with('@')).await?;
        let track = |kind, codec: RTCRtpCodecParameters, id: &str| {
            Arc::new(TrackLocalStaticRTP::new(MediaStreamTrack::new(
                "lume".into(),
                id.into(),
                id.into(),
                kind,
                vec![RTCRtpEncodingParameters {
                    rtp_coding_parameters: RTCRtpCodingParameters { ssrc: Some(rand::random::<u32>()), ..Default::default() },
                    codec: codec.rtp_codec,
                    ..Default::default()
                }],
            )))
        };
        let vtrack = track(RtpCodecKind::Video, h264(), "lume-video");
        let atrack = track(RtpCodecKind::Audio, opus(), "lume-audio");
        let offer = RTCSessionDescription::offer(offer_sdp).map_err(|x| x.to_string())?;
        pc.set_remote_description(offer).await.map_err(|x| x.to_string())?;
        let vsender = pc.add_track(vtrack.clone() as Arc<dyn TrackLocal>).await.map_err(|x| x.to_string())?;
        let asender = pc.add_track(atrack.clone() as Arc<dyn TrackLocal>).await.map_err(|x| x.to_string())?;
        let answer = pc.create_answer(None).await.map_err(|x| x.to_string())?;
        pc.set_local_description(answer).await.map_err(|x| x.to_string())?;
        let sdp = local_sdp(&pc, &mut gather, mapping).await?;

        // Repasse dos pacotes: troca SSRC e tipo de carga pelos desta conexão e tira as extensões
        // (os números delas foram negociados na outra conexão).
        for (mut rx, t, sender, video) in [(vrx, vtrack.clone(), vsender, true), (arx, atrack.clone(), asender, false)] {
            let up = upstream.clone();
            tokio::spawn(async move {
                let mut fixed: Option<(u32, u8)> = None;
                let mut errors = 0u32;
                // Só para teste: LUME_TEST_DROP=N descarta 1 a cada N pacotes de vídeo (simula perda).
                let drop_every: u64 = std::env::var("LUME_TEST_DROP").ok().and_then(|v| v.parse().ok()).unwrap_or(0);
                let mut count = 0u64;
                loop {
                    match rx.recv().await {
                        Ok(mut p) => {
                            count += 1;
                            if video && drop_every > 0 && count % drop_every == 0 {
                                continue;
                            }
                            if fixed.is_none() {
                                let ssrc = t.ssrcs().await.first().copied();
                                let pt = sender.get_parameters().await.ok().and_then(|x| x.rtp_parameters.codecs.first().map(|c| c.payload_type));
                                match (ssrc, pt) {
                                    (Some(s), Some(pt)) => fixed = Some((s, pt)),
                                    _ => continue, // ainda conectando
                                }
                            }
                            let (s, pt) = fixed.unwrap();
                            p.header.ssrc = s;
                            p.header.payload_type = pt;
                            p.header.extension = false;
                            p.header.extensions.clear();
                            if t.write_rtp(p).await.is_err() {
                                errors += 1;
                                if errors > 500 {
                                    break; // a conexão de quem assistia fechou
                                }
                            } else {
                                errors = 0;
                            }
                        }
                        Err(broadcast::error::RecvError::Lagged(_)) => {
                            if video {
                                ask_keyframe(&up);
                            }
                        }
                        Err(broadcast::error::RecvError::Closed) => break,
                    }
                }
            });
        }
        // Quem assiste pede quadro-chave (PLI/FIR) -> repassa a quem transmite.
        let up = upstream.clone();
        let vt = vtrack.clone();
        tokio::spawn(async move {
            let mut last: Option<Instant> = None;
            while let Some(TrackLocalEvent::OnRtcpPacket(pkts)) = vt.poll().await {
                let wants = pkts.iter().any(|p| p.as_any().is::<PictureLossIndication>() || p.as_any().is::<FullIntraRequest>());
                if wants && last.is_none_or(|l| l.elapsed() >= Duration::from_millis(500)) {
                    last = Some(Instant::now());
                    ask_keyframe(&up);
                }
            }
        });
        // Quem chegou agora precisa de um quadro-chave para começar a ver.
        let up = upstream.clone();
        tokio::spawn(async move {
            for _ in 0..3 {
                tokio::time::sleep(Duration::from_millis(1200)).await;
                ask_keyframe(&up);
            }
        });

        let old = {
            let mut g = self.sources.lock().await;
            match g.get_mut(source) {
                Some(s) => s.viewers.insert(viewer.to_string(), (pc, mapping)),
                None => {
                    let _ = pc.close().await;
                    return Err("A transmissão acabou".into());
                }
            }
        };
        if let Some((old, m)) = old {
            let _ = old.close().await;
            if let Some(m) = m {
                crate::net::close_port(m.port).await;
            }
        }
        Ok(sdp)
    }

    /// Alguém saiu da sala: para de repassar para ele.
    pub async fn drop_viewer(&self, viewer: &str) {
        let gone: Vec<_> = self.sources.lock().await.values_mut().filter_map(|s| s.viewers.remove(viewer)).collect();
        for (pc, m) in gone {
            let _ = pc.close().await;
            if let Some(m) = m {
                crate::net::close_port(m.port).await;
            }
        }
    }

    /// Quem transmitia parou ou saiu: fecha tudo da retransmissão dele.
    pub async fn stop(&self, source: &str) {
        let Some(s) = self.sources.lock().await.remove(source) else { return };
        let _ = s.pc.close().await;
        let mut ports: Vec<u16> = s.mapping.map(|m| m.port).into_iter().collect();
        for (_, (pc, m)) in s.viewers {
            let _ = pc.close().await;
            ports.extend(m.map(|m| m.port));
        }
        for p in ports {
            crate::net::close_port(p).await;
        }
    }

    pub async fn stop_all(&self) {
        let names: Vec<String> = self.sources.lock().await.keys().cloned().collect();
        for n in names {
            self.stop(&n).await;
        }
    }
}
