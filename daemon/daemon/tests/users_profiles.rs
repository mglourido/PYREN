//! `pyren-users` copies the modules' config files without knowing what is
//! in them - with one exception it has to name by hand, and this is where
//! that list is held against the struct it is about.

use pyren_fan::FanConfig;
use pyren_users::profiles::{MACHINE_KEYS, NAMESPACES};

/// The keys `pyren-users` keeps out of a user's fan profile are spelt the
/// way `FanConfig` spells them. A field renamed over there would otherwise
/// quietly start travelling with whoever logs in.
#[test]
fn the_fan_measurements_kept_out_of_a_profile_are_fan_config_fields() {
    let fan = serde_json::to_value(FanConfig::default()).expect("a serialisable fan config");
    let fields = fan.as_object().expect("an object");

    let (namespace, keys) = MACHINE_KEYS[0];
    assert_eq!(namespace, "fan");
    for key in keys {
        assert!(
            fields.contains_key(*key),
            "'{key}' is not a FanConfig field any more; update pyren_users::profiles::MACHINE_KEYS"
        );
    }
}

/// Every namespace with machine keys is one that is copied at all.
#[test]
fn machine_keys_only_name_namespaces_that_belong_to_a_user() {
    for (namespace, _) in MACHINE_KEYS {
        assert!(NAMESPACES.contains(&namespace));
    }
}
