# Pico W Rust Garage Light
Raspberry Pi Pico W Rust UDP server for turning on/off a light


To build the project, create a .env file with the wifi details used by the wifi driver. eg:
```
export WIFI_NETWORK=myssid
export WIFI_PASSWORD=mypassword
```

Then run `bash build.sh`.

Finally copy the uf2 file from the build directory to the pico w.