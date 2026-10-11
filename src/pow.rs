use base64::Engine;
use serde::{Deserialize, Serialize};

const KECCAK_RC: [u64; 24] = [
    0x0000000000000001,
    0x0000000000008082,
    0x800000000000808A,
    0x8000000080008000,
    0x000000000000808B,
    0x0000000080000001,
    0x8000000080008081,
    0x8000000000008009,
    0x000000000000008A,
    0x0000000000000088,
    0x0000000080008009,
    0x000000008000000A,
    0x000000008000808B,
    0x800000000000008B,
    0x8000000000008089,
    0x8000000000008003,
    0x8000000000008002,
    0x8000000000000080,
    0x000000000000800A,
    0x800000008000000A,
    0x8000000080008081,
    0x8000000000008080,
    0x0000000080000001,
    0x8000000080008008,
];

const KECCAK_ROT: [u32; 24] = [
    1, 3, 6, 10, 15, 21, 28, 36, 45, 55, 2, 14, 27, 41, 56, 8, 25, 43, 62, 18, 39, 61, 20, 44,
];
const KECCAK_PIL: [usize; 24] = [
    10, 7, 11, 17, 18, 3, 5, 16, 8, 21, 24, 4, 15, 23, 19, 13, 12, 2, 20, 14, 22, 9, 6, 1,
];

fn keccak_f23(st: &mut [u64; 25]) {
    for round in 1..24usize {
        let mut bc = [0u64; 5];
        for i in 0..5 {
            bc[i] = st[i] ^ st[i + 5] ^ st[i + 10] ^ st[i + 15] ^ st[i + 20];
        }
        for i in 0..5 {
            let t = bc[(i + 4) % 5] ^ bc[(i + 1) % 5].rotate_left(1);
            for j in (0..25).step_by(5) {
                st[j + i] ^= t;
            }
        }
        let mut t = st[1];
        for i in 0..24 {
            let j = KECCAK_PIL[i];
            let tmp = st[j];
            st[j] = t.rotate_left(KECCAK_ROT[i]);
            t = tmp;
        }
        for j in (0..25).step_by(5) {
            let bc = [st[j], st[j + 1], st[j + 2], st[j + 3], st[j + 4]];
            for i in 0..5 {
                st[j + i] = bc[i] ^ (!bc[(i + 1) % 5] & bc[(i + 2) % 5]);
            }
        }
        st[0] ^= KECCAK_RC[round];
    }
}

/// DeepSeekHashV1: reduced Keccak-f[1600] sponge - rate 136, SHA-3 padding,
/// 23 rounds (RC[0] skipped), output = first 32 state bytes.
pub fn deepseek_hash_v1(msg: &[u8]) -> [u8; 32] {
    const RATE: usize = 136;
    let mut st = [0u64; 25];

    let mut chunks = msg.chunks_exact(RATE);
    for block in &mut chunks {
        absorb_block(&mut st, block.try_into().expect("full block"));
    }
    let rem = chunks.remainder();
    let mut blk = [0u8; RATE];
    blk[..rem.len()].copy_from_slice(rem);
    blk[rem.len()] = 0x06;
    blk[RATE - 1] |= 0x80;
    absorb_block(&mut st, &blk);

    let mut out = [0u8; 32];
    for i in 0..4 {
        out[i * 8..(i + 1) * 8].copy_from_slice(&st[i].to_le_bytes());
    }
    out
}

fn absorb_block(st: &mut [u64; 25], blk: &[u8; 136]) {
    for i in 0..17 {
        st[i] ^= u64::from_le_bytes(blk[i * 8..(i + 1) * 8].try_into().expect("8 bytes"));
    }
    keccak_f23(st);
}

fn hex_decode_32(s: &str) -> Option<[u8; 32]> {
    let bytes = s.as_bytes();
    if bytes.len() != 64 {
        return None;
    }
    let mut out = [0u8; 32];
    for i in 0..32 {
        let hi = hex_val(bytes[i * 2])?;
        let lo = hex_val(bytes[i * 2 + 1])?;
        out[i] = hi << 4 | lo;
    }
    Some(out)
}

