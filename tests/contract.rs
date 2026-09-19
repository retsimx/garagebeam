use std::collections::BTreeMap;

fn parse(text: &str) -> BTreeMap<&str, &str> {
    let mut map = BTreeMap::new();
    for raw in text.lines() {
        let line = raw.split('#').next().unwrap().trim();
        if line.is_empty() {
            continue;
        }
        let (key, value) = line.split_once('=').expect("contract line missing '='");
        let key = key.trim();
        let value = value.trim().trim_matches('"');
        assert!(!key.is_empty(), "contract line with empty key");
        assert!(map.insert(key, value).is_none(), "duplicate key {key}");
    }
    map
}

#[test]
fn contract_matches_code_constants() {
    let map = parse(include_str!("../contract.toml"));
    let get = |key: &str| *map.get(key).unwrap_or_else(|| panic!("missing key {key}"));
    let num = |key: &str| {
        get(key)
            .parse::<u64>()
            .unwrap_or_else(|_| panic!("{key} is not a number"))
    };

    assert_eq!(num("schema"), 1);
    assert_eq!(get("service_uuid"), garage_beam::ble::SERVICE_UUID);
    assert_eq!(
        get("characteristic_uuid"),
        garage_beam::ble::CHARACTERISTIC_UUID
    );
    assert_eq!(num("value_len_bytes"), 1);
    assert_eq!(num("fact_intact"), garage_beam::ble::FACT_INTACT as u64);
    assert_eq!(num("fact_broken"), garage_beam::ble::FACT_BROKEN as u64);
    assert_eq!(
        num("conn_interval_us"),
        garage_beam::interval::TARGET_INTERVAL as u64 * 1250
    );
    assert_eq!(num("conn_interval_us"), 7500);
    assert_eq!(
        num("conn_slave_latency"),
        garage_beam::interval::SLAVE_LATENCY as u64
    );
    assert_eq!(
        num("conn_supervision_timeout_ms"),
        garage_beam::interval::SUPERVISION_TIMEOUT as u64 * 10
    );
    assert_eq!(num("conn_supervision_timeout_ms"), 1000);
    assert_eq!(
        num("keepalive_interval_ms"),
        garage_beam::KEEPALIVE.as_millis() as u64
    );
    assert_eq!(num("keepalive_interval_ms"), 10000);
    assert!(num("leash_timeout_ms") > num("keepalive_interval_ms"));
    assert!(!get("mqtt_topic").is_empty());
}
