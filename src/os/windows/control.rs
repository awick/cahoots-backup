// Copyright Andy Wick
// SPDX-License-Identifier: Apache-2.0

//! The daemon's control channel. Not written yet: it will be a named pipe open to this
//! user alone.

use anyhow::{Result, bail};
use std::path::Path;
use tokio::net::windows::named_pipe::{NamedPipeClient, NamedPipeServer};

/// A connection to the daemon.
pub type Stream = NamedPipeClient;

pub async fn connect(_home: &Path) -> Option<Stream> {
    None
}

#[cfg(feature = "gui")]
pub fn connect_blocking(_home: &Path) -> Option<std::fs::File> {
    None
}

pub struct Listener(());

impl Listener {
    pub async fn accept(&self) -> std::io::Result<NamedPipeServer> {
        std::future::pending().await
    }
}

pub fn listen(_home: &Path) -> Result<Listener> {
    bail!("the daemon does not run on Windows yet")
}

pub fn unlisten(_home: &Path) {}
