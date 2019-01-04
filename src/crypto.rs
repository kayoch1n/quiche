// Copyright (C) 2018, Cloudflare, Inc.
// Copyright (C) 2018, Alessandro Ghedini
// All rights reserved.
//
// Redistribution and use in source and binary forms, with or without
// modification, are permitted provided that the following conditions are
// met:
//
//     * Redistributions of source code must retain the above copyright
//       notice, this list of conditions and the following disclaimer.
//
//     * Redistributions in binary form must reproduce the above copyright
//       notice, this list of conditions and the following disclaimer in the
//       documentation and/or other materials provided with the distribution.
//
// THIS SOFTWARE IS PROVIDED BY THE COPYRIGHT HOLDERS AND CONTRIBUTORS "AS
// IS" AND ANY EXPRESS OR IMPLIED WARRANTIES, INCLUDING, BUT NOT LIMITED TO,
// THE IMPLIED WARRANTIES OF MERCHANTABILITY AND FITNESS FOR A PARTICULAR
// PURPOSE ARE DISCLAIMED. IN NO EVENT SHALL THE COPYRIGHT HOLDER OR
// CONTRIBUTORS BE LIABLE FOR ANY DIRECT, INDIRECT, INCIDENTAL, SPECIAL,
// EXEMPLARY, OR CONSEQUENTIAL DAMAGES (INCLUDING, BUT NOT LIMITED TO,
// PROCUREMENT OF SUBSTITUTE GOODS OR SERVICES; LOSS OF USE, DATA, OR
// PROFITS; OR BUSINESS INTERRUPTION) HOWEVER CAUSED AND ON ANY THEORY OF
// LIABILITY, WHETHER IN CONTRACT, STRICT LIABILITY, OR TORT (INCLUDING
// NEGLIGENCE OR OTHERWISE) ARISING IN ANY WAY OUT OF THE USE OF THIS
// SOFTWARE, EVEN IF ADVISED OF THE POSSIBILITY OF SUCH DAMAGE.

use ring::aead;
use ring::digest;
use ring::hkdf;
use ring::hmac;
use ring::unauthenticated_stream;

use crate::Result;
use crate::Error;

use crate::octets;

const INITIAL_SALT: [u8; 20] = [
    0xef, 0x4f, 0xb0, 0xab, 0xb4, 0x74, 0x70, 0xc4, 0x1b, 0xef,
    0xcf, 0x80, 0x31, 0x33, 0x4f, 0xae, 0x48, 0x5e, 0x09, 0xa0,
];

#[repr(C)]
#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Level {
    Initial = 0,
    ZeroRTT = 1,
    Handshake = 2,
    Application = 3,
}

#[derive(Clone, Copy, Debug, PartialEq)]
pub enum Algorithm {
    Null,

    #[allow(non_camel_case_types)]
    AES128_GCM,

    #[allow(non_camel_case_types)]
    AES256_GCM,

    #[allow(non_camel_case_types)]
    ChaCha20_Poly1305,
}

impl Algorithm {
    fn get_ring_aead(self) -> &'static aead::Algorithm {
        match self {
            Algorithm::AES128_GCM => &aead::AES_128_GCM,
            Algorithm::AES256_GCM => &aead::AES_256_GCM,
            Algorithm::ChaCha20_Poly1305 => &aead::CHACHA20_POLY1305,
            Algorithm::Null => panic!("Not a valid AEAD"),
        }
    }

    fn get_ring_stream(self) -> &'static unauthenticated_stream::Algorithm {
        match self {
            Algorithm::AES128_GCM => &unauthenticated_stream::AES_128_CTR,
            Algorithm::AES256_GCM => &unauthenticated_stream::AES_256_CTR,
            Algorithm::ChaCha20_Poly1305 => &unauthenticated_stream::CHACHA20,
            Algorithm::Null => panic!("Not a valid AEAD"),
        }
    }

    fn get_ring_digest(self) -> &'static digest::Algorithm {
        match self {
            Algorithm::AES128_GCM => &digest::SHA256,
            Algorithm::AES256_GCM => &digest::SHA384,
            Algorithm::ChaCha20_Poly1305 => &digest::SHA256,
            Algorithm::Null => panic!("Not a valid AEAD"),
        }
    }

    pub fn key_len(self) -> usize {
        self.get_ring_aead().key_len()
    }

    pub fn tag_len(self) -> usize {
        self.get_ring_aead().tag_len()
    }

    pub fn nonce_len(self) -> usize {
        self.get_ring_aead().nonce_len()
    }

    pub fn pn_nonce_len(self) -> usize {
        // For pkt num decryption a 4 bytes explicit counter is used along
        // with the normal nonce for both ChaCha20 and AES-CTR.
        self.get_ring_aead().nonce_len() + 4
    }
}

