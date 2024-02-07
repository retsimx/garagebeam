import sys

from micropython import const

import uasyncio as asyncio
import aioble
import bluetooth

import random
import struct

# org.bluetooth.service.environmental_sensing
_ENV_SENSE_UUID = bluetooth.UUID(0x181A)
# org.bluetooth.characteristic.temperature
_ENV_SENSE_TEMP_UUID = bluetooth.UUID(0x2A6E)
# org.bluetooth.characteristic.gap.appearance.xml
_ADV_APPEARANCE_GENERIC_THERMOMETER = const(768)

# How frequently to send advertising beacons.
_ADV_INTERVAL_MS = 250_000


# Register GATT server.
temp_service = aioble.Service(_ENV_SENSE_UUID)
temp_characteristic = aioble.Characteristic(
    temp_service, _ENV_SENSE_TEMP_UUID, read=True, notify=True
)
aioble.register_services(temp_service)


async def sensor_task(beam_pin):
    while True:
        last_value = None
        try:
            while True:
                new_value = beam_pin.value()
                if last_value != new_value:
                    last_value = new_value
                    temp_characteristic.write(struct.pack("<h", int(new_value)))

                await asyncio.sleep_ms(5)
        except:
            await asyncio.sleep_ms(1000)


async def poll_task(beam_pin):
    while True:
        try:
            temp_characteristic.write(struct.pack("<h", int(beam_pin.value())))
        except:
            pass

        await asyncio.sleep_ms(1000)


# Serially wait for connections. Don't advertise while a central is
# connected.
async def peripheral_task():
    while True:
        try:
            async with await aioble.advertise(
                _ADV_INTERVAL_MS,
                name="mpy-temp",
                services=[_ENV_SENSE_UUID],
                appearance=_ADV_APPEARANCE_GENERIC_THERMOMETER,
            ) as connection:
                print("Connection from", connection.device)
                await connection.disconnected()
        except:
            await asyncio.sleep_ms(1000)


# Run both tasks.
async def run_ble(beam_pin):
    t1 = asyncio.create_task(sensor_task(beam_pin))
    t2 = asyncio.create_task(peripheral_task())
    t3 = asyncio.create_task(poll_task(beam_pin))
    await asyncio.gather(t1, t2, t3)
