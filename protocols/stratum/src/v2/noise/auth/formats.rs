// Copyright (C) 2021  Braiins Systems s.r.o.
//
// This file is part of Braiins Open-Source Initiative (BOSI).
//
// BOSI is free software: you can redistribute it and/or modify
// it under the terms of the GNU General Public License as published by
// the Free Software Foundation, either version 3 of the License, or
// (at your option) any later version.
//
// This program is distributed in the hope that it will be useful,
// but WITHOUT ANY WARRANTY; without even the implied warranty of
// MERCHANTABILITY or FITNESS FOR A PARTICULAR PURPOSE.  See the
// GNU General Public License for more details.
//
// You should have received a copy of the GNU General Public License
// along with this program.  If not, see <https://www.gnu.org/licenses/>.
//
// Please, keep in mind that we may also license BOSI or any part thereof
// under a proprietary license. For more information on the terms and conditions
// of such proprietary license or if you have any other questions, please
// contact us at opensource@braiins.com.

//! All formats that need to be persisted as physical files, too

// use ed25519_dalek::ed25519::signature::Signature;
use serde::{Deserialize, Serialize};
use std::convert::TryFrom;
use std::fmt;
use std::time::SystemTime;

use ed25519_dalek::ed25519::signature::Signature;

use super::{SignatureNoiseMessage, SignedPart, SignedPartHeader};
use crate::error::{Error, Result};
use crate::v2::noise::{StaticPublicKey, StaticSecretKey};

/// Generates implementation for the encoded type, Display trait and the file format and
macro_rules! impl_basic_type {
    ($encoded_struct_type:tt, $format_struct_type:ident, $inner_encoded_struct_type:ty,
     $format_struct_inner_rename:expr, $( $tr:tt ), *) => {
        /// Helper that ensures serialization of the `$inner_encoded_struct_type` into a prefered
        /// encoding
        #[derive(Serialize, Deserialize, Debug, $( $tr ), *)]
        #[serde(into = "String", try_from = "String")]
        pub struct $encoded_struct_type {
            inner: $inner_encoded_struct_type,
        }
        impl $encoded_struct_type {
            pub fn new(inner: $inner_encoded_struct_type) -> Self {
                Self { inner }
            }

            pub fn into_inner(self) -> $inner_encoded_struct_type {
                self.inner
            }
        }
        impl fmt::Display for $encoded_struct_type {
            fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
                write!(f, "{}", String::from(self.clone()))
            }
        }
        #[derive(Serialize, Deserialize, Debug, Clone, PartialEq)]
        pub struct $format_struct_type {
            #[serde(rename = $format_struct_inner_rename)]
            inner: $encoded_struct_type,
        }
        impl $format_struct_type {
            pub fn new(inner: $inner_encoded_struct_type) -> Self {
                Self {
                    inner: $encoded_struct_type::new(inner),
                }
            }

            pub fn from_reader<T>(reader: T) -> Result<Self>
            where
                T: std::io::Read,
            {
                serde_json::from_reader(reader).map_err(Into::into)
            }

            pub fn into_inner(self) -> $inner_encoded_struct_type {
                self.inner.into_inner()
            }
        }
        impl TryFrom<String> for $format_struct_type {
            type Error = Error;

            fn try_from(value: String) -> Result<Self> {
                serde_json::from_str(value.as_str()).map_err(Into::into)
            }
        }
        /// Helper serializer into string
        impl TryFrom<$format_struct_type> for String {
            type Error = Error;
            fn try_from(value: $format_struct_type) -> Result<String> {
                serde_json::to_string_pretty(&value).map_err(Into::into)
            }
        }
    };
}

/// Generates implementation of conversions from/to Base58 encoding that we use for representing
/// Ed25519 keys, signatures etc.
macro_rules! generate_ed25519_structs {
    ($encoded_struct_type:tt, $format_struct_type:ident, $inner_encoded_struct_type:ty,
     $format_struct_inner_rename:expr, $( $tr:tt ), *) => {
        impl_basic_type!(
            $encoded_struct_type,
            $format_struct_type,
            $inner_encoded_struct_type,
            $format_struct_inner_rename,
            $($tr), *
        );

        impl TryFrom<String> for $encoded_struct_type {
            type Error = Error;

            fn try_from(value: String) -> Result<Self> {
                // Decode with checksum, don't verify version
                let bytes = bs58::decode(value).with_check(None).into_vec()?;
                Ok(Self::new(<$inner_encoded_struct_type>::from_bytes(&bytes)?))
            }
        }

        impl From<$encoded_struct_type> for String {
            fn from(value: $encoded_struct_type) -> Self {
                bs58::encode(&value.into_inner().to_bytes()[..]).with_check().into_string()
            }
        }
    };
}

