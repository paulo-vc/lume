//! Abrir porta no roteador sem UPnP: PCP (RFC 6887) e o antecessor dele, NAT-PMP (RFC 6886).
//! Muitos roteadores (e alguns de operadora) não têm UPnP, ou vêm com ele desligado, mas falam um
//! destes. O PCP também abre a "janela" do firewall de IPv6: com IPv6 o endereço já é público,
//! mas o roteador costuma barrar quem chega de fora; com a janela aberta, a conexão direta passa
//! mesmo quando o IPv4 está atrás de CGNAT.

use rand::RngCore;
use std::net::{IpAddr, Ipv4Addr, Ipv6Addr, SocketAddr, SocketAddrV6};
use std::time::Duration;
use tokio::net::UdpSocket;

const PORT: u16 = 5351;
const UDP: u8 = 17;

/// A rede de casa, vista pela placa que sai para a internet.
#[derive(Clone, Debug, Default)]
pub struct Lan {
    pub gw4: Option<Ipv4Addr>,
    pub gw6: Option<(Ipv6Addr, u32)>, // endereço do roteador e índice da placa (link-local)
    pub ip6: Option<Ipv6Addr>,        // IPv6 público (global) deste PC
}

pub fn lan() -> Lan {
    let Ok(i) = netdev::get_default_interface() else { return Lan::default() };
    let global = |a: &Ipv6Addr| {
        let s = a.segments()[0];
        (0x2000..=0x3fff).contains(&s) && s != 0x2001 // 2001:0::/32 é Teredo, não conta
            || (s == 0x2001 && a.segments()[1] != 0)
    };
    let ip6 = i.ipv6.iter().map(|n| n.addr()).find(global);
    let gw = i.gateway.as_ref();
    Lan {
        gw4: gw.and_then(|g| g.ipv4.first().copied()),
        gw6: gw.and_then(|g| g.ipv6.first().copied()).map(|a| (a, i.index)),
        ip6,
    }
}

fn mapped(ip: IpAddr) -> [u8; 16] {
    match ip {
        IpAddr::V4(v) => v.to_ipv6_mapped().octets(),
        IpAddr::V6(v) => v.octets(),
    }
}

async fn ask(to: SocketAddr, from: IpAddr, req: &[u8]) -> Option<Vec<u8>> {
    let sock = UdpSocket::bind(SocketAddr::new(from, 0)).await.ok()?;
    let mut buf = [0u8; 1100];
    // Reenvio com espera crescente (o protocolo manda começar com 250 ms).
    for wait in [250u64, 500, 1000] {
        sock.send_to(req, to).await.ok()?;
        if let Ok(Ok((n, _))) = tokio::time::timeout(Duration::from_millis(wait), sock.recv_from(&mut buf)).await {
            return Some(buf[..n].to_vec());
        }
    }
    None
}

/// Pedido MAP do PCP. Devolve (IP externo, porta externa). `lifetime` 0 apaga a regra.
async fn pcp_map(gw: SocketAddr, client: IpAddr, port: u16, lifetime: u32) -> Option<(IpAddr, u16)> {
    let mut req = vec![0u8; 60];
    req[0] = 2; // versão
    req[1] = 1; // MAP (pedido)
    req[4..8].copy_from_slice(&lifetime.to_be_bytes());
    req[8..24].copy_from_slice(&mapped(client));
    let mut nonce = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut nonce);
    req[24..36].copy_from_slice(&nonce);
    req[36] = UDP;
    req[40..42].copy_from_slice(&port.to_be_bytes());
    req[42..44].copy_from_slice(&port.to_be_bytes()); // pede a mesma porta lá fora
    let any = if client.is_ipv4() { Ipv4Addr::UNSPECIFIED.to_ipv6_mapped().octets() } else { [0u8; 16] };
    req[44..60].copy_from_slice(&any);
    let r = ask(gw, client, &req).await?;
    // resposta: versão 2, opcode 0x81, resultado 0 (sucesso), mesmo nonce
    if r.len() < 60 || r[0] != 2 || r[1] != 0x81 || r[3] != 0 || r[24..36] != nonce {
        return None;
    }
    let ext_port = u16::from_be_bytes([r[42], r[43]]);
    let ip = Ipv6Addr::from(<[u8; 16]>::try_from(&r[44..60]).ok()?);
    let ip = ip.to_ipv4_mapped().map(IpAddr::V4).unwrap_or(IpAddr::V6(ip));
    Some((ip, ext_port))
}