pub struct Open {
    alg: Algorithm,
    pn_key: unauthenticated_stream::DecryptingKey,
    key: aead::OpeningKey,
    nonce: Vec<u8>,
}

impl Open {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(alg: Algorithm, key: &[u8], iv: &[u8], pn_key: &[u8])
                                                            -> Result<Open> {
        Ok(Open {
            pn_key: unauthenticated_stream::DecryptingKey::new(
                            alg.get_ring_stream(), &pn_key).unwrap(),
            key: aead::OpeningKey::new(alg.get_ring_aead(), &key).unwrap(),
            nonce: Vec::from(iv),
            alg,
        })
    }

    pub fn open(&self, nonce: &[u8], ad: &[u8], buf: &mut [u8]) -> Result<usize> {
        let plain = aead::open_in_place(&self.key, nonce, ad, 0, buf)
                         .map_err(|_| Error::CryptoFail)?;

        Ok(plain.len())
    }

    pub fn open_with_u64_counter(&self, counter: u64, ad: &[u8], buf: &mut [u8])
                                                            -> Result<usize> {
        let mut counter_nonce: [u8; 12] = [0xba; 12];

        {
            let mut b = octets::Bytes::new(&mut counter_nonce);

            b.put_u32(0).unwrap();
            b.put_u64(counter).unwrap();
        }

        let mut nonce = self.nonce.clone();

        for i in 0 .. nonce.len() {
            nonce[i] ^= counter_nonce[i];
        }

        self.open(&nonce, ad, buf)
    }

    pub fn xor_keystream(&self, nonce: &[u8], buf: &mut [u8]) -> Result<usize> {
        let plain = unauthenticated_stream::decrypt_in_place(&self.pn_key,
                        nonce, buf).map_err(|_| Error::CryptoFail)?;

        Ok(plain.len())
    }

    pub fn alg(&self) -> Algorithm {
        self.alg
    }
}

pub struct Seal {
    alg: Algorithm,
    pn_key: unauthenticated_stream::EncryptingKey,
    key: aead::SealingKey,
    nonce: Vec<u8>,
}

impl Seal {
    #[allow(clippy::new_ret_no_self)]
    pub fn new(alg: Algorithm, key: &[u8], iv: &[u8], pn_key: &[u8])
                                                            -> Result<Seal> {
        Ok(Seal {
            pn_key: unauthenticated_stream::EncryptingKey::new(
                            alg.get_ring_stream(), &pn_key).unwrap(),
            key: aead::SealingKey::new(alg.get_ring_aead(), &key).unwrap(),
            nonce: Vec::from(iv),
            alg,
        })
    }

    pub fn seal(&self, nonce: &[u8], ad: &[u8], buf: &mut [u8]) -> Result<usize> {
        let cipher = aead::seal_in_place(&self.key, nonce, ad, buf, self.alg().tag_len())
                          .map_err(|_| Error::CryptoFail)?;

        Ok(cipher)
    }

    pub fn seal_with_u64_counter(&self, counter: u64, ad: &[u8], buf: &mut [u8])
                                                            -> Result<usize> {
        let mut counter_nonce: [u8; 12] = [0xba; 12];

        {
            let mut b = octets::Bytes::new(&mut counter_nonce);

            b.put_u32(0).unwrap();
            b.put_u64(counter).unwrap();
        }

        let mut nonce = self.nonce.clone();

        for i in 0 .. nonce.len() {
            nonce[i] ^= counter_nonce[i];
        }

        self.seal(&nonce, ad, buf)
    }

    pub fn xor_keystream(&self, nonce: &[u8], buf: &mut [u8]) -> Result<usize> {
        let plain = unauthenticated_stream::encrypt_in_place(&self.pn_key,
                        nonce, buf).map_err(|_| Error::CryptoFail)?;

        Ok(plain)
    }

    pub fn alg(&self) -> Algorithm {
        self.alg
    }
}

