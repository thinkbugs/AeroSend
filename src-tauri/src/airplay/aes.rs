//! AES-128 for the AirPlay keystreams.
//!
//! Only the encryption half of the block cipher is implemented: every use here
//! is a sender-side stream, where the receiver is the one that decrypts. The
//! S-box is computed from the field rather than copied from a table, and the
//! whole module is pinned to the FIPS-197 and SP 800-38A vectors in the tests
//! below.
//!
//! [`AesCtr`] keeps its counter and half-used keystream between calls. AirPlay
//! needs that in two places: pair-verify encrypts two signatures through one
//! cipher so the second must continue where the first stopped, and the mirroring
//! stream is one keystream across every frame on the connection.
//!
//! [`AesCbc`] is the other mode AirPlay uses, and it does not chain across
//! packets: mirrored audio restarts from the SETUP IV for every packet.

use anyhow::{bail, Result};
use std::sync::OnceLock;

const BLOCK: usize = 16;
const ROUNDS: usize = 10;
/// Counter blocks produced per keystream fill. A 4K key frame can exceed 1 MiB,
/// and one fill per frame keeps the cipher out of the hot loop.
const KEYSTREAM_BLOCKS: usize = 256;
const KEYSTREAM_BYTES: usize = KEYSTREAM_BLOCKS * BLOCK;

/// The AES S-box: GF(2^8) inverse followed by the affine map, with 0 → 0.
fn sbox() -> &'static [u8; 256] {
    static SBOX: OnceLock<[u8; 256]> = OnceLock::new();
    SBOX.get_or_init(build_sbox)
}

fn multiply(mut a: u8, mut b: u8) -> u8 {
    let mut product = 0u8;
    for _ in 0..8 {
        if b & 1 != 0 {
            product ^= a;
        }
        let overflow = a & 0x80;
        a <<= 1;
        if overflow != 0 {
            a ^= 0x1b;
        }
        b >>= 1;
    }
    product
}

/// x⁻¹ = x²⁵⁴ in GF(2^8); zero has no inverse and maps to itself here, which is
/// exactly what the S-box wants.
fn inverse(value: u8) -> u8 {
    if value == 0 {
        return 0;
    }
    let mut result = 1u8;
    let mut base = value;
    let mut exponent = 254u8;
    while exponent > 0 {
        if exponent & 1 != 0 {
            result = multiply(result, base);
        }
        base = multiply(base, base);
        exponent >>= 1;
    }
    result
}

fn build_sbox() -> [u8; 256] {
    let mut table = [0u8; 256];
    for (index, entry) in table.iter_mut().enumerate() {
        let inverse = inverse(index as u8);
        let affine = inverse
            ^ inverse.rotate_left(1)
            ^ inverse.rotate_left(2)
            ^ inverse.rotate_left(3)
            ^ inverse.rotate_left(4)
            ^ 0x63;
        *entry = affine;
    }
    table
}

fn substitute(byte: u8) -> u8 {
    sbox()[byte as usize]
}

/// AES-128 with the round keys already expanded.
pub struct Aes128 {
    rounds: [[u8; BLOCK]; ROUNDS + 1],
}

impl Aes128 {
    pub fn new(key: &[u8]) -> Result<Self> {
        if key.len() != BLOCK {
            bail!("aes-128 key is {} bytes, want 16", key.len());
        }
        // Round keys are the expanded key read four bytes at a time; the last
        // word of each 16-byte chunk is the one the next chunk derives from.
        let mut words = [[0u8; 4]; 4 * (ROUNDS + 1)];
        for (index, chunk) in key.chunks_exact(4).enumerate() {
            words[index].copy_from_slice(chunk);
        }
        // rcon starts at x^0 = 1 and doubles in GF(2^8), i.e. 1,2,4,…,0x1b.
        let mut rcon = 1u8;
        for index in 4..words.len() {
            let mut word = words[index - 1];
            if index % 4 == 0 {
                word = [
                    substitute(word[1]) ^ rcon,
                    substitute(word[2]),
                    substitute(word[3]),
                    substitute(word[0]),
                ];
                rcon = multiply(rcon, 2);
            }
            words[index] = {
                let previous = words[index - 4];
                std::array::from_fn(|byte| word[byte] ^ previous[byte])
            };
        }
        Ok(Self {
            rounds: std::array::from_fn(|round| {
                std::array::from_fn(|byte| words[round * 4 + byte / 4][byte % 4])
            }),
        })
    }

