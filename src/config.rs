use anyhow::{Context, Result, anyhow};
use evdev::KeyCode;
use serde::Deserialize;
use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::str::FromStr;

#[derive(Deserialize, Default)]
struct RawConfig {
    #[serde(default)]
    keys: RawKeysConfig,
}

#[derive(Deserialize, Default)]
struct RawKeysConfig {
    #[serde(default)]
    block: Vec<String>,
    #[serde(default)]
    remap: HashMap<String, String>,
}

#[derive(Default)]
pub struct Config {
    block: HashSet<u16>,
    remap: HashMap<u16, u16>,
}

fn parse_key(name: &str) -> Result<u16> {
    let code = KeyCode::from_str(name).map_err(|_| anyhow!("unknown key name: {name}"))?;
    Ok(code.0)
}

impl Config {
    pub fn load(path: &Path) -> Result<Self> {
        let text =
            std::fs::read_to_string(path).with_context(|| format!("reading {}", path.display()))?;
        let raw: RawConfig =
            toml::from_str(&text).with_context(|| format!("parsing {}", path.display()))?;

        let block = raw
            .keys
            .block
            .iter()
            .map(|n| parse_key(n))
            .collect::<Result<_>>()?;
        let remap = raw
            .keys
            .remap
            .iter()
            .map(|(f, t)| Ok((parse_key(f)?, parse_key(t)?)))
            .collect::<Result<_>>()?;

        Ok(Self { block, remap })
    }

    pub fn transform_key(&self, raw_code: u32) -> Option<u32> {
        let code = raw_code as u16;
        if self.block.contains(&code) {
            return None;
        }
        if let Some(&mapped) = self.remap.get(&code) {
            return Some(mapped as u32);
        }
        Some(raw_code)
    }
}
