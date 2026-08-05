use dogi_core::{ActiveApplication, DogiError, Result};

pub trait ActiveApplicationProvider {
    fn active_application(&mut self) -> Result<Option<ActiveApplication>>;
}

pub fn active_application() -> Result<Option<ActiveApplication>> {
    SystemActiveApplicationProvider::new().active_application()
}

#[cfg(target_os = "linux")]
mod platform {
    use super::*;
    use std::process::Command;

    pub struct SystemActiveApplicationProvider;

    impl Default for SystemActiveApplicationProvider {
        fn default() -> Self {
            Self::new()
        }
    }

    impl SystemActiveApplicationProvider {
        pub fn new() -> Self {
            Self
        }
    }

    impl ActiveApplicationProvider for SystemActiveApplicationProvider {
        fn active_application(&mut self) -> Result<Option<ActiveApplication>> {
            let root = run_xprop(&["-root", "_NET_ACTIVE_WINDOW"])?;
            let Some(window_id) = parse_active_window_id(&root) else {
                return Ok(None);
            };
            let props = run_xprop(&["-id", &window_id, "WM_CLASS", "_NET_WM_NAME", "WM_NAME"])?;
            Ok(parse_window_properties(&props))
        }
    }

    fn run_xprop(args: &[&str]) -> Result<String> {
        let output = Command::new("xprop").args(args).output().map_err(|error| {
            DogiError::BackendUnavailable(format!(
                "xprop is required for X11 active-window profile detection: {error}"
            ))
        })?;

        if !output.status.success() {
            return Err(DogiError::BackendUnavailable(format!(
                "xprop failed for active-window profile detection: {}",
                String::from_utf8_lossy(&output.stderr).trim()
            )));
        }

        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    }

    pub(crate) fn parse_active_window_id(output: &str) -> Option<String> {
        let value = output.split('#').nth(1)?.trim();
        let window_id = value.split_whitespace().next()?;
        (window_id != "0x0").then(|| window_id.to_owned())
    }

    pub(crate) fn parse_window_properties(output: &str) -> Option<ActiveApplication> {
        let mut title = None;
        let mut class = None;
        let mut executable = None;

        for line in output.lines() {
            if line.starts_with("_NET_WM_NAME") {
                title = parse_xprop_string_value(line).or(title);
            } else if line.starts_with("WM_NAME") && title.is_none() {
                title = parse_xprop_string_value(line);
            } else if line.starts_with("WM_CLASS") {
                let values = parse_xprop_string_list(line);
                executable = values.first().cloned().or(executable);
                class = values.last().cloned().or(class);
            }
        }

        let app = ActiveApplication::new(title, class, executable);
        (app.name.is_some() || app.class.is_some() || app.executable.is_some()).then_some(app)
    }

    fn parse_xprop_string_value(line: &str) -> Option<String> {
        let value = line.split_once('=')?.1.trim();
        parse_quoted_strings(value).into_iter().next()
    }

    fn parse_xprop_string_list(line: &str) -> Vec<String> {
        line.split_once('=')
            .map(|(_, value)| parse_quoted_strings(value))
            .unwrap_or_default()
    }

    fn parse_quoted_strings(value: &str) -> Vec<String> {
        let mut strings = Vec::new();
        let mut current = String::new();
        let mut in_quote = false;
        let mut escaped = false;

        for character in value.chars() {
            if escaped {
                current.push(character);
                escaped = false;
                continue;
            }

            match character {
                '\\' if in_quote => escaped = true,
                '"' if in_quote => {
                    in_quote = false;
                    let trimmed = current.trim();
                    if !trimmed.is_empty() {
                        strings.push(trimmed.to_owned());
                    }
                    current.clear();
                }
                '"' => {
                    in_quote = true;
                    current.clear();
                }
                _ if in_quote => current.push(character),
                _ => {}
            }
        }

        strings
    }

    #[cfg(test)]
    mod tests {
        use super::*;

        #[test]
        fn parses_active_window_id() {
            assert_eq!(
                parse_active_window_id("_NET_ACTIVE_WINDOW(WINDOW): window id # 0x03a00007\n"),
                Some("0x03a00007".to_owned())
            );
            assert_eq!(
                parse_active_window_id("_NET_ACTIVE_WINDOW(WINDOW): window id # 0x0\n"),
                None
            );
        }

        #[test]
        fn parses_window_properties() {
            let app = parse_window_properties(
                "WM_CLASS(STRING) = \"Navigator\", \"firefox\"\n\
                 _NET_WM_NAME(UTF8_STRING) = \"dogi - Mozilla Firefox\"\n",
            )
            .unwrap();

            assert_eq!(app.name.as_deref(), Some("dogi - Mozilla Firefox"));
            assert_eq!(app.class.as_deref(), Some("firefox"));
            assert_eq!(app.executable.as_deref(), Some("Navigator"));
        }

        #[test]
        fn parses_escaped_xprop_strings() {
            assert_eq!(
                parse_quoted_strings("\"one \\\"quoted\\\" value\", \"two\""),
                vec!["one \"quoted\" value".to_owned(), "two".to_owned()]
            );
        }
    }
}

#[cfg(not(target_os = "linux"))]
mod platform {
    use super::*;

    pub struct SystemActiveApplicationProvider;

    impl Default for SystemActiveApplicationProvider {
        fn default() -> Self {
            Self::new()
        }
    }

    impl SystemActiveApplicationProvider {
        pub fn new() -> Self {
            Self
        }
    }

    impl ActiveApplicationProvider for SystemActiveApplicationProvider {
        fn active_application(&mut self) -> Result<Option<ActiveApplication>> {
            Err(DogiError::BackendUnavailable(
                "active-window profile detection is only implemented for Linux X11".to_owned(),
            ))
        }
    }
}

pub use platform::SystemActiveApplicationProvider;
