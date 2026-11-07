//! The TLS crypto a lockdownd session needs, for rustls, over the RustCrypto crates (pure Rust):
//! AES-GCM records for TLS 1.3 and TLS 1.2 ECDHE-RSA, X25519 and P-256 key exchange, SHA-256 and
//! SHA-384, RSA signing with the pair record's host key, and RSA checks of the device's handshake
//! signature. Nothing else: the device's key is the RSA key it was given at pairing, so no ECDSA,
//! and every TLS stack iOS has used offers AES-GCM and P-256.

use aes_gcm::aead::consts::{U12, U16};
use aes_gcm::aead::{AeadInPlace, KeyInit};
use aes_gcm::{Aes128Gcm, Aes256Gcm, Tag};
use hmac::digest::core_api::BlockSizeUser;
use hmac::{Mac, SimpleHmac};
use rand_core::OsRng;
use rsa::pkcs1::{DecodeRsaPrivateKey, DecodeRsaPublicKey};
use rsa::pkcs8::DecodePrivateKey;
use rsa::signature::{RandomizedSigner, SignatureEncoding, Verifier};
use rsa::{RsaPrivateKey, RsaPublicKey};
use rustls::crypto::cipher::{
    AeadKey, InboundOpaqueMessage, InboundPlainMessage, Iv, KeyBlockShape, MessageDecrypter, MessageEncrypter, Nonce, OutboundOpaqueMessage, OutboundPlainMessage, PrefixedPayload, Tls12AeadAlgorithm, Tls13AeadAlgorithm,
    UnsupportedOperationError, make_tls12_aad, make_tls13_aad,
};
use rustls::crypto::{ActiveKeyExchange, CipherSuiteCommon, CryptoProvider, GetRandomFailed, KeyExchangeAlgorithm, KeyProvider, SecureRandom, SharedSecret, SupportedKxGroup, WebPkiSupportedAlgorithms, hash, hmac as rhmac, tls12, tls13};
use rustls::pki_types::PrivateKeyDer;
use rustls::sign::{Signer, SigningKey};
use rustls::{CipherSuite, ConnectionTrafficSecrets, ContentType, Error, NamedGroup, ProtocolVersion, SignatureAlgorithm, SignatureScheme, SupportedCipherSuite, Tls12CipherSuite, Tls13CipherSuite};
use sha2::{Digest, Sha256, Sha384, Sha512};
use std::marker::PhantomData;
use std::sync::Arc;

pub fn provider() -> CryptoProvider {
    CryptoProvider {
        cipher_suites: vec![TLS13_AES_128_GCM_SHA256, TLS13_AES_256_GCM_SHA384, TLS12_ECDHE_RSA_AES_128_GCM_SHA256, TLS12_ECDHE_RSA_AES_256_GCM_SHA384],
        kx_groups: vec![X25519, P256],
        // The device's signatures are checked by the pinning verifier (`verify`), never by webpki.
        signature_verification_algorithms: WebPkiSupportedAlgorithms { all: &[], mapping: &[] },
        secure_random: &Random,
        key_provider: &Random,
    }
}

// cipher suites

/// Records one AES-GCM key may protect (RFC 8446 section 5.5).
const LIMIT: u64 = 1 << 24;

const SHA256: &Hash<Sha256> = &Hash(PhantomData, hash::HashAlgorithm::SHA256);
const SHA384: &Hash<Sha384> = &Hash(PhantomData, hash::HashAlgorithm::SHA384);
const HMAC_SHA256: &Hmac<Sha256> = &Hmac(PhantomData);
const HMAC_SHA384: &Hmac<Sha384> = &Hmac(PhantomData);

