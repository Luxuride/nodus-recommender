//! Sealing of instance access tokens at rest.
//!
//! A token stored in the database is the one secret that lets this service read
//! a user's graph while they are not looking. It is therefore encrypted with
//! XChaCha20-Poly1305 under a key that lives only in the environment, so a
//! database dump on its own is inert.
//!
//! The envelope is deliberately boring: a 24-byte random nonce followed by the
//! ciphertext and its tag. `XChaCha20`'s 192-bit nonce means a random nonce per
//! seal is safe without a counter, which removes the one piece of state that
//! would otherwise have to be coordinated across restarts and replicas.

use base64::Engine as _;
use chacha20poly1305::XNonce;
use chacha20poly1305::{
    XChaCha20Poly1305,
    aead::{Aead, KeyInit},
};
use sha2::{Digest, Sha256};

/// Length of the key, in bytes. 256-bit, matching `XChaCha20`.
pub const KEY_LEN: usize = 32;

/// Length of the nonce, in bytes. `XChaCha20` uses a 192-bit nonce.
pub const NONCE_LEN: usize = 24;

/// Why a token could not be sealed or opened.
#[derive(Debug, thiserror::Error)]
pub enum CryptoError {
    /// `TOKEN_ENC_KEY` was not a valid base64 encoding of [`KEY_LEN`] bytes.
    #[error("token encryption key must be {KEY_LEN} bytes, base64-encoded")]
    InvalidKey,
    /// The stored column was shorter than a nonce.
    #[error("sealed token is truncated")]
    Truncated,
    /// The tag did not verify: wrong key, or the ciphertext was altered.
    #[error("sealed token failed authentication")]
    Decrypt,
    /// The plaintext was not valid UTF-8 (should be impossible for tokens).
    #[error("sealed token did not decode to text")]
    NotText,
}

/// Seals and opens instance access tokens.
///
/// The key is held inside the AEAD state; [`Debug`] prints nothing useful so a
/// stray `{:?}` of the configuration cannot leak it into a log line.
#[derive(Clone)]
pub struct TokenSealer {
    cipher: XChaCha20Poly1305,
}

impl std::fmt::Debug for TokenSealer {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("TokenSealer(<redacted>)")
    }
}

impl TokenSealer {
    /// Build a sealer from a base64-encoded 32-byte key.
    ///
    /// This is the form the key takes in the environment, and in a Kubernetes
    /// secret — base64 because raw bytes are not safe to put in an env var.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::InvalidKey`] if the value is not valid base64 of
    /// exactly [`KEY_LEN`] bytes.
    pub fn from_base64(raw: &str) -> Result<Self, CryptoError> {
        let bytes = base64::engine::general_purpose::STANDARD
            .decode(raw.trim())
            .map_err(|_| CryptoError::InvalidKey)?;
        Self::from_key_bytes(&bytes)
    }

    /// Build a sealer from raw key material.
    ///
    /// # Errors
    ///
    /// Returns [`CryptoError::InvalidKey`] if the slice is not [`KEY_LEN`] bytes.
    pub fn from_key_bytes(key: &[u8]) -> Result<Self, CryptoError> {
        let cipher = XChaCha20Poly1305::new_from_slice(key).map_err(|_| CryptoError::InvalidKey)?;
        Ok(Self { cipher })
    }

    /// Encrypt a token, producing nonce ‖ ciphertext.
    ///
    /// # Errors
    ///
    /// Encryption itself cannot fail for a valid in-memory key; the `Result`
    /// exists so callers do not have to distinguish this from [`Self::open`].
    pub fn seal(&self, token: &str) -> Result<Vec<u8>, CryptoError> {
        let mut nonce_bytes = [0_u8; NONCE_LEN];
        getrandom::fill(&mut nonce_bytes).map_err(|_| CryptoError::Decrypt)?;
        let nonce = XNonce::from(nonce_bytes);
        let ciphertext = self
            .cipher
            .encrypt(&nonce, token.as_bytes())
            .map_err(|_| CryptoError::Decrypt)?;
        let mut sealed = Vec::with_capacity(NONCE_LEN + ciphertext.len());
        sealed.extend_from_slice(nonce.as_slice());
        sealed.extend_from_slice(&ciphertext);
        Ok(sealed)
    }

