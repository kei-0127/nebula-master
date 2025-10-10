use anyhow::{Error, Result};
use reqwest;
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::fs;
use std::io::Write;
use std::process::Command;
use tera::{self, Context, Tera};
use thiserror::Error;

#[derive(Deserialize, Serialize, Debug)]
struct DiscoveryDirectoryList {
    items: Vec<DiscoveryItem>,
}

#[derive(Deserialize, Serialize, Debug)]
pub struct DiscoveryItem {
    kind: String,
    id: String,
    name: String,
    version: String,
    title: String,
    description: String,
    discoveryRestUrl: String,
}

#[derive(Deserialize, Serialize, Debug)]
struct DiscoveryApi {
    resources: HashMap<String, DiscoveryApiResource>,
    schemas: HashMap<String, DiscoveryApiSchema>,
}

#[derive(Deserialize, Serialize, Debug)]
struct DiscoveryApiResource {
    methods: Option<HashMap<String, DiscoveryApiMethod>>,
}

#[derive(Deserialize, Serialize, Debug)]
struct DiscoveryApiMethod {
    httpMethod: String,
    request: Option<DiscoveryApiRef>,
    response: Option<DiscoveryApiRef>,
    path: String,
}

#[derive(Deserialize, Serialize, Debug)]
struct DiscoveryApiRef {
    #[serde(rename(deserialize = "$ref"))]
    reference: String,
}

#[derive(Deserialize, Serialize, Debug)]
struct DiscoveryApiSchema {
    #[serde(rename(deserialize = "type"))]
    kind: String,
    properties: Option<serde_json::Value>,
}

#[derive(Debug, Error)]
pub enum DiscoveryError {
    #[error("invalid discovery")]
    InvalidDiscovery,
}

pub fn capitalize(
    value: &tera::Value,
    _: &HashMap<String, tera::Value>,
) -> Result<tera::Value, tera::Error> {
    let s = tera::try_get_value!("capitalize", "value", String, value);
    let mut chars = s.chars();
    match chars.next() {
        None => Ok(tera::to_value("").unwrap()),
        Some(f) => {
            let res = f.to_uppercase().collect::<String>() + &chars.as_str();
            Ok(tera::to_value(&res).unwrap())
        }
    }
}

pub fn discover() -> Result<(), Error> {
    let _ = fs::remove_dir_all("gen");
    let _ = fs::create_dir("gen");
    let list: DiscoveryDirectoryList =
        reqwest::blocking::get("https://www.googleapis.com/discovery/v1/apis/")?
            .json()?;
    for item in list.items.iter() {
        if let Err(e) = discovery_item(&item) {
            println!("{}", e);
        }
    }
    Ok(())
}

pub fn discovery_item(item: &DiscoveryItem) -> Result<(), Error> {
    match item.name.as_ref() {
        "texttospeech" => (),
        "storage" => (),
        _ => return Ok(()),
    }
    let mut tera = Tera::new("templates/*").unwrap();
    tera.register_filter("capitalize", capitalize);
    let api: serde_json::Value = reqwest::blocking::get(&item.discoveryRestUrl)?
        .json()
        .unwrap();
    println!("{}", &item.discoveryRestUrl);
    let name = format!(
        "{}{}",
        api["name"]
            .as_str()
            .ok_or(DiscoveryError::InvalidDiscovery)?,
        api["version"]
            .as_str()
            .ok_or(DiscoveryError::InvalidDiscovery)?
    );

    let _ = fs::create_dir(format!("gen/{}", name));
    let _ = fs::create_dir(format!("gen/{}/src", name));
    let context = Context::from_serialize(&api).unwrap();

    let lib_file_name = format!("gen/{}/src/lib.rs", name);
    let mut lib_file = fs::File::create(&lib_file_name)?;

    match tera.render("lib.j2", &context) {
        Ok(r) => {
            lib_file.write_all(r.as_bytes())?;
            let _ = Command::new("rustfmt").args(&[&lib_file_name]).output();
        }
        Err(e) => println!("{}", e),
    };

    let cargo_file_name = format!("gen/{}/Cargo.toml", name);
    let mut cargo_file = fs::File::create(&cargo_file_name)?;
    match tera.render("cargo.j2", &context) {
        Ok(r) => {
            cargo_file.write_all(r.as_bytes())?;
        }
        Err(e) => println!("{}", e),
    };
    Ok(())
}
