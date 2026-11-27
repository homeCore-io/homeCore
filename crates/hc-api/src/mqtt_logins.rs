//! Logins for devices that talk to homeCore's broker themselves: a Tasmota,
//! a Shelly, Zigbee2MQTT. One per device, made in Settings › Devices over
//! MQTT, its password shown once.
//!
//! Kept in `logins.toml` in the profiles folder (owner-only), each password
//! hashed, and checked by the broker as a device connects
//! ([`crate::plugin_runtimes::BrokerLogins`]), so a login works the moment it
//! is made and a removed one can't connect again. A connection already open
//! stays open until it drops.

use anyhow::{anyhow, bail, Context, Result};
use chrono::{DateTime, Utc};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Mutex;
use tracing::warn;

#[derive(Debug, Clone, Serialize, Deserialize)]
struct Stored {
    user: String,
    /// The ecosystem (profile name) it is for.
    profile: String,
    /// What it is for, in people's words: "Kitchen plug".
    #[serde(default)]
    label: String,
    hash: String,
    created: DateTime<Utc>,
}

#[derive(Debug, Default, Serialize, Deserialize)]
struct File {
    #[serde(default)]
    login: Vec<Stored>,
}

/// A device login as the app shows it: never its password.
#[derive(Debug, Clone, Serialize)]
pub struct DeviceLogin {
    pub user: String,
    pub profile: String,
    pub label: String,
    pub created: DateTime<Utc>,
    /// When it last connected, since homeCore started.
    pub last_login: Option<DateTime<Utc>>,
}

pub struct DeviceLogins {
    path: PathBuf,
    last: Mutex<HashMap<String, DateTime<Utc>>>,
    /// The client id each login last connected with, to push it off the
    /// broker when the login is removed or its password changed.
    clients: Mutex<HashMap<String, String>>,
    /// Writes one at a time.
    write: Mutex<()>,
}

/// A login's user name: what a device types in, so plain.
fn check_user(user: &str) -> Result<()> {
    let ok = (1..=64).contains(&user.len())
        && user
            .chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-'))
        && !user.starts_with('.')
        // Those are homeCore's own and its plugins'.
        && !user.starts_with("plugin.")
        && !user.starts_with("internal.")
        && !user.starts_with("core.");
    if !ok {
        bail!(
            "{user} can't be a login: letters, digits, '.', '_' and '-', up to 64, \
             and not starting with plugin., internal. or core."
        );
    }
    Ok(())
}

impl DeviceLogins {
    /// The logins in `<profiles folder>/logins.toml`.
    pub fn new(profiles_dir: impl Into<PathBuf>) -> Self {
        Self {
            path: profiles_dir.into().join("logins.toml"),
            last: Mutex::new(HashMap::new()),
            clients: Mutex::new(HashMap::new()),
            write: Mutex::new(()),
        }
    }

    /// The logins, for checking one: a file that can't be read accepts none.
    fn read(&self) -> File {
        self.read_for_change().unwrap_or_else(|e| {
            warn!(path = %self.path.display(), error = %format!("{e:#}"), "Device logins can't be read");
            File::default()
        })
    }

    /// The logins, to change: no file is none yet, but one that can't be read
    /// or doesn't parse is an error, not none — writing back a single login
    /// over it would lose every other.
    fn read_for_change(&self) -> Result<File> {
        match std::fs::read_to_string(&self.path) {
            Ok(t) => {
                toml::from_str(&t).context("logins.toml doesn't parse; fix or remove it first")
            }
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => Ok(File::default()),
            Err(e) => Err(e).context("reading logins.toml"),
        }
    }

    fn store(&self, file: &File) -> Result<()> {
        if let Some(dir) = self.path.parent() {
            std::fs::create_dir_all(dir)?;
        }
        let text = toml::to_string(file)?;
        let tmp = self.path.with_extension("toml.tmp");
        {
            use std::io::Write;
            let mut opts = std::fs::OpenOptions::new();
            opts.write(true).create(true).truncate(true);
            #[cfg(unix)]
            {
                use std::os::unix::fs::OpenOptionsExt;
                opts.mode(0o600);
            }
            let mut f = opts.open(&tmp).context("writing logins.toml")?;
            f.write_all(text.as_bytes())?;
        }
        std::fs::rename(&tmp, &self.path).context("writing logins.toml")?;
        Ok(())
    }

    pub fn list(&self, profile: Option<&str>) -> Vec<DeviceLogin> {
        let last = self.last.lock().unwrap();
        let mut out: Vec<DeviceLogin> = self
            .read()
            .login
            .into_iter()
            .filter(|l| profile.is_none_or(|p| l.profile == p))
            .map(|l| DeviceLogin {
                last_login: last.get(&l.user).copied(),
                user: l.user,
                profile: l.profile,
                label: l.label,
                created: l.created,
            })
            .collect();
        out.sort_by(|a, b| a.user.cmp(&b.user));
        out
    }

