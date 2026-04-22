//! `hc-auth` — multi-user authentication and authorisation for HomeCore.

pub mod actor;
pub mod api_key;
pub mod jwt;
pub mod mqtt_creds;
pub mod password;
pub mod refresh;
pub mod user;

pub use actor::Actor;
pub use jwt::{Claims, JwtService};
pub use mqtt_creds::{MqttCredStore, MqttCredential};
pub use password::{hash_password, verify_password};
pub use user::{Role, User, UserInfo};
