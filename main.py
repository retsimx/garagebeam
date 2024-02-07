import asyncio

import machine

from mqtt_as import MQTTClient, config
from secrets import WIFI_SSID, WIFI_PASSWORD, MQTT_IP

BEAM_PIN_NUM = 26

beam_pin = machine.Pin(BEAM_PIN_NUM, machine.Pin.IN, pull=machine.Pin.PULL_UP)

# Local configuration
config['ssid'] = WIFI_SSID
config['wifi_pw'] = WIFI_PASSWORD
config['server'] = MQTT_IP


async def messages(client):
    async for topic, msg, retained in client.queue:
        if topic.startswith("garagebeam/reset"):
            machine.reset()

        else:
            print("Unknown MQTT message:", topic, msg, retained)


async def up(client):
    while True:
        await client.up.wait()
        client.up.clear()
        await client.subscribe("garagebeam/reset", 0)


async def poll(client):
    last = beam_pin.value()
    while True:
        new_value = beam_pin.value()
        if last != new_value:
            last = new_value
            await client.publish("garagebeam/status", str(beam_pin.value()))
        await asyncio.sleep(0.05)


async def main(client):
    await client.connect()
    for coroutine in (up, messages):
        asyncio.create_task(coroutine(client))

    asyncio.create_task(poll(client))

    while True:
        await client.publish("garagebeam/status", str(beam_pin.value()))
        await asyncio.sleep(1)


config["queue_len"] = 6
MQTTClient.DEBUG = True
_client = MQTTClient(config)
try:
    asyncio.run(main(_client))
finally:
    _client.close()
