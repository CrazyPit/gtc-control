//! Modbus client abstraction.
//!
//! Defines [`ModbusClient`] — the narrow async interface the rest of
//! the app talks to — alongside two concrete implementations:
//!
//! - [`TcpClient`] connects to a real device over Modbus TCP using
//!   the `tokio-modbus` crate.
//! - [`FakeModbusClient`] is an in-memory fake used by unit tests in
//!   the `app` layer; it lives next to the trait so test code only
//!   depends on this module rather than reaching into the integration
//!   suite.
//!
//! Callers should reach the bus through the orchestration helpers in
//! [`crate::app`] rather than holding a [`ModbusClient`] directly.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::time::Duration;

use async_trait::async_trait;
use thiserror::Error;
use tokio::net::lookup_host;
use tokio::time;
use tokio_modbus::client::Context;
use tokio_modbus::prelude::{Reader, Writer};
use tokio_modbus::slave::Slave;

use crate::config::ModbusConfig;

/// Narrow async interface over the Modbus client.
///
/// Implementors are required to be `Send` only — the `tokio-modbus`
/// `Context` boxes a non-`Sync` transport handle internally, so a
/// `Sync` bound here would exclude it. Callers in the `app` layer hold
/// a `&mut dyn ModbusClient` from a single task, so `Sync` is not
/// needed. All addresses are zero-based.
#[async_trait]
pub trait ModbusClient: Send {
    /// Read `count` holding registers starting at `address` (FC 0x03).
    ///
    /// # Errors
    /// See [`ModbusError`].
    async fn read_holding(&mut self, address: u16, count: u16) -> Result<Vec<u16>, ModbusError>;

    /// Read `count` input registers starting at `address` (FC 0x04).
    ///
    /// # Errors
    /// See [`ModbusError`].
    async fn read_input(&mut self, address: u16, count: u16) -> Result<Vec<u16>, ModbusError>;

    /// Read `count` coils starting at `address` (FC 0x01).
    ///
    /// # Errors
    /// See [`ModbusError`].
    async fn read_coils(&mut self, address: u16, count: u16) -> Result<Vec<bool>, ModbusError>;

    /// Read `count` discrete inputs starting at `address` (FC 0x02).
    ///
    /// # Errors
    /// See [`ModbusError`].
    async fn read_discrete(&mut self, address: u16, count: u16) -> Result<Vec<bool>, ModbusError>;

    /// Write a single holding register (FC 0x06).
    ///
    /// # Errors
    /// See [`ModbusError`].
    async fn write_holding(&mut self, address: u16, value: u16) -> Result<(), ModbusError>;

    /// Write a single coil (FC 0x05).
    ///
    /// # Errors
    /// See [`ModbusError`].
    async fn write_coil(&mut self, address: u16, value: bool) -> Result<(), ModbusError>;
}

/// Errors surfaced by [`ModbusClient`] implementations.
#[derive(Debug, Error)]
pub enum ModbusError {
    /// DNS lookup for `host:port` returned no results.
    #[error("could not resolve `{endpoint}` to a socket address")]
    Resolve {
        /// The `host:port` string that failed to resolve.
        endpoint: String,
    },
    /// A request did not complete inside the configured timeout.
    #[error("modbus operation timed out after {millis} ms")]
    Timeout {
        /// Configured timeout, surfaced for log clarity.
        millis: u64,
    },
    /// Underlying I/O error from the TCP transport.
    #[error("modbus transport I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Transport-layer error reported by `tokio-modbus` (framing,
    /// decode, mid-request disconnect).
    #[error("modbus transport error: {0}")]
    Transport(String),
    /// The device replied with a Modbus protocol exception (e.g.
    /// `IllegalDataAddress`, `SlaveDeviceBusy`).
    #[error("modbus exception: {0}")]
    Exception(String),
}

impl From<tokio_modbus::Error> for ModbusError {
    fn from(err: tokio_modbus::Error) -> Self {
        Self::Transport(err.to_string())
    }
}

