// Copyright 2022 MaidSafe.net limited.
//
// This SAFE Network Software is licensed to you under The General Public License (GPL), version 3.
// Unless required by applicable law or agreed to in writing, the SAFE Network Software distributed
// under the GPL Licence is distributed on an "AS IS" BASIS, WITHOUT WARRANTIES OR CONDITIONS OF ANY
// KIND, either express or implied. Please review the Licences for the specific language governing
// permissions and limitations relating to use of the SAFE Network Software.

//! Module providing keys, keypairs, and signatures.
//!
//! The easiest way to get a `PublicKey` is to create a random `Keypair` first through one of the
//! `new` functions. A `PublicKey` can't be generated by itself; it must always be derived from a
//! secret key.

use super::super::{Error, Result};
use super::super::{PublicKey, SecretKey, Signature, SignatureShare};

use bls::{self, serde_impl::SerdeSecret, PublicKeySet};
use bytes::Bytes;
use ed25519_dalek::Signer;
use serde::{Deserialize, Serialize};
use std::sync::Arc;

#[derive(Clone, Debug, PartialEq, Eq)]
/// Entity that owns the data or tokens.
pub enum OwnerType {
    /// Single owner
    Single(PublicKey),
    /// Multi sig owner
    Multi(PublicKeySet),
}

impl OwnerType {
    /// Returns the owner public key
    pub fn public_key(&self) -> PublicKey {
        match self {
            Self::Single(key) => *key,
            Self::Multi(key_set) => PublicKey::Bls(key_set.public_key()),
        }
    }

    /// Tries to get the key set in case this is a Multi owner.
    pub fn public_key_set(&self) -> Result<PublicKeySet> {
        match self {
            Self::Single(_) => Err(Error::InvalidOwnerNotPublicKeySet),
            Self::Multi(key_set) => Ok(key_set.clone()),
        }
    }

    ///
    pub fn verify<T: Serialize>(&self, signature: &Signature, data: &T) -> bool {
        let data = match bincode::serialize(&data) {
            Err(_) => return false,
            Ok(data) => data,
        };
        match signature {
            Signature::Bls(sig) => {
                if let OwnerType::Multi(set) = self {
                    set.public_key().verify(sig, data)
                } else {
                    false
                }
            }
            ed @ Signature::Ed25519(_) => self.public_key().verify(ed, data).is_ok(),
            Signature::BlsShare(share) => {
                if let OwnerType::Multi(set) = self {
                    let pubkey_share = set.public_key_share(share.index);
                    pubkey_share.verify(&share.share, data)
                } else {
                    false
                }
            }
        }
    }
}

/// Ability to sign and validate data/tokens, as well as specify the type of ownership of that data
pub trait Signing {
    ///
    fn id(&self) -> OwnerType;
    ///
    fn sign<T: Serialize>(&self, data: &T) -> Result<Signature>;
    ///
    fn verify<T: Serialize>(&self, sig: &Signature, data: &T) -> bool;
}

/// Ability to encrypt and decrypt bytes
pub trait Encryption: Sync + Send {
    ///
    fn public_key(&self) -> &PublicKey;
    ///
    fn encrypt(&self, bytes: Bytes) -> Result<Bytes>;
    ///
    fn decrypt(&self, encrypted_data: Bytes) -> Result<Bytes>;
}

impl Signing for Keypair {
    fn id(&self) -> OwnerType {
        match self {
            Keypair::Ed25519(pair) => OwnerType::Single(PublicKey::Ed25519(pair.public)),
            Keypair::Bls(pair) => OwnerType::Single(PublicKey::Bls(pair.public)),
            Keypair::BlsShare(share) => OwnerType::Multi(share.public_key_set.clone()),
        }
    }

    fn sign<T: Serialize>(&self, data: &T) -> Result<Signature> {
        let bytes = bincode::serialize(data).map_err(|e| Error::Serialisation(e.to_string()))?;
        Ok(self.sign(&bytes))
    }

    fn verify<T: Serialize>(&self, signature: &Signature, data: &T) -> bool {
        self.id().verify(signature, data)
    }
}