    /// Decrypt a token produced by [`Self::seal`].
    ///
    /// # Errors
    ///
    /// [`CryptoError::Truncated`] if the column is shorter than a nonce,
    /// [`CryptoError::Decrypt`] if authentication fails — which is the signal to
    /// treat the stored token as revoked rather than to retry.
    pub fn open(&self, sealed: &[u8]) -> Result<String, CryptoError> {
        if sealed.len() <= NONCE_LEN {
            return Err(CryptoError::Truncated);
        }
        let (nonce_bytes, ciphertext) = sealed.split_at(NONCE_LEN);
        let mut nonce = [0_u8; NONCE_LEN];
        nonce.copy_from_slice(nonce_bytes);
        let plaintext = self
            .cipher
            .decrypt(&XNonce::from(nonce), ciphertext)
            .map_err(|_| CryptoError::Decrypt)?;
        String::from_utf8(plaintext).map_err(|_| CryptoError::NotText)
    }
}

/// Stable, non-reversible identifier for a token.
///
/// The service sees the same bearer token on every request, so it needs a key to
/// look up a user without storing the token itself. `sha256` of the token serves
/// as that key: it is what the `token_fingerprint` column holds and what the
/// verification cache is indexed by.
#[must_use]
pub fn fingerprint(token: &str) -> [u8; 32] {
    Sha256::digest(token.as_bytes()).into()
}

#[cfg(test)]
mod tests {
    use super::{CryptoError, TokenSealer, fingerprint};

    const KEY: &[u8] = b"0123456789abcdef0123456789abcdef";

    fn sealer() -> TokenSealer {
        TokenSealer::from_key_bytes(KEY).expect("key accepts 32 bytes")
    }

    #[test]
    fn a_sealed_token_round_trips() {
        let sealer = sealer();
        let envelope = sealer.seal("tok-123").expect("seal succeeds");
        assert_eq!(sealer.open(&envelope).expect("open succeeds"), "tok-123");
    }

    #[test]
    fn the_same_token_seals_differently_each_time() {
        let sealer = sealer();
        let first = sealer.seal("tok-123").expect("seal succeeds");
        let second = sealer.seal("tok-123").expect("seal succeeds");
        assert_ne!(first, second, "the nonce must be fresh per seal");
        assert_eq!(first.len(), second.len());
    }

    #[test]
    fn a_tampered_ciphertext_does_not_open() {
        let sealer = sealer();
        let mut envelope = sealer.seal("tok-123").expect("seal succeeds");
        let last = envelope.len() - 1;
        envelope[last] ^= 0x01;
        assert!(matches!(sealer.open(&envelope), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn another_key_does_not_open_the_token() {
        let envelope = sealer().seal("tok-123").expect("seal succeeds");
        let other = TokenSealer::from_key_bytes(b"fedcba9876543210fedcba9876543210")
            .expect("key accepts 32 bytes");
        assert!(matches!(other.open(&envelope), Err(CryptoError::Decrypt)));
    }

    #[test]
    fn a_truncated_column_is_rejected_before_decryption() {
        let sealer = sealer();
        assert!(matches!(
            sealer.open(&[0_u8; 3]),
            Err(CryptoError::Truncated)
        ));
    }

    #[test]
    fn a_key_of_the_wrong_length_is_rejected() {
        assert!(matches!(
            TokenSealer::from_key_bytes(b"too short"),
            Err(CryptoError::InvalidKey)
        ));
        assert!(matches!(
            TokenSealer::from_base64("not base64!"),
            Err(CryptoError::InvalidKey)
        ));
    }

    #[test]
    fn the_fingerprint_is_stable_and_hides_the_token() {
        use std::fmt::Write as _;
        let mut hex = String::new();
        for byte in fingerprint("tok-123") {
            write!(hex, "{byte:02x}").expect("writing to a String cannot fail");
        }
        assert_eq!(hex.len(), 64);
        assert_eq!(fingerprint("tok-123"), fingerprint("tok-123"));
        assert_ne!(fingerprint("tok-123"), fingerprint("tok-124"));
        assert!(!hex.contains("tok"));
    }

    #[test]
    fn debug_output_does_not_leak_the_key() {
        let rendered = format!("{:?}", sealer());
        assert_eq!(rendered, "TokenSealer(<redacted>)");
    }
}