impl From<tokio_modbus::ExceptionCode> for ModbusError {
    fn from(err: tokio_modbus::ExceptionCode) -> Self {
        Self::Exception(format!("{err:?}"))
    }
}

/// `tokio-modbus`-backed Modbus TCP client.
///
/// The connection is established lazily on the first request and held
/// across subsequent calls. If the connection drops, the next call
/// surfaces a transport error and the caller should construct a fresh
/// [`TcpClient`].
pub struct TcpClient {
    cfg: ModbusConfig,
    ctx: Option<Context>,
}

impl TcpClient {
    /// Build an unconnected client from a [`ModbusConfig`].
    #[must_use]
    pub fn new(cfg: ModbusConfig) -> Self {
        Self { cfg, ctx: None }
    }

    /// Resolve `host:port` and dial the controller, replacing any
    /// existing context.
    ///
    /// # Errors
    /// Surfaces resolution, timeout, and transport errors via
    /// [`ModbusError`].
    pub async fn connect(&mut self) -> Result<(), ModbusError> {
        let endpoint = format!("{}:{}", self.cfg.host, self.cfg.port);
        let addr: SocketAddr = lookup_host(endpoint.as_str())
            .await?
            .next()
            .ok_or_else(|| ModbusError::Resolve {
                endpoint: endpoint.clone(),
            })?;
        let timeout = Duration::from_millis(self.cfg.timeout_ms);
        let ctx = time::timeout(
            timeout,
            tokio_modbus::client::tcp::connect_slave(addr, Slave(self.cfg.unit_id)),
        )
        .await
        .map_err(|_| ModbusError::Timeout {
            millis: self.cfg.timeout_ms,
        })??;
        self.ctx = Some(ctx);
        Ok(())
    }

    async fn ctx(&mut self) -> Result<&mut Context, ModbusError> {
        if self.ctx.is_none() {
            self.connect().await?;
        }
        self.ctx.as_mut().ok_or_else(|| {
            ModbusError::Io(std::io::Error::other(
                "modbus context unexpectedly absent after connect",
            ))
        })
    }

    fn timeout(&self) -> Timeout {
        Timeout {
            duration: Duration::from_millis(self.cfg.timeout_ms),
            millis: self.cfg.timeout_ms,
        }
    }
}

#[derive(Clone, Copy)]
struct Timeout {
    duration: Duration,
    millis: u64,
}

#[async_trait]
impl ModbusClient for TcpClient {
    async fn read_holding(&mut self, address: u16, count: u16) -> Result<Vec<u16>, ModbusError> {
        let t = self.timeout();
        let ctx = self.ctx().await?;
        Ok(
            time::timeout(t.duration, ctx.read_holding_registers(address, count))
                .await
                .map_err(|_| ModbusError::Timeout { millis: t.millis })???,
        )
    }

    async fn read_input(&mut self, address: u16, count: u16) -> Result<Vec<u16>, ModbusError> {
        let t = self.timeout();
        let ctx = self.ctx().await?;
        Ok(
            time::timeout(t.duration, ctx.read_input_registers(address, count))
                .await
                .map_err(|_| ModbusError::Timeout { millis: t.millis })???,
        )
    }

    async fn read_coils(&mut self, address: u16, count: u16) -> Result<Vec<bool>, ModbusError> {
        let t = self.timeout();
        let ctx = self.ctx().await?;
        Ok(time::timeout(t.duration, ctx.read_coils(address, count))
            .await
            .map_err(|_| ModbusError::Timeout { millis: t.millis })???)
    }

    async fn read_discrete(&mut self, address: u16, count: u16) -> Result<Vec<bool>, ModbusError> {
        let t = self.timeout();
        let ctx = self.ctx().await?;
        Ok(
            time::timeout(t.duration, ctx.read_discrete_inputs(address, count))
                .await
                .map_err(|_| ModbusError::Timeout { millis: t.millis })???,
        )
    }

