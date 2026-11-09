//! `sigstore-crypto`'s [`CryptoProvider`] over ring (R3.42, ADR 0066), so
//! keyless verification keeps the tree's ring-only rule: `sigstore-crypto`'s
//! only built-in backend is aws-lc-rs, which Reef never links.
//!
//! It hashes (SHA-256, SHA-384, SHA-512) and verifies ECDSA P-256 and P-384
//! (ASN.1 signatures), ed25519, and RSA PKCS#1 v1.5 and PSS (2048 to 8192
//! bits): every algorithm a Sigstore trusted root, a Fulcio leaf or a
//! transparency log uses. ring verifies only over the signed message, never
//! from a precomputed digest, so [`CryptoProvider::verify_prehashed`] is
//! unsupported and Reef always passes the signed bytes (a DSSE envelope's
//! PAE). It generates and loads no keys: Reef signs with its own keys.

use ring::digest;
use ring::signature::{self, UnparsedPublicKey, VerificationAlgorithm};
use sigstore_crypto::{
    CryptoProvider, Error, HashContext, Result, SigningScheme, install_default_provider,
    try_default_provider,
};
use sigstore_types::HashAlgorithm;

/// The ring provider.
#[derive(Debug)]
pub struct RingProvider;

static RING_PROVIDER: RingProvider = RingProvider;

/// Installs [`RingProvider`] as `sigstore-crypto`'s process-wide provider,
/// once; later calls do nothing. Every verification in this module calls it
/// first.
pub fn install() {
    if try_default_provider().is_none() {
        // Another thread may have won the race: either way ring is installed,
        // since nothing else in Reef installs a provider.
        let _ = install_default_provider(&RING_PROVIDER);
    }
}

struct RingHash(digest::Context);

impl HashContext for RingHash {
    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }

    fn finalize(self: Box<Self>) -> Vec<u8> {
        self.0.finish().as_ref().to_vec()
    }
}

/// ring's algorithm for `scheme`; `None` for ML-DSA, which ring lacks.
fn algorithm(scheme: SigningScheme) -> Option<&'static dyn VerificationAlgorithm> {
    Some(match scheme {
        SigningScheme::EcdsaP256Sha256 => &signature::ECDSA_P256_SHA256_ASN1,
        SigningScheme::EcdsaP256Sha384 => &signature::ECDSA_P256_SHA384_ASN1,
        SigningScheme::EcdsaP384Sha256 => &signature::ECDSA_P384_SHA256_ASN1,
        SigningScheme::EcdsaP384Sha384 => &signature::ECDSA_P384_SHA384_ASN1,
        SigningScheme::Ed25519 => &signature::ED25519,
        SigningScheme::RsaPssSha256 => &signature::RSA_PSS_2048_8192_SHA256,
        SigningScheme::RsaPssSha384 => &signature::RSA_PSS_2048_8192_SHA384,
        SigningScheme::RsaPssSha512 => &signature::RSA_PSS_2048_8192_SHA512,
        SigningScheme::RsaPkcs1Sha256 => &signature::RSA_PKCS1_2048_8192_SHA256,
        SigningScheme::RsaPkcs1Sha384 => &signature::RSA_PKCS1_2048_8192_SHA384,
        SigningScheme::RsaPkcs1Sha512 => &signature::RSA_PKCS1_2048_8192_SHA512,
        _ => return None,
    })
}

impl CryptoProvider for RingProvider {
    fn hash(&self, algorithm: HashAlgorithm) -> Result<Box<dyn HashContext>> {
        let algorithm = match algorithm {
            HashAlgorithm::Sha2256 => &digest::SHA256,
            HashAlgorithm::Sha2384 => &digest::SHA384,
            HashAlgorithm::Sha2512 => &digest::SHA512,
            #[allow(unreachable_patterns, reason = "the enum is non-exhaustive upstream")]
            other => {
                return Err(Error::UnsupportedAlgorithm(format!(
                    "hash {other:?} is not supported by Reef's ring provider"
                )));
            }
        };
        Ok(Box::new(RingHash(digest::Context::new(algorithm))))
    }

    fn verify_raw(
        &self,
        scheme: SigningScheme,
        spki_pubkey_bytes: &[u8],
        data: &[u8],
        signature: &[u8],
    ) -> Result<()> {
        let algorithm = algorithm(scheme).ok_or_else(|| {
            Error::UnsupportedAlgorithm(format!(
                "{scheme:?} is not supported by Reef's ring provider"
            ))
        })?;
        UnparsedPublicKey::new(algorithm, spki_pubkey_bytes)
            .verify(data, signature)
            .map_err(|_| Error::Verification(format!("the {scheme:?} signature does not verify")))
    }

    fn verify_prehashed(
        &self,
        scheme: SigningScheme,
        _spki_pubkey_bytes: &[u8],
        _digest: &[u8],
        _signature: &[u8],
    ) -> Result<()> {
        Err(Error::UnsupportedAlgorithm(format!(
            "{scheme:?} from a precomputed digest: Reef's ring provider verifies only over the signed bytes"
        )))
    }
}

#[cfg(test)]
mod tests {
    use ring::rand::SystemRandom;
    use ring::signature::{EcdsaKeyPair, Ed25519KeyPair, KeyPair as _};

    use super::*;

    #[test]
    fn hashes_match_ring() {
        for (algorithm, ring_algorithm) in [
            (HashAlgorithm::Sha2256, &digest::SHA256),
            (HashAlgorithm::Sha2384, &digest::SHA384),
            (HashAlgorithm::Sha2512, &digest::SHA512),
        ] {
            let mut context = RingProvider.hash(algorithm).unwrap();
            context.update(b"hello ");
            context.update(b"world");
            assert_eq!(
                context.finalize(),
                digest::digest(ring_algorithm, b"hello world").as_ref()
            );
        }
    }

    #[test]
    fn verifies_ecdsa_and_ed25519_and_refuses_prehashed() {
        let rng = SystemRandom::new();
        for (scheme, signing) in [
            (
                SigningScheme::EcdsaP256Sha256,
                &signature::ECDSA_P256_SHA256_ASN1_SIGNING,
            ),
            (
                SigningScheme::EcdsaP384Sha384,
                &signature::ECDSA_P384_SHA384_ASN1_SIGNING,
            ),
        ] {
            let pkcs8 = EcdsaKeyPair::generate_pkcs8(signing, &rng).unwrap();
            let key = EcdsaKeyPair::from_pkcs8(signing, pkcs8.as_ref(), &rng).unwrap();
            let sig = key.sign(&rng, b"message").unwrap();
            let public = key.public_key().as_ref();
            RingProvider
                .verify_raw(scheme, public, b"message", sig.as_ref())
                .unwrap();
            assert!(
                RingProvider
                    .verify_raw(scheme, public, b"other", sig.as_ref())
                    .is_err()
            );
            assert!(
                RingProvider
                    .verify_prehashed(scheme, public, &[0; 32], sig.as_ref())
                    .is_err()
            );
        }
        let pkcs8 = Ed25519KeyPair::generate_pkcs8(&rng).unwrap();
        let key = Ed25519KeyPair::from_pkcs8(pkcs8.as_ref()).unwrap();
        let sig = key.sign(b"message");
        RingProvider
            .verify_raw(
                SigningScheme::Ed25519,
                key.public_key().as_ref(),
                b"message",
                sig.as_ref(),
            )
            .unwrap();
        assert!(
            RingProvider
                .verify_raw(
                    SigningScheme::MlDsa44,
                    key.public_key().as_ref(),
                    b"message",
                    sig.as_ref()
                )
                .is_err()
        );
    }
}