/// Wrapper for different keypair types.
#[derive(Serialize, Deserialize, Clone, custom_debug::Debug)]
pub enum Keypair {
    Ed25519(#[debug(skip)] Arc<ed25519_dalek::Keypair>),
    Bls(Arc<BlsKeypair>),
    BlsShare(Arc<BlsKeypairShare>),
}

// Need to manually implement this due to a missing impl in `Ed25519::Keypair`.
impl PartialEq for Keypair {
    fn eq(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Ed25519(keypair), Self::Ed25519(other_keypair)) => {
                // TODO: After const generics land, remove the `to_vec()` calls.
                keypair.to_bytes().to_vec() == other_keypair.to_bytes().to_vec()
            }
            (Self::BlsShare(keypair), Self::BlsShare(other_keypair)) => keypair == other_keypair,
            (Self::Bls(keypair), Self::Bls(other_keypair)) => keypair == other_keypair,
            _ => false,
        }
    }
}

// Need to manually implement this due to a missing impl in `Ed25519::Keypair`.
impl Eq for Keypair {}

impl Keypair {
    /// Constructs a random Ed25519 keypair.
    pub fn new_ed25519() -> Self {
        let mut rng = rand_07::thread_rng();
        let keypair = ed25519_dalek::Keypair::generate(&mut rng);
        Self::Ed25519(Arc::new(keypair))
    }

    pub fn new_bls() -> Self {
        let sk = bls::SecretKey::random();
        let pk = sk.public_key();
        let keypair = BlsKeypair {
            secret: SerdeSecret(sk),
            public: pk,
        };
        Self::Bls(Arc::new(keypair))
    }

    /// Constructs a BLS keypair share.
    pub fn new_bls_share(
        index: usize,
        secret_share: bls::SecretKeyShare,
        public_key_set: PublicKeySet,
    ) -> Self {
        Self::BlsShare(Arc::new(BlsKeypairShare {
            index,
            secret: SerdeSecret(secret_share.clone()),
            public: secret_share.public_key_share(),
            public_key_set,
        }))
    }

    /// Get a BLS keypair from a hex representation of the secret key.
    ///
    /// In BLS, the public key is derivable from the secret key.
    pub fn bls_from_hex(sk_hex: &str) -> Result<Self> {
        let sk = bls::SecretKey::from_hex(sk_hex)?;
        let pk = sk.public_key();
        let keypair = BlsKeypair {
            secret: SerdeSecret(sk),
            public: pk,
        };
        Ok(Self::Bls(Arc::new(keypair)))
    }

    /// Returns the public key associated with this keypair.
    pub fn public_key(&self) -> PublicKey {
        match self {
            Self::Ed25519(keypair) => PublicKey::Ed25519(keypair.public),
            Self::Bls(keypair) => PublicKey::Bls(keypair.public),
            Self::BlsShare(keypair) => PublicKey::BlsShare(keypair.public),
        }
    }

    /// Returns the secret key associated with this keypair.
    pub fn secret_key(&self) -> Result<SecretKey> {
        match self {
            Self::Ed25519(keypair) => {
                let bytes = keypair.secret.to_bytes();
                match ed25519_dalek::SecretKey::from_bytes(&bytes) {
                    Ok(sk) => Ok(SecretKey::Ed25519(sk)),
                    Err(_) => Err(Error::FailedToParse(
                        "Could not deserialise Ed25519 secret key".to_string(),
                    )),
                }
            }
            Self::Bls(keypair) => Ok(SecretKey::Bls(keypair.secret.clone())),
            Self::BlsShare(keypair) => Ok(SecretKey::BlsShare(keypair.secret.clone())),
        }
    }

    /// Signs with the underlying keypair.
    pub fn sign(&self, data: &[u8]) -> Signature {
        match self {
            Self::Ed25519(keypair) => Signature::Ed25519(keypair.sign(data)),
            Self::Bls(keypair) => Signature::Bls(keypair.secret.sign(data)),
            Self::BlsShare(keypair) => Signature::BlsShare(SignatureShare {
                index: keypair.index,
                share: keypair.secret.sign(data),
            }),
        }
    }

    pub fn to_hex(&self) -> Result<(String, String)> {
        let pk = self.public_key();
        let pk_hex = match pk {
            PublicKey::Ed25519(key) => hex::encode(key.to_bytes()),
            PublicKey::Bls(key) => key.to_hex(),
            PublicKey::BlsShare(key) => hex::encode(key.to_bytes()),
        };
        let sk = self.secret_key()?;
        let sk_hex = match sk {
            SecretKey::Ed25519(key) => hex::encode(key.to_bytes()),
            SecretKey::Bls(key) => key.to_hex(),
            SecretKey::BlsShare(key) => key.inner().reveal(),
        };
        Ok((pk_hex, sk_hex))
    }
}

