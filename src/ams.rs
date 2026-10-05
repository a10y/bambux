//! AMS (automatic material system) state from MQTT reports.
//!
//! Reports may carry only the slots that changed, and the generic report
//! merge replaces arrays wholesale, so AMS fields are merged here per unit
//! and per slot instead.

use std::collections::BTreeMap;

use serde_json::{Map, Value};

/// `tray_now` values that aren't an AMS slot.
const TRAY_EXTERNAL: u32 = 254;
const TRAY_NONE: u32 = 255;

#[derive(Debug, Clone, Default, PartialEq)]
pub struct Spool {
    pub loaded: bool,
    /// Material, e.g. `PLA`.
    pub kind: String,
    /// Product line, e.g. `PLA Basic`.
    pub name: String,
    /// `0xRRGGBB`.
    pub color: Option<u32>,
    /// Estimated percent left. Only known for RFID-tagged Bambu spools.
    pub remain: Option<u8>,
    pub bambu: bool,
    /// Stable spool identity from the RFID tag, when present.
    pub uuid: Option<String>,
}

#[derive(Debug, Clone, Default, PartialEq)]
pub struct AmsUnit {
    pub id: u32,
    /// Relative humidity in percent (`humidity_raw`).
    pub humidity: Option<f32>,
    /// Bambu's coarse 1–5 humidity grade, for units without a raw reading.
    pub humidity_level: Option<u8>,
    pub temperature: Option<f32>,
    /// Four slots, in order.
    pub slots: Vec<Spool>,
}

/// Which spool is feeding the extruder.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ActiveSpool {
    Slot { unit: u32, slot: u32 },
    External,
}

#[derive(Debug, Clone, Default)]
pub struct AmsState {
    pub units: Vec<AmsUnit>,
    pub external: Option<Spool>,
    pub active: Option<ActiveSpool>,
    unit_fields: BTreeMap<u32, Map<String, Value>>,
    slot_fields: BTreeMap<(u32, u32), Map<String, Value>>,
    external_fields: Map<String, Value>,
    exist_bits: Option<u64>,
}

impl AmsState {
    /// Fold the `print` object of one report into the AMS state.
    pub fn apply(&mut self, print: &Value) {
        let mut touched = false;
        if let Some(ams) = print.get("ams") {
            touched = true;
            if let Some(bits) = ams.get("tray_exist_bits").and_then(hex_bits) {
                self.exist_bits = Some(bits);
            }
            if let Some(now) = ams.get("tray_now").and_then(number) {
                let now = now as u32;
                self.active = match now {
                    TRAY_NONE => None,
                    TRAY_EXTERNAL => Some(ActiveSpool::External),
                    index => Some(ActiveSpool::Slot {
                        unit: index / 4,
                        slot: index % 4,
                    }),
                };
            }
            for unit in ams
                .get("ams")
                .and_then(Value::as_array)
                .into_iter()
                .flatten()
            {
                let Some(id) = unit.get("id").and_then(number).map(|id| id as u32) else {
                    continue;
                };
                let fields = self.unit_fields.entry(id).or_default();
                for (key, value) in unit.as_object().into_iter().flatten() {
                    if key != "tray" {
                        fields.insert(key.clone(), value.clone());
                    }
                }
                for tray in unit
                    .get("tray")
                    .and_then(Value::as_array)
                    .into_iter()
                    .flatten()
                {
                    let Some(slot) = tray.get("id").and_then(number).map(|id| id as u32) else {
                        continue;
                    };
                    let Some(object) = tray.as_object() else {
                        continue;
                    };
                    // Only merge: whether a slot holds filament comes from
                    // `tray_exist_bits`, so stale fields on an emptied slot are harmless.
                    self.slot_fields
                        .entry((id, slot))
                        .or_default()
                        .extend(object.clone());
                }
            }
        }
        // External spool holder: `vir_slot` on newer firmware, `vt_tray` on older.
        let external = print
            .get("vir_slot")
            .and_then(Value::as_array)
            .and_then(|slots| slots.first())
            .or_else(|| print.get("vt_tray"));
        if let Some(object) = external.and_then(Value::as_object) {
            touched = true;
            self.external_fields.extend(object.clone());
        }
        if touched {
            self.rebuild();
        }
    }

    fn rebuild(&mut self) {
        self.units = self
            .unit_fields
            .iter()
            .map(|(&id, fields)| AmsUnit {
                id,
                humidity: fields
                    .get("humidity_raw")
                    .and_then(number)
                    .map(|v| v as f32),
                humidity_level: fields.get("humidity").and_then(number).map(|v| v as u8),
                temperature: fields.get("temp").and_then(number).map(|v| v as f32),
                slots: (0..4)
                    .map(|slot| {
                        let mut spool = self
                            .slot_fields
                            .get(&(id, slot))
                            .map(spool_from)
                            .unwrap_or_default();
                        if let Some(bits) = self.exist_bits {
                            spool.loaded = bits >> (id * 4 + slot) & 1 == 1;
                        }
                        spool
                    })
                    .collect(),
            })
            .collect();
        let external = spool_from(&self.external_fields);
        self.external = external.loaded.then_some(external);
    }

