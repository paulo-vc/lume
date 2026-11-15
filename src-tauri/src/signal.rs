//! Ponto de encontro: relays MQTT públicos, usados só para os dois lados trocarem os dados
//! de conexão (cifrados com a chave da sala). Vídeo e áudio nunca passam por aqui.
//! Usa vários relays ao mesmo tempo; basta um funcionar.

use rumqttc::{AsyncClient, Event, MqttOptions, Packet, QoS};
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::mpsc;
use tokio::task::JoinHandle;

const RELAYS: &[(&str, u16)] = &[("broker.emqx.io", 1883), ("broker.hivemq.com", 1883), ("test.mosquitto.org", 1883)];

pub struct Bus {
    clients: Vec<AsyncClient>,
    tasks: Vec<JoinHandle<()>>,
    pub online: Arc<AtomicUsize>,
}

impl Bus {
    /// Conecta em todos os relays e assina `topic`. As mensagens chegam em `rx` (com repetição
    /// entre relays; quem recebe deve ignorar duplicatas).
    pub fn start(topic: String) -> (Self, mpsc::Receiver<Vec<u8>>) {
        let (tx, rx) = mpsc::channel(64);
        let online = Arc::new(AtomicUsize::new(0));
        let (mut clients, mut tasks) = (vec![], vec![]);
        for (host, port) in RELAYS {
            let mut o = MqttOptions::new(format!("lume-{}", crate::room::new_id()), *host, *port);
            o.set_keep_alive(Duration::from_secs(20));
            o.set_max_packet_size(128 * 1024, 128 * 1024);
            let (client, mut ev) = AsyncClient::new(o, 32);
            let (c2, tx, topic, online) = (client.clone(), tx.clone(), topic.clone(), online.clone());
            tasks.push(tokio::spawn(async move {
                let mut up = false;
                loop {
                    match ev.poll().await {
                        Ok(Event::Incoming(Packet::ConnAck(_))) => {
                            let _ = c2.try_subscribe(topic.clone(), QoS::AtLeastOnce);
                            if !up {
                                up = true;
                                online.fetch_add(1, Ordering::SeqCst);
                            }
                        }
                        Ok(Event::Incoming(Packet::Publish(p))) => {
                            if tx.send(p.payload.to_vec()).await.is_err() {
                                break;
                            }
                        }
                        Ok(_) => {}
                        Err(_) => {
                            if up {
                                up = false;
                                online.fetch_sub(1, Ordering::SeqCst);
                            }
                            tokio::time::sleep(Duration::from_secs(2)).await;
                        }
                    }
                }
            }));
            clients.push(client);
        }
        (Self { clients, tasks, online }, rx)
    }

    /// Publica em todos os relays (o que estiver fora do ar é ignorado).
    pub fn publish(&self, topic: &str, payload: &[u8]) {
        for c in &self.clients {
            let _ = c.try_publish(topic, QoS::AtLeastOnce, false, payload.to_vec());
        }
    }
}

impl Drop for Bus {
    fn drop(&mut self) {
        for c in &self.clients {
            let _ = c.try_disconnect();
        }
        for t in &self.tasks {
            t.abort();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::room::{JoinReq, JoinRes, Keys};

    /// Pedido e resposta passando pelos relays públicos de verdade. `cargo test relay -- --ignored`
    #[tokio::test]
    #[ignore]
    async fn relay_roundtrip() {
        let code = crate::room::new_code();
        let keys = Keys::from_code(&code).unwrap();
        let (host, mut host_rx) = Bus::start(keys.requests());
        let id = crate::room::new_id();
        let (guest, mut guest_rx) = Bus::start(keys.answer(&id));
        tokio::time::sleep(Duration::from_secs(3)).await;
        println!("relays no ar: anfitrião {}, convidado {}", host.online.load(Ordering::SeqCst), guest.online.load(Ordering::SeqCst));

        let t = std::time::Instant::now();
        guest.publish(&keys.requests(), &keys.seal(&JoinReq { id: id.clone(), name: "Ana".into(), sdp: "x".repeat(9000) }));
        let got = tokio::time::timeout(Duration::from_secs(10), host_rx.recv()).await.expect("pedido não chegou").unwrap();
        let req: JoinReq = keys.open(&got).expect("pedido não abriu");
        assert_eq!(req.name, "Ana");
        host.publish(&keys.answer(&req.id), &keys.seal(&JoinRes { ok: true, sdp: "resposta".into(), why: String::new() }));
        let got = tokio::time::timeout(Duration::from_secs(10), guest_rx.recv()).await.expect("resposta não chegou").unwrap();
        let res: JoinRes = keys.open(&got).unwrap();
        assert!(res.ok && res.sdp == "resposta");
        println!("ida e volta em {} ms", t.elapsed().as_millis());
    }
}