const TLS13_AES_128_GCM_SHA256: SupportedCipherSuite = SupportedCipherSuite::Tls13(&Tls13CipherSuite {
    common: CipherSuiteCommon { suite: CipherSuite::TLS13_AES_128_GCM_SHA256, hash_provider: SHA256, confidentiality_limit: LIMIT },
    hkdf_provider: &tls13::HkdfUsingHmac(HMAC_SHA256),
    aead_alg: &Gcm13::<Aes128Gcm>(PhantomData),
    quic: None,
});
const TLS13_AES_256_GCM_SHA384: SupportedCipherSuite = SupportedCipherSuite::Tls13(&Tls13CipherSuite {
    common: CipherSuiteCommon { suite: CipherSuite::TLS13_AES_256_GCM_SHA384, hash_provider: SHA384, confidentiality_limit: LIMIT },
    hkdf_provider: &tls13::HkdfUsingHmac(HMAC_SHA384),
    aead_alg: &Gcm13::<Aes256Gcm>(PhantomData),
    quic: None,
});
const RSA_SCHEMES: &[SignatureScheme] = &[
    SignatureScheme::RSA_PSS_SHA512,
    SignatureScheme::RSA_PSS_SHA384,
    SignatureScheme::RSA_PSS_SHA256,
    SignatureScheme::RSA_PKCS1_SHA512,
    SignatureScheme::RSA_PKCS1_SHA384,
    SignatureScheme::RSA_PKCS1_SHA256,
];
const TLS12_ECDHE_RSA_AES_128_GCM_SHA256: SupportedCipherSuite = SupportedCipherSuite::Tls12(&Tls12CipherSuite {
    common: CipherSuiteCommon { suite: CipherSuite::TLS_ECDHE_RSA_WITH_AES_128_GCM_SHA256, hash_provider: SHA256, confidentiality_limit: LIMIT },
    prf_provider: &tls12::PrfUsingHmac(HMAC_SHA256),
    kx: KeyExchangeAlgorithm::ECDHE,
    sign: RSA_SCHEMES,
    aead_alg: &Gcm12::<Aes128Gcm>(PhantomData),
});
const TLS12_ECDHE_RSA_AES_256_GCM_SHA384: SupportedCipherSuite = SupportedCipherSuite::Tls12(&Tls12CipherSuite {
    common: CipherSuiteCommon { suite: CipherSuite::TLS_ECDHE_RSA_WITH_AES_256_GCM_SHA384, hash_provider: SHA384, confidentiality_limit: LIMIT },
    prf_provider: &tls12::PrfUsingHmac(HMAC_SHA384),
    kx: KeyExchangeAlgorithm::ECDHE,
    sign: RSA_SCHEMES,
    aead_alg: &Gcm12::<Aes256Gcm>(PhantomData),
});

// hashes

struct Hash<D>(PhantomData<fn() -> D>, hash::HashAlgorithm);

impl<D: Digest + Clone + Send + Sync + 'static> hash::Hash for Hash<D> {
    fn start(&self) -> Box<dyn hash::Context> {
        Box::new(HashContext(D::new()))
    }
    fn hash(&self, data: &[u8]) -> hash::Output {
        hash::Output::new(&D::digest(data))
    }
    fn output_len(&self) -> usize {
        <D as Digest>::output_size()
    }
    fn algorithm(&self) -> hash::HashAlgorithm {
        self.1
    }
}

struct HashContext<D>(D);

impl<D: Digest + Clone + Send + Sync + 'static> hash::Context for HashContext<D> {
    fn fork_finish(&self) -> hash::Output {
        hash::Output::new(&self.0.clone().finalize())
    }
    fn fork(&self) -> Box<dyn hash::Context> {
        Box::new(HashContext(self.0.clone()))
    }
    fn finish(self: Box<Self>) -> hash::Output {
        hash::Output::new(&self.0.finalize())
    }
    fn update(&mut self, data: &[u8]) {
        self.0.update(data);
    }
}

struct Hmac<D>(PhantomData<fn() -> D>);