    pub fn is_empty(&self) -> bool {
        self.units.is_empty() && self.external.is_none()
    }
}

fn spool_from(fields: &Map<String, Value>) -> Spool {
    let text = |key: &str| {
        fields
            .get(key)
            .and_then(Value::as_str)
            .unwrap_or_default()
            .trim()
            .to_owned()
    };
    let kind = text("tray_type");
    let uuid = text("tray_uuid");
    Spool {
        loaded: !kind.is_empty(),
        name: text("tray_sub_brands"),
        // `RRGGBBAA`; a fully transparent colour means "unknown".
        color: u32::from_str_radix(text("tray_color").get(..8).unwrap_or(""), 16)
            .ok()
            .filter(|rgba| rgba & 0xff != 0)
            .map(|rgba| rgba >> 8),
        remain: fields
            .get("remain")
            .and_then(number)
            .filter(|remain| (0.0..=100.0).contains(remain))
            .map(|remain| remain as u8),
        bambu: !text("tag_uid").trim_matches('0').is_empty(),
        uuid: (!uuid.trim_matches('0').is_empty()).then_some(uuid),
        kind,
    }
}

fn number(value: &Value) -> Option<f64> {
    value
        .as_f64()
        .or_else(|| value.as_str()?.trim().parse().ok())
}

fn hex_bits(value: &Value) -> Option<u64> {
    u64::from_str_radix(value.as_str()?.trim(), 16).ok()
}

#[cfg(test)]
mod tests {
    use serde_json::json;

    use super::*;

    fn full_report() -> Value {
        // Trimmed from a real P2S `pushall`.
        json!({
            "ams": {
                "tray_exist_bits": "3",
                "tray_now": "1",
                "ams": [{
                    "id": "0", "humidity": "1", "humidity_raw": "51", "temp": "27.1",
                    "tray": [
                        {"id": "0", "tray_type": "PLA", "tray_sub_brands": "PLA Basic",
                         "tray_color": "FFFFFFFF", "remain": 18, "tag_uid": "C7D1777B00000100",
                         "tray_uuid": "CD010EDE8D9F42A99263CCBE4FE9EE56"},
                        {"id": "1", "tray_type": "PLA", "tray_color": "000000FF", "remain": 72,
                         "tag_uid": "0000000000000001"},
                        {"id": "2", "state": 0},
                        {"id": "3", "state": 0}
                    ]
                }]
            },
            "vir_slot": [{"id": "255", "tray_type": "", "tray_color": "00000000", "remain": 0}]
        })
    }

    #[test]
    fn parses_units_slots_and_active_spool() {
        let mut ams = AmsState::default();
        ams.apply(&full_report());
        let unit = &ams.units[0];
        assert_eq!(unit.humidity, Some(51.0));
        assert_eq!(unit.temperature, Some(27.1));
        assert_eq!(unit.slots[0].remain, Some(18));
        assert_eq!(unit.slots[0].color, Some(0xFFFFFF));
        assert!(unit.slots[0].bambu);
        assert_eq!(unit.slots[1].color, Some(0x000000));
        assert!(!unit.slots[2].loaded);
        assert_eq!(ams.active, Some(ActiveSpool::Slot { unit: 0, slot: 1 }));
        assert!(ams.external.is_none());
    }

    #[test]
    fn exist_bits_mark_removed_spools_empty() {
        let mut ams = AmsState::default();
        ams.apply(&full_report());
        ams.apply(&json!({"ams": {"tray_exist_bits": "1"}}));
        assert!(ams.units[0].slots[0].loaded);
        assert!(!ams.units[0].slots[1].loaded);
    }

    #[test]
    fn partial_slot_updates_keep_other_slots() {
        let mut ams = AmsState::default();
        ams.apply(&full_report());
        ams.apply(&json!({"ams": {"ams": [{"id": "0", "tray": [{"id": "1", "remain": 70}]}]}}));
        let unit = &ams.units[0];
        assert_eq!(unit.slots[1].remain, Some(70));
        assert_eq!(unit.slots[1].kind, "PLA");
        assert_eq!(unit.slots[0].remain, Some(18));
        assert_eq!(unit.humidity, Some(51.0));
    }

    #[test]
    fn unknown_remaining_is_none() {
        let mut ams = AmsState::default();
        ams.apply(&json!({"ams": {"ams": [{"id": "0", "tray": [
            {"id": "0", "tray_type": "PETG", "tray_color": "FF0000FF", "remain": -1}
        ]}]}}));
        assert_eq!(ams.units[0].slots[0].remain, None);
        assert!(ams.units[0].slots[0].loaded);
    }
}
