use crate::domain::{
    DeviceSettingValue, DogiError, HidppFeature, HidppFeatureInfo, Result, SettingsApplyOperation,
};

use super::transactions::{PreparedHidChange, VerificationByte};

const POINTER_SPEED: u16 = 0x2205;
const ADJUSTABLE_DPI: u16 = 0x2201;

/// Prepare only reads the device. Writes and rollback use the common transaction engine.
pub(super) fn prepare(
    features: &[HidppFeatureInfo],
    percent: u8,
    mut read: impl FnMut(u8, u8, &[u8]) -> Result<Vec<u8>>,
) -> Result<PreparedHidChange> {
    let percent = percent.clamp(50, 200);
    if let Some(feature) = features
        .iter()
        .find(|feature| feature.feature_id == POINTER_SPEED)
    {
        let current = read(feature.index, 0x00, &[])?;
        let before = current
            .get(..2)
            .ok_or_else(|| malformed("missing pointer multiplier"))?;
        let raw = u16::from_be_bytes([before[0], before[1]]);
        let after = ((u16::from(percent) * 256 + 50) / 100)
            .clamp(0x80, 0x1ff)
            .to_be_bytes();
        return Ok(change(
            percent,
            POINTER_SPEED,
            before.to_vec(),
            after.to_vec(),
            DeviceSettingValue::PointerSpeed {
                percent: ((u32::from(raw) * 100 + 128) / 256).clamp(1, 255) as u8,
            },
            DeviceSettingValue::PointerSpeed { percent },
        ));
    }

    let feature = features
        .iter()
        .find(|feature| feature.feature_id == ADJUSTABLE_DPI)
        .ok_or_else(|| {
            DogiError::UnsupportedFeature(
                "pointer sensitivity requires POINTER_SPEED or ADJUSTABLE_DPI".to_owned(),
            )
        })?;
    let count = read(feature.index, 0x00, &[])?;
    let count = count
        .first()
        .ok_or_else(|| malformed("missing sensor count"))?;
    if *count != 1 {
        return Err(DogiError::UnsupportedFeature(
            "pointer sensitivity requires one motion sensor".to_owned(),
        ));
    }
    // HID++ 0x2201: sensor 0, list (0x10), get (0x20), set (0x30).
    let supported = supported_dpi(&read(feature.index, 0x10, &[0])?)?;
    let current = read(feature.index, 0x20, &[0])?;
    if current.len() < 5 || current[0] != 0 {
        return Err(malformed("invalid sensor DPI response"));
    }
    let before_dpi = u16::from_be_bytes([current[1], current[2]]);
    let default_dpi = u16::from_be_bytes([current[3], current[4]]);
    if default_dpi == 0 {
        return Err(DogiError::UnsupportedFeature(
            "this mouse does not report the default DPI required for percentage-based sensitivity"
                .to_owned(),
        ));
    }
    if !supported.contains(&before_dpi) || !supported.contains(&default_dpi) {
        return Err(malformed(
            "current or default DPI is outside the supported values",
        ));
    }
    // A stable device-provided baseline prevents repeated starts from compounding sensitivity.
    let requested = u32::from(default_dpi) * u32::from(percent);
    let after_dpi = supported
        .into_iter()
        .min_by_key(|dpi| ((u32::from(*dpi) * 100).abs_diff(requested), *dpi))
        .ok_or_else(|| malformed("empty DPI list"))?;
    let [high, low] = after_dpi.to_be_bytes();
    Ok(change(
        percent,
        ADJUSTABLE_DPI,
        current[..3].to_vec(),
        vec![0, high, low],
        DeviceSettingValue::PointerDpi { dpi: before_dpi },
        DeviceSettingValue::PointerDpi { dpi: after_dpi },
    ))
}

fn malformed(detail: &str) -> DogiError {
    DogiError::Protocol(format!("pointer sensitivity: {detail}"))
}

fn supported_dpi(reply: &[u8]) -> Result<Vec<u16>> {
    if reply.first().copied() != Some(0) {
        return Err(malformed("invalid DPI list sensor"));
    }
    let mut values = Vec::new();
    let mut terminated = false;
    for &bytes in reply[1..].as_chunks::<2>().0 {
        let value = u16::from_be_bytes(bytes);
        if value == 0 {
            terminated = true;
            break;
        }
        values.push(value);
    }
    if !terminated || values.is_empty() {
        return Err(malformed("unterminated or empty DPI list"));
    }
    if let [low, step, high] = values.as_slice()
        && step & 0xe000 == 0xe000
    {
        let step = step & 0x1fff;
        if step == 0 || low > high || *high >= 0xe000 || (high - low) % step != 0 {
            return Err(malformed("invalid DPI range"));
        }
        return Ok((*low..=*high).step_by(usize::from(step)).collect());
    }
    if values.iter().any(|value| *value >= 0xe000)
        || values.windows(2).any(|pair| pair[0] >= pair[1])
    {
        return Err(malformed("invalid discrete DPI list"));
    }
    Ok(values)
}

