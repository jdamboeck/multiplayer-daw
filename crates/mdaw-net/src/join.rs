//! The join string: everything a client needs to reach a server and to trust it.

use anyhow::{Context, Result, bail};
use std::fmt;
use std::str::FromStr;

/// `mdaw://host:port?fp=<sha256 of the certificate, hex>&token=<session secret>`
#[derive(Clone, PartialEq, Eq, Debug)]
pub struct JoinInfo {
    pub host: String,
    pub port: u16,
    pub fingerprint: [u8; 32],
    pub token: String,
}

impl fmt::Display for JoinInfo {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        // IPv6 literals need brackets to keep their colons apart from the port's.
        if self.host.contains(':') {
            write!(f, "mdaw://[{}]:{}?fp=", self.host, self.port)?;
        } else {
            write!(f, "mdaw://{}:{}?fp=", self.host, self.port)?;
        }
        self.fingerprint.iter().try_for_each(|b| write!(f, "{b:02x}"))?;
        write!(f, "&token={}", self.token)
    }
}

impl FromStr for JoinInfo {
    type Err = anyhow::Error;

    fn from_str(s: &str) -> Result<Self> {
        let rest = s
            .trim()
            .strip_prefix("mdaw://")
            .context("a join string starts with mdaw://")?;
        let (authority, query) = rest
            .split_once('?')
            .context("the join string has no ?fp=…&token=… part")?;
        let (host, port) = authority
            .rsplit_once(':')
            .context("the join string has no port")?;
        let host = host
            .strip_prefix('[')
            .and_then(|h| h.strip_suffix(']'))
            .unwrap_or(host);
        if host.is_empty() {
            bail!("the join string has no host");
        }
        let port = port.parse().with_context(|| format!("{port:?} is not a port"))?;
        let (mut fingerprint, mut token) = (None, None);
        for pair in query.split('&') {
            match pair.split_once('=') {
                Some(("fp", value)) => fingerprint = Some(parse_fingerprint(value)?),
                Some(("token", value)) => token = Some(value.to_string()),
                // Fields added later must not break older clients.
                _ => {}
            }
        }
        Ok(Self {
            host: host.to_string(),
            port,
            fingerprint: fingerprint.context("the join string has no fp= field")?,
            token: token.context("the join string has no token= field")?,
        })
    }
}

fn parse_fingerprint(hex: &str) -> Result<[u8; 32]> {
    let mut out = [0u8; 32];
    if hex.len() != 64 || !hex.is_ascii() {
        bail!("the fingerprint must be 64 hex digits");
    }
    for (byte, pair) in out.iter_mut().zip(hex.as_bytes().chunks(2)) {
        let pair = std::str::from_utf8(pair)?;
        *byte = u8::from_str_radix(pair, 16).context("the fingerprint must be 64 hex digits")?;
    }
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn round_trip() {
        let info = JoinInfo {
            host: "studio.example".into(),
            port: 7464,
            fingerprint: [0xab; 32],
            token: "s3cret".into(),
        };
        let text = info.to_string();
        assert!(text.starts_with("mdaw://studio.example:7464?fp=abab"));
        assert_eq!(text.parse::<JoinInfo>().unwrap(), info);

        let v6 = JoinInfo {
            host: "::1".into(),
            ..info
        };
        assert!(v6.to_string().starts_with("mdaw://[::1]:7464?"));
        assert_eq!(v6.to_string().parse::<JoinInfo>().unwrap(), v6);
    }

    #[test]
    fn rejects_malformed_strings() {
        let fp = "00".repeat(32);
        for bad in [
            "http://host:1?fp=00&token=x".to_string(),
            format!("mdaw://host?fp={fp}&token=x"),
            format!("mdaw://:1?fp={fp}&token=x"),
            format!("mdaw://host:99999?fp={fp}&token=x"),
            "mdaw://host:1?fp=zz&token=x".to_string(),
            "mdaw://host:1?token=x".to_string(),
            format!("mdaw://host:1?fp={fp}"),
        ] {
            assert!(bad.parse::<JoinInfo>().is_err(), "{bad}");
        }
    }
}
