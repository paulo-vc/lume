//! Salas privadas por convite, sem servidor.
//!
//! Convite:  `sl://join/<room_id>#<segredo base64>` (segredo de 256 bits, expira).
//! Fluxo de entrada (troca manual ou por relay):
//!   1. Convidado gera um PEDIDO = {nome, nonce, sdp_offer, prova = HMAC(segredo, "join"|room|nonce|sdp)}
//!   2. Anfitrião valida a prova, rejeita nonce repetido, aprova e gera RESPOSTA =
//!      {sdp_answer, prova = HMAC(segredo, "reply"|room|nonce|sdp)}
//!   3. Convidado valida a resposta (prova que fala com o anfitrião verdadeiro).
//! Quem não tem o convite não consegue gerar pedido válido; um SDP adulterado invalida a prova.

use base64::{engine::general_purpose::URL_SAFE_NO_PAD as B64, Engine};
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use hmac::{Hmac, Mac};
use rand::RngCore;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::Sha256;
use std::collections::HashSet;
use std::io::{Read, Write};
use std::time::{SystemTime, UNIX_EPOCH};

type HmacSha256 = Hmac<Sha256>;

pub const MAX_MEMBERS: usize = 8;

fn now() -> u64 {
    SystemTime::now().duration_since(UNIX_EPOCH).unwrap().as_secs()
}

fn rand_bytes<const N: usize>() -> [u8; N] {
    let mut b = [0u8; N];
    rand::thread_rng().fill_bytes(&mut b);
    b
}

fn mac(secret: &[u8], parts: &[&[u8]]) -> HmacSha256 {
    let mut m = HmacSha256::new_from_slice(secret).expect("hmac key");
    for p in parts {
        m.update(&(p.len() as u32).to_le_bytes());
        m.update(p);
    }
    m
}

pub fn pack<T: Serialize>(prefix: &str, v: &T) -> Result<String, String> {
    let json = serde_json::to_vec(v).map_err(|e| e.to_string())?;
    let mut enc = DeflateEncoder::new(Vec::new(), Compression::best());
    enc.write_all(&json).map_err(|e| e.to_string())?;
    Ok(format!("{prefix}{}", B64.encode(enc.finish().map_err(|e| e.to_string())?)))
}

pub fn unpack<T: DeserializeOwned>(prefix: &str, code: &str) -> Result<T, String> {
    let body = code
        .trim()
        .strip_prefix(prefix)
        .ok_or_else(|| "Código de tipo errado".to_string())?;
    let raw = B64.decode(body).map_err(|_| "Código corrompido".to_string())?;
    let mut s = Vec::new();
    DeflateDecoder::new(&raw[..])
        .take(256 * 1024)
        .read_to_end(&mut s)
        .map_err(|_| "Código corrompido".to_string())?;
    serde_json::from_slice(&s).map_err(|_| "Código inválido".to_string())
}

#[derive(Clone, Debug)]
pub struct Invite {
    pub room_id: String,
    pub secret: [u8; 32],
    pub expires_at: u64,
}

impl Invite {
    pub fn to_link(&self) -> String {
        format!("sl://join/{}.{}#{}", self.room_id, self.expires_at, B64.encode(self.secret))
    }

    pub fn parse(link: &str) -> Result<Self, String> {
        let rest = link
            .trim()
            .strip_prefix("sl://join/")
            .ok_or("Convite inválido")?;
        let (head, key) = rest.split_once('#').ok_or("Convite sem chave")?;
        let (room_id, exp) = head.split_once('.').ok_or("Convite inválido")?;
        let expires_at: u64 = exp.parse().map_err(|_| "Convite inválido")?;
        let k = B64.decode(key).map_err(|_| "Chave inválida")?;
        let secret: [u8; 32] = k.try_into().map_err(|_| "Chave inválida")?;
        if now() > expires_at {
            return Err("Convite expirado".into());
        }
        Ok(Self { room_id: room_id.into(), secret, expires_at })
    }
}

#[derive(Serialize, Deserialize)]
pub struct JoinRequest {
    pub room: String,
    pub name: String,
    pub nonce: String,
    pub sdp: String,
    pub proof: String,
}

#[derive(Serialize, Deserialize)]
pub struct JoinReply {
    pub room: String,
    pub nonce: String,
    pub sdp: String,
    pub proof: String,
}

pub struct HostRoom {
    pub name: String,
    pub invite: Invite,
    pub used_nonces: HashSet<String>,
    pub members: Vec<String>,
}