    /// Encrypts one block in place. The state is column-major, the way the
    /// specification lays it out: `state[row + 4 * column]`.
    fn encrypt_block(&self, block: &mut [u8; BLOCK]) {
        add_round_key(block, &self.rounds[0]);
        for round in 1..ROUNDS {
            sub_bytes(block);
            shift_rows(block);
            mix_columns(block);
            add_round_key(block, &self.rounds[round]);
        }
        sub_bytes(block);
        shift_rows(block);
        add_round_key(block, &self.rounds[ROUNDS]);
    }
}

fn add_round_key(state: &mut [u8; BLOCK], round_key: &[u8; BLOCK]) {
    for (byte, key) in state.iter_mut().zip(round_key) {
        *byte ^= key;
    }
}

fn sub_bytes(state: &mut [u8; BLOCK]) {
    for byte in state.iter_mut() {
        *byte = substitute(*byte);
    }
}

fn shift_rows(state: &mut [u8; BLOCK]) {
    let mut shifted = *state;
    for row in 1..4 {
        for column in 0..4 {
            shifted[row + 4 * column] = state[row + 4 * ((column + row) % 4)];
        }
    }
    *state = shifted;
}

fn mix_columns(state: &mut [u8; BLOCK]) {
    for column in 0..4 {
        let at = column * 4;
        let (a, b, c, d) = (
            state[at],
            state[at + 1],
            state[at + 2],
            state[at + 3],
        );
        // Each output byte is the sum of the column multiplied by {2,3,1,1}.
        let double = |value: u8| -> u8 {
            let shifted = value << 1;
            if value & 0x80 != 0 {
                shifted ^ 0x1b
            } else {
                shifted
            }
        };
        state[at] = double(a) ^ (double(b) ^ b) ^ c ^ d;
        state[at + 1] = a ^ double(b) ^ (double(c) ^ c) ^ d;
        state[at + 2] = a ^ b ^ double(c) ^ (double(d) ^ d);
        state[at + 3] = (double(a) ^ a) ^ b ^ c ^ double(d);
    }
}

/// One AES-128-CTR stream, continuous across calls.
pub struct AesCtr {
    cipher: Aes128,
    counter: [u8; BLOCK],
    keystream: [u8; KEYSTREAM_BYTES],
    /// Next unused keystream byte; `KEYSTREAM_BYTES` means exhausted.
    offset: usize,
}

impl AesCtr {
    pub fn new(key: &[u8], iv: &[u8]) -> Result<Self> {
        if iv.len() != BLOCK {
            bail!("aes-ctr iv is {} bytes, want 16", iv.len());
        }
        let mut counter = [0u8; BLOCK];
        counter.copy_from_slice(iv);
        Ok(Self {
            cipher: Aes128::new(key)?,
            counter,
            keystream: [0u8; KEYSTREAM_BYTES],
            offset: KEYSTREAM_BYTES,
        })
    }

    fn refill(&mut self) {
        let mut value = u128::from_be_bytes(self.counter);
        for slot in self.keystream.chunks_exact_mut(BLOCK) {
            let block = value.to_be_bytes();
            // CNG refuses in-place output and this is no different: the counter
            // is encrypted into a scratch block, then copied to the keystream.
            let mut encrypted = block;
            self.cipher.encrypt_block(&mut encrypted);
            slot.copy_from_slice(&encrypted);
            value = value.wrapping_add(1);
        }
        self.counter = value.to_be_bytes();
        self.offset = 0;
    }

