use crate::prelude::*;
use crate::{
    aead::{Aead, AeadTag, AesGcm128, AesGcm256, ChaCha20Poly1305},
    kdf::{HkdfSha256, HkdfSha384, HkdfSha512, Kdf as KdfTrait},
    kem::{encap_with_eph, Kem as KemTrait, X25519HkdfSha256},
    kex::{KeyExchange, Marshallable, Unmarshallable},
    op_mode::{OpModeR, Psk, PskBundle},
    setup::setup_receiver,
};

use std::fs::File;

use hex;
use serde::{de::Error as SError, Deserialize, Deserializer};
use serde_json;

// Tells serde how to deserialize bytes from the hex representation
fn bytes_from_hex<'de, D>(deserializer: D) -> Result<Vec<u8>, D::Error>
where
    D: Deserializer<'de>,
{
    let mut hex_str = String::deserialize(deserializer)?;
    // Prepend a 0 if it's not even length
    if hex_str.len() % 2 == 1 {
        hex_str.insert(0, '0');
    }
    hex::decode(hex_str).map_err(|e| SError::custom(format!("{:?}", e)))
}

// Tells serde how to deserialize bytes from an optional field with hex encoding
fn bytes_from_hex_opt<'de, D>(deserializer: D) -> Result<Option<Vec<u8>>, D::Error>
where
    D: Deserializer<'de>,
{
    bytes_from_hex(deserializer).map(|v| Some(v))
}

// Each individual test case looks like this
#[derive(Deserialize)]
struct MainTestVector {
    // Parameters
    mode: u8,
    #[serde(rename = "kemID")]
    kem_id: u16,
    #[serde(rename = "kdfID")]
    kdf_id: u16,
    #[serde(rename = "aeadID")]
    aead_id: u16,
    #[serde(deserialize_with = "bytes_from_hex")]
    info: Vec<u8>,

    // Private keys
    #[serde(rename = "skR", deserialize_with = "bytes_from_hex")]
    sk_recip: Vec<u8>,
    #[serde(default, rename = "skS", deserialize_with = "bytes_from_hex_opt")]
    sk_sender: Option<Vec<u8>>,
    #[serde(rename = "skE", deserialize_with = "bytes_from_hex")]
    sk_eph: Vec<u8>,
    #[serde(default, deserialize_with = "bytes_from_hex_opt")]
    psk: Option<Vec<u8>>,
    #[serde(default, rename = "pskID", deserialize_with = "bytes_from_hex_opt")]
    psk_id: Option<Vec<u8>>,

    // Public Keys
    #[serde(rename = "pkR", deserialize_with = "bytes_from_hex")]
    pk_recip: Vec<u8>,
    #[serde(default, rename = "pkS", deserialize_with = "bytes_from_hex_opt")]
    pk_sender: Option<Vec<u8>>,
    #[serde(rename = "pkE", deserialize_with = "bytes_from_hex")]
    pk_eph: Vec<u8>,

    // Key schedule inputs and computations
    #[serde(rename = "enc", deserialize_with = "bytes_from_hex")]
    encapped_key: Vec<u8>,
    #[serde(rename = "zz", deserialize_with = "bytes_from_hex")]
    _shared_secret: Vec<u8>,
    #[serde(rename = "context", deserialize_with = "bytes_from_hex")]
    _hpke_context: Vec<u8>,
    #[serde(rename = "secret", deserialize_with = "bytes_from_hex")]
    _key_schedule_secret: Vec<u8>,
    #[serde(rename = "key", deserialize_with = "bytes_from_hex")]
    _aead_key: Vec<u8>,
    #[serde(rename = "nonce", deserialize_with = "bytes_from_hex")]
    _aead_nonce: Vec<u8>,
    #[serde(rename = "exporterSecret", deserialize_with = "bytes_from_hex")]
    _exporter_secret: Vec<u8>,

    encryptions: Vec<EncryptionTestVector>,
    exports: Vec<ExporterTestVector>,
}

