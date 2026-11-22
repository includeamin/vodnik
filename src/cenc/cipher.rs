//! AES-128-CBC with the `cbcs` pattern (ISO/IEC 23001-7, 10.4). A library CBC mode would chain
//! through the skipped blocks, so the chaining is done here over a raw block cipher.

use aes::Aes128;
use aes::cipher::{BlockCipherEncrypt, KeyInit};

/// How many 16-byte blocks are encrypted, then skipped, repeating across a protected range.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct Pattern {
    pub(crate) crypt: u8,
    pub(crate) skip: u8,
}

impl Pattern {
    /// Video in `cbcs`: one block in ten.
    pub(crate) const VIDEO: Self = Self { crypt: 1, skip: 9 };
    /// Audio in `cbcs`: no pattern; every whole block.
    pub(crate) const FULL: Self = Self { crypt: 0, skip: 0 };
}

pub(crate) struct Cipher(Aes128);

impl Cipher {
    pub(crate) fn new(key: &[u8; 16]) -> Self {
        Self(Aes128::new(&(*key).into()))
    }

    /// Encrypts one protected range in place, starting the chain from `iv`. Skipped blocks and a
    /// trailing partial block stay clear.
    pub(crate) fn encrypt_range(&self, iv: &[u8; 16], data: &mut [u8], pattern: Pattern) {
        let crypt = usize::from(pattern.crypt);
        let period = crypt + usize::from(pattern.skip);
        let mut chain = *iv;
        for (index, block) in data.as_chunks_mut::<16>().0.iter_mut().enumerate() {
            if period != 0 && index % period >= crypt {
                continue;
            }
            for (byte, previous) in block.iter_mut().zip(chain) {
                *byte ^= previous;
            }
            self.0.encrypt_block(block.into());
            chain = *block;
        }
    }
}

/// AES-128 in counter mode for the `cenc` scheme (ISO/IEC 23001-7, 10.1): the counter block is the
/// sample's 8-byte IV followed by an 8-byte block counter that starts at zero. A sample's protected
/// ranges are one continuous stream, so the keystream carries on across them, and across a range
/// that ends part way through a block.
pub(crate) struct CtrStream<'a> {
    cipher: &'a Cipher,
    counter: [u8; 16],
    /// Bytes of the stream already used.
    position: u64,
    block: [u8; 16],
}

impl<'a> CtrStream<'a> {
    pub(crate) fn new(cipher: &'a Cipher, iv: [u8; 8]) -> Self {
        let mut counter = [0u8; 16];
        counter[..8].copy_from_slice(&iv);
        Self::from_counter_block(cipher, counter)
    }

    fn from_counter_block(cipher: &'a Cipher, counter: [u8; 16]) -> Self {
        Self {
            cipher,
            counter,
            position: 0,
            block: [0; 16],
        }
    }

