use serde::{Deserialize, Deserializer};

// deserialize comma separated string into into Vec<String>
pub fn deserialize_csv<'de, D>(deserializer: D) -> Result<Vec<String>, D::Error>
where
    D: Deserializer<'de>,
{
    let s = String::deserialize(deserializer).unwrap_or_default();
    Ok(s.split(',')
        .map(str::trim)
        .filter(|s| !s.is_empty())
        .map(String::from)
        .collect())
}
