use anyhow::{Context, Result};
use async_trait::async_trait;
use core::str::FromStr;
use std::time::Duration;

use btleplug::api::{
    Central, Characteristic, Manager as _, Peripheral as _, ScanFilter, WriteType,
};
use btleplug::platform::{Manager, Peripheral};

#[cfg(test)]
use mockall::{automock, predicate::*};

#[cfg_attr(test, automock)]
#[async_trait]
pub trait BleClient: Send + Sync {
    async fn connect(&mut self) -> Result<()>;
    async fn write_state(&self, state: bool) -> Result<()>;
    #[allow(dead_code)]
    async fn disconnect(&mut self) -> Result<()>;
}

pub struct BtleplugClient {
    mac_address: String,
    peripheral: Option<Peripheral>,
    characteristic: Option<Characteristic>,
}

impl BtleplugClient {
    pub fn new(mac_address: String) -> Self {
        Self {
            mac_address,
            peripheral: None,
            characteristic: None,
        }
    }
}

#[async_trait]
impl BleClient for BtleplugClient {
    async fn connect(&mut self) -> Result<()> {
        let manager = Manager::new()
            .await
            .context("Failed to create BLE manager")?;
        let adapters = manager
            .adapters()
            .await
            .context("Failed to get BLE adapters")?;
        let central = adapters
            .into_iter()
            .next()
            .ok_or_else(|| anyhow::anyhow!("No Bluetooth adapter found"))?;

        tracing::info!("Starting scan for {}", self.mac_address);
        central
            .start_scan(ScanFilter::default())
            .await
            .context("Failed to start scan")?;

        tokio::time::sleep(Duration::from_secs(2)).await;

        let peripherals = central
            .peripherals()
            .await
            .context("Failed to get peripherals")?;
        for p in peripherals {
            if let Ok(Some(_properties)) = p.properties().await {
                if p.address()
                    .to_string()
                    .eq_ignore_ascii_case(&self.mac_address)
                {
                    tracing::info!("Found target device, connecting...");
                    p.connect().await.context("Failed to connect")?;
                    p.discover_services()
                        .await
                        .context("Failed to discover services")?;

                    let chars = p.characteristics();
                    let char_181a_2a6e_uuid =
                        uuid::Uuid::from_str("00002A6E-0000-1000-8000-00805F9B34FB").unwrap();
                    let char_181a_2a6e = chars.into_iter().find(|c| c.uuid == char_181a_2a6e_uuid);

                    if let Some(c) = char_181a_2a6e {
                        tracing::info!("Connected and found characteristic");
                        self.characteristic = Some(c);
                        self.peripheral = Some(p);
                        return Ok(());
                    } else {
                        return Err(anyhow::anyhow!("Characteristic 2A6E not found"));
                    }
                }
            }
        }

        Err(anyhow::anyhow!(
            "Device with MAC {} not found during scan",
            self.mac_address
        ))
    }

    async fn write_state(&self, state: bool) -> Result<()> {
        if let (Some(p), Some(c)) = (&self.peripheral, &self.characteristic) {
            // Pack state as little-endian 16-bit signed integer (`<h`) to match Python
            let val = if state { 1i16 } else { 0i16 };
            let val_bytes = val.to_le_bytes();
            p.write(c, &val_bytes, WriteType::WithResponse)
                .await
                .context("Failed to write to char")?;
            Ok(())
        } else {
            Err(anyhow::anyhow!("Not connected"))
        }
    }

    async fn disconnect(&mut self) -> Result<()> {
        if let Some(p) = &self.peripheral {
            p.disconnect().await.context("Failed to disconnect")?;
        }
        self.peripheral = None;
        self.characteristic = None;
        Ok(())
    }
}
