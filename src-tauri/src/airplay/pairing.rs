//! The sender's half of `pair-verify`: an X25519 exchange, then two signatures.
//!
//! Nothing is looked up in a pairing database. The sender publishes an ephemeral
//! ed25519 identity inside M1 and proves it owns the matching private key in M2,
//! which is why an unpaired Windows machine can mirror without ever showing a
//! code.
//!
//! ```text
//! M1  [1,0,0,0] ‖ x_public ‖ ed_public              → 96 bytes
//!     reply = x_theirs ‖ AES-CTR(signature)
//! M2  [0,0,0,0] ‖ AES-CTR(our signature)             → 200
//! ```
//!
//! Both signatures cover the two X25519 public keys in an order that depends on
//! who is speaking: the receiver signs `its key ‖ our key`, we sign
//! `our key ‖ its key`. Getting that backwards is a signature failure with no
//! other symptom, so the tests below pin both directions.
//!
//! The receiver's signature is checked against the ed25519 identity it handed
//! over in `/pair-setup` — the same key its mDNS advert publishes as `pk`. The
//! X25519 key in the reply is ephemeral and proves nothing on its own.
//!
//! The X25519 shared secret outlives the handshake: it is half of the mirroring
//! stream key derivation in [`crate::airplay::mirror::stream_keys`].

use anyhow::{bail, Context, Result};
use sha2::{Digest, Sha512};

use crate::airplay::aes::AesCtr;
use crate::airplay::nacl;

const BASE_POINT: [u8; 32] = {
    let mut point = [0u8; 32];
    point[0] = 9;
    point
};

/// The 4-byte flag followed by a 64-byte payload in both messages.
const M1_LENGTH: usize = 68;
const M2_LENGTH: usize = 68;
/// A reply to M1 is the receiver's X25519 key and its 64-byte signature.
const M1_REPLY_LENGTH: usize = 96;

pub struct PairVerify {
    x_secret: [u8; 32],
    x_public: [u8; 32],
    ed_seed: [u8; 32],
    ed_public: [u8; 32],
    /// Filled in by [`PairVerify::finish`].
    shared: Option<[u8; 32]>,
}

impl PairVerify {
    /// Fresh ephemeral keys for one session. A sender that reused its X25519
    /// secret across sessions would reuse the mirroring keystream too.
    pub fn start() -> Self {
        Self::with_secrets(rand::random(), rand::random())
    }

    fn with_secrets(x_secret: [u8; 32], ed_seed: [u8; 32]) -> Self {
        Self {
            x_public: nacl::x25519(&x_secret, &BASE_POINT),
            ed_public: nacl::ed25519_public_key(&ed_seed),
            x_secret,
            ed_seed,
            shared: None,
        }
    }

    /// The body of the first `POST /pair-verify`.
    pub fn challenge(&self) -> Vec<u8> {
        let body = [
            vec![1, 0, 0, 0],
            self.x_public.to_vec(),
            self.ed_public.to_vec(),
        ]
        .concat();
        debug_assert_eq!(body.len(), M1_LENGTH);
        body
    }