impl HostRoom {
    pub fn new(name: String, ttl_min: u64) -> Self {
        let id: [u8; 6] = rand_bytes();
        Self {
            name,
            invite: Invite {
                room_id: B64.encode(id),
                secret: rand_bytes(),
                expires_at: now() + ttl_min * 60,
            },
            used_nonces: HashSet::new(),
            members: Vec::new(),
        }
    }

    /// Valida um pedido de entrada. Não aceita o membro ainda: o anfitrião aprova depois.
    pub fn check_request(&self, code: &str) -> Result<JoinRequest, String> {
        let req: JoinRequest = unpack("SLQ-", code)?;
        if req.room != self.invite.room_id {
            return Err("Pedido é de outra sala".into());
        }
        if now() > self.invite.expires_at {
            return Err("O convite desta sala expirou; gere um novo".into());
        }
        if self.used_nonces.contains(&req.nonce) {
            return Err("Pedido já usado".into());
        }
        if self.members.len() >= MAX_MEMBERS {
            return Err("Sala cheia".into());
        }
        let proof = B64.decode(&req.proof).map_err(|_| "Prova inválida")?;
        mac(&self.invite.secret, &[b"join", req.room.as_bytes(), req.nonce.as_bytes(), req.sdp.as_bytes()])
            .verify_slice(&proof)
            .map_err(|_| "Prova inválida: o convite não confere".to_string())?;
        Ok(req)
    }

    pub fn approve(&mut self, req_code: &str, answer_sdp: String) -> Result<String, String> {
        let req = self.check_request(req_code)?;
        self.used_nonces.insert(req.nonce.clone());
        self.members.push(req.name.clone());
        let proof = mac(
            &self.invite.secret,
            &[b"reply", req.room.as_bytes(), req.nonce.as_bytes(), answer_sdp.as_bytes()],
        )
        .finalize()
        .into_bytes();
        pack(
            "SLR-",
            &JoinReply { room: req.room, nonce: req.nonce, sdp: answer_sdp, proof: B64.encode(proof) },
        )
    }

    pub fn rotate_invite(&mut self, ttl_min: u64) {
        self.invite.secret = rand_bytes();
        self.invite.expires_at = now() + ttl_min * 60;
    }
}

pub struct GuestCtx {
    pub invite: Invite,
    pub nonce: String,
}

impl GuestCtx {
    pub fn make_request(invite: Invite, name: &str, offer_sdp: &str) -> Result<(Self, String), String> {
        let nonce = B64.encode(rand_bytes::<16>());
        let proof = mac(
            &invite.secret,
            &[b"join", invite.room_id.as_bytes(), nonce.as_bytes(), offer_sdp.as_bytes()],
        )
        .finalize()
        .into_bytes();
        let code = pack(
            "SLQ-",
            &JoinRequest {
                room: invite.room_id.clone(),
                name: name.chars().take(32).collect(),
                nonce: nonce.clone(),
                sdp: offer_sdp.into(),
                proof: B64.encode(proof),
            },
        )?;
        Ok((Self { invite, nonce }, code))
    }

    pub fn open_reply(&self, code: &str) -> Result<String, String> {
        let r: JoinReply = unpack("SLR-", code)?;
        if r.room != self.invite.room_id || r.nonce != self.nonce {
            return Err("Resposta não corresponde ao seu pedido".into());
        }
        let proof = B64.decode(&r.proof).map_err(|_| "Prova inválida")?;
        mac(&self.invite.secret, &[b"reply", r.room.as_bytes(), r.nonce.as_bytes(), r.sdp.as_bytes()])
            .verify_slice(&proof)
            .map_err(|_| "Resposta não veio do anfitrião verdadeiro".to_string())?;
        Ok(r.sdp)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn full_handshake() {
        let mut host = HostRoom::new("Teste".into(), 15);
        let inv = Invite::parse(&host.invite.to_link()).unwrap();
        let (guest, req) = GuestCtx::make_request(inv, "Ana", "v=0 offer").unwrap();
        assert_eq!(host.check_request(&req).unwrap().name, "Ana");
        let reply = host.approve(&req, "v=0 answer".into()).unwrap();
        assert_eq!(guest.open_reply(&reply).unwrap(), "v=0 answer");
        assert!(host.approve(&req, "x".into()).is_err(), "replay deve falhar");
    }

    #[test]
    fn wrong_secret_rejected() {
        let host = HostRoom::new("A".into(), 15);
        let mut fake = host.invite.clone();
        fake.secret = [7; 32];
        let (_, req) = GuestCtx::make_request(fake, "Intruso", "sdp").unwrap();
        assert!(host.check_request(&req).is_err());
    }
}
