use std::fmt;
use std::fs;
use std::path::Path;
use std::process::Command;

use anyhow::{Context, Result, anyhow, bail};
use venues::Venue;

#[derive(Clone, Copy, Debug)]
pub enum Scope {
    Read,
    Withdraw,
    Trade,
}

impl Scope {
    fn id(self) -> &'static str {
        match self {
            Scope::Read => "read",
            Scope::Withdraw => "withdraw",
            Scope::Trade => "trade",
        }
    }
}

pub struct Credentials {
    pub api_key: String,
    pub secret: String,
    pub passphrase: Option<String>,
}

impl fmt::Debug for Credentials {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str("Credentials { .. }")
    }
}

impl Credentials {
    pub fn load(venue: Venue, scope: Scope) -> Result<Self> {
        let service = format!("federbot-{}-{}", venue.id(), scope.id());
        let field = |account: &str| secret(&service, account);
        Ok(Credentials {
            api_key: field("api_key")?,
            secret: field("secret")?,
            passphrase: match venue {
                Venue::Okx => Some(field("passphrase")?),
                _ => None,
            },
        })
    }
}

const SECRETS_DIR: &str = "/run/secrets";

pub fn secret(service: &str, account: &str) -> Result<String> {
    let file = Path::new(SECRETS_DIR).join(service);
    let value = if file.exists() {
        let text =
            fs::read_to_string(&file).with_context(|| format!("odczyt {}", file.display()))?;
        field(&text, account).with_context(|| format!("brak {account}= w {}", file.display()))
    } else if cfg!(target_os = "macos") {
        keychain(service, account)
            .with_context(|| format!("brak wpisu {service} / {account} w pęku kluczy"))
    } else {
        Err(anyhow!("brak pliku {} (Docker secret)", file.display()))
    }?;
    if value.is_empty() {
        bail!("pusty wpis {service} / {account}");
    }
    Ok(value)
}

fn field(text: &str, account: &str) -> Option<String> {
    text.lines().find_map(|line| {
        let (key, value) = line.split_once('=')?;
        (key.trim() == account).then(|| value.trim().to_owned())
    })
}

fn keychain(service: &str, account: &str) -> Result<String> {
    let output = Command::new("/usr/bin/security")
        .args(["find-generic-password", "-s", service, "-a", account, "-w"])
        .output()
        .context("uruchomienie /usr/bin/security")?;
    if !output.status.success() {
        bail!("security: kod {}", output.status);
    }
    let value = String::from_utf8(output.stdout).context("wpis nie jest tekstem")?;
    Ok(value.trim().to_owned())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn reads_a_field_of_a_secret_file() {
        let text = "api_key=abc\n secret = x=y= \npassphrase=\n";
        assert_eq!(field(text, "api_key").as_deref(), Some("abc"));
        assert_eq!(field(text, "secret").as_deref(), Some("x=y="));
        assert_eq!(field(text, "passphrase").as_deref(), Some(""));
        assert_eq!(field(text, "token"), None);
    }
}
