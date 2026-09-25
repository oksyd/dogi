use crate::domain::{AppProfile, ButtonBinding, GestureBindings, Master3sButton, Master3sSettings};

/// Applies only edits relative to the provisional baseline onto the identified device's settings.
pub(super) fn rebase(
    base: &Master3sSettings,
    draft: &Master3sSettings,
    saved: &Master3sSettings,
) -> Master3sSettings {
    macro_rules! field {
        ($name:ident) => {
            merge(&base.$name, &draft.$name, &saved.$name)
        };
    }

    Master3sSettings {
        profile_name: field!(profile_name),
        pointer_speed_percent: field!(pointer_speed_percent),
        smart_shift_enabled: field!(smart_shift_enabled),
        smart_shift_threshold: field!(smart_shift_threshold),
        ratchet_mode: field!(ratchet_mode),
        high_resolution_scroll: field!(high_resolution_scroll),
        natural_scroll: field!(natural_scroll),
        thumb_wheel: field!(thumb_wheel),
        thumb_wheel_speed_percent: field!(thumb_wheel_speed_percent),
        buttons: Master3sButton::ALL
            .into_iter()
            .map(|button| ButtonBinding {
                button,
                action: merge(
                    &base.button_action(button),
                    &draft.button_action(button),
                    &saved.button_action(button),
                ),
            })
            .collect(),
        gestures: merge_gestures(&base.gestures, &draft.gestures, &saved.gestures),
        app_profiles: merge_profiles(&base.app_profiles, &draft.app_profiles, &saved.app_profiles),
    }
    .normalized()
}

fn merge<T: Clone + PartialEq>(base: &T, draft: &T, saved: &T) -> T {
    if draft != base {
        draft.clone()
    } else {
        saved.clone()
    }
}

fn merge_gestures(
    base: &GestureBindings,
    draft: &GestureBindings,
    saved: &GestureBindings,
) -> GestureBindings {
    GestureBindings {
        threshold: merge(&base.threshold, &draft.threshold, &saved.threshold),
        click: merge(&base.click, &draft.click, &saved.click),
        up: merge(&base.up, &draft.up, &saved.up),
        down: merge(&base.down, &draft.down, &saved.down),
        left: merge(&base.left, &draft.left, &saved.left),
        right: merge(&base.right, &draft.right, &saved.right),
    }
}

fn merge_profiles(
    base: &[AppProfile],
    draft: &[AppProfile],
    saved: &[AppProfile],
) -> Vec<AppProfile> {
    let key = |profile: &AppProfile| super::normalize_app_profile_key(&profile.name);
    let mut merged = saved.to_vec();
    // An editor submission replaces one named profile; other saved profiles keep their order.
    merged.retain(|profile| {
        !base.iter().any(|old| key(old) == key(profile))
            || draft.iter().any(|edited| key(edited) == key(profile))
    });
    for profile in draft {
        if base.iter().find(|old| key(old) == key(profile)) == Some(profile) {
            continue;
        }
        if let Some(existing) = merged.iter_mut().find(|saved| key(saved) == key(profile)) {
            *existing = profile.clone();
        } else {
            merged.push(profile.clone());
        }
    }
    merged
}

#[cfg(test)]
mod tests {
    use crate::domain::{
        Action, ApplicationMatchField, ApplicationMatcher, ButtonAction, WheelRatchetMode,
    };

    use super::*;

    #[test]
    fn editing_one_control_preserves_other_saved_controls() {
        let base = Master3sSettings::default();
        let mut saved = base.clone();
        saved.thumb_wheel_speed_percent = 392;
        saved.ratchet_mode = WheelRatchetMode::FreeSpin;
        saved.gestures.threshold = 90;
        saved.gestures.left = Action::Copy;
        saved.set_button_action(Master3sButton::Forward, ButtonAction::Action(Action::Paste));
        let saved = saved.normalized();
        let mut draft = base.clone();
        draft.smart_shift_threshold = 35;
        draft.gestures.up = Action::MiddleClick;
        draft.set_button_action(Master3sButton::Back, ButtonAction::Action(Action::Copy));

        let merged = rebase(&base, &draft, &saved);

        let mut expected = saved;
        expected.smart_shift_threshold = 35;
        expected.gestures.up = Action::MiddleClick;
        expected.set_button_action(Master3sButton::Back, ButtonAction::Action(Action::Copy));
        assert_eq!(merged, expected);
    }

    #[test]
    fn explicit_speed_edit_wins_and_reverted_edits_do_not_override_saved_settings() {
        let base = Master3sSettings::default();
        let saved = Master3sSettings {
            pointer_speed_percent: 135,
            thumb_wheel_speed_percent: 392,
            ..base.clone()
        };
        let draft = Master3sSettings {
            thumb_wheel_speed_percent: 225,
            ..base.clone()
        };
        let merged = rebase(&base, &draft, &saved);
        assert_eq!(merged.thumb_wheel_speed_percent, 225);
        assert_eq!(merged.pointer_speed_percent, 135);
        assert_eq!(rebase(&base, &base, &saved), saved);
    }

    #[test]
    fn profile_edits_preserve_unrelated_saved_profiles() {
        let profile = |name: &str, speed: u8| AppProfile {
            name: name.to_owned(),
            matcher: ApplicationMatcher {
                field: ApplicationMatchField::Executable,
                value: name.to_owned(),
            },
            overrides: crate::domain::AppProfileOverrides {
                pointer_speed_percent: Some(speed),
                ..Default::default()
            },
        };
        let base = vec![profile("Firefox", 100), profile("Remove", 100)];
        let draft = vec![profile(" firefox ", 125), profile("New", 130)];
        let saved = vec![
            profile("Terminal", 90),
            profile("Firefox", 110),
            profile("Remove", 120),
        ];

        assert_eq!(
            merge_profiles(&base, &draft, &saved),
            vec![
                profile("Terminal", 90),
                profile(" firefox ", 125),
                profile("New", 130)
            ]
        );
    }
}
