//! # Utilities Module
//! 
//! This module provides common utility functions used throughout the Nebula VoIP system.
//! It includes cryptographic functions, random number generation, UUID generation,
//! and system information utilities.
//! 
//! ## Key Features
//! 
//! ### Cryptographic Functions
//! - **MD5**: MD5 hash generation for legacy compatibility
//! - **SHA1**: SHA-1 hash generation for checksums
//! - **SHA256**: SHA-256 hash generation for security
//! 
//! ### Random Generation
//! - **Random Numbers**: Secure random number generation
//! - **Random Strings**: Alphanumeric string generation
//! - **Random Bytes**: Cryptographically secure random bytes
//! 
//! ### UUID Generation
//! - **UUID v4**: Random UUID generation
//! - **UUID v5**: Namespace-based UUID generation
//! 
//! ### System Information
//! - **Hostname**: System hostname retrieval
//! - **Local IP**: Primary local IP address detection
//! 
//! ## Usage
//! 
//! ```rust
//! use nebula_utils::{sha256, uuid, get_local_ip};
//! 
//! // Generate hash
//! let hash = sha256("hello world");
//! 
//! // Generate UUID
//! let id = uuid();
//! 
//! // Get system information
//! let ip = get_local_ip();
//! ```

use std::process::Command;

use crypto::digest::Digest;
use crypto::md5::Md5;
use crypto::sha1::Sha1;
use crypto::sha2::Sha256;
use rand::distributions::Alphanumeric;
use rand::Rng;
use uuid::Uuid;

pub fn md5(input: &str) -> String {
    let mut hasher = Md5::new();
    hasher.input(input.as_bytes());
    hasher.result_str()
}

pub fn sha256(input: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.input(input.as_bytes());
    hasher.result_str()
}

pub fn sha1(input: &str) -> String {
    let mut hasher = Sha1::new();
    hasher.input(input.as_bytes());
    hasher.result_str()
}

pub fn uuid() -> String {
    Uuid::new_v4().to_string()
}

pub fn uuid_v5(input: &str) -> Uuid {
    Uuid::new_v5(&Uuid::NAMESPACE_X500, input.as_bytes())
}

pub fn rand_i32() -> i32 {
    let mut rng = rand::thread_rng();
    let n: i32 = rng.gen();
    n
}

pub fn rand_number(n: usize) -> String {
    rand::thread_rng()
        .gen_range(10usize.pow(n as u32 - 1), 10usize.pow(n as u32) - 1)
        .to_string()
}

pub fn rand_string(n: usize) -> String {
    rand::thread_rng()
        .sample_iter(&Alphanumeric)
        .take(n)
        .collect::<String>()
        .to_lowercase()
}

pub fn rand_bytes(n: usize) -> Vec<u8> {
    (0..n).map(|_| rand::random::<u8>()).collect()
}

pub fn get_hostname() -> Option<String> {
    let output = match Command::new("hostname").output() {
        Ok(ok) => ok,
        Err(_) => {
            return None;
        }
    };

    let stdout = match String::from_utf8(output.stdout) {
        Ok(ok) => ok,
        Err(_) => {
            return None;
        }
    };

    Some(stdout.trim().to_string())
}

pub fn get_local_ip() -> Option<String> {
    let output = match Command::new("hostname").args(["-I"]).output() {
        Ok(ok) => ok,
        Err(_) => {
            return None;
        }
    };

    let stdout = match String::from_utf8(output.stdout) {
        Ok(ok) => ok,
        Err(_) => {
            return None;
        }
    };

    let ips: Vec<&str> = stdout.trim().split(' ').collect::<Vec<&str>>();
    let first = ips.first();
    match first {
        Some(first) => {
            if !first.is_empty() {
                Some(first.to_string())
            } else {
                None
            }
        }
        None => None,
    }
}
