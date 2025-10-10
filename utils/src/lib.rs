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