impl<D: Digest + BlockSizeUser + Clone + Send + Sync + 'static> rhmac::Hmac for Hmac<D> {
    fn with_key(&self, key: &[u8]) -> Box<dyn rhmac::Key> {
        Box::new(HmacKey(<SimpleHmac<D> as KeyInit>::new_from_slice(key).expect("HMAC takes a key of any length")))
    }
    fn hash_output_len(&self) -> usize {
        <D as Digest>::output_size()
    }
}

struct HmacKey<D: Digest + BlockSizeUser>(SimpleHmac<D>);

impl<D: Digest + BlockSizeUser + Clone + Send + Sync + 'static> rhmac::Key for HmacKey<D> {
    fn sign_concat(&self, first: &[u8], middle: &[&[u8]], last: &[u8]) -> rhmac::Tag {
        let mut m = self.0.clone();
        m.update(first);
        for part in middle {
            m.update(part);
        }
        m.update(last);
        rhmac::Tag::new(&m.finalize().into_bytes())
    }
    fn tag_len(&self) -> usize {
        <D as Digest>::output_size()
    }
}

// records

const TAG: usize = 16;

/// AES-GCM as TLS uses it: 12-byte nonces, 16-byte tags.
trait Gcm: AeadInPlace<TagSize = U16, NonceSize = U12> + KeyInit + Send + Sync + 'static {}
impl<T: AeadInPlace<TagSize = U16, NonceSize = U12> + KeyInit + Send + Sync + 'static> Gcm for T {}
const EXPLICIT_NONCE: usize = 8;

fn tag16(tag: &[u8]) -> Result<Tag, Error> {
    <[u8; TAG]>::try_from(tag).map(Tag::from).map_err(|_| Error::DecryptError)
}

fn cipher<A: KeyInit>(key: &AeadKey) -> A {
    A::new_from_slice(key.as_ref()).expect("rustls sizes the key by key_len")
}

struct Gcm13<A>(PhantomData<fn() -> A>);

impl<A: Gcm> Tls13AeadAlgorithm for Gcm13<A> {
    fn encrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageEncrypter> {
        Box::new(Record13(cipher::<A>(&key), iv))
    }
    fn decrypter(&self, key: AeadKey, iv: Iv) -> Box<dyn MessageDecrypter> {
        Box::new(Record13(cipher::<A>(&key), iv))
    }
    fn key_len(&self) -> usize {
        A::key_size()
    }
    fn extract_keys(&self, _: AeadKey, _: Iv) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Err(UnsupportedOperationError)
    }
}

struct Record13<A>(A, Iv);

impl<A: Gcm> MessageEncrypter for Record13<A> {
    fn encrypt(&mut self, m: OutboundPlainMessage<'_>, seq: u64) -> Result<OutboundOpaqueMessage, Error> {
        let total = self.encrypted_payload_len(m.payload.len());
        let mut payload = PrefixedPayload::with_capacity(total);
        payload.extend_from_chunks(&m.payload);
        payload.extend_from_slice(&m.typ.to_array());
        let nonce = Nonce::new(&self.1, seq).0;
        let tag = self.0.encrypt_in_place_detached(&nonce.into(), &make_tls13_aad(total), payload.as_mut()).map_err(|_| Error::EncryptError)?;
        payload.extend_from_slice(&tag);
        Ok(OutboundOpaqueMessage::new(ContentType::ApplicationData, ProtocolVersion::TLSv1_2, payload))
    }
    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + 1 + TAG
    }
}

impl<A: Gcm> MessageDecrypter for Record13<A> {
    fn decrypt<'a>(&mut self, mut m: InboundOpaqueMessage<'a>, seq: u64) -> Result<InboundPlainMessage<'a>, Error> {
        let payload = &mut m.payload;
        let Some(text) = payload.len().checked_sub(TAG) else { return Err(Error::DecryptError) };
        let aad = make_tls13_aad(payload.len());
        let nonce = Nonce::new(&self.1, seq).0;
        let (msg, tag) = payload.split_at_mut(text);
        self.0.decrypt_in_place_detached(&nonce.into(), &aad, msg, &tag16(tag)?).map_err(|_| Error::DecryptError)?;
        payload.truncate(text);
        m.into_tls13_unpadded_message()
    }
}

