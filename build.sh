rustup target add thumbv6m-none-eabi
cargo install uf2conv cargo-binutils
rustup component add llvm-tools-preview

# Put WIFI_NETWORK and WIFI_PASSWORD in this file
source .env

export RANDOM_SEED=$(od -A n -t u8 -N 8 /dev/urandom | sed 's/ *$//g' | sed 's/^ *//g')

mkdir -p build
cargo build --release
cargo objcopy --release -- -O binary build/garagebeam.bin
uf2conv build/garagebeam.bin --base 0x10000000 --family 0xe48bff56 --output build/garagebeam.uf2