fn change(
    percent: u8,
    feature_id: u16,
    before: Vec<u8>,
    after: Vec<u8>,
    before_value: DeviceSettingValue,
    after_value: DeviceSettingValue,
) -> PreparedHidChange {
    let (read_function, read_payload, write_function) = if feature_id == ADJUSTABLE_DPI {
        (0x20, vec![0], 0x30)
    } else {
        (0x00, vec![], 0x10)
    };
    let verification = before
        .iter()
        .zip(&after)
        .enumerate()
        .map(|(response_index, (&before, &after))| VerificationByte {
            response_index,
            mask: 0xff,
            before,
            after,
        })
        .collect();
    PreparedHidChange {
        title: format!("Set pointer speed to {percent}%"),
        operation: SettingsApplyOperation::PointerSpeed { percent },
        feature: HidppFeature::PointerSpeed,
        feature_id,
        read_function,
        read_payload,
        write_function,
        before_write: before,
        after_write: after,
        verification,
        before_value,
        after_value,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn feature(id: u16) -> HidppFeatureInfo {
        HidppFeatureInfo {
            index: 13,
            feature_id: id,
            name: "test".to_owned(),
            flags: 0,
            version: 2,
        }
    }

    fn list(values: &[u16]) -> Vec<u8> {
        std::iter::once(0)
            .chain(values.iter().flat_map(|value| value.to_be_bytes()))
            .chain([0, 0])
            .collect()
    }

    fn dpi_change(percent: u8, current: u16, default: u16) -> Result<PreparedHidChange> {
        prepare(
            &[feature(ADJUSTABLE_DPI)],
            percent,
            |index, function, payload| {
                assert_eq!(index, 13);
                match function {
                    0x00 => {
                        assert!(payload.is_empty());
                        Ok(vec![1])
                    }
                    0x10 => {
                        assert_eq!(payload, [0]);
                        Ok(list(&[200, 0xe032, 8000]))
                    }
                    0x20 => {
                        assert_eq!(payload, [0]);
                        Ok([
                            vec![0],
                            current.to_be_bytes().to_vec(),
                            default.to_be_bytes().to_vec(),
                        ]
                        .concat())
                    }
                    _ => panic!("preparation must not write"),
                }
            },
        )
    }

    #[test]
    fn master3s_uses_dpi_without_requiring_pointer_speed() {
        let change = dpi_change(125, 800, 1000).unwrap();
        assert_eq!(change.feature_id, ADJUSTABLE_DPI);
        assert_eq!(change.read_function, 0x20);
        assert_eq!(change.write_function, 0x30);
        assert_eq!(change.before_write, [0, 3, 32]);
        assert_eq!(change.after_write, [0, 4, 226]);
        assert_eq!(
            change.before_value,
            DeviceSettingValue::PointerDpi { dpi: 800 }
        );
        assert_eq!(
            change.after_value,
            DeviceSettingValue::PointerDpi { dpi: 1250 }
        );
        assert_eq!(change.verification[1].response_index, 1);
        assert_eq!(change.verification[2].before, 32);
    }

    #[test]
    fn percentage_uses_default_dpi_not_the_last_applied_value() {
        let first = dpi_change(150, 1000, 1000).unwrap();
        let repeated = dpi_change(150, 1500, 1000).unwrap();
        assert_eq!(first.after_write, repeated.after_write);
        assert_eq!(repeated.before_write, repeated.after_write);
        assert_eq!(
            dpi_change(100, 1500, 1000).unwrap().after_value,
            DeviceSettingValue::PointerDpi { dpi: 1000 }
        );
    }

    #[test]
    fn dpi_values_are_rounded_to_supported_steps_and_bounded() {
        assert_eq!(
            dpi_change(123, 1000, 1000).unwrap().after_value,
            DeviceSettingValue::PointerDpi { dpi: 1250 }
        );
        assert_eq!(
            dpi_change(200, 8000, 8000).unwrap().after_value,
            DeviceSettingValue::PointerDpi { dpi: 8000 }
        );
    }

    #[test]
    fn missing_default_is_unsupported_instead_of_compounding_current_dpi() {
        assert!(matches!(
            dpi_change(120, 1000, 0),
            Err(DogiError::UnsupportedFeature(_))
        ));
    }

    #[test]
    fn unsupported_pointer_capability_never_sends_a_request() {
        assert!(matches!(
            prepare(&[], 100, |_, _, _| panic!("unexpected I/O")),
            Err(DogiError::UnsupportedFeature(_))
        ));
    }

    #[test]
    fn transport_failures_are_not_reported_as_missing_capabilities() {
        assert!(matches!(
            prepare(&[feature(ADJUSTABLE_DPI)], 100, |_, _, _| {
                Err(DogiError::Transport("timed out".to_owned()))
            }),
            Err(DogiError::Transport(_))
        ));
        assert!(matches!(
            prepare(&[feature(ADJUSTABLE_DPI)], 100, |_, _, _| Ok(vec![])),
            Err(DogiError::Protocol(_))
        ));
    }

    #[test]
    fn preserves_multiplier_support_for_other_device_firmware() {
        let change = prepare(&[feature(POINTER_SPEED)], 200, |_, function, payload| {
            assert_eq!(function, 0);
            assert!(payload.is_empty());
            Ok(vec![1, 0])
        })
        .unwrap();
        assert_eq!(change.feature_id, POINTER_SPEED);
        assert_eq!(change.after_write, [1, 255]);
    }

    #[test]
    fn parses_discrete_and_range_dpi_without_accepting_malformed_lists() {
        assert_eq!(
            supported_dpi(&list(&[400, 800, 1600])).unwrap(),
            [400, 800, 1600]
        );
        assert_eq!(
            supported_dpi(&list(&[400, 0xe064, 700])).unwrap(),
            [400, 500, 600, 700]
        );
        for reply in [
            vec![],
            vec![1, 0, 0],
            vec![0, 1, 144],
            list(&[]),
            list(&[800, 400]),
            list(&[400, 0xe000, 800]),
            list(&[400, 0xe064, 450]),
            list(&[400, 0xe001]),
        ] {
            assert!(supported_dpi(&reply).is_err(), "{reply:?}");
        }
    }
}
