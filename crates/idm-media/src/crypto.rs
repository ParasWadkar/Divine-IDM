//! Decrypting AES-128 HLS segments.
//!
//! A playlist that declares `METHOD=AES-128` encrypts each segment whole,
//! with AES-128-CBC and PKCS#7 padding. The key is fetched from the address
//! in the `EXT-X-KEY` tag — sixteen bytes, nothing else — and the
//! initialisation vector is either stated in the tag or, far more often,
//! derived from the segment's media sequence number.
//!
//! That derivation is the part worth being careful about. Each segment is
//! encrypted independently with its *own* IV, so a stream decrypted with a
//! single IV produces a first segment that is correct and a file that is
//! ruined from the second segment onwards — which looks, at a glance, like
//! a download that worked.

use aes::cipher::block_padding::Pkcs7;
use aes::cipher::{BlockModeDecrypt, KeyIvInit};

use crate::{MediaError, Result};

type Aes128CbcDec = cbc::Decryptor<aes::Aes128>;

pub const KEY_LEN: usize = 16;
pub const IV_LEN: usize = 16;

/// The initialisation vector for one segment.
///
/// `EXT-X-KEY` may state an IV. When it does not, RFC 8216 says to use the
/// segment's media sequence number as a 16-octet big-endian value — which
/// means consecutive segments have different IVs, and a playlist with a
/// `#EXT-X-MEDIA-SEQUENCE` offset does not start at zero.
pub fn iv_for(sequence: u64, stated: Option<[u8; IV_LEN]>) -> [u8; IV_LEN] {
    if let Some(iv) = stated {
        return iv;
    }
    let mut iv = [0u8; IV_LEN];
    // Right-aligned: the number occupies the last eight octets, the rest
    // stay zero.
    iv[IV_LEN - 8..].copy_from_slice(&sequence.to_be_bytes());
    iv
}

/// Reads a fetched key, which must be exactly sixteen bytes.
///
/// A key server that returns an error page instead of a key produces
/// something of the wrong length, and refusing here gives a message about
/// the key rather than a file full of noise.
pub fn key_from_bytes(bytes: &[u8]) -> Result<[u8; KEY_LEN]> {
    if bytes.len() != KEY_LEN {
        return Err(MediaError::Unsupported(format!(
            "the decryption key is {} bytes; AES-128 needs {KEY_LEN}",
            bytes.len()
        )));
    }
    let mut key = [0u8; KEY_LEN];
    key.copy_from_slice(bytes);
    Ok(key)
}