/// NAT-PMP (só IPv4): mapeamento UDP e o IP externo.
async fn natpmp_map(gw: Ipv4Addr, local: Ipv4Addr, port: u16, lifetime: u32) -> Option<(IpAddr, u16)> {
    let to = SocketAddr::new(IpAddr::V4(gw), PORT);
    let mut req = vec![0u8, 1, 0, 0];
    req.extend_from_slice(&port.to_be_bytes());
    req.extend_from_slice(&port.to_be_bytes());
    req.extend_from_slice(&lifetime.to_be_bytes());
    let r = ask(to, IpAddr::V4(local), &req).await?;
    if r.len() < 16 || r[0] != 0 || r[1] != 129 || r[2..4] != [0, 0] {
        return None;
    }
    let ext_port = u16::from_be_bytes([r[10], r[11]]);
    let r = ask(to, IpAddr::V4(local), &[0, 0]).await?;
    if r.len() < 12 || r[1] != 128 || r[2..4] != [0, 0] {
        return None;
    }
    Some((IpAddr::V4(Ipv4Addr::new(r[8], r[9], r[10], r[11])), ext_port))
}

/// Abre `port` (UDP) no IPv4 pelo PCP ou, se o roteador não falar PCP, pelo NAT-PMP.
pub async fn map_v4(lan: &Lan, local: Ipv4Addr, port: u16, lifetime: u32) -> Option<(Ipv4Addr, u16)> {
    let gw = lan.gw4?;
    let r = match pcp_map(SocketAddr::new(IpAddr::V4(gw), PORT), IpAddr::V4(local), port, lifetime).await {
        Some(r) => Some(r),
        None => natpmp_map(gw, local, port, lifetime).await,
    };
    match r {
        Some((IpAddr::V4(ip), p)) => Some((ip, p)),
        _ => None,
    }
}

/// Abre a janela do firewall de IPv6 do roteador para `port` (UDP) deste PC.
pub async fn pinhole_v6(lan: &Lan, port: u16, lifetime: u32) -> bool {
    let (Some((gw, scope)), Some(ip6)) = (lan.gw6, lan.ip6) else { return false };
    let scope = if gw.segments()[0] & 0xffc0 == 0xfe80 { scope } else { 0 };
    let to = SocketAddr::V6(SocketAddrV6::new(gw, PORT, 0, scope));
    pcp_map(to, IpAddr::V6(ip6), port, lifetime).await.is_some()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Um roteador PCP de mentira, na própria máquina, responde como a RFC 6887 manda.
    #[test]
    fn pcp_map_roundtrip() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let srv = UdpSocket::bind("127.0.0.1:5351").await.unwrap();
            tokio::spawn(async move {
                let mut b = [0u8; 1100];
                let (n, from) = srv.recv_from(&mut b).await.unwrap();
                let q = &b[..n];
                assert_eq!((q.len(), q[0], q[1], q[36]), (60, 2, 1, 17));
                assert_eq!(&q[8..24], &Ipv4Addr::LOCALHOST.to_ipv6_mapped().octets());
                let mut r = q.to_vec();
                r[1] = 0x81;
                r[3] = 0; // sucesso
                r[42..44].copy_from_slice(&40123u16.to_be_bytes());
                r[44..60].copy_from_slice(&Ipv4Addr::new(203, 0, 113, 5).to_ipv6_mapped().octets());
                srv.send_to(&r, from).await.unwrap();
            });
            let got = pcp_map(SocketAddr::new(IpAddr::V4(Ipv4Addr::LOCALHOST), PORT), IpAddr::V4(Ipv4Addr::LOCALHOST), 40000, 60).await;
            assert_eq!(got, Some((IpAddr::V4(Ipv4Addr::new(203, 0, 113, 5)), 40123)));
        });
    }

    /// O que o roteador daqui responde. `cargo test pcp_here -- --ignored --nocapture`
    #[test]
    #[ignore]
    fn pcp_here() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let l = lan();
            println!("rede: {l:?}");
            let local = match netdev::get_default_interface().ok().and_then(|i| i.ipv4.first().map(|n| n.addr())) {
                Some(a) => a,
                None => return println!("sem IPv4"),
            };
            let port = 45000 + rand::random::<u16>() % 1000;
            match map_v4(&l, local, port, 60).await {
                Some((ip, p)) => {
                    println!("PCP/NAT-PMP IPv4: {ip}:{p} -> {local}:{port}");
                    let _ = map_v4(&l, local, port, 0).await;
                }
                None => println!("PCP/NAT-PMP IPv4: o roteador não respondeu"),
            }
            println!("janela IPv6: {}", pinhole_v6(&l, port, 60).await);
        });
    }
}