struct Gcm12<A>(PhantomData<fn() -> A>);

impl<A: Gcm> Tls12AeadAlgorithm for Gcm12<A> {
    fn encrypter(&self, key: AeadKey, iv: &[u8], extra: &[u8]) -> Box<dyn MessageEncrypter> {
        let mut full = [0u8; 12];
        full[..4].copy_from_slice(iv);
        full[4..].copy_from_slice(extra);
        Box::new(Encrypt12(cipher::<A>(&key), Iv::new(full)))
    }
    fn decrypter(&self, key: AeadKey, iv: &[u8]) -> Box<dyn MessageDecrypter> {
        let mut salt = [0u8; 4];
        salt.copy_from_slice(iv);
        Box::new(Decrypt12(cipher::<A>(&key), salt))
    }
    fn key_block_shape(&self) -> KeyBlockShape {
        KeyBlockShape { enc_key_len: A::key_size(), fixed_iv_len: 4, explicit_nonce_len: EXPLICIT_NONCE }
    }
    fn extract_keys(&self, _: AeadKey, _: &[u8], _: &[u8]) -> Result<ConnectionTrafficSecrets, UnsupportedOperationError> {
        Err(UnsupportedOperationError)
    }
}

struct Encrypt12<A>(A, Iv);

impl<A: Gcm> MessageEncrypter for Encrypt12<A> {
    fn encrypt(&mut self, m: OutboundPlainMessage<'_>, seq: u64) -> Result<OutboundOpaqueMessage, Error> {
        let nonce = Nonce::new(&self.1, seq).0;
        let aad = make_tls12_aad(seq, m.typ, m.version, m.payload.len());
        let mut payload = PrefixedPayload::with_capacity(self.encrypted_payload_len(m.payload.len()));
        payload.extend_from_slice(&nonce[4..]);
        payload.extend_from_chunks(&m.payload);
        let tag = self.0.encrypt_in_place_detached(&nonce.into(), &aad, &mut payload.as_mut()[EXPLICIT_NONCE..]).map_err(|_| Error::EncryptError)?;
        payload.extend_from_slice(&tag);
        Ok(OutboundOpaqueMessage::new(m.typ, m.version, payload))
    }
    fn encrypted_payload_len(&self, payload_len: usize) -> usize {
        payload_len + EXPLICIT_NONCE + TAG
    }
}

struct Decrypt12<A>(A, [u8; 4]);

impl<A: Gcm> MessageDecrypter for Decrypt12<A> {
    fn decrypt<'a>(&mut self, mut m: InboundOpaqueMessage<'a>, seq: u64) -> Result<InboundPlainMessage<'a>, Error> {
        let (typ, version) = (m.typ, m.version);
        let payload = &mut m.payload;
        let Some(text) = payload.len().checked_sub(EXPLICIT_NONCE + TAG) else { return Err(Error::DecryptError) };
        let mut nonce = [0u8; 12];
        nonce[..4].copy_from_slice(&self.1);
        nonce[4..].copy_from_slice(&payload[..EXPLICIT_NONCE]);
        let aad = make_tls12_aad(seq, typ, version, text);
        let (msg, tag) = payload[EXPLICIT_NONCE..].split_at_mut(text);
        self.0.decrypt_in_place_detached(&nonce.into(), &aad, msg, &tag16(tag)?).map_err(|_| Error::DecryptError)?;
        payload.copy_within(EXPLICIT_NONCE..EXPLICIT_NONCE + text, 0);
        payload.truncate(text);
        Ok(m.into_plain_message())
    }
}

// key exchange

#[derive(Debug)]
struct X25519Group;
const X25519: &X25519Group = &X25519Group;