macro_rules! generate_noise_keypair_structs {
    ($encoded_struct_type:tt, $format_struct_type: ident, $inner_encoded_struct_type:ty,
     $format_struct_inner_rename:expr) => {
        impl_basic_type!(
            $encoded_struct_type,
            $format_struct_type,
            $inner_encoded_struct_type,
            $format_struct_inner_rename,
            PartialEq,
            Clone
        );

        impl TryFrom<String> for $encoded_struct_type {
            type Error = Error;

            fn try_from(value: String) -> Result<Self> {
                let bytes = bs58::decode(value).with_check(None).into_vec()?;
                Ok(Self::new(bytes))
            }
        }

        impl From<$encoded_struct_type> for String {
            fn from(value: $encoded_struct_type) -> Self {
                bs58::encode(&value.into_inner()).with_check().into_string()
            }
        }
    };
}

generate_ed25519_structs!(
    EncodedEd25519PublicKey,
    Ed25519PublicKeyFormat,
    ed25519_dalek::PublicKey,
    "ed25519_public_key",
    PartialEq,
    Clone
);

generate_ed25519_structs!(
    EncodedEd25519SecretKey,
    Ed25519SecretKeyFormat,
    ed25519_dalek::SecretKey,
    "ed25519_secret_key",
);

/// Required by serde's Serialize trait, `ed25519_dalek::SecretKey` doesn't support
/// clone
impl Clone for EncodedEd25519SecretKey {
    fn clone(&self) -> Self {
        // Cloning the secret key should never fail and is considered bug as the original private
        // key is correct
        Self::new(
            ed25519_dalek::SecretKey::from_bytes(self.inner.as_bytes())
                .expect("BUG: cannot clone secret key"),
        )
    }
}

/// Required only to comply with the required interface of impl_ed25519_encoding_conversion macro
/// that generates
impl PartialEq for EncodedEd25519SecretKey {
    fn eq(&self, other: &Self) -> bool {
        self.inner.as_bytes() == other.inner.as_bytes()
    }
}

generate_ed25519_structs!(
    EncodedEd25519Signature,
    Ed25519SignatureFormat,
    ed25519_dalek::Signature,
    "ed25519_signature",
    PartialEq,
    Clone
);

generate_noise_keypair_structs!(
    EncodedStaticPublicKey,
    StaticPublicKeyFormat,
    StaticPublicKey,
    "noise_public_key"
);

generate_noise_keypair_structs!(
    EncodedStaticSecretKey,
    StaticSecretKeyFormat,
    StaticSecretKey,
    "noise_secret_key"
);

/// Certificate is intended to be serialized and deserialized from/into a file and loaded on the
/// stratum server.
/// Second use of the certificate is to build it from `SignatureNoiseMessage` and check its
/// validity
#[derive(Serialize, Deserialize, PartialEq, Clone, Debug)]
pub struct Certificate {
    pub signed_part_header: SignedPartHeader,
    pub public_key: StaticPublicKeyFormat,
    pub authority_public_key: Ed25519PublicKeyFormat,
    pub signature: Ed25519SignatureFormat,
}

impl Certificate {
    pub fn new(signed_part: SignedPart, signature: ed25519_dalek::Signature) -> Self {
        Self {
            signed_part_header: signed_part.header,
            public_key: StaticPublicKeyFormat::new(signed_part.pubkey),
            authority_public_key: Ed25519PublicKeyFormat::new(signed_part.authority_public_key),
            signature: Ed25519SignatureFormat::new(signature),
        }
    }

    // TODO research if it is possible to generate the public key via existing 'snow' API as we don't
    // want to cross the API boundary that carefully hides the underalying type of the keys
    //    /// TODO implement unit test
    //    /// Ensures that the secret key generates the same public key as the one present in this
    //    /// certificate
    //    pub fn validate_secret_key(&self, secret_key: StaticSecretKey) -> Result<StaticPublicKey> {
    //        let public_key = SecretStaticKeyFormat::new(ed25519_dalek::PublicKey::from(secret_key));
    //
    //        match public_key == self.pubkey {
    //            true => Ok(public_key.into_inner()),
    //            false => Err(ErrorKind::Noise(format!(
    //                "Invalid certificate: public key({}) doesn't match public key({}) generated from \
    //                 secret key",
    //                public_key.inner, self.pubkey.inner,
    //            ))
    //            .into()),
    //        }
    //    }

