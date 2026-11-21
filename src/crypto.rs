// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

use anyhow::{Result, anyhow, bail};
use chacha20poly1305::{
    XChaCha20Poly1305, XNonce,
    aead::{Aead, KeyInit, Payload},
};
use serde::{Deserialize, Serialize};

pub type Id = [u8; 32];

/// First byte of every encrypted blob; names the cipher, compression and layout that follow.
pub const BLOB_FORMAT: u8 = 1;

pub struct Keys {
    id_key: [u8; 32],
    cipher: XChaCha20Poly1305,
}

fn cipher(key: &[u8; 32]) -> XChaCha20Poly1305 {
    XChaCha20Poly1305::new_from_slice(key).expect("32 byte key")
}

fn encrypt(c: &XChaCha20Poly1305, aad: &[u8], msg: &[u8]) -> Result<Vec<u8>> {
    let nonce: [u8; 24] = rand::random();
    let ct = c.encrypt(&XNonce::from(nonce), Payload { msg, aad }).map_err(|_| anyhow!("encryption failed"))?;
    let mut out = vec![BLOB_FORMAT];
    out.extend(nonce);
    out.extend(ct);
    Ok(out)
}

fn decrypt(c: &XChaCha20Poly1305, aad: &[u8], blob: &[u8]) -> Result<Vec<u8>> {
    if blob.len() < 1 + 24 + 16 {
        bail!("ciphertext too short");
    }
    if blob[0] != BLOB_FORMAT {
        bail!("data is in format {} but this sdb only reads format {BLOB_FORMAT}; upgrade sdb", blob[0]);
    }
    let nonce: [u8; 24] = blob[1..25].try_into().unwrap();
    c.decrypt(&XNonce::from(nonce), Payload { msg: &blob[25..], aad })
        .map_err(|_| anyhow!("decryption failed (wrong key or corrupted data)"))
}

impl Keys {
    pub fn new(master: &[u8; 32]) -> Self {
        Keys {
            id_key: blake3::derive_key("super-duper-backup v1 chunk id", master),
            cipher: cipher(&blake3::derive_key("super-duper-backup v1 encryption", master)),
        }
    }

    pub fn chunk_id(&self, data: &[u8]) -> Id {
        *blake3::keyed_hash(&self.id_key, data).as_bytes()
    }

    pub fn seal_raw(&self, aad: &[u8], plain: &[u8]) -> Result<Vec<u8>> {
        encrypt(&self.cipher, aad, &zstd::encode_all(plain, 5)?)
    }

    pub fn open_raw(&self, aad: &[u8], blob: &[u8]) -> Result<Vec<u8>> {
        Ok(zstd::decode_all(&decrypt(&self.cipher, aad, blob)?[..])?)
    }

    pub fn seal(&self, id: &Id, plain: &[u8]) -> Result<Vec<u8>> {
        self.seal_raw(id, plain)
    }

    pub fn open(&self, id: &Id, blob: &[u8]) -> Result<Vec<u8>> {
        let plain = self.open_raw(id, blob)?;
        if &self.chunk_id(&plain) != id {
            bail!("chunk {} failed its integrity check", hex::encode(&id[..8]));
        }
        Ok(plain)
    }
}

#[derive(Serialize, Deserialize)]
struct Kit {
    format: String,
    #[serde(default)]
    created: String,
    salt: String,
    data: String,
}

fn kit_key(pass: &str, salt: &[u8]) -> Result<[u8; 32]> {
    let params = argon2::Params::new(64 * 1024, 3, 1, Some(32)).map_err(|e| anyhow!("{e}"))?;
    let a = argon2::Argon2::new(argon2::Algorithm::Argon2id, argon2::Version::V0x13, params);
    let mut key = [0u8; 32];
    a.hash_password_into(pass.as_bytes(), salt, &mut key).map_err(|e| anyhow!("{e}"))?;
    Ok(key)
}

pub fn kit_seal(pass: &str, plain: &[u8]) -> Result<String> {
    let salt: [u8; 16] = rand::random();
    let data = encrypt(&cipher(&kit_key(pass, &salt)?), b"sdb-recovery-kit", plain)?;
    Ok(serde_json::to_string_pretty(&Kit {
        format: "super-duper-backup recovery kit v1".into(),
        created: chrono::Utc::now().to_rfc3339(),
        salt: hex::encode(salt),
        data: hex::encode(data),
    })?)
}

pub fn kit_open(pass: &str, kit: &str) -> Result<Vec<u8>> {
    let kit: Kit = serde_json::from_str(kit).map_err(|_| anyhow!("not a recovery kit"))?;
    if kit.format != "super-duper-backup recovery kit v1" {
        bail!("this recovery kit ({}) needs a newer sdb", kit.format);
    }
    let key = kit_key(pass, &hex::decode(&kit.salt)?)?;
    decrypt(&cipher(&key), b"sdb-recovery-kit", &hex::decode(&kit.data)?)
        .map_err(|_| anyhow!("wrong passphrase or damaged recovery kit"))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn chunk_round_trip_and_tamper() {
        let keys = Keys::new(&[7; 32]);
        let data = b"hello hello hello hello".repeat(100);
        let id = keys.chunk_id(&data);
        let blob = keys.seal(&id, &data).unwrap();
        assert_eq!(blob[0], BLOB_FORMAT);
        assert!(blob.len() < data.len(), "repetitive data should compress");
        assert_eq!(keys.open(&id, &blob).unwrap(), data);

        let mut bad = blob.clone();
        let last = bad.len() - 1;
        bad[last] ^= 1;
        assert!(keys.open(&id, &bad).is_err(), "a flipped bit must be rejected");

        let other = keys.chunk_id(b"something else");
        assert!(keys.open(&other, &blob).is_err(), "a chunk served under another id must be rejected");
        assert!(Keys::new(&[8; 32]).open(&id, &blob).is_err(), "another key must not decrypt");
    }

    #[test]
    fn ids_are_keyed() {
        assert_ne!(Keys::new(&[1; 32]).chunk_id(b"same"), Keys::new(&[2; 32]).chunk_id(b"same"));
    }

    #[test]
    fn unknown_blob_format_is_refused() {
        let keys = Keys::new(&[7; 32]);
        let mut blob = keys.seal_raw(b"aad", b"data").unwrap();
        blob[0] = BLOB_FORMAT + 1;
        let err = keys.open_raw(b"aad", &blob).unwrap_err().to_string();
        assert!(err.contains("upgrade"), "{err}");
    }

    #[test]
    fn recovery_kit_needs_the_passphrase() {
        let kit = kit_seal("correct horse", b"secret").unwrap();
        assert_eq!(kit_open("correct horse", &kit).unwrap(), b"secret");
        assert!(kit_open("wrong", &kit).is_err());
        assert!(kit_open("correct horse", "not json").is_err());
    }
}
