//! Deixar o PC alcançável sem servidor externo: pede ao roteador para abrir uma porta UDP para
//! cada conexão e anuncia o endereço público junto dos outros candidatos. Com a porta aberta, a
//! outra pessoa conecta mesmo estando atrás de NAT restritivo.
//!
//! Três jeitos de pedir, do mais comum ao menos: UPnP, PCP/NAT-PMP (`pcp.rs`) e, no IPv6, a
//! janela do firewall pelo PCP. Com IPv6 o endereço já é público; abrir a janela resolve até quando
//! o IPv4 está atrás de CGNAT (o endereço "público" do roteador é do provedor).

use crate::pcp::{self, Lan};
use igd_next::aio::tokio::{search_gateway, Tokio};
use igd_next::aio::Gateway;
use igd_next::{PortMappingProtocol, SearchOptions};
use serde::Serialize;
use std::collections::HashMap;
use std::net::{IpAddr, Ipv4Addr, SocketAddr};
use std::time::Duration;
use tokio::sync::Mutex;

#[derive(Clone, Serialize, Default)]
pub struct Status {
    /// ainda procurando o roteador
    pub checking: bool,
    /// o roteador abre portas no IPv4 (por UPnP ou PCP/NAT-PMP) e o IP dele é público
    pub upnp: bool,
    /// por qual caminho: "upnp", "pcp" ou ""
    pub method: String,
    /// a internet usa CGNAT (o roteador não tem IP público)
    pub cgnat: bool,
    /// este PC tem IPv6 público
    pub ipv6: bool,
    /// o roteador abriu a janela do firewall de IPv6
    pub ipv6_open: bool,
}

#[derive(Clone, Copy, PartialEq)]
enum Via {
    Upnp,
    Pcp,
    Nothing,
}

struct State {
    done: bool,
    gw: Option<Gateway<Tokio>>,
    local: Option<Ipv4Addr>,
    external: Option<Ipv4Addr>,
    via: Via,
    lan: Option<Lan>,
    v6_open: bool,
    /// portas abertas agora: porta -> (como foi aberta no IPv4, se abriu janela no IPv6)
    open: Vec<(u16, Via, bool)>,
}

static STATE: Mutex<State> =
    Mutex::const_new(State { done: false, gw: None, local: None, external: None, via: Via::Nothing, lan: None, v6_open: false, open: Vec::new() });

fn is_cgnat(ip: Ipv4Addr) -> bool {
    let o = ip.octets();
    ip.is_private() || ip.is_loopback() || ip.is_unspecified() || (o[0] == 100 && (o[1] & 0xc0) == 64)
}