    /// See  https://docs.rs/ed25519-dalek/1.0.1/ed25519_dalek/struct.PublicKey.html on
    /// details for the strict verification.
    /// Returns expiration timestamp stated in certificate represented as SystemTime
    pub fn validate<FN>(&self, get_current_time: FN) -> Result<SystemTime>
    where
        FN: FnOnce() -> SystemTime,
    {
        let signed_part = SignedPart::new(
            self.signed_part_header.clone(),
            self.public_key.clone().into_inner(),
            self.authority_public_key.clone().into_inner(),
        );
        signed_part.verify(&self.signature.clone().into_inner())?;
        signed_part.verify_expiration(get_current_time())
    }

    pub fn from_noise_message(
        signature_noise_message: SignatureNoiseMessage,
        pubkey: StaticPublicKey,
        authority_public_key: ed25519_dalek::PublicKey,
    ) -> Self {
        Self::new(
            SignedPart::new(signature_noise_message.header, pubkey, authority_public_key),
            signature_noise_message.signature,
        )
    }

    pub fn build_noise_message(&self) -> SignatureNoiseMessage {
        SignatureNoiseMessage {
            header: self.signed_part_header.clone(),
            signature: self.signature.clone().into_inner(),
        }
    }
}

impl TryFrom<String> for Certificate {
    type Error = Error;

    fn try_from(value: String) -> Result<Self> {
        serde_json::from_str(value.as_str()).map_err(Into::into)
    }
}

impl TryFrom<Certificate> for String {
    type Error = Error;
    fn try_from(value: Certificate) -> Result<String> {
        serde_json::to_string_pretty(&value).map_err(Into::into)
    }
}

/// Server security bundle is held by the server and provided to each (noise secured) connection so
/// that it can successfully perform the noise handshake and authenticate itself to the client
/// NOTE: this struct intentionally implements Debug manually to prevent leakage of the secure key
/// into log messages
#[derive(Serialize, Deserialize, PartialEq, Clone)]
pub struct ServerSecurityBundle {
    #[serde(flatten)]
    pub certificate: Certificate,
    secret_key: StaticSecretKeyFormat,
}

impl ServerSecurityBundle {
    pub fn new(certificate: Certificate, secret_key: StaticSecretKeyFormat) -> Self {
        Self {
            certificate,
            secret_key,
        }
    }

    fn authority_pubkey(&self) -> EncodedEd25519PublicKey {
        EncodedEd25519PublicKey::new(self.certificate.authority_public_key.clone().into_inner())
    }

    pub fn read_from_string(raw_bundle: &str) -> Result<Self> {
        let bundle = serde_json::from_str::<Self>(raw_bundle)?;
        Ok(bundle)
    }

    pub fn read_from_strings(certificate: &str, secret_key: &str) -> Result<Self> {
        let bundle = serde_json::from_str::<Certificate>(certificate).and_then(|certificate| {
            serde_json::from_str::<StaticSecretKeyFormat>(secret_key).map(|secret_key| Self {
                certificate,
                secret_key,
            })
        })?;
        Ok(bundle)
    }

