use dogi_core::{
    DeviceSettingValue, DogiError, HidppFeature, Master3sSettings, Result, SettingsApplyOperation,
    SettingsApplyPlan, SettingsApplyPreview, SettingsApplyPreviewStep, build_master3s_apply_plan,
};
use serde::{Deserialize, Serialize};

pub(crate) const FORMAT_VERSION: u8 = 2;

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedDeviceIdentity {
    pub(crate) receiver_id: String,
    pub(crate) receiver_vendor_id: u16,
    pub(crate) receiver_product_id: u16,
    pub(crate) receiver_serial: Option<String>,
    pub(crate) slot: u8,
    pub(crate) wpid: Option<String>,
    #[serde(default)]
    pub(crate) pairing_serial: Option<String>,
    pub(crate) unit_id: Option<String>,
    pub(crate) model_id: Option<String>,
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct PreparedSettingsTransaction {
    pub(crate) version: u8,
    pub(crate) device_id: String,
    pub(crate) identity: PreparedDeviceIdentity,
    pub(crate) profile_name: String,
    pub(crate) changes: Vec<PreparedHidChange>,
}

impl PreparedSettingsTransaction {
    pub fn device_id(&self) -> &str {
        &self.device_id
    }

    pub fn profile_name(&self) -> &str {
        &self.profile_name
    }

    pub fn stable_device_key(&self) -> String {
        if let Some(unit) = strong_identifier(self.identity.unit_id.as_deref()) {
            return format!("unit-{unit}");
        }
        match (
            strong_identifier(self.identity.receiver_serial.as_deref()),
            strong_identifier(self.identity.pairing_serial.as_deref()),
        ) {
            (Some(receiver), Some(pairing)) => format!("receiver-{receiver}-pairing-{pairing}"),
            _ => "unrecoverable".to_owned(),
        }
    }

    pub fn is_empty(&self) -> bool {
        self.changes.is_empty()
    }

    pub fn preview(&self) -> SettingsApplyPreview {
        SettingsApplyPreview {
            device_id: self.device_id.clone(),
            profile_name: self.profile_name.clone(),
            steps: self
                .changes
                .iter()
                .map(|change| SettingsApplyPreviewStep {
                    operation: change.operation.clone(),
                    feature: change.feature,
                    before: change.before_value.clone(),
                    after: change.after_value.clone(),
                })
                .collect(),
        }
    }
}

pub(crate) fn validate_recoverable_identity(
    transaction: &PreparedSettingsTransaction,
) -> Result<()> {
    let is_strong = strong_identifier(transaction.identity.unit_id.as_deref()).is_some()
        || (strong_identifier(transaction.identity.receiver_serial.as_deref()).is_some()
            && strong_identifier(transaction.identity.pairing_serial.as_deref()).is_some());
    if !is_strong {
        return Err(DogiError::InvalidArgument(
            "settings transaction has no physical device instance identifier".to_owned(),
        ));
    }
    Ok(())
}

fn strong_identifier(value: Option<&str>) -> Option<String> {
    normalized_identifier(value).filter(|value| value.chars().any(|character| character != '0'))
}

pub(crate) fn normalized_identifier(value: Option<&str>) -> Option<String> {
    value
        .map(|value| {
            value
                .chars()
                .filter(|character| character.is_ascii_alphanumeric())
                .flat_map(char::to_uppercase)
                .collect::<String>()
        })
        .filter(|value| !value.is_empty())
}

#[derive(Clone, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct PreparedHidChange {
    pub(crate) title: String,
    pub(crate) operation: SettingsApplyOperation,
    pub(crate) feature: HidppFeature,
    pub(crate) feature_id: u16,
    pub(crate) read_function: u8,
    pub(crate) read_payload: Vec<u8>,
    pub(crate) write_function: u8,
    pub(crate) before_write: Vec<u8>,
    pub(crate) after_write: Vec<u8>,
    pub(crate) verification: Vec<VerificationByte>,
    pub(crate) before_value: DeviceSettingValue,
    pub(crate) after_value: DeviceSettingValue,
}

#[derive(Clone, Copy, Debug, Eq, PartialEq, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub(crate) struct VerificationByte {
    pub(crate) response_index: usize,
    pub(crate) mask: u8,
    pub(crate) before: u8,
    pub(crate) after: u8,
}

pub(crate) fn validate_apply_plan(
    settings: &Master3sSettings,
    plan: &SettingsApplyPlan,
) -> Result<()> {
    if plan.profile_name != settings.profile_name {
        return Err(DogiError::InvalidArgument(format!(
            "apply plan profile {:?} does not match settings profile {:?}",
            plan.profile_name, settings.profile_name
        )));
    }

    let canonical = build_master3s_apply_plan(&plan.device_id, settings);
    let mut consumed = vec![false; canonical.steps.len()];
    for step in &plan.steps {
        let matching_index = canonical
            .steps
            .iter()
            .enumerate()
            .find(|(index, candidate)| {
                !consumed[*index] && apply_step_matches_canonical(step, candidate)
            })
            .map(|(index, _)| index)
            .ok_or_else(|| {
                DogiError::InvalidArgument(format!(
                    "apply plan contains an operation that is inconsistent with the canonical settings: {}",
                    step.title()
                ))
            })?;
        consumed[matching_index] = true;
    }

    Ok(())
}

fn apply_step_matches_canonical(
    step: &dogi_core::SettingsApplyStep,
    canonical: &dogi_core::SettingsApplyStep,
) -> bool {
    if step.feature != canonical.feature
        || step.requires_device_write != canonical.requires_device_write
    {
        return false;
    }

    match (&step.operation, &canonical.operation) {
        (
            SettingsApplyOperation::ScrollBehavior {
                high_resolution,
                natural,
            },
            SettingsApplyOperation::ScrollBehavior {
                high_resolution: canonical_high_resolution,
                natural: canonical_natural,
            },
        ) => {
            (high_resolution.is_some() || natural.is_some())
                && high_resolution.is_none_or(|value| Some(value) == *canonical_high_resolution)
                && natural.is_none_or(|value| Some(value) == *canonical_natural)
        }
        _ => step.operation == canonical.operation,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rejects_values_that_do_not_match_the_settings() {
        let settings = Master3sSettings::default();
        let mut plan = build_master3s_apply_plan("device-a", &settings);
        plan.steps[0].operation = SettingsApplyOperation::PointerSpeed { percent: 200 };

        let error = validate_apply_plan(&settings, &plan).unwrap_err();
        assert!(matches!(error, DogiError::InvalidArgument(_)));
    }

    #[test]
    fn accepts_a_partial_scroll_diff() {
        let baseline = Master3sSettings::default();
        let target = Master3sSettings {
            natural_scroll: !baseline.natural_scroll,
            ..baseline.clone()
        };
        let plan = dogi_core::build_master3s_device_diff_plan("device-a", &baseline, &target);

        validate_apply_plan(&target, &plan).unwrap();
    }
}
