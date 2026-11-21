//! Windows integration: the Windows service and its pipe, the cable state
//! they share with the window, the administrator helper and the driver
//! download.

pub mod broker;
pub mod control;
pub mod download;
pub mod remote;
pub mod service;
pub mod window;

pub use broker::run_attach_broker;
pub use service::{install_service, run_service, uninstall_service};
