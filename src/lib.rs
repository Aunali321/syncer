//! Peer-to-peer folder sync between your own machines that understands Git.
//! DESIGN.md explains how it works.

pub mod cluster;
pub mod control;
pub mod folder;
pub mod git;
pub mod home;
pub mod ids;
pub mod net;
pub mod report;
pub mod service;
pub mod sync;
pub mod syncthing;
pub mod util;
