use anyhow::{anyhow, Result};
use std::time::Duration;
use tracing::{error, info};
use tracing_subscriber::EnvFilter;

use garage_beam::ble::BtleplugClient;
use garage_beam::gpio::ChardevGpio;
use garage_beam::interval::HciInterval;
use garage_beam::run_loop;

const DEVICE_ADDRESS_ENV: &str = "GARAGE_BEAM_DEVICE_ADDRESS";

fn address_from(value: Option<&str>) -> Result<String> {
    value
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
        .ok_or_else(|| anyhow!("{DEVICE_ADDRESS_ENV} must be set to the lamp peripheral address"))
}

fn default_filter() -> &'static str {
    "garage_beam=info,warn"
}

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt()
        .with_env_filter(
            EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new(default_filter())),
        )
        .init();

    let address = address_from(std::env::var(DEVICE_ADDRESS_ENV).ok().as_deref())?;

    loop {
        info!("Initializing system...");
        match ChardevGpio::new("pinctrl-bcm2835", 4) {
            Ok(gpio) => {
                let client = BtleplugClient::new(address.clone());
                let interval = HciInterval::new(address.clone());
                let result = run_loop(Box::new(client), Box::new(gpio), Box::new(interval)).await;

                if let Err(e) = result {
                    error!("Error within run loop: {:?}", e);
                }
            }
            Err(e) => {
                error!("Failed to initialize GPIO: {:?}", e);
            }
        }

        info!("Restarting in 1s...");
        tokio::time::sleep(Duration::from_secs(1)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn address_from_rejects_missing() {
        assert!(address_from(None).is_err());
    }

    #[test]
    fn address_from_rejects_empty() {
        assert!(address_from(Some("")).is_err());
    }

    #[test]
    fn address_from_rejects_whitespace_only() {
        assert!(address_from(Some("   ")).is_err());
    }

    #[test]
    fn address_from_returns_trimmed_value() {
        let address = address_from(Some("  AA:BB:CC:DD:EE:FF  ")).unwrap();
        assert_eq!(address, "AA:BB:CC:DD:EE:FF");
    }

    #[test]
    fn default_filter_is_scoped_to_crate() {
        assert_eq!(default_filter(), "garage_beam=info,warn");
    }
}