pub fn derive_initial_key_material(cid: &[u8], is_server: bool)
                                                    -> Result<(Open, Seal)> {
    let mut secret: [u8; 32] =  unsafe { std::mem::uninitialized() };

    let aead = Algorithm::AES128_GCM;

    let key_len = aead.key_len();
    let nonce_len = aead.nonce_len();

    let initial_secret = derive_initial_secret(&cid)?;

    // Client.
    let mut client_key = vec![0; key_len];
    let mut client_iv = vec![0; nonce_len];
    let mut client_pn_key = vec![0; key_len];

    derive_client_initial_secret(&initial_secret, &mut secret)?;
    derive_pkt_key(aead, &secret, &mut client_key)?;
    derive_pkt_iv(aead, &secret, &mut client_iv)?;
    derive_hdr_key(aead, &secret, &mut client_pn_key)?;

    // Server.
    let mut server_key = vec![0; key_len];
    let mut server_iv = vec![0; nonce_len];
    let mut server_pn_key = vec![0; key_len];

    derive_server_initial_secret(&initial_secret, &mut secret)?;
    derive_pkt_key(aead, &secret, &mut server_key)?;
    derive_pkt_iv(aead, &secret, &mut server_iv)?;
    derive_hdr_key(aead, &secret, &mut server_pn_key)?;

    let (open, seal) = if is_server {
        (Open::new(aead, &client_key, &client_iv, &client_pn_key)?,
         Seal::new(aead, &server_key, &server_iv, &server_pn_key)?)
    } else {
        (Open::new(aead, &server_key, &server_iv, &server_pn_key)?,
         Seal::new(aead, &client_key, &client_iv, &client_pn_key)?)
    };

    Ok((open, seal))
}

fn derive_initial_secret(secret: &[u8]) -> Result<hmac::SigningKey> {
    let salt = hmac::SigningKey::new(&digest::SHA256, &INITIAL_SALT);
    Ok(hkdf::extract(&salt, secret))
}

fn derive_client_initial_secret(prk: &hmac::SigningKey, out: &mut [u8]) -> Result<()> {
    const LABEL: &[u8] = b"client in";
    hkdf_expand_label(prk, LABEL, out)
}

fn derive_server_initial_secret(prk: &hmac::SigningKey, out: &mut [u8]) -> Result<()> {
    const LABEL: &[u8] = b"server in";
    hkdf_expand_label(prk, LABEL, out)
}

pub fn derive_hdr_key(aead: Algorithm, secret: &[u8], out: &mut [u8])
                                                                -> Result<()> {
    const LABEL: &[u8] = b"quic hp";

    let key_len = aead.key_len();

    if key_len > out.len() {
        return Err(Error::CryptoFail);
    }

    let secret = hmac::SigningKey::new(aead.get_ring_digest(), secret);
    hkdf_expand_label(&secret, LABEL, &mut out[..key_len])
}

pub fn derive_pkt_key(aead: Algorithm, secret: &[u8], out: &mut [u8])
                                                                -> Result<()> {
    const LABEL: &[u8] = b"quic key";

    let key_len = aead.key_len();

    if key_len > out.len() {
        return Err(Error::CryptoFail);
    }

    let secret = hmac::SigningKey::new(aead.get_ring_digest(), secret);
    hkdf_expand_label(&secret, LABEL, &mut out[..key_len])
}

pub fn derive_pkt_iv(aead: Algorithm, secret: &[u8], out: &mut [u8])
                                                                -> Result<()> {
    const LABEL: &[u8] = b"quic iv";

    let nonce_len = aead.nonce_len();

    if nonce_len > out.len() {
        return Err(Error::CryptoFail);
    }

    let secret = hmac::SigningKey::new(aead.get_ring_digest(), secret);
    hkdf_expand_label(&secret, LABEL, &mut out[..nonce_len])
}

fn hkdf_expand_label(prk: &hmac::SigningKey, label: &[u8],  out: &mut [u8])
                                                            -> Result<()> {
    const LABEL_PREFIX: &[u8] = b"tls13 ";

    let mut info: [u8; 256] = unsafe { std::mem::uninitialized() };

    let info_len = {
        let mut b = octets::Bytes::new(&mut info);

        if b.put_u16(out.len() as u16).is_err() ||
           b.put_u8((LABEL_PREFIX.len() + label.len()) as u8).is_err() ||
           b.put_bytes(LABEL_PREFIX).is_err() ||
           b.put_bytes(label).is_err() ||
           b.put_u8(0).is_err() {
            return Err(Error::CryptoFail);
        }

        b.off()
    };

    hkdf::expand(prk, &info[..info_len], out);

    Ok(())
}


