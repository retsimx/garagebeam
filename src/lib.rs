pub mod ble;
pub mod gpio;

use anyhow::Result;
use ble::BleClient;
use gpio::GpioReader;
use std::time::Duration;
use tracing::info;

const DELAY_MS: u64 = 10;

pub async fn run_loop(
    mut client: Box<dyn BleClient>,
    mut reader: Box<dyn GpioReader>,
) -> Result<()> {
    client.connect().await?;
    info!("Connected successfully!");

    let mut last_state = None;

    loop {
        let current_state = reader.read_pin()?;
        if last_state != Some(current_state) {
            last_state = Some(current_state);
            info!("State changed to: {}", current_state);
            client.write_state(current_state).await?;
        }

        tokio::time::sleep(Duration::from_millis(DELAY_MS)).await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ble::MockBleClient;
    use crate::gpio::MockGpioReader;
    use std::sync::{Arc, Mutex};

    #[tokio::test]
    async fn test_run_loop_state_changes() {
        let mut mock_client = MockBleClient::new();
        let mut mock_gpio = MockGpioReader::new();

        mock_client.expect_connect().times(1).returning(|| Ok(()));

        let states = vec![
            Ok(true),
            Ok(true),
            Ok(false),
            Ok(false),
            Err(anyhow::anyhow!("Stop")),
        ];
        let index = Arc::new(Mutex::new(0));

        mock_gpio.expect_read_pin().returning(move || {
            let mut i = index.lock().unwrap();
            let state = states[*i]
                .as_ref()
                .map(|b| *b)
                .map_err(|e| anyhow::anyhow!("{}", e));
            *i += 1;
            state
        });

        // verify we write true then false
        mock_client
            .expect_write_state()
            .with(mockall::predicate::eq(true))
            .times(1)
            .returning(|_| Ok(()));
        mock_client
            .expect_write_state()
            .with(mockall::predicate::eq(false))
            .times(1)
            .returning(|_| Ok(()));

        let res = run_loop(Box::new(mock_client), Box::new(mock_gpio)).await;
        assert!(res.is_err());
        assert_eq!(res.unwrap_err().to_string(), "Stop");
    }
}
