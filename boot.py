import time

import machine
import network
import rp2

import micropython_ota
from secrets import *

filenames = [
    'boot.py',
    'main.py',
    'micropython_ota.py',
    'mqtt_as.py'
]

rp2.country('AU')

# Connect to WLAN for a minute before resetting to try again
wlan = network.WLAN(network.STA_IF)
wlan.active(True)
wlan.config(pm=0xA11140)
wlan.connect(WIFI_SSID, WIFI_PASSWORD)

for _ in range(60):
    if wlan.isconnected():
        break

    print('Waiting for connection...')
    time.sleep(1)

if not wlan.isconnected():
    machine.reset()

led = machine.Pin("LED", machine.Pin.OUT)
led.on()

print("Connected, checking for update...")

micropython_ota.ota_update(
    OTA_URL,
    OTA_PROJECT,
    filenames,
    user=OTA_USER,
    passwd=OTA_PASSWORD,
    use_version_prefix=True,
    hard_reset_device=True,
    soft_reset_device=False,
    timeout=15
)

led.off()
print("No update available")
