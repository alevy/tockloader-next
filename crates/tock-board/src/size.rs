use serde::{Deserialize, Serialize};

/// A size or address in a board file: an integer (TOML allows `0x` hex) or a
/// string such as `"256K"`, `"1M"` or `"0x40000"`.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize)]
#[serde(transparent)]
pub struct Size(pub u64);

impl Size {
    pub fn parse(s: &str) -> Option<u64> {
        let s = s.trim().replace('_', "");
        let (digits, mult) = match s.chars().last()? {
            'K' | 'k' => (&s[..s.len() - 1], 1024),
            'M' | 'm' => (&s[..s.len() - 1], 1024 * 1024),
            _ => (&s[..], 1),
        };
        let n = match digits
            .strip_prefix("0x")
            .or_else(|| digits.strip_prefix("0X"))
        {
            Some(hex) => u64::from_str_radix(hex, 16).ok()?,
            None => digits.parse().ok()?,
        };
        n.checked_mul(mult)
    }
}

impl<'de> Deserialize<'de> for Size {
    fn deserialize<D: serde::Deserializer<'de>>(d: D) -> Result<Size, D::Error> {
        #[derive(Deserialize)]
        #[serde(untagged)]
        enum Raw {
            Int(u64),
            Str(String),
        }
        match Raw::deserialize(d)? {
            Raw::Int(n) => Ok(Size(n)),
            Raw::Str(s) => Size::parse(&s).map(Size).ok_or_else(|| {
                serde::de::Error::custom(format!(
                    "invalid size `{s}` (expected e.g. 4096, 0x1000, \"4K\", \"1M\")"
                ))
            }),
        }
    }
}

#[cfg(test)]
mod tests;
