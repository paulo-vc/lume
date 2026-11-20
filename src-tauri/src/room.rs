//! Salas privadas por código curto.
//!
//! O código da sala (12 caracteres, ~60 bits) é o segredo: dele saem
//!   - o "endereço" do encontro (tópico) nos relays públicos, e
//!   - a chave que cifra e autentica tudo que passa por lá (ChaCha20-Poly1305).
//! Quem não tem o código não acha a sala nem consegue ler ou forjar mensagens, e o
//! relay só enxerga bytes cifrados. Mesmo com o código, o anfitrião ainda aprova cada entrada.

use chacha20poly1305::aead::{Aead, KeyInit};
use chacha20poly1305::{ChaCha20Poly1305, Nonce};
use flate2::{read::DeflateDecoder, write::DeflateEncoder, Compression};
use rand::RngCore;
use serde::{de::DeserializeOwned, Deserialize, Serialize};
use sha2::Sha256;
use std::io::{Read, Write};

pub const MAX_MEMBERS: usize = 8;
/// Base32 de Crockford: sem I, L, O, U (não confunde ao ditar ou digitar).
const ALPHABET: &[u8; 32] = b"0123456789ABCDEFGHJKMNPQRSTVWXYZ";

pub fn new_code() -> String {
    let mut b = [0u8; 12];
    rand::thread_rng().fill_bytes(&mut b);
    let s: Vec<u8> = b.iter().map(|x| ALPHABET[(x & 31) as usize]).collect();
    format!("{}-{}-{}", String::from_utf8_lossy(&s[..4]), String::from_utf8_lossy(&s[4..8]), String::from_utf8_lossy(&s[8..]))
}

/// Aceita o código digitado de qualquer jeito (minúsculas, espaços, sem traços, O no lugar de 0...).
pub fn normalize(code: &str) -> Result<String, String> {
    let s: String = code
        .chars()
        .filter(|c| c.is_ascii_alphanumeric())
        .map(|c| match c.to_ascii_uppercase() {
            'O' => '0',
            'I' | 'L' => '1',
            c => c,
        })
        .collect();
    if s.len() != 12 || !s.bytes().all(|b| ALPHABET.contains(&b)) {
        return Err("Código inválido: são 12 letras e números, como K7QM-2XPD-9RTA".into());
    }
    Ok(format!("{}-{}-{}", &s[..4], &s[4..8], &s[8..]))
}

#[derive(Clone)]
pub struct Keys {
    pub topic: String,
    key: [u8; 32],
}

/// Parâmetros do scrypt (iguais na versão web, `web/lume-web.js`).
/// Cada tentativa de adivinhar um código custa ~0,1 s de CPU e 32 MB de memória: testar os
/// 2^60 códigos possíveis fica fora de alcance, mesmo vendo o endereço no relay público.
const KDF_LOG_N: u8 = 15;
const KDF_R: u32 = 8;
const KDF_SALT: &[u8] = b"lume-v2-room";

impl Keys {
    pub fn from_code(code: &str) -> Result<Self, String> {
        let code = normalize(code)?;
        let params = scrypt::Params::new(KDF_LOG_N, KDF_R, 1, 44).map_err(|e| e.to_string())?;
        let mut out = [0u8; 44];
        scrypt::scrypt(code.as_bytes(), KDF_SALT, &params, &mut out).map_err(|e| e.to_string())?;
        let mut key = [0u8; 32];
        key.copy_from_slice(&out[..32]);
        let topic = format!("lume2/{}", hex::encode(&out[32..]));
        Ok(Self { topic, key })
    }

    pub fn requests(&self) -> String {
        format!("{}/q", self.topic)
    }
    pub fn answer(&self, id: &str) -> String {
        format!("{}/a/{id}", self.topic)
    }

    pub fn seal<T: Serialize>(&self, v: &T) -> Vec<u8> {
        let json = serde_json::to_vec(v).expect("json");
        let mut z = DeflateEncoder::new(Vec::new(), Compression::best());
        z.write_all(&json).expect("deflate");
        let plain = z.finish().expect("deflate");
        let mut nonce = [0u8; 12];
        rand::thread_rng().fill_bytes(&mut nonce);
        let ct = ChaCha20Poly1305::new((&self.key).into()).encrypt(&Nonce::from(nonce), plain.as_ref()).expect("cifra");
        [nonce.to_vec(), ct].concat()
    }