/// IP deste PC na rede de casa (a que tem o roteador). VPNs como Radmin e Hamachi usam faixas
/// que não são de rede local (26.x, 25.x), então ficam de fora.
fn home_ip() -> Option<Ipv4Addr> {
    let lan = |ip: Ipv4Addr| ip.is_private() && !ip.is_link_local();
    // O IP que o sistema usaria para sair para a internet.
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

/// Uma porta livre aqui no IPv4 e no IPv6 (a conexão vai usar a mesma nos dois).
fn free_port() -> Option<u16> {
    for _ in 0..8 {
        let port = 40000 + rand::random::<u16>() % 20000;
        if std::net::UdpSocket::bind(("0.0.0.0", port)).is_ok() && std::net::UdpSocket::bind(("::", port)).is_ok() {
            return Some(port);
        }
    }
    None
}

/// Procura o roteador e vê o que ele aceita (uma vez; chamado na abertura do app).
pub async fn probe() {
    let mut st = STATE.lock().await;
    if st.done {
        return;
    }
    st.local = home_ip();
    let lan = pcp::lan();
    if let Some(local) = st.local {
        // UPnP pela placa que sai para a internet (não por VPNs como Radmin/Hamachi, que também
        // respondem e não têm nada a ver com o roteador de casa).
        let opts = SearchOptions { bind_addr: SocketAddr::new(IpAddr::V4(local), 0), timeout: Some(Duration::from_secs(4)), ..Default::default() };
        if let Ok(gw) = search_gateway(opts).await {
            let same_net = matches!(gw.addr.ip(), IpAddr::V4(g) if g.octets()[..3] == local.octets()[..3]);
            if same_net {
                if let Ok(IpAddr::V4(ext)) = gw.get_external_ip().await {
                    st.external = Some(ext);
                }
                st.gw = Some(gw);
                st.via = Via::Upnp;
            }
        }
        // Sem UPnP: tenta PCP/NAT-PMP com uma porta de teste de vida curta.
        if st.via == Via::Nothing {
            if let Some(p) = free_port() {
                if let Some((ext, _)) = pcp::map_v4(&lan, local, p, 30).await {
                    st.external = Some(ext);
                    st.via = Via::Pcp;
                    let _ = pcp::map_v4(&lan, local, p, 0).await;
                }
            }
        }
    }
    // IPv6 público: o roteador abre a janela do firewall?
    if lan.ip6.is_some() {
        if let Some(p) = free_port() {
            st.v6_open = pcp::pinhole_v6(&lan, p, 30).await;
            if st.v6_open {
                let _ = pcp::pinhole_v6(&lan, p, 0).await;
            }
        }
    }
    st.lan = Some(lan);
    st.done = true;
}

pub async fn status() -> Status {
    let st = STATE.lock().await;
    Status {
        checking: !st.done,
        upnp: st.via != Via::Nothing && st.external.is_some_and(|e| !is_cgnat(e)),
        method: match st.via {
            Via::Upnp => "upnp",
            Via::Pcp => "pcp",
            Via::Nothing => "",
        }
        .into(),
        cgnat: st.external.is_some_and(is_cgnat),
        ipv6: st.lan.as_ref().is_some_and(|l| l.ip6.is_some()),
        ipv6_open: st.v6_open,
    }
}

/// Portas abertas no roteador para uma conexão.
#[derive(Clone, Copy)]
pub struct Mapping {
    pub port: u16,
    /// IP público e porta lá fora, quando o IPv4 foi aberto
    pub external: Option<(Ipv4Addr, u16)>,
    /// a janela do IPv6 foi aberta para esta porta
    #[allow(dead_code)]
    pub v6: bool,
}

/// Endereços para a conexão escutar: a porta aberta no IPv4 e no IPv6, ou qualquer uma.
pub fn udp_addrs(m: Option<Mapping>) -> Vec<String> {
    match m {
        Some(m) => vec![format!("0.0.0.0:{}", m.port), format!("[::]:{}", m.port)],
        None => vec!["0.0.0.0:0".into(), "[::]:0".into()],
    }
}

async fn open_v4(st: &State, port: u16, lifetime: u32) -> Option<(Ipv4Addr, u16)> {
    let (local, external) = (st.local?, st.external?);
    if is_cgnat(external) {
        return None; // abrir porta não adianta: o endereço público é do provedor
    }
    match st.via {
        Via::Upnp => {
            let gw = st.gw.as_ref()?;
            let addr = SocketAddr::new(IpAddr::V4(local), port);
            let ok = tokio::time::timeout(Duration::from_secs(3), gw.add_port(PortMappingProtocol::UDP, port, addr, lifetime, "Lume")).await.is_ok_and(|r| r.is_ok());
            ok.then_some((external, port))
        }
        Via::Pcp => pcp::map_v4(st.lan.as_ref()?, local, port, lifetime).await,
        Via::Nothing => None,
    }
}

/// Abre uma porta UDP para uma conexão (IPv4 e/ou IPv6). None se o roteador não abrir nada.
pub async fn open_port() -> Option<Mapping> {
    probe().await;
    let port = free_port()?;
    let mut st = STATE.lock().await;
    let external = open_v4(&st, port, 7200).await;
    let v6 = st.v6_open && pcp::pinhole_v6(st.lan.as_ref()?, port, 7200).await;
    if external.is_none() && !v6 {
        return None;
    }
    let via = if external.is_some() { st.via } else { Via::Nothing };
    st.open.push((port, via, v6));
    Some(Mapping { port, external, v6 })
}

pub async fn close_port(port: u16) {
    let mut st = STATE.lock().await;
    let Some(i) = st.open.iter().position(|o| o.0 == port) else { return };
    let (_, via, v6) = st.open.remove(i);
    match via {
        Via::Upnp => {
            if let Some(gw) = &st.gw {
                let _ = tokio::time::timeout(Duration::from_secs(3), gw.remove_port(PortMappingProtocol::UDP, port)).await;
            }
        }
        Via::Pcp => {
            if let (Some(lan), Some(local)) = (&st.lan, st.local) {
                let _ = pcp::map_v4(lan, local, port, 0).await;
            }
        }
        Via::Nothing => {}
    }
    if v6 {
        if let Some(lan) = &st.lan {
            let _ = pcp::pinhole_v6(lan, port, 0).await;
        }
    }
}

/// Para as conexões do navegador interno (quem assiste): abre no roteador as portas UDP que a
/// oferta dele vai usar e acrescenta o IP público (IPv4) e o IPv6 como candidatos. Assim, quem
/// transmite consegue chegar até aqui mesmo se a rede DELE for restritiva.
pub async fn expose_offer(sdp: &str) -> String {
    probe().await;
    let mut st = STATE.lock().await;
    let Some(local) = st.local else { return sdp.to_string() };
    let ip6 = st.lan.as_ref().and_then(|l| l.ip6).filter(|_| st.v6_open);
    // Candidatos locais UDP desta máquina (o WebView2 esconde o IP como "xxxx.local").
    let ours = |f: &[&str]| f.len() >= 8 && f[1] == "1" && f[2].eq_ignore_ascii_case("udp") && f[7] == "host" && (f[4] == local.to_string() || f[4].ends_with(".local"));
    let mut ports = vec![];
    for line in sdp.lines() {
        let Some(rest) = line.trim_end().strip_prefix("a=candidate:") else { continue };
        let f: Vec<&str> = rest.split_whitespace().collect();
        if ours(&f) {
            if let Ok(p) = f[5].parse::<u16>() {
                if !ports.contains(&p) && ports.len() < 4 {
                    ports.push(p);
                }
            }
        }
    }
    let mut opened: HashMap<u16, (Option<(Ipv4Addr, u16)>, bool)> = HashMap::new();
    for p in ports {
        let v4 = open_v4(&st, p, 3600).await;
        let v6 = ip6.is_some() && pcp::pinhole_v6(st.lan.as_ref().unwrap(), p, 3600).await;
        if v4.is_some() || v6 {
            opened.insert(p, (v4, v6));
            let via = if v4.is_some() { st.via } else { Via::Nothing };
            st.open.push((p, via, v6));
        }
    }
    let mut out = String::with_capacity(sdp.len() + 300);
    for line in sdp.split_inclusive('\n') {
        out.push_str(line);
        let Some(rest) = line.trim_end().strip_prefix("a=candidate:") else { continue };
        let f: Vec<&str> = rest.split_whitespace().collect();
        if !ours(&f) {
            continue;
        }
        let Some((v4, v6)) = f[5].parse::<u16>().ok().and_then(|p| opened.get(&p)) else { continue };
        let prio = (f[3].parse::<u64>().unwrap_or(0) / 2).max(1);
        if let Some((ext, eport)) = v4 {
            out.push_str(&format!("a=candidate:{}9 1 udp {} {} {} typ srflx raddr 0.0.0.0 rport 0\r\n", f[0], prio, ext, eport));
        }
        if let (true, Some(ip6)) = (*v6, ip6) {
            // O IPv6 já é público: vai como candidato local, com a janela aberta no roteador.
            out.push_str(&format!("a=candidate:{}8 1 udp {} {} {} typ host\r\n", f[0], prio, ip6, f[5]));
        }
    }
    out
}

/// Acrescenta o endereço público como candidato (srflx) ao lado do candidato local dessa porta.
pub fn add_public_candidate(sdp: &str, m: Mapping) -> String {
    let Some((ext, eport)) = m.external else { return sdp.to_string() };
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
            out.push_str(&format!("a=candidate:{}9 {} udp {} {} {} typ srflx raddr {} rport {}\r\n", f[0], f[1], prio, ext, eport, f[4], m.port));
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
        let m = Mapping { port: 45000, external: Some(("177.10.20.30".parse().unwrap(), 45000)), v6: false };
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
            println!("rede de casa: {:?}", home_ip());
            probe().await;
            let s = status().await;
            println!("status: abre porta={} ({}) cgnat={} ipv6={} janela ipv6={}", s.upnp, s.method, s.cgnat, s.ipv6, s.ipv6_open);
            if let Some(m) = open_port().await {
                println!("porta {} aberta: ipv4={:?} ipv6={}", m.port, m.external, m.v6);
                close_port(m.port).await;
            } else {
                println!("não foi possível abrir porta");
            }
        });
    }
}
