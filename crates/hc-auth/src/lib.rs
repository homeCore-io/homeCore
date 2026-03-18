//! `hc-auth` — multi-user authentication and authorisation for HomeCore.

pub mod jwt;
pub mod mqtt_creds;
pub mod password;
pub mod user;

pub use jwt::{Claims, JwtService};
pub use mqtt_creds::{MqttCredential, MqttCredStore};
pub use password::{hash_password, verify_password};
pub use user::{Role, User, UserInfo};