#[derive(Deserialize)]
struct EncryptionTestVector {
    #[serde(deserialize_with = "bytes_from_hex")]
    plaintext: Vec<u8>,
    #[serde(deserialize_with = "bytes_from_hex")]
    aad: Vec<u8>,
    #[serde(rename = "nonce", deserialize_with = "bytes_from_hex")]
    _nonce: Vec<u8>,
    #[serde(deserialize_with = "bytes_from_hex")]
    ciphertext: Vec<u8>,
}

#[derive(Deserialize)]
struct ExporterTestVector {
    #[serde(rename = "context", deserialize_with = "bytes_from_hex")]
    info: Vec<u8>,
    #[serde(rename = "exportLength")]
    export_len: usize,
    #[serde(rename = "exportValue", deserialize_with = "bytes_from_hex")]
    export_val: Vec<u8>,
}

/// Returns a KEX keypair given the secret bytes and pubkey bytes, and ensures that the pubkey does
/// indeed correspond to that secret key
fn get_and_assert_keypair<Kex: KeyExchange>(
    sk_bytes: &[u8],
    pk_bytes: &[u8],
) -> (Kex::PrivateKey, Kex::PublicKey) {
    // Unmarshall the secret key
    let sk = <Kex as KeyExchange>::PrivateKey::unmarshal(sk_bytes).unwrap();
    // Unmarshall the pubkey
    let pk = <Kex as KeyExchange>::PublicKey::unmarshal(pk_bytes).unwrap();

    // Make sure the derived pubkey matches the given pubkey
    assert_eq!(pk.marshal(), Kex::sk_to_pk(&sk).marshal());

    (sk, pk)
}

/// Constructs an `OpModeR` from the given components. The variant constructed is determined solely
/// by `mode_id`. This will panic if there is insufficient data to construct the variants specified
/// by `mode_id`.
fn make_op_mode_r<Kex: KeyExchange, Kdf: KdfTrait>(
    mode_id: u8,
    pk_sender_bytes: Option<Vec<u8>>,
    psk: Option<Vec<u8>>,
    psk_id: Option<Vec<u8>>,
) -> OpModeR<Kex, Kdf> {
    // Unmarshal the optional pubkey
    let pk =
        pk_sender_bytes.map(|bytes| <Kex as KeyExchange>::PublicKey::unmarshal(&bytes).unwrap());
    // Unmarshal the optinoal bundle
    let bundle = psk.map(|bytes| PskBundle::<Kdf> {
        psk: Psk::<Kdf>::from_bytes(bytes),
        psk_id: psk_id.unwrap(),
    });

    // These better be set if the mode ID calls for them
    match mode_id {
        0 => OpModeR::Base,
        1 => OpModeR::Psk(bundle.unwrap()),
        2 => OpModeR::Auth(pk.unwrap()),
        3 => OpModeR::AuthPsk(pk.unwrap(), bundle.unwrap()),
        _ => panic!("Invalid mode ID: {}", mode_id),
    }
}