    /// XORs the keystream over `data`. Encrypting and decrypting are the same
    /// operation in CTR mode.
    pub fn apply_keystream(&mut self, data: &mut [u8]) {
        let mut at = 0usize;
        while at < data.len() {
            if self.offset == KEYSTREAM_BYTES {
                self.refill();
            }
            let take = (KEYSTREAM_BYTES - self.offset).min(data.len() - at);
            let end = self.offset + take;
            for (byte, mask) in data[at..at + take]
                .iter_mut()
                .zip(self.keystream[self.offset..end].iter())
            {
                *byte ^= mask;
            }
            self.offset = end;
            at += take;
        }
    }

    pub fn process(&mut self, data: &[u8]) -> Vec<u8> {
        let mut out = data.to_vec();
        self.apply_keystream(&mut out);
        out
    }
}

/// One AES-128-CBC stream per packet, the way RAOP encrypts audio payloads.
///
/// The IV goes back to its initial value before every packet: a receiver that
/// chained across packets would decrypt only the first one. Only whole 16-byte
/// blocks are encrypted; RAOP copies a trailing partial block verbatim, so a
/// sender has to leave it in the clear as well.
pub struct AesCbc {
    cipher: Aes128,
    iv: [u8; BLOCK],
}

impl AesCbc {
    pub fn new(key: &[u8], iv: &[u8]) -> Result<Self> {
        if iv.len() != BLOCK {
            bail!("aes-cbc iv is {} bytes, want 16", iv.len());
        }
        let mut initial = [0u8; BLOCK];
        initial.copy_from_slice(iv);
        Ok(Self {
            cipher: Aes128::new(key)?,
            iv: initial,
        })
    }

    /// Encrypts `data` in place from the IV, packet after packet.
    pub fn encrypt_packet(&self, data: &mut [u8]) {
        let mut previous = self.iv;
        for block in data.chunks_exact_mut(BLOCK) {
            for (byte, mask) in block.iter_mut().zip(previous.iter()) {
                *byte ^= mask;
            }
            let mut state = [0u8; BLOCK];
            state.copy_from_slice(block);
            self.cipher.encrypt_block(&mut state);
            block.copy_from_slice(&state);
            previous = state;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn unhex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|at| u8::from_str_radix(&text[at..at + 2], 16).expect("hex"))
            .collect()
    }

    /// FIPS 197 Appendix C.1: the one vector every AES implementation is
    /// measured against.
    #[test]
    fn the_block_cipher_matches_fips_197() {
        let key = unhex("000102030405060708090a0b0c0d0e0f");
        let input = unhex("00112233445566778899aabbccddeeff");
        let expected = unhex("69c4e0d86a7b0430d8cdb78070b4c55a");
        let cipher = Aes128::new(&key).unwrap();
        let mut block = [0u8; BLOCK];
        block.copy_from_slice(&input);
        cipher.encrypt_block(&mut block);
        assert_eq!(block.to_vec(), expected);
    }

    /// The S-box is generated, not copied, so the known entries are the check
    /// that the field arithmetic and the affine map agree with the table.
    #[test]
    fn the_generated_sbox_matches_the_known_entries() {
        let table = sbox();
        assert_eq!(table[0x00], 0x63);
        assert_eq!(table[0x01], 0x7c);
        assert_eq!(table[0x53], 0xed);
        assert_eq!(table[0x7f], 0xd2);
        assert_eq!(table[0xff], 0x16);
        // A substitution table that is not a bijection would silently make the
        // cipher irreversible.
        let mut seen = [false; 256];
        for entry in table.iter() {
            assert!(!seen[*entry as usize], "s-box value {entry} repeats");
            seen[*entry as usize] = true;
        }
    }