/// Decrypts one whole segment.
pub fn decrypt_segment(
    key: &[u8; KEY_LEN],
    iv: &[u8; IV_LEN],
    ciphertext: &[u8],
) -> Result<Vec<u8>> {
    if ciphertext.is_empty() {
        return Ok(Vec::new());
    }
    if ciphertext.len() % 16 != 0 {
        return Err(MediaError::Unsupported(format!(
            "an encrypted segment is {} bytes, which is not a whole number of AES blocks",
            ciphertext.len()
        )));
    }

    Aes128CbcDec::new(key.into(), iv.into()).decrypt_padded_vec::<Pkcs7>(ciphertext).map_err(|_| {
        MediaError::Unsupported(
            "a segment did not decrypt; the key or the initialisation vector is wrong".to_owned(),
        )
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use aes::cipher::{BlockModeEncrypt, block_padding::Pkcs7 as Pad};

    type Aes128CbcEnc = cbc::Encryptor<aes::Aes128>;

    fn encrypt(key: &[u8; 16], iv: &[u8; 16], plaintext: &[u8]) -> Vec<u8> {
        Aes128CbcEnc::new(key.into(), iv.into()).encrypt_padded_vec::<Pad>(plaintext)
    }

    #[test]
    fn a_segment_survives_a_round_trip() {
        let key = [0x42u8; 16];
        let iv = iv_for(7, None);
        let plaintext: Vec<u8> = (0..1000u32).map(|i| (i % 251) as u8).collect();

        let ciphertext = encrypt(&key, &iv, &plaintext);
        assert_ne!(ciphertext, plaintext);
        assert_eq!(decrypt_segment(&key, &iv, &ciphertext).unwrap(), plaintext);
    }

    #[test]
    fn the_sequence_number_becomes_the_iv_when_none_is_stated() {
        // Right-aligned, big-endian, per RFC 8216.
        let iv = iv_for(1, None);
        assert_eq!(iv, [0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 1]);

        let iv = iv_for(0x0102_0304_0506_0708, None);
        assert_eq!(&iv[8..], &[1, 2, 3, 4, 5, 6, 7, 8]);
        assert_eq!(&iv[..8], &[0; 8]);
    }

    #[test]
    fn a_stated_iv_wins_over_the_sequence_number() {
        let stated = [0xAA; 16];
        assert_eq!(iv_for(99, Some(stated)), stated);
    }

    #[test]
    fn every_segment_gets_its_own_iv() {
        // The failure this guards is nasty: one IV for the whole stream
        // decrypts the first segment perfectly and ruins every one after,
        // which looks like a download that worked.
        let key = [0x11u8; 16];
        let plaintext = b"the same bytes in every segment, encrypted differently".to_vec();

        let first = encrypt(&key, &iv_for(0, None), &plaintext);
        let second = encrypt(&key, &iv_for(1, None), &plaintext);
        assert_ne!(first, second, "different IVs must produce different ciphertext");

        assert_eq!(decrypt_segment(&key, &iv_for(1, None), &second).unwrap(), plaintext);
        // And the wrong IV must not quietly produce plausible output.
        let wrong = decrypt_segment(&key, &iv_for(0, None), &second);
        assert!(wrong.is_err() || wrong.unwrap() != plaintext);
    }

    #[test]
    fn a_media_sequence_offset_is_carried_into_the_iv() {
        // A playlist starting at #EXT-X-MEDIA-SEQUENCE:9000 does not start
        // its IVs at zero.
        let key = [0x33u8; 16];
        let plaintext = b"segment body".to_vec();
        let ciphertext = encrypt(&key, &iv_for(9000, None), &plaintext);
        assert_eq!(decrypt_segment(&key, &iv_for(9000, None), &ciphertext).unwrap(), plaintext);
    }

    #[test]
    fn a_key_of_the_wrong_length_is_refused_by_name() {
        // A key server returning an error page is the common way this goes
        // wrong, and "the key is 142 bytes" is far more use than a file of
        // noise.
        let page = b"<html><body>404 Not Found</body></html>";
        assert_ne!(page.len(), KEY_LEN, "the test input has to be the wrong length");
        let error = key_from_bytes(page).unwrap_err().to_string();
        assert!(error.contains("16"), "{error}");
        assert!(key_from_bytes(&[0u8; 16]).is_ok());
        assert!(key_from_bytes(&[]).is_err());
    }

    #[test]
    fn a_truncated_segment_is_refused_rather_than_half_decrypted() {
        let key = [0x55u8; 16];
        let iv = iv_for(0, None);
        let ciphertext = encrypt(&key, &iv, b"some content");
        let truncated = &ciphertext[..ciphertext.len() - 3];
        assert!(decrypt_segment(&key, &iv, truncated).is_err());
    }

    #[test]
    fn the_wrong_key_fails_rather_than_producing_noise() {
        // PKCS#7 padding makes most wrong keys detectable, which is worth
        // having: the alternative is a file that is silently garbage.
        let iv = iv_for(0, None);
        let ciphertext = encrypt(&[0x01; 16], &iv, b"content worth protecting");
        assert!(decrypt_segment(&[0x02; 16], &iv, &ciphertext).is_err());
    }

    #[test]
    fn an_empty_segment_is_not_an_error() {
        assert_eq!(decrypt_segment(&[0u8; 16], &[0u8; 16], &[]).unwrap(), Vec::<u8>::new());
    }

    #[test]
    fn known_answer_from_the_aes_specification() {
        // NIST SP 800-38A, CBC-AES128.Decrypt, first block. Without a vector
        // from outside this file, the round-trip tests only prove the two
        // halves agree with each other.
        let key = [
            0x2b, 0x7e, 0x15, 0x16, 0x28, 0xae, 0xd2, 0xa6, 0xab, 0xf7, 0x15, 0x88, 0x09, 0xcf,
            0x4f, 0x3c,
        ];
        let iv = [
            0x00, 0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, 0x09, 0x0a, 0x0b, 0x0c, 0x0d,
            0x0e, 0x0f,
        ];
        let ciphertext = [
            0x76, 0x49, 0xab, 0xac, 0x81, 0x19, 0xb2, 0x46, 0xce, 0xe9, 0x8e, 0x9b, 0x12, 0xe9,
            0x19, 0x7d,
        ];
        let expected = [
            0x6b, 0xc1, 0xbe, 0xe2, 0x2e, 0x40, 0x9f, 0x96, 0xe9, 0x3d, 0x7e, 0x11, 0x73, 0x93,
            0x17, 0x2a,
        ];

        // One block alone has no padding, so it is decrypted as a prefix of
        // a longer message: encrypt the expected plaintext with padding and
        // check the first block of ciphertext matches the specification's.
        let padded = encrypt(&key, &iv, &expected);
        assert_eq!(&padded[..16], &ciphertext, "AES-128-CBC does not match the specification");
    }
}