    /// The body of the second `POST /pair-verify`.
    ///
    /// `receiver_identity` is the ed25519 key from `/pair-setup`. A reply that
    /// does not verify against it is refused before a single frame is encrypted:
    /// accepting an unverified reply would hand a keystream to whoever answered.
    pub fn finish(&mut self, reply: &[u8], receiver_identity: &[u8; 32]) -> Result<Vec<u8>> {
        if reply.len() < M1_REPLY_LENGTH {
            bail!(
                "pair-verify reply is {} bytes, want at least {M1_REPLY_LENGTH}",
                reply.len()
            );
        }
        let mut their_public = [0u8; 32];
        their_public.copy_from_slice(&reply[..32]);
        self.shared = Some(nacl::x25519(&self.x_secret, &their_public));
        let shared = self.shared.expect("just set");

        let (key, iv) = verify_keys(&shared);
        let mut cipher = AesCtr::new(&key, &iv).context("pair-verify aes key rejected")?;
        let signature: [u8; 64] = cipher
            .process(&reply[32..M1_REPLY_LENGTH])
            .as_slice()
            .try_into()
            .map_err(|_| anyhow::anyhow!("pair-verify signature length"))?;

        let mut signed = [0u8; 64];
        signed[..32].copy_from_slice(&their_public);
        signed[32..].copy_from_slice(&self.x_public);
        if !nacl::ed25519_verify(receiver_identity, &signed, &signature) {
            bail!(
                "the receiver's pair-verify signature did not verify \
                 against the key it advertised"
            );
        }

        // Our signature: the same two keys in our order, and the keystream
        // continues — the receiver reads M2 with the cipher mid-stream.
        let mut ours = [0u8; 64];
        ours[..32].copy_from_slice(&self.x_public);
        ours[32..].copy_from_slice(&their_public);
        let body = [
            vec![0, 0, 0, 0],
            cipher.process(&nacl::ed25519_sign(&self.ed_seed, &ours)),
        ]
        .concat();
        debug_assert_eq!(body.len(), M2_LENGTH);
        Ok(body)
    }

    /// The X25519 shared secret, needed to derive the mirroring stream keys.
    pub fn shared_secret(&self) -> Result<[u8; 32]> {
        self.shared
            .context("pair-verify has not completed, so there is no shared secret")
    }
}

/// `Pair-Verify-AES-Key` and `Pair-Verify-AES-IV` are SHA-512 over the label and
/// the shared secret, truncated to the first 16 bytes.
fn verify_keys(shared: &[u8]) -> ([u8; 16], [u8; 16]) {
    (
        label_hash("Pair-Verify-AES-Key", shared),
        label_hash("Pair-Verify-AES-IV", shared),
    )
}

