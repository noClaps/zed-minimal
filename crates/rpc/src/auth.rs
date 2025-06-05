use anyhow::{Context as _, Result};
use base64::prelude::*;
use rand::{Rng as _, thread_rng};
use rsa::pkcs1::{DecodeRsaPublicKey, EncodeRsaPublicKey};
use rsa::traits::PaddingScheme;
use rsa::{Oaep, Pkcs1v15Encrypt, RsaPrivateKey, RsaPublicKey};
use sha2::Sha256;
use std::convert::TryFrom;

fn oaep_sha256_padding() -> impl PaddingScheme {
    Oaep::new::<Sha256>()
}

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum EncryptionFormat {
    /// The original encryption format.
    ///
    /// This is using [`Pkcs1v15Encrypt`], which is vulnerable to side-channel attacks.
    /// As such, we're in the process of phasing it out.
    ///
    /// See [here](https://people.redhat.com/~hkario/marvin/) for more details.
    V0,

    /// The new encryption key format using Optimal Asymmetric Encryption Padding (OAEP) with a SHA-256 digest.
    V1,
}

pub struct PublicKey(RsaPublicKey);

pub struct PrivateKey(RsaPrivateKey);

/// Generate a public and private key for asymmetric encryption.
pub fn keypair() -> Result<(PublicKey, PrivateKey)> {
    let mut rng = thread_rng();
    let bits = 2048;
    let private_key = RsaPrivateKey::new(&mut rng, bits)?;
    let public_key = RsaPublicKey::from(&private_key);
    Ok((PublicKey(public_key), PrivateKey(private_key)))
}

/// Generate a random 64-character base64 string.
pub fn random_token() -> String {
    let mut rng = thread_rng();
    let mut token_bytes = [0; 48];
    for byte in token_bytes.iter_mut() {
        *byte = rng.r#gen();
    }
    BASE64_URL_SAFE.encode(token_bytes)
}

impl PublicKey {
    /// Convert a string to a base64-encoded string that can only be decoded with the corresponding
    /// private key.
    pub fn encrypt_string(&self, string: &str, format: EncryptionFormat) -> Result<String> {
        let mut rng = thread_rng();
        let bytes = string.as_bytes();
        let encrypted_bytes = match format {
            EncryptionFormat::V0 => self.0.encrypt(&mut rng, Pkcs1v15Encrypt, bytes),
            EncryptionFormat::V1 => self.0.encrypt(&mut rng, oaep_sha256_padding(), bytes),
        }
        .context("failed to encrypt string with public key")?;
        let encrypted_string = BASE64_URL_SAFE.encode(&encrypted_bytes);
        Ok(encrypted_string)
    }
}

impl PrivateKey {
    /// Decrypt a base64-encoded string that was encrypted by the corresponding public key.
    pub fn decrypt_string(&self, encrypted_string: &str) -> Result<String> {
        let encrypted_bytes = BASE64_URL_SAFE
            .decode(encrypted_string)
            .context("failed to base64-decode encrypted string")?;
        let bytes = self
            .0
            .decrypt(oaep_sha256_padding(), &encrypted_bytes)
            .or_else(|_err| {
                // If we failed to decrypt using the new format, try decrypting with the old
                // one to handle mismatches between the client and server.
                self.0.decrypt(Pkcs1v15Encrypt, &encrypted_bytes)
            })
            .context("failed to decrypt string with private key")?;
        let string = String::from_utf8(bytes).context("decrypted content was not valid utf8")?;
        Ok(string)
    }
}

impl TryFrom<PublicKey> for String {
    type Error = anyhow::Error;
    fn try_from(key: PublicKey) -> Result<Self> {
        let bytes = key
            .0
            .to_pkcs1_der()
            .context("failed to serialize public key")?;
        let string = BASE64_URL_SAFE.encode(&bytes);
        Ok(string)
    }
}

impl TryFrom<String> for PublicKey {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        let bytes = BASE64_URL_SAFE
            .decode(&value)
            .context("failed to base64-decode public key string")?;
        let key = Self(RsaPublicKey::from_pkcs1_der(&bytes).context("failed to parse public key")?);
        Ok(key)
    }
}
