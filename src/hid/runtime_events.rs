use crate::domain::{HidppFeatureInfo, Master3sRuntimeEvent, master3s_button_from_control_id};

const HIDPP_SHORT_REPORT_ID: u8 = 0x10;
const HIDPP_LONG_REPORT_ID: u8 = 0x11;
const HIDPP_FEATURE_REPROG_CONTROLS_V4: u16 = 0x1b04;
const HIDPP_FEATURE_THUMB_WHEEL: u16 = 0x2150;

pub fn parse_notification(
    report: &[u8],
    features: &[HidppFeatureInfo],
) -> Option<Master3sRuntimeEvent> {
    let report_id = *report.first()?;
    if report_id != HIDPP_SHORT_REPORT_ID && report_id != HIDPP_LONG_REPORT_ID {
        return None;
    }

    let feature_index = *report.get(2)?;
    let address = *report.get(3)?;
    if feature_index == 0 || address & 0x0f != 0 {
        return None;
    }

    let function = address >> 4;
    let data = report.get(4..)?;
    let feature_id = features
        .iter()
        .find(|feature| feature.index == feature_index)
        .map(|feature| feature.feature_id)?;

    match (feature_id, function) {
        (HIDPP_FEATURE_THUMB_WHEEL, 0) => parse_thumb_wheel(data),
        (HIDPP_FEATURE_REPROG_CONTROLS_V4, 0) => parse_diverted_buttons(data),
        (HIDPP_FEATURE_REPROG_CONTROLS_V4, 1) => parse_raw_movement(data),
        _ => None,
    }
}

fn parse_thumb_wheel(data: &[u8]) -> Option<Master3sRuntimeEvent> {
    let delta = i16::from_be_bytes([*data.first()?, *data.get(1)?]);
    Some(Master3sRuntimeEvent::ThumbWheel {
        delta,
        phase: data.get(4).copied(),
        resolution: 1,
        direction: 1,
    })
}

fn parse_diverted_buttons(data: &[u8]) -> Option<Master3sRuntimeEvent> {
    let control_bytes = data.get(..8)?;
    let mut buttons = Vec::new();
    let mut unknown_control_ids = Vec::new();

    let (control_ids, remainder) = control_bytes.as_chunks::<2>();
    if !remainder.is_empty() {
        return None;
    }
    for [high, low] in control_ids {
        let control_id = u16::from_be_bytes([*high, *low]);
        if control_id == 0 {
            continue;
        }

        if let Some(button) = master3s_button_from_control_id(control_id) {
            buttons.push(button);
        } else {
            unknown_control_ids.push(control_id);
        }
    }

    Some(Master3sRuntimeEvent::DivertedButtons {
        buttons,
        unknown_control_ids,
    })
}

fn parse_raw_movement(data: &[u8]) -> Option<Master3sRuntimeEvent> {
    Some(Master3sRuntimeEvent::RawMovement {
        x: i16::from_be_bytes([*data.first()?, *data.get(1)?]),
        y: i16::from_be_bytes([*data.get(2)?, *data.get(3)?]),
    })
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::Master3sButton;

    #[test]
    fn parses_thumb_wheel_notification() {
        let features = vec![feature(16, HIDPP_FEATURE_THUMB_WHEEL)];
        let report = [HIDPP_SHORT_REPORT_ID, 1, 16, 0x00, 0xff, 0xec, 0, 0, 1];

        assert_eq!(
            parse_notification(&report, &features),
            Some(Master3sRuntimeEvent::ThumbWheel {
                delta: -20,
                phase: Some(1),
                resolution: 1,
                direction: 1,
            })
        );
    }

    #[test]
    fn parses_diverted_button_notification() {
        let features = vec![feature(9, HIDPP_FEATURE_REPROG_CONTROLS_V4)];
        let report = [
            HIDPP_LONG_REPORT_ID,
            1,
            9,
            0x00,
            0x00,
            0x53,
            0x00,
            0xc3,
            0xbe,
            0xef,
            0,
            0,
        ];

        assert_eq!(
            parse_notification(&report, &features),
            Some(Master3sRuntimeEvent::DivertedButtons {
                buttons: vec![Master3sButton::Back, Master3sButton::Gesture],
                unknown_control_ids: vec![0xbeef],
            })
        );
    }

    #[test]
    fn parses_raw_movement_notification() {
        let features = vec![feature(9, HIDPP_FEATURE_REPROG_CONTROLS_V4)];
        let report = [HIDPP_LONG_REPORT_ID, 1, 9, 0x10, 0xff, 0xec, 0x00, 0x19];

        assert_eq!(
            parse_notification(&report, &features),
            Some(Master3sRuntimeEvent::RawMovement { x: -20, y: 25 })
        );
    }

    #[test]
    fn ignores_request_replies_and_unknown_features() {
        let features = vec![feature(16, HIDPP_FEATURE_THUMB_WHEEL)];
        let reply = [HIDPP_SHORT_REPORT_ID, 1, 16, 0x08, 0, 0];
        let unknown_feature = [HIDPP_SHORT_REPORT_ID, 1, 17, 0x00, 0, 0];

        assert_eq!(parse_notification(&reply, &features), None);
        assert_eq!(parse_notification(&unknown_feature, &features), None);
    }

    fn feature(index: u8, feature_id: u16) -> HidppFeatureInfo {
        HidppFeatureInfo {
            index,
            feature_id,
            name: format!("FEATURE_{feature_id:04X}"),
            flags: 0,
            version: 1,
        }
    }
}
