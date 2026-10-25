//! The FormalSpec installer. It collects the choices for a new machine, writes them as a spec
//! repo, and installs that spec onto a disk through `fspec-switch`'s `disk init`, so the machine is
//! managed by FormalSpec from its first boot.

pub mod backend;
pub mod catalog;
pub mod demo;
pub mod install;
pub mod live;
pub mod model;
pub mod secure_boot;
pub mod spec;
pub mod ui;
