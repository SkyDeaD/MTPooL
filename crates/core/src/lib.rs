//! Pure pool logic without I/O.

pub mod health;
pub mod links;
pub mod settings;
pub mod types;

pub use health::{FailedCondition, GpStatus, Latch, NodeHealth, failed_conditions};
pub use links::build_proxy_link;
pub use settings::{GlobalpingSettings, ProxyUser, Settings, SettingsError, SrmdSettings};
pub use types::{
    EngineKind, Generation, IdError, NodeId, NodeState, OwnerId, Ts, is_public_ipv4, mask_ip,
};