    /// SP 800-38A F.5.1: four counter-mode plaintext blocks.
    #[test]
    fn the_ctr_stream_matches_sp_800_38a() {
        let key = unhex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = unhex("f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff");
        let plaintext = unhex(
            "6bc1bee22e409f96e93d7e117393172a\
             ae2d8a571e03ac9c9eb76fac45af8e51\
             30c81c46a35ce411e5fbc1191a0a52ef\
             f69f2445df4f9b17ad2b417be66c3710",
        );
        let expected = unhex(
            "874d6191b620e3261bef6864990db6ce\
             9806f66b7970fdff8617187bb9fffdff\
             5ae4df3edbd5d35e5b4f09020db03eab\
             1e031dda2fbe03d1792170a0f3009cee",
        );
        let mut cipher = AesCtr::new(&key, &iv).unwrap();
        assert_eq!(cipher.process(&plaintext), expected);
    }

    /// One stream across several calls, which is what pair-verify and the
    /// mirroring frames both rely on: re-encrypting the same bytes separately
    /// would restart the counter and break the receiver's keystream.
    #[test]
    fn the_keystream_runs_across_call_boundaries() {
        let key = [0x2cu8; 16];
        let iv = [0x11u8; 16];
        let data: Vec<u8> = (0..(KEYSTREAM_BYTES + 700)).map(|i| i as u8).collect();

        let mut one = AesCtr::new(&key, &iv).unwrap();
        let whole = one.process(&data);

        let mut split = AesCtr::new(&key, &iv).unwrap();
        let mut pieces = Vec::new();
        for chunk in data.chunks(3000) {
            pieces.extend_from_slice(&split.process(chunk));
        }
        assert_eq!(whole, pieces);

        // Decryption is the same operation, and it has to give the input back.
        let mut back = AesCtr::new(&key, &iv).unwrap();
        let mut restored = whole.clone();
        back.apply_keystream(&mut restored);
        assert_eq!(restored, data);
    }

    #[test]
    fn a_short_key_or_iv_is_refused() {
        assert!(Aes128::new(&[0u8; 15]).is_err());
        assert!(AesCtr::new(&[0u8; 16], &[0u8; 15]).is_err());
        assert!(AesCbc::new(&[0u8; 16], &[0u8; 15]).is_err());
    }

    /// SP 800-38A F.2.1: four ciphertext blocks in CBC mode.
    #[test]
    fn the_cbc_mode_matches_sp_800_38a() {
        let key = unhex("2b7e151628aed2a6abf7158809cf4f3c");
        let iv = unhex("000102030405060708090a0b0c0d0e0f");
        let mut plaintext = unhex(
            "6bc1bee22e409f96e93d7e117393172a\
             ae2d8a571e03ac9c9eb76fac45af8e51\
             30c81c46a35ce411e5fbc1191a0a52ef\
             f69f2445df4f9b17ad2b417be66c3710",
        );
        let expected = unhex(
            "7649abac8119b246cee98e9b12e9197d\
             5086cb9b507219ee95db113a917678b2\
             73bed6b8e3c1743b7116e69e22229516\
             3ff1caa1681fac09120eca307586e1a7",
        );
        let cipher = AesCbc::new(&key, &iv).unwrap();
        cipher.encrypt_packet(&mut plaintext);
        assert_eq!(plaintext, expected);
    }

    /// Each packet restarts from the IV, and the trailing bytes RAOP leaves
    /// outside the block boundary stay as they were.
    #[test]
    fn every_packet_restarts_from_the_iv_and_leaves_the_tail_alone() {
        let cipher = AesCbc::new(&[0x5au8; 16], &[0x33u8; 16]).unwrap();
        let mut first = vec![0x11u8; 16 + 5];
        let mut second = first.clone();
        cipher.encrypt_packet(&mut first);
        cipher.encrypt_packet(&mut second);
        assert_eq!(first, second, "a chained IV would differ on the second packet");
        assert_eq!(&first[16..], &[0x11u8; 5], "partial block must stay plaintext");

        // Two full blocks chain inside one packet: encrypting the second block
        // alone must not reproduce block two of the pair.
        let mut pair = vec![0u8; 32];
        cipher.encrypt_packet(&mut pair);
        let mut alone = vec![0u8; 16];
        cipher.encrypt_packet(&mut alone);
        assert_ne!(&pair[16..], &alone[..], "CBC must chain within a packet");
    }
}