#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn derive_initial_secrets() {
        let dcid: [u8; 8] = [0xc6, 0x54, 0xef, 0xd8, 0xa3, 0x1b, 0x47, 0x92];

        let mut secret: [u8; 32] = [0; 32];
        let mut pkt_key: [u8; 16] = [0; 16];
        let mut pkt_iv: [u8; 12] = [0; 12];
        let mut hdr_key: [u8; 16] = [0; 16];

        let aead = Algorithm::AES128_GCM;

        let initial_secret = derive_initial_secret(&dcid).unwrap();

        // Client.
        assert!(derive_client_initial_secret(&initial_secret, &mut secret).is_ok());
        let expected_client_initial_secret: [u8; 32] = [
            0x0c, 0x74, 0xbb, 0x95, 0xa1, 0x04, 0x8e, 0x52,
            0xef, 0x3b, 0x72, 0xe1, 0x28, 0x89, 0x35, 0x1c,
            0xd7, 0x3a, 0x55, 0x0f, 0xb6, 0x2c, 0x4b, 0xb0,
            0x87, 0xe9, 0x15, 0xcc, 0xe9, 0x6c, 0xe3, 0xa0,
        ];
        assert_eq!(&secret, &expected_client_initial_secret);

        assert!(derive_pkt_key(aead, &secret, &mut pkt_key).is_ok());
        let expected_client_pkt_key: [u8; 16] = [
            0x86, 0xd1, 0x83, 0x04, 0x80, 0xb4, 0x0f, 0x86,
            0xcf, 0x9d, 0x68, 0xdc, 0xad, 0xf3, 0x5d, 0xfe,
        ];
        assert_eq!(&pkt_key, &expected_client_pkt_key);

        assert!(derive_pkt_iv(aead, &secret, &mut pkt_iv).is_ok());
        let expected_client_pkt_iv: [u8; 12] = [
            0x12, 0xf3, 0x93, 0x8a, 0xca, 0x34, 0xaa, 0x02,
            0x54, 0x31, 0x63, 0xd4,
        ];
        assert_eq!(&pkt_iv, &expected_client_pkt_iv);

        assert!(derive_hdr_key(aead, &secret, &mut hdr_key).is_ok());
        let expected_cliet_hdr_key: [u8; 16] = [
            0xcd, 0x25, 0x3a, 0x36, 0xff, 0x93, 0x93, 0x7c,
            0x46, 0x93, 0x84, 0xa8, 0x23, 0xaf, 0x6c, 0x56,
        ];
        assert_eq!(&hdr_key, &expected_cliet_hdr_key);

        // Server.
        assert!(derive_server_initial_secret(&initial_secret, &mut secret).is_ok());
        let expected_server_initial_secret: [u8; 32] = [
            0x4c, 0x9e, 0xdf, 0x24, 0xb0, 0xe5, 0xe5, 0x06,
            0xdd, 0x3b, 0xfa, 0x4e, 0x0a, 0x03, 0x11, 0xe8,
            0xc4, 0x1f, 0x35, 0x42, 0x73, 0xd8, 0xcb, 0x49,
            0xdd, 0xd8, 0x46, 0x41, 0x38, 0xd4, 0x7e, 0xc6,
        ];
        assert_eq!(&secret, &expected_server_initial_secret);

        assert!(derive_pkt_key(aead, &secret, &mut pkt_key).is_ok());
        let expected_server_pkt_key: [u8; 16] = [
            0x2c, 0x78, 0x63, 0x3e, 0x20, 0x6e, 0x99, 0xad,
            0x25, 0x19, 0x64, 0xf1, 0x9f, 0x6d, 0xcd, 0x6d,
        ];
        assert_eq!(&pkt_key, &expected_server_pkt_key);

        assert!(derive_pkt_iv(aead, &secret, &mut pkt_iv).is_ok());
        let expected_server_pkt_iv: [u8; 12] = [
            0x7b, 0x50, 0xbf, 0x36, 0x98, 0xa0, 0x6d, 0xfa,
            0xbf, 0x75, 0xf2, 0x87,
        ];
        assert_eq!(&pkt_iv, &expected_server_pkt_iv);

        assert!(derive_hdr_key(aead, &secret, &mut hdr_key).is_ok());
        let expected_server_hdr_key: [u8; 16] = [
            0x25, 0x79, 0xd8, 0x69, 0x6f, 0x85, 0xed, 0xa6,
            0x8d, 0x35, 0x02, 0xb6, 0x55, 0x96, 0x58, 0x6b,
        ];
        assert_eq!(&hdr_key, &expected_server_hdr_key);
    }

    #[test]
    fn derive_initial_secrets2() {
        let dcid: [u8; 8] = [ 0x83, 0x94, 0xc8, 0xf0, 0x3e, 0x51, 0x57, 0x08 ];

        let mut secret: [u8; 32] = [0; 32];
        let mut pkt_key: [u8; 16] = [0; 16];
        let mut pkt_iv: [u8; 12] = [0; 12];
        let mut hdr_key: [u8; 16] = [0; 16];

        let aead = Algorithm::AES128_GCM;

        let initial_secret = derive_initial_secret(&dcid).unwrap();

        // Client.
        assert!(derive_client_initial_secret(&initial_secret, &mut secret).is_ok());
        let expected_client_initial_secret: [u8; 32] = [
            0x8a, 0x35, 0x15, 0xa1, 0x4a, 0xe3, 0xc3, 0x1b,
            0x9c, 0x2d, 0x6d, 0x5b, 0xc5, 0x85, 0x38, 0xca,
            0x5c, 0xd2, 0xba, 0xa1, 0x19, 0x08, 0x71, 0x43,
            0xe6, 0x08, 0x87, 0x42, 0x8d, 0xcb, 0x52, 0xf6,
        ];
        assert_eq!(&secret, &expected_client_initial_secret);

        assert!(derive_pkt_key(aead, &secret, &mut pkt_key).is_ok());
        let expected_client_pkt_key: [u8; 16] = [
            0x98, 0xb0, 0xd7, 0xe5, 0xe7, 0xa4, 0x02, 0xc6,
            0x7c, 0x33, 0xf3, 0x50, 0xfa, 0x65, 0xea, 0x54,
        ];
        assert_eq!(&pkt_key, &expected_client_pkt_key);

        assert!(derive_pkt_iv(aead, &secret, &mut pkt_iv).is_ok());
        let expected_client_pkt_iv: [u8; 12] = [
            0x19, 0xe9, 0x43, 0x87, 0x80, 0x5e, 0xb0, 0xb4,
            0x6c, 0x03, 0xa7, 0x88,
        ];
        assert_eq!(&pkt_iv, &expected_client_pkt_iv);

        assert!(derive_hdr_key(aead, &secret, &mut hdr_key).is_ok());
        let expected_cliet_hdr_key: [u8; 16] = [
            0x0e, 0xdd, 0x98, 0x2a, 0x6a, 0xc5, 0x27, 0xf2,
            0xed, 0xdc, 0xbb, 0x73, 0x48, 0xde, 0xa5, 0xd7,
        ];
        assert_eq!(&hdr_key, &expected_cliet_hdr_key);

        // Server.
        assert!(derive_server_initial_secret(&initial_secret, &mut secret).is_ok());
        let expected_server_initial_secret: [u8; 32] = [
            0x47, 0xb2, 0xea, 0xea, 0x6c, 0x26, 0x6e, 0x32,
            0xc0, 0x69, 0x7a, 0x9e, 0x2a, 0x89, 0x8b, 0xdf,
            0x5c, 0x4f, 0xb3, 0xe5, 0xac, 0x34, 0xf0, 0xe5,
            0x49, 0xbf, 0x2c, 0x58, 0x58, 0x1a, 0x38, 0x11,
        ];
        assert_eq!(&secret, &expected_server_initial_secret);

        assert!(derive_pkt_key(aead, &secret, &mut pkt_key).is_ok());
        let expected_server_pkt_key: [u8; 16] = [
            0x9a, 0x8b, 0xe9, 0x02, 0xa9, 0xbd, 0xd9, 0x1d,
            0x16, 0x06, 0x4c, 0xa1, 0x18, 0x04, 0x5f, 0xb4,
        ];
        assert_eq!(&pkt_key, &expected_server_pkt_key);

        assert!(derive_pkt_iv(aead, &secret, &mut pkt_iv).is_ok());
        let expected_server_pkt_iv: [u8; 12] = [
            0x0a, 0x82, 0x08, 0x6d, 0x32, 0x20, 0x5b, 0xa2,
            0x22, 0x41, 0xd8, 0xdc,
        ];
        assert_eq!(&pkt_iv, &expected_server_pkt_iv);

        assert!(derive_hdr_key(aead, &secret, &mut hdr_key).is_ok());
        let expected_server_hdr_key: [u8; 16] = [
            0x94, 0xb9, 0x45, 0x2d, 0x2b, 0x3c, 0x7c, 0x7f,
            0x6d, 0xa7, 0xfd, 0xd8, 0x59, 0x35, 0x37, 0xfd,
        ];
        assert_eq!(&hdr_key, &expected_server_hdr_key);
    }
}