    /// Encrypts (or decrypts: it is the same operation) the next bytes of the stream in place.
    pub(crate) fn apply(&mut self, data: &mut [u8]) {
        for byte in data {
            let within = (self.position % 16) as usize;
            if within == 0 {
                self.block = self.counter;
                self.cipher.0.encrypt_block((&mut self.block).into());
                let low = u64::from_be_bytes(self.counter[8..].try_into().expect("eight bytes"));
                self.counter[8..].copy_from_slice(&low.wrapping_add(1).to_be_bytes());
            }
            *byte ^= self.block[within];
            self.position += 1;
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn hex(text: &str) -> Vec<u8> {
        (0..text.len())
            .step_by(2)
            .map(|index| u8::from_str_radix(&text[index..index + 2], 16).unwrap())
            .collect()
    }

    // NIST SP 800-38A, F.2.1 (CBC-AES128.Encrypt).
    const KEY: &str = "2b7e151628aed2a6abf7158809cf4f3c";
    const IV: &str = "000102030405060708090a0b0c0d0e0f";
    const PLAIN: [&str; 4] = [
        "6bc1bee22e409f96e93d7e117393172a",
        "ae2d8a571e03ac9c9eb76fac45af8e51",
        "30c81c46a35ce411e5fbc1191a0a52ef",
        "f69f2445df4f9b17ad2b417be66c3710",
    ];
    const CIPHER: [&str; 4] = [
        "7649abac8119b246cee98e9b12e9197d",
        "5086cb9b507219ee95db113a917678b2",
        "73bed6b8e3c1743b7116e69e22229516",
        "3ff1caa1681fac09120eca307586e1a7",
    ];

    fn cipher() -> (Cipher, [u8; 16]) {
        (
            Cipher::new(&hex(KEY).try_into().unwrap()),
            hex(IV).try_into().unwrap(),
        )
    }

    #[test]
    fn with_no_pattern_it_is_plain_cbc_and_leaves_a_partial_block_clear() {
        let (cipher, iv) = cipher();
        let mut data = PLAIN
            .iter()
            .flat_map(|block| hex(block))
            .collect::<Vec<_>>();
        data.extend_from_slice(b"tail");

        cipher.encrypt_range(&iv, &mut data, Pattern::FULL);

        let expected = CIPHER
            .iter()
            .flat_map(|block| hex(block))
            .collect::<Vec<_>>();
        assert_eq!(&data[..64], expected.as_slice());
        assert_eq!(&data[64..], b"tail");
    }

    #[test]
    fn one_in_ten_chains_only_the_encrypted_blocks() {
        let (cipher, iv) = cipher();
        // Blocks 0 and 10 are encrypted; their chain skips the nine clear blocks between them,
        // so block 10 is encrypted exactly as the second block of plain CBC would be.
        let mut data = vec![0x55; 16 * 20];
        data[..16].copy_from_slice(&hex(PLAIN[0]));
        data[160..176].copy_from_slice(&hex(PLAIN[1]));
        let original = data.clone();

        cipher.encrypt_range(&iv, &mut data, Pattern::VIDEO);

        assert_eq!(&data[..16], hex(CIPHER[0]).as_slice());
        assert_eq!(&data[160..176], hex(CIPHER[1]).as_slice());
        for block in (1..20).filter(|block| *block != 10) {
            let range = block * 16..block * 16 + 16;
            assert_eq!(
                data[range.clone()],
                original[range],
                "block {block} stays clear"
            );
        }
    }

    // NIST SP 800-38A, F.5.1 (CTR-AES128.Encrypt).
    const CTR_COUNTER: &str = "f0f1f2f3f4f5f6f7f8f9fafbfcfdfeff";
    const CTR_CIPHER: [&str; 4] = [
        "874d6191b620e3261bef6864990db6ce",
        "9806f66b7970fdff8617187bb9fffdff",
        "5ae4df3edbd5d35e5b4f09020db03eab",
        "1e031dda2fbe03d1792170a0f3009cee",
    ];

    fn ctr_plain() -> Vec<u8> {
        PLAIN.iter().flat_map(|block| hex(block)).collect()
    }

    fn ctr_expected() -> Vec<u8> {
        CTR_CIPHER.iter().flat_map(|block| hex(block)).collect()
    }

    #[test]
    fn counter_mode_matches_the_nist_vector() {
        let (cipher, _) = cipher();
        let mut stream =
            CtrStream::from_counter_block(&cipher, hex(CTR_COUNTER).try_into().unwrap());
        let mut data = ctr_plain();
        stream.apply(&mut data);
        assert_eq!(data, ctr_expected());
    }

    #[test]
    fn the_keystream_carries_across_ranges_and_part_blocks() {
        let (cipher, _) = cipher();
        let mut stream =
            CtrStream::from_counter_block(&cipher, hex(CTR_COUNTER).try_into().unwrap());
        // The same bytes handed over in uneven pieces, as a sample's protected ranges are.
        let mut data = ctr_plain();
        let mut out = Vec::new();
        let mut rest = data.as_mut_slice();
        for piece in [5usize, 20, 1, 38] {
            let (head, tail) = rest.split_at_mut(piece);
            stream.apply(head);
            out.extend_from_slice(head);
            rest = tail;
        }
        assert_eq!(out, ctr_expected());
    }

    #[test]
    fn the_counter_is_the_iv_then_a_block_count_from_zero() {
        let (cipher, _) = cipher();
        let iv = [1, 2, 3, 4, 5, 6, 7, 8];
        let mut zeros = vec![0u8; 32];
        CtrStream::new(&cipher, iv).apply(&mut zeros);
        let mut first = [1, 2, 3, 4, 5, 6, 7, 8, 0, 0, 0, 0, 0, 0, 0, 0];
        let mut second = first;
        second[15] = 1;
        let block = |block: &mut [u8; 16]| {
            cipher.0.encrypt_block((&mut *block).into());
            *block
        };
        assert_eq!(&zeros[..16], block(&mut first));
        assert_eq!(&zeros[16..], block(&mut second));
    }
}
