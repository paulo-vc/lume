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

/// IP deste PC na rede de casa (a que tem o roteador). VPNs como Radmin e Hamachi usam faixas
/// que não são de rede local (26.x, 25.x), então ficam de fora.
fn home_ip() -> Option<Ipv4Addr> {
    let lan = |ip: Ipv4Addr| ip.is_private() && !ip.is_link_local();
    // O IP que o Windows usaria para sair para a internet.
    let routed = std::net::UdpSocket::bind("0.0.0.0:0")
        .and_then(|s| s.connect("1.1.1.1:80").map(|_| s))
        .and_then(|s| s.local_addr())
        .ok()
        .and_then(|a| match a.ip() {
            IpAddr::V4(ip) => Some(ip),
            _ => None,
        });
    if let Some(ip) = routed.filter(|ip| lan(*ip)) {
        return Some(ip);
    }
    local_ip_address::list_afinet_netifas().ok()?.into_iter().find_map(|(_, ip)| match ip {
        IpAddr::V4(ip) if lan(ip) => Some(ip),
        _ => None,
    })
}

/// Procura o roteador (uma vez; chamado na abertura do app).
pub async fn probe() {
    let mut st = STATE.lock().await;
    if st.done {
        return;
    }
    st.local = home_ip();
    // Busca pela placa que sai para a internet (não por VPNs como Radmin/Hamachi, que também
    // respondem e não têm nada a ver com o roteador de casa).
    let Some(local) = st.local else {
        st.done = true;
        return;
    };
    let opts = SearchOptions {
        bind_addr: SocketAddr::new(IpAddr::V4(local), 0),
        timeout: Some(Duration::from_secs(4)),
        ..Default::default()
    };
    if let Ok(gw) = search_gateway(opts).await {
        let same_net = match gw.addr.ip() {
            IpAddr::V4(g) => g.octets()[..3] == local.octets()[..3],
            _ => false,
        };
        if same_net {
            if let Ok(IpAddr::V4(ext)) = gw.get_external_ip().await {
                st.external = Some(ext);
            }
            st.gw = Some(gw);
        }
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

/// Para as conexões do navegador interno (quem assiste): abre no roteador as portas UDP que a
/// oferta dele vai usar e acrescenta o IP público como candidato. Assim, quem transmite consegue
/// chegar até aqui mesmo se a rede DELE for restritiva. Sem UPnP, devolve a oferta como veio.
pub async fn expose_offer(sdp: &str) -> String {
    probe().await;
    let (gw, local, external) = {
        let st = STATE.lock().await;
        match (st.gw.clone(), st.local, st.external) {
            (Some(g), Some(l), Some(e)) if !is_cgnat(e) => (g, l, e),
            _ => return sdp.to_string(),
        }
    };
    // Candidatos locais UDP desta máquina (o WebView2 pode esconder o IP como "xxxx.local").
    let mut ports = vec![];
    for line in sdp.lines() {
        let Some(rest) = line.trim_end().strip_prefix("a=candidate:") else { continue };
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() >= 8 && f[1] == "1" && f[2].eq_ignore_ascii_case("udp") && f[7] == "host" && (f[4] == local.to_string() || f[4].ends_with(".local")) {
            if let Ok(p) = f[5].parse::<u16>() {
                if !ports.contains(&p) && ports.len() < 4 {
                    ports.push(p);
                }
            }
        }
    }
    let mut opened = vec![];
    for p in ports {
        let addr = SocketAddr::new(IpAddr::V4(local), p);
        if tokio::time::timeout(Duration::from_secs(3), gw.add_port(PortMappingProtocol::UDP, p, addr, 3600, "Lume"))
            .await
            .is_ok_and(|r| r.is_ok())
        {
            opened.push(p);
        }
    }
    let mut out = String::with_capacity(sdp.len() + 200);
    for line in sdp.split_inclusive('\n') {
        out.push_str(line);
        let Some(rest) = line.trim_end().strip_prefix("a=candidate:") else { continue };
        let f: Vec<&str> = rest.split_whitespace().collect();
        if f.len() >= 8 && f[1] == "1" && f[7] == "host" && f[5].parse::<u16>().is_ok_and(|p| opened.contains(&p)) && (f[4] == local.to_string() || f[4].ends_with(".local")) {
            let prio = (f[3].parse::<u64>().unwrap_or(0) / 2).max(1);
            out.push_str(&format!("a=candidate:{}9 1 udp {} {} {} typ srflx raddr 0.0.0.0 rport 0\r\n", f[0], prio, external, f[5]));
        }
    }
    out
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
            let opts = SearchOptions { timeout: Some(Duration::from_secs(6)), ..Default::default() };
            match search_gateway(opts).await {
                Ok(gw) => println!("qualquer placa: {} ip externo: {:?}", gw.addr, gw.get_external_ip().await),
                Err(e) => println!("qualquer placa: {e}"),
            }
            println!("ip local: {:?} (rede de casa: {:?})", local_ip_address::local_ip(), home_ip());
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
