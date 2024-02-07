mkdir -p /tmp/firmware
sshfs root@10.0.1.5:/var/www/firmware /tmp/firmware

# Get old and new version numbers
old_version=`cat /tmp/firmware/garagebeam/version`
new_version=$((old_version + 1))

# Update to the new version number
echo $new_version > /tmp/firmware/garagebeam/version

# Copy firmware files
cp boot.py /tmp/firmware/garagebeam/${new_version}_boot.py
cp main.py /tmp/firmware/garagebeam/${new_version}_main.py
cp micropython_ota.py /tmp/firmware/garagebeam/${new_version}_micropython_ota.py
cp mqtt_as.py /tmp/firmware/garagebeam/${new_version}_mqtt_as.py

# Remove old firmware files
rm -f /tmp/firmware/garagebeam/${old_version}_*.py

umount /tmp/firmware

mosquitto_pub -h 10.0.20.179 -t "garagebeam/reset" -m 'reset'

echo "Deployed new version $new_version successfully"