    async fn write_holding(&mut self, address: u16, value: u16) -> Result<(), ModbusError> {
        let t = self.timeout();
        let ctx = self.ctx().await?;
        time::timeout(t.duration, ctx.write_single_register(address, value))
            .await
            .map_err(|_| ModbusError::Timeout { millis: t.millis })???;
        Ok(())
    }

    async fn write_coil(&mut self, address: u16, value: bool) -> Result<(), ModbusError> {
        let t = self.timeout();
        let ctx = self.ctx().await?;
        time::timeout(t.duration, ctx.write_single_coil(address, value))
            .await
            .map_err(|_| ModbusError::Timeout { millis: t.millis })???;
        Ok(())
    }
}

/// In-memory fake [`ModbusClient`] for unit tests.
///
/// Pre-seed `holding`, `input`, `coils`, and `discrete` maps with the
/// addresses your test expects to read. Writes against `holding` /
/// `coils` are persisted into the corresponding map and can be asserted
/// against from the test body.
#[derive(Debug, Default)]
pub struct FakeModbusClient {
    /// Holding-register address → word.
    pub holding: HashMap<u16, u16>,
    /// Input-register address → word.
    pub input: HashMap<u16, u16>,
    /// Coil address → bit.
    pub coils: HashMap<u16, bool>,
    /// Discrete-input address → bit.
    pub discrete: HashMap<u16, bool>,
}

impl FakeModbusClient {
    /// Construct an empty fake.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }
}

#[async_trait]
impl ModbusClient for FakeModbusClient {
    async fn read_holding(&mut self, address: u16, count: u16) -> Result<Vec<u16>, ModbusError> {
        Ok((0..count)
            .map(|i| {
                self.holding
                    .get(&(address + i))
                    .copied()
                    .unwrap_or_default()
            })
            .collect())
    }

    async fn read_input(&mut self, address: u16, count: u16) -> Result<Vec<u16>, ModbusError> {
        Ok((0..count)
            .map(|i| self.input.get(&(address + i)).copied().unwrap_or_default())
            .collect())
    }

    async fn read_coils(&mut self, address: u16, count: u16) -> Result<Vec<bool>, ModbusError> {
        Ok((0..count)
            .map(|i| self.coils.get(&(address + i)).copied().unwrap_or_default())
            .collect())
    }

    async fn read_discrete(&mut self, address: u16, count: u16) -> Result<Vec<bool>, ModbusError> {
        Ok((0..count)
            .map(|i| {
                self.discrete
                    .get(&(address + i))
                    .copied()
                    .unwrap_or_default()
            })
            .collect())
    }

    async fn write_holding(&mut self, address: u16, value: u16) -> Result<(), ModbusError> {
        self.holding.insert(address, value);
        Ok(())
    }

    async fn write_coil(&mut self, address: u16, value: bool) -> Result<(), ModbusError> {
        self.coils.insert(address, value);
        Ok(())
    }
}

#[cfg(test)]
#[allow(clippy::expect_used, clippy::unwrap_used)]
mod tests {
    use super::*;

    #[tokio::test]
    async fn fake_round_trips_holding_writes() {
        let mut client = FakeModbusClient::new();
        client.write_holding(0x10, 0x4242).await.unwrap();
        let read = client.read_holding(0x10, 1).await.unwrap();
        assert_eq!(read, vec![0x4242]);
    }

    #[tokio::test]
    async fn fake_returns_zero_for_unseeded_addresses() {
        let mut client = FakeModbusClient::new();
        let read = client.read_input(0, 3).await.unwrap();
        assert_eq!(read, vec![0, 0, 0]);
    }

    #[tokio::test]
    async fn fake_round_trips_coil_writes() {
        let mut client = FakeModbusClient::new();
        client.write_coil(5, true).await.unwrap();
        let read = client.read_coils(5, 1).await.unwrap();
        assert_eq!(read, vec![true]);
    }
}