// Implements a test case for a given AEAD implementation
macro_rules! test_case {
    ($tv:ident, $aead_ty:ty, $kdf_ty:ty) => {{
        type A = $aead_ty;
        type Kdf = $kdf_ty;
        type Kem = X25519HkdfSha256;
        type Kex = <X25519HkdfSha256 as KemTrait>::Kex;

        // First, unmarshall all the relevant keys so we can reconstruct the encapped key
        let (sk_recip, pk_recip) = get_and_assert_keypair::<Kex>(&$tv.sk_recip, &$tv.pk_recip);
        let (sk_eph, _) = get_and_assert_keypair::<Kex>(&$tv.sk_eph, &$tv.pk_eph);

        let sk_sender = $tv
            .sk_sender
            .map(|bytes| <Kex as KeyExchange>::PrivateKey::unmarshal(&bytes).unwrap());
        let pk_sender = $tv
            .pk_sender
            .clone()
            .map(|bytes| <Kex as KeyExchange>::PublicKey::unmarshal(&bytes).unwrap());
        // If sk_sender is Some, then so is pk_sender
        let sender_keypair = sk_sender.map(|sk| (sk, pk_sender.unwrap()));

        // Now derive the encapped key with the deterministic encap function, using all the inputs
        // above
        let (_, encapped_key) =
            encap_with_eph::<Kem>(&pk_recip, sender_keypair.as_ref(), sk_eph.clone())
                .expect("encap failed");
        // Now assert that the derived encapped key is identical to the one provided
        assert_eq!(
            encapped_key.marshal().as_slice(),
            $tv.encapped_key.as_slice()
        );

        // We're going to test the encryption contexts. First, construct the appropriate OpMode.
        let mode = make_op_mode_r($tv.mode, $tv.pk_sender, $tv.psk, $tv.psk_id);
        let mut aead_ctx =
            setup_receiver::<A, Kdf, Kem>(&mode, &sk_recip, &encapped_key, &$tv.info)
                .expect("setup_receiver failed");

        // Go through all the plaintext-ciphertext pairs of this test vector and assert the
        // ciphertext decrypts to the corresponding plaintext
        for enc_packet in $tv.encryptions {
            let aad = enc_packet.aad;

            // The test vector's ciphertext is of the form ciphertext || tag. Break it up into two
            // pieces so we can call open() on it.
            let (mut ciphertext, tag) = {
                let mut ciphertext_and_tag = enc_packet.ciphertext;
                let total_len = ciphertext_and_tag.len();

                let tag_size = AeadTag::<A>::size();
                let (ciphertext_bytes, tag_bytes) =
                    ciphertext_and_tag.split_at_mut(total_len - tag_size);

                (
                    ciphertext_bytes.to_vec(),
                    AeadTag::unmarshal(tag_bytes).unwrap(),
                )
            };

            // Open the ciphertext in place and assert that this succeeds
            aead_ctx
                .open(&mut ciphertext, &aad, &tag)
                .expect("open failed");
            // Rename for clarity
            let plaintext = ciphertext;

            // Assert the plaintext equals the expected plaintext
            assert_eq!(plaintext, enc_packet.plaintext.as_slice());
        }

        // Now check that AeadCtx::export returns the expected values
        for export in $tv.exports {
            let mut exported_val = vec![0u8; export.export_len];
            aead_ctx.export(&export.info, &mut exported_val).unwrap();
            assert_eq!(exported_val, export.export_val);
        }
    }};
}

#[test]
fn kat_test() {
    let file = File::open("test-vectors-d1dbba6.json").unwrap();
    let tvs: Vec<MainTestVector> = serde_json::from_reader(file).unwrap();

    for tv in tvs.into_iter() {
        // Ignore everything that doesn't use X25519, since that's all we support right now
        if tv.kem_id != X25519HkdfSha256::KEM_ID {
            continue;
        }

        match (tv.aead_id, tv.kdf_id) {
            (AesGcm128::AEAD_ID, HkdfSha256::KDF_ID) => test_case!(tv, AesGcm128, HkdfSha256),
            (AesGcm128::AEAD_ID, HkdfSha384::KDF_ID) => test_case!(tv, AesGcm128, HkdfSha384),
            (AesGcm128::AEAD_ID, HkdfSha512::KDF_ID) => test_case!(tv, AesGcm128, HkdfSha512),
            (AesGcm256::AEAD_ID, HkdfSha256::KDF_ID) => test_case!(tv, AesGcm256, HkdfSha256),
            (AesGcm256::AEAD_ID, HkdfSha384::KDF_ID) => test_case!(tv, AesGcm256, HkdfSha384),
            (AesGcm256::AEAD_ID, HkdfSha512::KDF_ID) => test_case!(tv, AesGcm256, HkdfSha512),
            (ChaCha20Poly1305::AEAD_ID, HkdfSha256::KDF_ID) => {
                test_case!(tv, ChaCha20Poly1305, HkdfSha256)
            }
            (ChaCha20Poly1305::AEAD_ID, HkdfSha384::KDF_ID) => {
                test_case!(tv, ChaCha20Poly1305, HkdfSha384)
            }
            (ChaCha20Poly1305::AEAD_ID, HkdfSha512::KDF_ID) => {
                test_case!(tv, ChaCha20Poly1305, HkdfSha512)
            }
            _ => panic!(
                "Invalid (AEAD ID, KDF ID) combo: ({}, {})",
                tv.aead_id, tv.kdf_id
            ),
        };
    }
}