    /// Make a login; returns its password, which is not kept anywhere.
    pub fn add(&self, profile: &str, user: &str, label: &str) -> Result<String> {
        check_user(user)?;
        let _w = self.write.lock().unwrap();
        let mut file = self.read_for_change()?;
        if file.login.iter().any(|l| l.user == user) {
            bail!("there is already a login {user}");
        }
        let password = crate::plugin_runtimes::mint_mqtt_password();
        file.login.push(Stored {
            user: user.to_string(),
            profile: profile.to_string(),
            label: label.trim().to_string(),
            hash: hc_auth::api_key::hash_token(&password)?,
            created: Utc::now(),
        });
        self.store(&file)?;
        Ok(password)
    }

    /// A new password for a login; the old one stops working.
    pub fn reset(&self, user: &str) -> Result<String> {
        let _w = self.write.lock().unwrap();
        let mut file = self.read_for_change()?;
        let l = file
            .login
            .iter_mut()
            .find(|l| l.user == user)
            .ok_or_else(|| anyhow!("there is no login {user}"))?;
        let password = crate::plugin_runtimes::mint_mqtt_password();
        l.hash = hc_auth::api_key::hash_token(&password)?;
        self.store(&file)?;
        Ok(password)
    }

    pub fn remove(&self, user: &str) -> Result<()> {
        let _w = self.write.lock().unwrap();
        let mut file = self.read_for_change()?;
        let before = file.login.len();
        file.login.retain(|l| l.user != user);
        if file.login.len() == before {
            bail!("there is no login {user}");
        }
        self.store(&file)?;
        self.last.lock().unwrap().remove(user);
        Ok(())
    }

    /// Whether a connecting device's user and password are a login here.
    pub fn accepts(&self, user: &str, pass: &str) -> bool {
        if pass.is_empty() || check_user(user).is_err() {
            return false;
        }
        let ok = self
            .read()
            .login
            .iter()
            .find(|l| l.user == user)
            .is_some_and(|l| hc_auth::api_key::verify_token(pass, &l.hash));
        if ok {
            self.last
                .lock()
                .unwrap()
                .insert(user.to_string(), Utc::now());
        }
        ok
    }

    /// Note the client id a login connected with.
    pub fn connected_as(&self, user: &str, client_id: &str) {
        self.clients
            .lock()
            .unwrap()
            .insert(user.to_string(), client_id.to_string());
    }

    /// The client id a login last connected with, forgotten: to push it off
    /// the broker now that its password no longer works.
    pub fn take_client(&self, user: &str) -> Option<String> {
        self.clients.lock().unwrap().remove(user)
    }

    /// Whether `user` is a device login here (to tell it apart from others).
    pub fn has(&self, user: &str) -> bool {
        self.read().login.iter().any(|l| l.user == user)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_login_works_once_made_with_its_password_and_never_after_removal() {
        let dir = tempfile::tempdir().unwrap();
        let logins = DeviceLogins::new(dir.path());
        let pw = logins
            .add("tasmota", "tasmota-kitchen_plug", "Kitchen plug")
            .unwrap();
        assert!(logins.accepts("tasmota-kitchen_plug", &pw));
        assert!(!logins.accepts("tasmota-kitchen_plug", "wrong"));
        assert!(!logins.accepts("tasmota-kitchen_plug", ""));
        let listed = logins.list(Some("tasmota"));
        assert_eq!(listed.len(), 1);
        assert!(listed[0].last_login.is_some());
        assert!(logins.list(Some("zigbee2mqtt")).is_empty());

        // The file holds no password, and only its owner reads it.
        let text = std::fs::read_to_string(dir.path().join("logins.toml")).unwrap();
        assert!(!text.contains(&pw));
        #[cfg(unix)]
        {
            use std::os::unix::fs::PermissionsExt;
            let mode = std::fs::metadata(dir.path().join("logins.toml"))
                .unwrap()
                .permissions()
                .mode();
            assert_eq!(mode & 0o777, 0o600);
        }

        let pw2 = logins.reset("tasmota-kitchen_plug").unwrap();
        assert!(!logins.accepts("tasmota-kitchen_plug", &pw));
        assert!(logins.accepts("tasmota-kitchen_plug", &pw2));

        logins.remove("tasmota-kitchen_plug").unwrap();
        assert!(!logins.accepts("tasmota-kitchen_plug", &pw2));
    }

    /// A logins file that doesn't parse (a typo made by hand) is not empty:
    /// making a login must not write one login back over all the others.
    #[test]
    fn a_logins_file_that_doesnt_parse_is_left_alone() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("logins.toml");
        std::fs::write(&path, "[[login]\nuser = ").unwrap();
        let logins = DeviceLogins::new(dir.path());
        assert!(logins.add("tasmota", "tasmota-plug", "").is_err());
        assert!(logins.remove("anyone").is_err());
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "[[login]\nuser = ");
    }

    #[test]
    fn a_login_can_t_take_a_name_that_is_homecore_s_or_already_taken() {
        let dir = tempfile::tempdir().unwrap();
        let logins = DeviceLogins::new(dir.path());
        assert!(logins.add("x", "plugin.hue", "").is_err());
        assert!(logins.add("x", "internal.core", "").is_err());
        assert!(logins.add("x", "has space", "").is_err());
        logins.add("x", "zigbee2mqtt", "").unwrap();
        assert!(logins.add("x", "zigbee2mqtt", "").is_err());
    }
}