impl From<ed25519_dalek::SecretKey> for Keypair {
    fn from(secret: ed25519_dalek::SecretKey) -> Self {
        let keypair = ed25519_dalek::Keypair {
            public: (&secret).into(),
            secret,
        };

        Self::Ed25519(Arc::new(keypair))
    }
}

/// BLS keypair share.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize, custom_debug::Debug)]
pub struct BlsKeypairShare {
    /// Share index.
    pub index: usize,
    /// Secret key share.
    #[debug(skip)]
    pub secret: SerdeSecret<bls::SecretKeyShare>,
    /// Public key share.
    pub public: bls::PublicKeyShare,
    /// Public key set. Necessary for producing proofs.
    pub public_key_set: PublicKeySet,
}

/// BLS keypair.
#[derive(Clone, Eq, PartialEq, Serialize, Deserialize, custom_debug::Debug)]
pub struct BlsKeypair {
    pub public: bls::PublicKey,
    pub secret: SerdeSecret<bls::SecretKey>,
}

#[cfg(test)]
mod tests {
    use super::super::super::utils;
    use super::*;

    #[test]
    fn should_serialise_an_ed25519_keypair() -> Result<()> {
        let keypair = Keypair::new_ed25519();
        let encoded = utils::serialise(&keypair)?;
        let decoded: Keypair = utils::deserialise(&encoded)?;
        assert_eq!(decoded, keypair);
        Ok(())
    }

    #[test]
    fn should_serialise_a_bls_keypair() -> Result<()> {
        let keypair = Keypair::new_bls();
        let encoded = utils::serialise(&keypair)?;
        let decoded: Keypair = utils::deserialise(&encoded)?;
        assert_eq!(decoded, keypair);
        Ok(())
    }

    #[test]
    fn should_serialise_a_bls_share() -> Result<()> {
        let bls_secret_key = bls::SecretKeySet::random(1, &mut rand::thread_rng());
        let keypair = Keypair::new_bls_share(
            0,
            bls_secret_key.secret_key_share(0),
            bls_secret_key.public_keys(),
        );
        let encoded = utils::serialise(&keypair)?;
        let decoded: Keypair = utils::deserialise(&encoded)?;
        assert_eq!(decoded, keypair);
        Ok(())
    }

    #[test]
    fn to_hex_should_convert_ed25519_keypair_to_hex() -> Result<()> {
        let keypair = Keypair::new_ed25519();
        let pk = keypair.public_key();

        let (pk_hex, sk_hex) = keypair.to_hex()?;

        let pk_from_hex = PublicKey::ed25519_from_hex(&pk_hex)?;
        let result = SecretKey::ed25519_from_hex(&sk_hex);
        // Two SecretKey types can't be compared, so just assert that parsing the string from hex
        // was successful.
        assert!(result.is_ok());
        assert_eq!(pk, pk_from_hex);
        Ok(())
    }

    #[test]
    fn to_hex_should_convert_bls_keypair_to_hex() -> Result<()> {
        let keypair = Keypair::new_bls();
        let (pk_hex, sk_hex) = keypair.to_hex()?;
        let _ =
            bls::PublicKey::from_hex(&pk_hex).map_err(|e| Error::Serialisation(e.to_string()))?;
        let _ =
            bls::SecretKey::from_hex(&sk_hex).map_err(|e| Error::Serialisation(e.to_string()))?;
        Ok(())
    }

    #[test]
    fn bls_from_hex_should_deserialize_hex_to_keypair() -> Result<()> {
        match Keypair::bls_from_hex(
            "374776f600eded836298292746edab504c079e1b9c9d36ac2c4a16dc6f2c0c22",
        ) {
            Ok(keypair) => match keypair {
                Keypair::Bls(_) => Ok(()),
                Keypair::BlsShare(_) => Err(Error::Serialisation(
                    "A BlsKeypair should be returned".to_string(),
                )),
                Keypair::Ed25519(_) => Err(Error::Serialisation(
                    "A BlsKeypair should be returned".to_string(),
                )),
            },
            Err(e) => Err(Error::Serialisation(e.to_string())),
        }
    }
}
