//! What is known about the connected tablet model.
//!
//! Only one model has been tested. For the others everything comes from the tablet itself: its
//! name, its size and how many express keys it has. Whether it has a dial it does not say, so
//! that is learned the first time the dial is turned.

use serde::{Deserialize, Serialize};

/// USB product ids of the models Inkswitch has been tested with.
const TESTED: [u16; 1] = [0x096f];
/// Models known to have a dial without having to wait for it to be turned.
const WITH_DIAL: [u16; 1] = [0x096f];

#[derive(Serialize, Deserialize, Clone, PartialEq, Debug)]
pub struct DeviceInfo {
    pub name: String,
    pub product_id: u16,
    /// Number of express keys.
    pub keys: usize,
    pub dial: bool,
    /// Whether the tablet is also a screen.
    pub display: bool,
}

impl DeviceInfo {
    /// Describes a tablet that was just connected. `previous` is what was known about the last
    /// one, which carries over what was learned if it is the same model.
    pub fn new(name: &str, product_id: u16, keys: usize, previous: Option<&DeviceInfo>) -> DeviceInfo {
        let learned = previous.is_some_and(|p| p.product_id == product_id && p.dial);
        DeviceInfo {
            name: name.to_string(),
            product_id,
            keys,
            dial: learned || WITH_DIAL.contains(&product_id),
            // XP-Pen calls its screen tablets Artist; the others (Deco, Star) have none.
            display: name.to_lowercase().contains("artist"),
        }
    }

    pub fn tested(&self) -> bool {
        TESTED.contains(&self.product_id)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn describes_models() {
        let tested = DeviceInfo::new("Artist 15.6 Pro V2", 0x096f, 8, None);
        assert!(tested.tested() && tested.dial && tested.display);

        let other = DeviceInfo::new("Deco 01 V2", 0x0905, 8, None);
        assert!(!other.tested() && !other.dial && !other.display);

        // A dial seen once is remembered for that model only.
        let learned = DeviceInfo { dial: true, ..other.clone() };
        assert!(DeviceInfo::new("Deco 01 V2", 0x0905, 8, Some(&learned)).dial);
        assert!(!DeviceInfo::new("Deco L", 0x0935, 4, Some(&learned)).dial);
    }
}