fn hex_val(c: u8) -> Option<u8> {
    match c {
        b'0'..=b'9' => Some(c - b'0'),
        b'a'..=b'f' => Some(c - b'a' + 10),
        b'A'..=b'F' => Some(c - b'A' + 10),
        _ => None,
    }
}

#[cfg(test)]
fn hex_encode(bytes: &[u8]) -> String {
    let mut s = String::with_capacity(bytes.len() * 2);
    for b in bytes {
        s.push_str(&format!("{b:02x}"));
    }
    s
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PowChallenge {
    pub algorithm: String,
    pub challenge: String,
    pub salt: String,
    pub signature: String,
    pub difficulty: i64,
    pub expire_at: i64,
    pub target_path: String,
}

/// Finds the nonce whose DeepSeekHashV1 hash of `<salt>_<expire_at>_<nonce>`
/// equals the challenge. Expected work is ~`difficulty` hashes.
pub fn solve_pow(ch: &PowChallenge) -> Option<u64> {
    let target = hex_decode_32(&ch.challenge)?;
    let prefix = format!("{}_{}_", ch.salt, ch.expire_at).into_bytes();
    let mut nonce: u64 = 0;
    loop {
        let mut msg = Vec::with_capacity(prefix.len() + 20);
        msg.extend_from_slice(&prefix);
        msg.extend_from_slice(nonce.to_string().as_bytes());
        if deepseek_hash_v1(&msg) == target {
            return Some(nonce);
        }
        nonce = nonce.checked_add(1)?;
    }
}

/// Builds the `x-ds-pow-response` header value: base64(JSON with the answer).
pub fn pow_header_value(ch: &PowChallenge, nonce: u64) -> String {
    let answer = serde_json::json!({
        "algorithm": ch.algorithm,
        "challenge": ch.challenge,
        "salt": ch.salt,
        "answer": nonce,
        "signature": ch.signature,
        "target_path": ch.target_path,
    });
    base64::engine::general_purpose::STANDARD.encode(answer.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hash_hex(msg: &str) -> String {
        hex_encode(&deepseek_hash_v1(msg.as_bytes()))
    }

    #[test]
    fn hash_is_deterministic_and_length_correct() {
        let a = hash_hex("salt_123_0");
        let b = hash_hex("salt_123_0");
        assert_eq!(a, b);
        assert_eq!(a.len(), 64);
        assert_ne!(a, hash_hex("salt_123_1"));
    }

    #[test]
    fn solver_finds_planted_nonce() {
        let nonce = 424242u64;
        let ch = PowChallenge {
            algorithm: "DeepSeekHashV1".to_owned(),
            challenge: hex_encode(&deepseek_hash_v1(
                format!("deadsalt_1699999999999_{nonce}").as_bytes(),
            )),
            salt: "deadsalt".to_owned(),
            signature: "00".repeat(32),
            difficulty: 500000,
            expire_at: 1699999999999,
            target_path: "/api/v0/chat/completion".to_owned(),
        };
        assert_eq!(solve_pow(&ch), Some(nonce));
    }

    #[test]
    fn solver_returns_none_on_bad_challenge_hex() {
        let ch = PowChallenge {
            algorithm: "DeepSeekHashV1".to_owned(),
            challenge: "zz".repeat(32),
            salt: "x".to_owned(),
            signature: String::new(),
            difficulty: 1,
            expire_at: 1,
            target_path: String::new(),
        };
        assert_eq!(solve_pow(&ch), None);
    }

    #[test]
    fn header_value_is_base64_json_with_answer() {
        let ch = PowChallenge {
            algorithm: "DeepSeekHashV1".to_owned(),
            challenge: "ab".repeat(32),
            salt: "cd".repeat(10),
            signature: "ef".repeat(32),
            difficulty: 1000,
            expire_at: 1700000000000,
            target_path: "/api/v0/chat/completion".to_owned(),
        };
        let header = pow_header_value(&ch, 7);
        let decoded = base64::engine::general_purpose::STANDARD
            .decode(&header)
            .expect("valid base64");
        let json: serde_json::Value = serde_json::from_slice(&decoded).expect("valid json");
        assert_eq!(json["answer"], 7);
        assert_eq!(json["algorithm"], "DeepSeekHashV1");
        assert_eq!(json["target_path"], "/api/v0/chat/completion");
    }
}
