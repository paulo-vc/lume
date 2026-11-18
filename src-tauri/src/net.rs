//! Deixar o PC alcançável sem servidor externo: pede ao roteador (UPnP) para abrir uma porta UDP
//! para cada conexão e anuncia o endereço público junto dos outros candidatos. Com a porta aberta,
//! a outra pessoa conecta mesmo estando atrás de NAT restritivo. Não resolve quando a própria
//! internet usa CGNAT (o endereço "público" do roteador é do provedor) — isso é detectado e avisado.

use igd_next::aio::tokio::{search_gateway, Tokio};
use igd_next::aio::Gateway;
use igd_next::{PortMappingProtocol, SearchOptions};
use serde::Serialize;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Clone, Serialize, Default)]
pub struct Status {
    /// ainda procurando o roteador
    pub checking: bool,
    /// o roteador aceita abrir portas
    pub upnp: bool,
    /// a internet usa CGNAT (o roteador não tem IP público)
    pub cgnat: bool,
}

struct State {
    done: bool,
    gw: Option<Gateway<Tokio>>,
    local: Option<Ipv4Addr>,
    external: Option<Ipv4Addr>,
}

static STATE: Mutex<State> = Mutex::const_new(State { done: false, gw: None, local: None, external: None });

fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private() || ip.is_loopback() || ip.is_unspecified() || (o[0] == 100 && (o[1] & 0xc0) == 64)
}

/// Procura o roteador (uma vez; chamado na abertura do app).
pub async fn probe() {
    let mut st = STATE.lock().await;
    if st.done {
        return;
    }
    st.local = match local_ip_address::local_ip() {
        Ok(IpAddr::V4(ip)) => Some(ip),
        _ => None,
    };
    let opts = SearchOptions { timeout: Some(Duration::from_secs(4)), ..Default::default() };
    if let Ok(gw) = search_gateway(opts).await {
        if let Ok(IpAddr::V4(ext)) = gw.get_external_ip().await {
            st.external = Some(ext);
        }
        st.gw = Some(gw);
    }
    st.done = true;
}

pub async fn status() -> Status {
    let st = STATE.lock().await;
    Status {
        checking: !st.done,
        upnp: st.gw.is_some() && st.external.is_some_and(|e| !is_cgnat(e)),
        cgnat: st.external.is_some_and(is_cgnat),
    }
}

/// Uma porta aberta no roteador: (porta local = porta externa, IP público, IP local).
#[derive(Clone, Copy)]
pub struct Mapping {
    pub port: u16,
    pub external: Ipv4Addr,
    #[allow(dead_code)]
    pub local: Ipv4Addr,
}

/// Abre uma porta UDP para uma conexão. None se o roteador não deixar (segue sem ela).
pub async fn open_port() -> Option<Mapping> {
    probe().await;
    let st = STATE.lock().await;
    let (gw, local, external) = (st.gw.as_ref()?, st.local?, st.external?);
    if is_cgnat(external) {
        return None; // abrir porta não adianta: o endereço público é do provedor
    }
    for _ in 0..4 {
        let port = 40000 + rand::random::<u16>() % 20000;
        // A porta precisa estar livre aqui também (a conexão vai usá-la).
        if std::net::UdpSocket::bind(("0.0.0.0", port)).is_err() {
            continue;
        }
        let addr = SocketAddr::new(IpAddr::V4(local), port);
        if tokio::time::timeout(Duration::from_secs(3), gw.add_port(PortMappingProtocol::UDP, port, addr, 7200, "Lume"))
            .await
            .is_ok_and(|r| r.is_ok())
        {
            return Some(Mapping { port, external, local });
        }
    }
    None
}

pub async fn close_port(port: u16) {
    let st = STATE.lock().await;
    if let Some(gw) = &st.gw {
        let _ = tokio::time::timeout(Duration::from_secs(3), gw.remove_port(PortMappingProtocol::UDP, port)).await;
    }
}

/// Acrescenta o endereço público como candidato (srflx) ao lado do candidato local dessa porta.
pub fn add_public_candidate(sdp: &str, m: Mapping) -> String {
    let mut out = String::with_capacity(sdp.len() + 200);
    for line in sdp.split_inclusive('\n') {
        out.push_str(line);
        let t = line.trim_end();
        let Some(rest) = t.strip_prefix("a=candidate:") else { continue };
        let f: Vec<&str> = rest.split_whitespace().collect();
        // foundation componente transporte prioridade ip porta typ host
        if f.len() >= 8 && f[2].eq_ignore_ascii_case("udp") && f[5] == m.port.to_string() && f[7] == "host" && f[4].parse::<Ipv4Addr>().is_ok() {
            let prio: u64 = f[3].parse().unwrap_or(0);
            let prio = (prio / 2).max(1); // abaixo do candidato local (rede de casa continua preferida)
            out.push_str(&format!(
                "a=candidate:{}9 {} udp {} {} {} typ srflx raddr {} rport {}\r\n",
                f[0], f[1], prio, m.external, m.port, f[4], m.port
            ));
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cgnat_ranges() {
        assert!(is_cgnat("100.70.1.2".parse().unwrap()));
        assert!(is_cgnat("192.168.0.5".parse().unwrap()));
        assert!(!is_cgnat("177.10.20.30".parse().unwrap()));
    }

    #[test]
    fn adds_candidate() {
        let sdp = "v=0\r\na=candidate:1 1 udp 2130706431 192.168.0.10 45000 typ host\r\na=candidate:2 1 udp 2130706431 192.168.0.10 50000 typ host\r\n";
        let m = Mapping { port: 45000, external: "177.10.20.30".parse().unwrap(), local: "192.168.0.10".parse().unwrap() };
        let out = add_public_candidate(sdp, m);
        assert!(out.contains("a=candidate:19 1 udp 1065353215 177.10.20.30 45000 typ srflx raddr 192.168.0.10 rport 45000\r\n"));
        assert_eq!(out.matches("typ srflx").count(), 1);
    }

    /// Mostra o que o roteador daqui permite. `cargo test upnp_here -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn upnp_here() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            probe().await;
            println!("status: upnp={} cgnat={}", status().await.upnp, status().await.cgnat);
            if let Some(m) = open_port().await {
                println!("porta {} aberta: {}:{} -> {}:{}", m.port, m.external, m.port, m.local, m.port);
                close_port(m.port).await;
            } else {
                println!("não foi possível abrir porta");
            }
        });
    }
}
