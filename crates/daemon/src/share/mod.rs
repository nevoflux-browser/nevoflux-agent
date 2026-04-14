//! Canvas share module — ID generation, password handling, owner tokens,
//! encryption, and types.

pub mod binary_format;
pub mod crypto;
pub mod owner_token;
pub mod password;
pub mod share_id;
pub mod types;

pub use binary_format::{deserialize, serialize, NfebHeader, MAGIC, VERSION};
pub use crypto::{decrypt_share_bundle, derive_key, encrypt_share_bundle};
pub use owner_token::{generate_owner_token, hash_owner_token};
pub use password::{format_password, generate_password, parse_password};
pub use share_id::{generate_share_id, validate_share_id};
pub use types::{EncryptedShareBundle, KdfParams, ShareBundle, ShareMetadata};
