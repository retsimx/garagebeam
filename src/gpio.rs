use anyhow::{Context, Result};
use std::fs::{File, OpenOptions};
use std::io::{Read, Seek, SeekFrom, Write};
use std::path::Path;

#[cfg(test)]
use mockall::{automock, predicate::*};

#[cfg_attr(test, automock)]
pub trait GpioReader: Send + Sync {
    fn read_pin(&mut self) -> Result<bool>;
}

pub struct SysfsGpio {
    value_file: File,
}

impl SysfsGpio {
    pub fn new(pin: u32) -> Result<Self> {
        let gpio_dir = format!("/sys/class/gpio/gpio{}", pin);

        let path = Path::new(&gpio_dir);
        if !path.exists() {
            let mut export_file = OpenOptions::new()
                .write(true)
                .open("/sys/class/gpio/export")
                .context("Failed to open gpio export file")?;
            write!(export_file, "{}", pin).context("Failed to write to gpio export")?;

            // Wait for udev or kernel to create the directory
            std::thread::sleep(std::time::Duration::from_millis(100));
        }

        let direction_path = format!("{}/direction", gpio_dir);
        let mut direction_file = OpenOptions::new()
            .write(true)
            .open(&direction_path)
            .context("Failed to open gpio direction file")?;
        write!(direction_file, "in").context("Failed to set gpio direction to in")?;

        // Wait briefly for polarity/permissions
        std::thread::sleep(std::time::Duration::from_millis(10));

        let value_path = format!("{}/value", gpio_dir);
        let value_file = OpenOptions::new()
            .read(true)
            .open(&value_path)
            .context("Failed to open gpio value file")?;

        Ok(SysfsGpio { value_file })
    }
}

impl GpioReader for SysfsGpio {
    fn read_pin(&mut self) -> Result<bool> {
        self.value_file
            .seek(SeekFrom::Start(0))
            .context("Failed to seek value file")?;
        let mut buf = [0u8; 1];
        self.value_file
            .read_exact(&mut buf)
            .context("Failed to read value file")?;

        match buf[0] {
            b'1' => Ok(true),
            b'0' => Ok(false),
            _ => Err(anyhow::anyhow!("Unexpected value in gpio file: {}", buf[0])),
        }
    }
}
