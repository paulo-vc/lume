//! Deixa os pedidos de quadro-chave de quem assiste (PLI/FIR) chegarem ao app.
//!
//! A biblioteca de WebRTC entrega o RTCP que chega só aos interceptadores dela e descarta o resto
//! antes do app, a menos que algum interceptador marque o pacote com `DeliverToApplication`. Sem
//! isto, o pedido "manda uma imagem completa, a minha estragou" nunca era atendido e a tela de
//! quem assiste ficava parada até o quadro-chave automático.

use rtc::interceptor::{Attribute, Interceptor, Packet, StreamInfo, TaggedPacket};
use rtc::rtcp::payload_feedbacks::full_intra_request::FullIntraRequest;
use rtc::rtcp::payload_feedbacks::picture_loss_indication::PictureLossIndication;
use rtc::sansio::Protocol;
use rtc::shared::error::Error;
use std::collections::VecDeque;
use std::time::Instant;

/// Só observa: não cria nem segura pacotes, então pode ficar em qualquer posição da cadeia.
pub const SLOT: usize = 11_500;

#[derive(Default)]
pub struct KeyframeRequests {
    read_queue: VecDeque<TaggedPacket>,
    write_queue: VecDeque<TaggedPacket>,
}

impl Protocol<TaggedPacket, TaggedPacket, ()> for KeyframeRequests {
    type Rout = TaggedPacket;
    type Wout = TaggedPacket;
    type Eout = ();
    type Error = Error;
    type Time = Instant;

    fn handle_read(&mut self, mut msg: TaggedPacket) -> Result<(), Self::Error> {
        if let Packet::Rtcp(pkts) = &msg.message.packet {
            let wants = pkts.iter().any(|p| p.as_any().is::<PictureLossIndication>() || p.as_any().is::<FullIntraRequest>());
            if wants && !msg.message.has(&Attribute::DeliverToApplication) {
                msg.message.add(Attribute::DeliverToApplication);
            }
        }
        self.read_queue.push_back(msg);
        Ok(())
    }

    fn poll_read(&mut self) -> Option<Self::Rout> {
        self.read_queue.pop_front()
    }

    fn handle_write(&mut self, msg: TaggedPacket) -> Result<(), Self::Error> {
        self.write_queue.push_back(msg);
        Ok(())
    }

    fn poll_write(&mut self) -> Option<Self::Wout> {
        self.write_queue.pop_front()
    }
}

impl Interceptor for KeyframeRequests {
    fn bind_local_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_local_stream(&mut self, _info: &StreamInfo) {}
    fn bind_remote_stream(&mut self, _info: &StreamInfo) {}
    fn unbind_remote_stream(&mut self, _info: &StreamInfo) {}
}
