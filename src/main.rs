use anyhow::Result;
use std::time::Duration;
use tracing::{error, info};

use garage_beam::ble::BtleplugClient;
use garage_beam::gpio::SysfsGpio;
use garage_beam::run_loop;

const DEVICE_MAC: &str = "28:CD:C1:02:87:F5";
const GPIO_PIN: u32 = 516;

#[tokio::main]
async fn main() -> Result<()> {
    tracing_subscriber::fmt::init();

    loop {
        info!("Initializing system...");
        match SysfsGpio::new(GPIO_PIN) {
            Ok(sysfs_gpio) => {
                let client = BtleplugClient::new(DEVICE_MAC.to_string());
                let result = run_loop(Box::new(client), Box::new(sysfs_gpio)).await;

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