fn label_hash(label: &str, shared: &[u8]) -> [u8; 16] {
    let mut hasher = Sha512::new();
    hasher.update(label.as_bytes());
    hasher.update(shared);
    let digest = hasher.finalize();
    let mut out = [0u8; 16];
    out.copy_from_slice(&digest[..16]);
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A stand-in receiver: its own X25519 key for the exchange and its own
    /// ed25519 identity for the signature, derived the way the sender's are.
    struct Receiver {
        x_secret: [u8; 32],
        x_public: [u8; 32],
        ed_seed: [u8; 32],
        ed_public: [u8; 32],
    }

    impl Receiver {
        fn new(seed: u8) -> Self {
            let ed_seed = [seed.wrapping_add(0x12); 32];
            Self {
                x_secret: [seed; 32],
                x_public: nacl::x25519(&[seed; 32], &BASE_POINT),
                ed_public: nacl::ed25519_public_key(&ed_seed),
                ed_seed,
            }
        }

        /// The reply to M1, with the cipher left mid-stream exactly as a real
        /// receiver leaves it for M2.
        fn reply(&self, sender: &PairVerify) -> (Vec<u8>, AesCtr) {
            let shared = nacl::x25519(&self.x_secret, &sender.x_public);
            let (key, iv) = verify_keys(&shared);
            let mut cipher = AesCtr::new(&key, &iv).unwrap();
            let mut signed = [0u8; 64];
            signed[..32].copy_from_slice(&self.x_public);
            signed[32..].copy_from_slice(&sender.x_public);
            let signature = cipher.process(&nacl::ed25519_sign(&self.ed_seed, &signed));
            ([self.x_public.to_vec(), signature].concat(), cipher)
        }
    }

    /// Both halves of the exchange run against each other: the shared secrets
    /// agree, and M2 is byte for byte what a receiver expects to read.
    #[test]
    fn an_exchange_derives_one_shared_secret() {
        let sender = PairVerify::with_secrets([0x21u8; 32], [0x55u8; 32]);
        assert_eq!(sender.challenge().len(), M1_LENGTH);
        assert_eq!(&sender.challenge()[..4], &[1, 0, 0, 0]);

        let receiver = Receiver::new(0x77);
        let (reply, mut cipher) = receiver.reply(&sender);
        assert_eq!(reply.len(), M1_REPLY_LENGTH);

        // What M2 has to carry, computed the way the receiver will read it.
        let shared = nacl::x25519(&receiver.x_secret, &sender.x_public);
        let mut ours = [0u8; 64];
        ours[..32].copy_from_slice(&sender.x_public);
        ours[32..].copy_from_slice(&receiver.x_public);
        let plain = nacl::ed25519_sign(&sender.ed_seed, &ours);
        assert!(
            nacl::ed25519_verify(&sender.ed_public, &ours, plain.as_slice().try_into().unwrap()),
            "the signature must verify against the key published in M1"
        );
        let expected = cipher.process(&plain);

        let mut sender = sender;
        let m2 = sender.finish(&reply, &receiver.ed_public).unwrap();
        assert_eq!(m2.len(), M2_LENGTH);
        assert_eq!(&m2[..4], &[0, 0, 0, 0]);
        assert_eq!(m2[4..].to_vec(), expected);
        assert_eq!(sender.shared_secret().unwrap(), shared);
    }

    /// Signing the two keys in the sender's order instead of the receiver's is
    /// the one mistake that shows as a signature failure and nothing else.
    #[test]
    fn keys_signed_in_the_wrong_order_are_refused() {
        let sender = PairVerify::with_secrets([0x21u8; 32], [0x55u8; 32]);
        let receiver = Receiver::new(0x77);
        let shared = nacl::x25519(&receiver.x_secret, &sender.x_public);
        let (key, iv) = verify_keys(&shared);
        let mut cipher = AesCtr::new(&key, &iv).unwrap();

        let mut backwards = [0u8; 64];
        backwards[..32].copy_from_slice(&sender.x_public);
        backwards[32..].copy_from_slice(&receiver.x_public);
        let signature = cipher.process(&nacl::ed25519_sign(&receiver.ed_seed, &backwards));
        let reply = [receiver.x_public.to_vec(), signature].concat();

        let mut sender = sender;
        let error = sender
            .finish(&reply, &receiver.ed_public)
            .expect_err("keys in the wrong order must not verify");
        assert!(
            error.to_string().contains("signature"),
            "unexpected error: {error}"
        );
    }

    /// A reply signed by someone other than the receiver the advert described.
    #[test]
    fn a_reply_from_a_different_identity_is_refused() {
        let sender = PairVerify::with_secrets([0x21u8; 32], [0x55u8; 32]);
        let attacker = Receiver::new(0x99);
        let (reply, _) = attacker.reply(&sender);

        let honest = Receiver::new(0x77).ed_public;
        let mut sender = sender;
        let error = sender
            .finish(&reply, &honest)
            .expect_err("an imposter's signature must not pass");
        assert!(error.to_string().contains("advertised"), "{error}");
    }

    #[test]
    fn a_short_reply_is_refused_and_leaves_no_shared_secret() {
        let mut sender = PairVerify::with_secrets([0x21u8; 32], [0x55u8; 32]);
        assert!(sender.finish(&[0u8; 40], &[0x33u8; 32]).is_err());
        assert!(sender.shared_secret().is_err());
    }

    #[test]
    fn the_key_derivation_is_the_label_and_the_shared_secret() {
        let shared = [0x5au8; 32];
        let (key, iv) = verify_keys(&shared);
        let mut expected = Sha512::new();
        expected.update(b"Pair-Verify-AES-Key");
        expected.update(shared);
        assert_eq!(key.to_vec(), expected.finalize()[..16].to_vec());
        assert_ne!(key, iv, "the two labels must give two different halves");
    }

    /// `start()` is the path a real session takes, so what it publishes has to be
    /// the public halves of the secrets it keeps — and different every session.
    #[test]
    fn a_fresh_sender_publishes_its_own_public_keys() {
        let sender = PairVerify::start();
        let challenge = sender.challenge();
        assert_eq!(&challenge[4..36], sender.x_public);
        assert_eq!(&challenge[36..68], sender.ed_public);
        assert_eq!(nacl::x25519(&sender.x_secret, &BASE_POINT), sender.x_public);
        assert_ne!(PairVerify::start().x_public, sender.x_public);
    }
}
