use anyhow::{Context as _, Result, bail};
use aws_lc_rs::{
    encoding::{AsDer, PublicKeyX509Der},
    rsa::{
        KeySize, OAEP_SHA256_MGF1SHA256, OaepPrivateDecryptingKey, OaepPublicEncryptingKey,
        PrivateDecryptingKey, PublicEncryptingKey, PublicKey as RsaPublicKey,
    },
};
use base64::prelude::*;
use rand::prelude::*;
use std::convert::TryFrom;

#[derive(Debug, PartialEq, Eq, Clone, Copy)]
pub enum EncryptionFormat {
    /// Retained for protocol parsing; legacy encryption is refused.
    V0,
    V1,
}

pub struct PublicKey(RsaPublicKey);
pub struct PrivateKey(OaepPrivateDecryptingKey);

pub fn keypair() -> Result<(PublicKey, PrivateKey)> {
    let private_key = PrivateDecryptingKey::generate(KeySize::Rsa2048)?;
    let public_der: PublicKeyX509Der<'_> = private_key.public_key().as_der()?;
    let public_key = RsaPublicKey::from_der(public_der.as_ref())?;
    Ok((
        PublicKey(public_key),
        PrivateKey(OaepPrivateDecryptingKey::new(private_key)?),
    ))
}

pub fn random_token() -> String {
    let mut rng = rand::rng();
    let mut token_bytes = [0; 48];
    for byte in token_bytes.iter_mut() {
        *byte = rng.random();
    }
    BASE64_URL_SAFE.encode(token_bytes)
}

impl PublicKey {
    pub fn encrypt_string(&self, string: &str, format: EncryptionFormat) -> Result<String> {
        if format == EncryptionFormat::V0 {
            bail!("Legacy RSA encryption is disabled; use OAEP with SHA256");
        }
        let public_der: PublicKeyX509Der<'_> = self.0.as_der()?;
        let encrypter =
            OaepPublicEncryptingKey::new(PublicEncryptingKey::from_der(public_der.as_ref())?)?;
        let mut ciphertext = vec![0; encrypter.ciphertext_size()];
        let ciphertext = encrypter.encrypt(
            &OAEP_SHA256_MGF1SHA256,
            string.as_bytes(),
            &mut ciphertext,
            None,
        )?;
        Ok(BASE64_URL_SAFE.encode(ciphertext))
    }
}

impl PrivateKey {
    pub fn decrypt_string(&self, encrypted_string: &str) -> Result<String> {
        let ciphertext = BASE64_URL_SAFE
            .decode(encrypted_string)
            .context("Invalid encrypted authentication value")?;
        let mut plaintext = vec![0; self.0.min_output_size()];
        // Never retry with legacy padding: a fallback restores a padding oracle.
        let plaintext = self
            .0
            .decrypt(&OAEP_SHA256_MGF1SHA256, &ciphertext, &mut plaintext, None)
            .context("Invalid encrypted authentication value")?;
        Ok(std::str::from_utf8(plaintext)
            .context("Invalid encrypted authentication value")?
            .to_owned())
    }
}

impl TryFrom<PublicKey> for String {
    type Error = anyhow::Error;
    fn try_from(key: PublicKey) -> Result<Self> {
        Ok(BASE64_URL_SAFE.encode(key.0.as_ref()))
    }
}

impl TryFrom<String> for PublicKey {
    type Error = anyhow::Error;
    fn try_from(value: String) -> Result<Self> {
        let bytes = BASE64_URL_SAFE
            .decode(value)
            .context("Invalid public key encoding")?;
        Ok(Self(RsaPublicKey::from_der(&bytes)?))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_generate_encrypt_and_decrypt_token() {
        // CLIENT:
        // * generate a keypair for asymmetric encryption
        // * serialize the public key to send it to the server.
        let (public, private) = keypair().unwrap();
        let public_string = String::try_from(public).unwrap();
        assert_printable(&public_string);

        // SERVER:
        // * parse the public key
        // * generate a random token.
        // * encrypt the token using the public key.
        let public = PublicKey::try_from(public_string).unwrap();
        let token = random_token();
        let encrypted_token = public.encrypt_string(&token, EncryptionFormat::V1).unwrap();
        assert_eq!(token.len(), 64);
        assert_ne!(encrypted_token, token);
        assert_printable(&token);
        assert_printable(&encrypted_token);

        // CLIENT:
        // * decrypt the token using the private key.
        let decrypted_token = private.decrypt_string(&encrypted_token).unwrap();
        assert_eq!(decrypted_token, token);
    }

    #[test]
    fn rejects_legacy_padding_and_malformed_ciphertexts() -> Result<()> {
        let (public, private) = keypair()?;
        assert!(
            public
                .encrypt_string("secret", EncryptionFormat::V0)
                .is_err()
        );
        let public_der: PublicKeyX509Der<'_> = public.0.as_der()?;
        let encrypter = aws_lc_rs::rsa::Pkcs1PublicEncryptingKey::new(
            PublicEncryptingKey::from_der(public_der.as_ref())?,
        )?;
        let mut ciphertext = vec![0; encrypter.ciphertext_size()];
        let ciphertext = encrypter.encrypt(b"secret", &mut ciphertext)?;
        assert!(
            private
                .decrypt_string(&BASE64_URL_SAFE.encode(ciphertext))
                .is_err()
        );
        assert!(private.decrypt_string("not base64!").is_err());
        assert!(
            private
                .decrypt_string(&BASE64_URL_SAFE.encode(b"short"))
                .is_err()
        );
        Ok(())
    }

    #[test]
    fn test_encode_and_decode_base64_public_key() {
        // A base64-encoded public key.
        //
        // We're using a literal string to ensure that encoding and decoding works across differences in implementations.
        let encoded_public_key = "MIGJAoGBAMPvufou8wOuUIF1Wlkbtn0ZMM9nC55QJ06nTZvgMfZv5esFVU9-cQO_JC1P9ZoEcMDJweFERnQuQLqzsrMDLFbkdgL128ZU43WOLiQraxaICFIZsPUeTtWMKp2D5bPWsNxs-lnCma7vCAry6fpXuj5AKQdk7cTZJNucgvZQ0uUfAgMBAAE=".to_string();

        // Make sure we can parse the public key.
        let public_key = PublicKey::try_from(encoded_public_key.clone()).unwrap();

        // Make sure we re-encode to the same format.
        assert_eq!(encoded_public_key, String::try_from(public_key).unwrap());
    }

    #[test]
    fn test_tokens_are_always_url_safe() {
        for _ in 0..5 {
            let token = random_token();
            let (public_key, _) = keypair().unwrap();
            let encrypted_token = public_key
                .encrypt_string(&token, EncryptionFormat::V1)
                .unwrap();
            let public_key_str = String::try_from(public_key).unwrap();

            assert_printable(&token);
            assert_printable(&public_key_str);
            assert_printable(&encrypted_token);
        }
    }

    fn assert_printable(token: &str) {
        for c in token.chars() {
            assert!(
                c.is_ascii_graphic(),
                "token {:?} has non-printable char {}",
                token,
                c
            );
            assert_ne!(c, '/', "token {:?} is not URL-safe", token);
            assert_ne!(c, '&', "token {:?} is not URL-safe", token);
        }
    }
}