    /// Returns remaining time of certificate validity or error if the certificate has expired
    /// ```
    /// use std::time::{Duration, UNIX_EPOCH};
    /// use ii_stratum::v2::noise::auth::ServerSecurityBundle;
    /// let ctx = ServerSecurityBundle::read_from_string(r#"{
    ///   "signed_part_header": {
    ///     "version": 0,
    ///     "valid_from": 1612897727,
    ///     "not_valid_after": 1612954827
    ///   },
    ///   "public_key": {
    ///     "noise_public_key": "2Nki8zRNjrYLdcGbRLFrTbwLsDfKSiDMsiK3UWGTJNJpaPjAZW"
    ///   },
    ///   "authority_public_key": {
    ///     "ed25519_public_key": "2eMjqMKXXFjhY1eAdvnmhk3xuWYdPpawYSWXXabPxVmCdeuWx"
    ///   },
    ///   "signature": {
    ///     "ed25519_signature": "ZAefGhUNHn6u26Vob5T4UM32mH9Wujx7oDR1bmf4ei6cVNvrFtbaNkSvdRyJz13KdU92tK3DrdcG4AwfSAuj7MXRFdKLE"
    ///   },
    ///   "secret_key": {
    ///     "noise_secret_key": "2owBcKCGg7k46rTUYEwNEKJsnT2TqYDtFsMAuicrsLXhi3VwK4"
    ///   }
    /// }"#).expect("BUG: Failed to parse certificate");
    ///
    /// let time_before_expiration = || UNIX_EPOCH + Duration::from_secs(1612954826);
    /// let time_after_expiration = || UNIX_EPOCH + Duration::from_secs(1612954828);
    ///
    /// assert!(
    ///     ctx.validate_by_time(time_before_expiration).is_ok(),
    ///     "BUG: Certificate should be valid"
    /// );
    /// assert!(
    ///     ctx.validate_by_time(time_after_expiration).is_err(),
    ///     "BUG: Certificate shouldn't be valid"
    /// );
    /// ```
    pub fn validate_by_time<FN>(&self, get_current_time: FN) -> Result<SystemTime>
    where
        FN: FnOnce() -> SystemTime,
    {
        self.certificate
            .validate(get_current_time)
            .map_err(|_| Error::Noise("Time validation failed".into()))
    }
}
/// Show certificate authority public key and expiry timestamp
/// ```
/// use ii_stratum::v2::noise::auth::ServerSecurityBundle;
/// let ctx = ServerSecurityBundle::read_from_string(r#"{
///   "signed_part_header": {
///     "version": 0,
///     "valid_from": 1613145976,
///     "not_valid_after": 2477145976
///   },
///   "public_key": {
///     "noise_public_key": "2Nki8zRNjrYLdcGbRLFrTbwLsDfKSiDMsiK3UWGTJNJpaPjAZW"
///   },
///   "authority_public_key": {
///     "ed25519_public_key": "2eMjqMKXXFjhY1eAdvnmhk3xuWYdPpawYSWXXabPxVmCdeuWx"
///   },
///   "signature": {
///     "ed25519_signature": "AdrgZxKNM3wCQmv5q3aTn8T96DV6egAYYFQRgcxuQjfiKvraR2xp3pNLRuDTvwQApYZc6YXnwbxXzUdHbGxaxSMq4g67c"
///   },
///   "secret_key": {
///     "noise_secret_key": "2owBcKCGg7k46rTUYEwNEKJsnT2TqYDtFsMAuicrsLXhi3VwK4"
///   }
/// }"#).expect("BUG: Failed to parse certificate");
/// assert_eq!(
///     format!("{:?}", ctx),
///     String::from(
///r#"ServerSecurityBundle { certificate_authority: "2eMjqMKXXFjhY1eAdvnmhk3xuWYdPpawYSWXXabPxVmCdeuWx", certificate_expiry: "2477145976" }"#)
/// );
///
/// ```
impl fmt::Debug for ServerSecurityBundle {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        let certificate_authority = self.authority_pubkey();
        let expiry_timestamp = self.certificate.validate(SystemTime::now).map_or_else(
            |_| "certificate is invalid".to_owned(),
            |t| {
                let expiration_time = t
                    .duration_since(std::time::UNIX_EPOCH)
                    .expect("BUG: Invalid expiry date");
                format!("{:?}", expiration_time.as_secs())
            },
        );
        f.debug_struct("ServerSecurityBundle")
            .field("certificate_authority", &certificate_authority.to_string())
            .field("certificate_expiry", &expiry_timestamp)
            .finish()
    }
}

#[cfg(test)]
pub mod test {
    use super::super::test::build_test_signed_part_and_auth;
    use super::*;

    #[test]
    fn certificate_validate() {
        let (signed_part, _authority_keypair, _static_keypair, signature) =
            build_test_signed_part_and_auth();
        let certificate = Certificate::new(signed_part, signature);

        certificate
            .validate(SystemTime::now)
            .expect("BUG: Certificate not valid!");
    }

    #[test]
    fn certificate_serialization() {
        let (signed_part, _authority_keypair, _static_keypair, signature) =
            build_test_signed_part_and_auth();
        let certificate = Certificate::new(signed_part, signature);

        // TODO fix test to use the serialization methods!
        let serialized_cert =
            serde_json::to_string(&certificate).expect("BUG: cannot serialize certificate");
        let deserialized_cert = serde_json::from_str(serialized_cert.as_str())
            .expect("BUG: cannot deserialized certificate");

        assert_eq!(certificate, deserialized_cert, "Certificates don't match!");
    }
}