impl SupportedKxGroup for X25519Group {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        let secret = x25519_dalek::EphemeralSecret::random_from_rng(OsRng);
        let public = x25519_dalek::PublicKey::from(&secret).to_bytes();
        Ok(Box::new(X25519Exchange { secret, public }))
    }
    fn name(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

struct X25519Exchange {
    secret: x25519_dalek::EphemeralSecret,
    public: [u8; 32],
}

impl ActiveKeyExchange for X25519Exchange {
    fn complete(self: Box<Self>, peer: &[u8]) -> Result<SharedSecret, Error> {
        let peer: [u8; 32] = peer.try_into().map_err(|_| Error::from(rustls::PeerMisbehaved::InvalidKeyShare))?;
        let shared = self.secret.diffie_hellman(&x25519_dalek::PublicKey::from(peer));
        if !shared.was_contributory() {
            return Err(rustls::PeerMisbehaved::InvalidKeyShare.into());
        }
        Ok(SharedSecret::from(&shared.as_bytes()[..]))
    }
    fn pub_key(&self) -> &[u8] {
        &self.public
    }
    fn group(&self) -> NamedGroup {
        NamedGroup::X25519
    }
}

#[derive(Debug)]
struct P256Group;
const P256: &P256Group = &P256Group;

impl SupportedKxGroup for P256Group {
    fn start(&self) -> Result<Box<dyn ActiveKeyExchange>, Error> {
        use p256::elliptic_curve::sec1::ToEncodedPoint;
        let secret = p256::ecdh::EphemeralSecret::random(&mut OsRng);
        let public = secret.public_key().to_encoded_point(false).as_bytes().to_vec();
        Ok(Box::new(P256Exchange { secret, public }))
    }
    fn name(&self) -> NamedGroup {
        NamedGroup::secp256r1
    }
}

struct P256Exchange {
    secret: p256::ecdh::EphemeralSecret,
    public: Vec<u8>,
}

impl ActiveKeyExchange for P256Exchange {
    fn complete(self: Box<Self>, peer: &[u8]) -> Result<SharedSecret, Error> {
        // TLS sends uncompressed points only.
        if peer.len() != 65 || peer[0] != 4 {
            return Err(rustls::PeerMisbehaved::InvalidKeyShare.into());
        }
        let peer = p256::PublicKey::from_sec1_bytes(peer).map_err(|_| Error::from(rustls::PeerMisbehaved::InvalidKeyShare))?;
        Ok(SharedSecret::from(&self.secret.diffie_hellman(&peer).raw_secret_bytes()[..]))
    }
    fn pub_key(&self) -> &[u8] {
        &self.public
    }
    fn group(&self) -> NamedGroup {
        NamedGroup::secp256r1
    }
}

// randomness and the host key

#[derive(Debug)]
struct Random;

impl SecureRandom for Random {
    fn fill(&self, buf: &mut [u8]) -> Result<(), GetRandomFailed> {
        getrandom::getrandom(buf).map_err(|_| GetRandomFailed)
    }
}

impl KeyProvider for Random {
    fn load_private_key(&self, der: PrivateKeyDer<'static>) -> Result<Arc<dyn SigningKey>, Error> {
        let key = match &der {
            PrivateKeyDer::Pkcs1(k) => RsaPrivateKey::from_pkcs1_der(k.secret_pkcs1_der()).ok(),
            PrivateKeyDer::Pkcs8(k) => RsaPrivateKey::from_pkcs8_der(k.secret_pkcs8_der()).ok(),
            _ => None,
        };
        key.map(|k| Arc::new(RsaKey(Arc::new(k))) as Arc<dyn SigningKey>).ok_or_else(|| Error::General("the pair record's host key is not an RSA key".into()))
    }
}

struct RsaKey(Arc<RsaPrivateKey>);

impl std::fmt::Debug for RsaKey {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str("RsaKey")
    }
}

impl SigningKey for RsaKey {
    fn choose_scheme(&self, offered: &[SignatureScheme]) -> Option<Box<dyn Signer>> {
        RSA_SCHEMES.iter().find(|s| offered.contains(s)).map(|&s| Box::new(RsaSigner(self.0.clone(), s)) as Box<dyn Signer>)
    }
    fn algorithm(&self) -> SignatureAlgorithm {
        SignatureAlgorithm::RSA
    }
}