    pub fn open<T: DeserializeOwned>(&self, data: &[u8]) -> Option<T> {
        if data.len() < 12 + 16 {
            return None;
        }
        let nonce: [u8; 12] = data[..12].try_into().ok()?;
        let plain = ChaCha20Poly1305::new((&self.key).into()).decrypt(&Nonce::from(nonce), &data[12..]).ok()?;
        let mut s = Vec::new();
        DeflateDecoder::new(&plain[..]).take(256 * 1024).read_to_end(&mut s).ok()?;
        serde_json::from_slice(&s).ok()
    }
}

/// Pedido de entrada (convidado -> anfitrião).
#[derive(Clone, Serialize, Deserialize)]
pub struct JoinReq {
    pub id: String,
    pub name: String,
    pub sdp: String,
    /// foto de perfil (data URL pequena) ou vazio
    #[serde(default)]
    pub av: String,
    /// Prova de que este PC já foi aceito antes: HMAC-SHA256(chave = identificador do PC,
    /// mensagem = id + "\n" + sdp). O identificador em si nunca passa pelo relay (vai só pelo
    /// canal cifrado de ponta a ponta, depois de conectar), e a prova não serve para outro pedido
    /// porque está presa à oferta (impressão digital DTLS) deste pedido.
    #[serde(default)]
    pub proof: String,
}

pub fn proof(uid: &str, id: &str, sdp: &str) -> String {
    use hmac::{Hmac, Mac};
    let mut m = <Hmac<Sha256> as Mac>::new_from_slice(uid.as_bytes()).expect("hmac");
    m.update(id.as_bytes());
    m.update(b"\n");
    m.update(sdp.as_bytes());
    hex::encode(m.finalize().into_bytes())
}

/// Resposta (anfitrião -> convidado).
#[derive(Clone, Serialize, Deserialize)]
pub struct JoinRes {
    pub ok: bool,
    #[serde(default)]
    pub sdp: String,
    #[serde(default)]
    pub why: String,
}

pub fn new_id() -> String {
    let mut b = [0u8; 8];
    rand::thread_rng().fill_bytes(&mut b);
    b.iter().map(|x| format!("{x:02x}")).collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn code_roundtrip_and_typing_mistakes() {
        let code = new_code();
        assert_eq!(normalize(&code).unwrap(), code);
        assert_eq!(normalize(&code.to_lowercase().replace('-', " ")).unwrap(), code);
        assert_eq!(normalize("k7qm 2xpd 9rta").unwrap(), "K7QM-2XPD-9RTA");
        assert_eq!(normalize("OIL0-0000-0000").unwrap(), "0110-0000-0000");
        assert!(normalize("curto").is_err());
    }

    /// Valores de referência: a versão web (`web/lume-web.js`) tem de chegar exatamente neles.
    #[test]
    fn reference_vectors() {
        let k = Keys::from_code("K7QM-2XPD-9RTA").unwrap();
        println!("topic={} key={}", k.topic, hex::encode(k.key));
        println!("proof={}", proof("0123456789abcdef0123456789abcdef", "a1b2c3d4e5f60718", "v=0\r\n"));
        assert_eq!(k.topic, Keys::from_code("k7qm 2xpd 9rta").unwrap().topic);
    }

    #[test]
    fn seal_open_and_wrong_code() {
        let k = Keys::from_code("K7QM-2XPD-9RTA").unwrap();
        let req = JoinReq { id: new_id(), name: "Ana".into(), sdp: "v=0 offer".repeat(200), av: String::new(), proof: String::new() };
        let sealed = k.seal(&req);
        let back: JoinReq = k.open(&sealed).unwrap();
        assert_eq!(back.sdp, req.sdp);
        let other = Keys::from_code("K7QM-2XPD-9RTB").unwrap();
        assert_ne!(other.topic, k.topic);
        assert!(other.open::<JoinReq>(&sealed).is_none(), "código errado não abre");
        let mut bad = sealed.clone();
        *bad.last_mut().unwrap() ^= 1;
        assert!(k.open::<JoinReq>(&bad).is_none(), "mensagem adulterada não abre");
    }
}