struct RsaSigner(Arc<RsaPrivateKey>, SignatureScheme);

impl std::fmt::Debug for RsaSigner {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "RsaSigner({:?})", self.1)
    }
}

impl Signer for RsaSigner {
    fn sign(&self, message: &[u8]) -> Result<Vec<u8>, Error> {
        use rsa::{pkcs1v15, pss};
        let k = (*self.0).clone();
        let sig = match self.1 {
            SignatureScheme::RSA_PSS_SHA512 => pss::BlindedSigningKey::<Sha512>::new(k).try_sign_with_rng(&mut OsRng, message).map(|s| s.to_vec()),
            SignatureScheme::RSA_PSS_SHA384 => pss::BlindedSigningKey::<Sha384>::new(k).try_sign_with_rng(&mut OsRng, message).map(|s| s.to_vec()),
            SignatureScheme::RSA_PSS_SHA256 => pss::BlindedSigningKey::<Sha256>::new(k).try_sign_with_rng(&mut OsRng, message).map(|s| s.to_vec()),
            SignatureScheme::RSA_PKCS1_SHA512 => pkcs1v15::SigningKey::<Sha512>::new(k).try_sign_with_rng(&mut OsRng, message).map(|s| s.to_vec()),
            SignatureScheme::RSA_PKCS1_SHA384 => pkcs1v15::SigningKey::<Sha384>::new(k).try_sign_with_rng(&mut OsRng, message).map(|s| s.to_vec()),
            _ => pkcs1v15::SigningKey::<Sha256>::new(k).try_sign_with_rng(&mut OsRng, message).map(|s| s.to_vec()),
        };
        sig.map_err(|e| Error::General(format!("signing with the host key: {e}")))
    }
    fn scheme(&self) -> SignatureScheme {
        self.1
    }
}

// the device's handshake signature

/// The schemes `verify` checks.
pub const VERIFY_SCHEMES: &[SignatureScheme] = RSA_SCHEMES;

/// Whether `signature` over `message` is the device's under `scheme`, given the RSA public key (the
/// certificate's subjectPublicKey bits, PKCS#1).
pub fn verify(scheme: SignatureScheme, key: &[u8], message: &[u8], signature: &[u8]) -> bool {
    use rsa::{pkcs1v15, pss};
    let Ok(key) = RsaPublicKey::from_pkcs1_der(key) else { return false };
    match scheme {
        SignatureScheme::RSA_PSS_SHA512 => pss::Signature::try_from(signature).is_ok_and(|s| pss::VerifyingKey::<Sha512>::new(key).verify(message, &s).is_ok()),
        SignatureScheme::RSA_PSS_SHA384 => pss::Signature::try_from(signature).is_ok_and(|s| pss::VerifyingKey::<Sha384>::new(key).verify(message, &s).is_ok()),
        SignatureScheme::RSA_PSS_SHA256 => pss::Signature::try_from(signature).is_ok_and(|s| pss::VerifyingKey::<Sha256>::new(key).verify(message, &s).is_ok()),
        SignatureScheme::RSA_PKCS1_SHA512 => pkcs1v15::Signature::try_from(signature).is_ok_and(|s| pkcs1v15::VerifyingKey::<Sha512>::new(key).verify(message, &s).is_ok()),
        SignatureScheme::RSA_PKCS1_SHA384 => pkcs1v15::Signature::try_from(signature).is_ok_and(|s| pkcs1v15::VerifyingKey::<Sha384>::new(key).verify(message, &s).is_ok()),
        SignatureScheme::RSA_PKCS1_SHA256 => pkcs1v15::Signature::try_from(signature).is_ok_and(|s| pkcs1v15::VerifyingKey::<Sha256>::new(key).verify(message, &s).is_ok()),
        _ => false,
    }
}
