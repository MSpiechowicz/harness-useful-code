//! What the commands share: their options and the usage line.
//!
//! Usage errors exit 2, failures (the data folder is unknown, the switch file cannot be written) exit 1 with
//! the reason on stderr.

use std::process::ExitCode;

/// The arguments of a command, read against the options it takes.
#[derive(Debug, Default)]
pub(crate) struct Args<'a> {
    /// The arguments that do not start with `-`, in order.
    pub positional: Vec<&'a str>,
    flags: Vec<&'a str>,
    values: Vec<(&'a str, &'a str)>,
}

impl<'a> Args<'a> {
    /// Reads `args`: the `flags` stand alone, the `valued` options take a value (`--name VALUE`
    /// or `--name=VALUE`) and may be given once. `None` for an unknown or repeated option, a missing value, or an
    /// argument that is not UTF-8.
    pub(crate) fn parse(args: &[Option<&'a str>], flags: &[&str], valued: &[&str]) -> Option<Self> {
        Self::parse_repeating(args, flags, valued, &[])
    }

    /// Like [`Self::parse`], and the `repeated` options take a value too and may be given any number of times
    /// (read them with [`Self::values`]).
    pub(crate) fn parse_repeating(
        args: &[Option<&'a str>],
        flags: &[&str],
        valued: &[&str],
        repeated: &[&str],
    ) -> Option<Self> {
        let mut parsed = Self::default();
        let mut rest = args.iter().copied();

        while let Some(arg) = rest.next() {
            let arg = arg?;

            if !arg.starts_with('-') {
                parsed.positional.push(arg);
                continue;
            }

            if flags.contains(&arg) {
                if parsed.flag(arg) {
                    return None;
                }

                parsed.flags.push(arg);
                continue;
            }

            let (name, value) = match arg.split_once('=') {
                Some((name, value)) => (name, value),
                None => (arg, rest.next().flatten()?),
            };

            let once = valued.contains(&name);

            if !(once || repeated.contains(&name)) || (once && parsed.value(name).is_some()) {
                return None;
            }

            parsed.values.push((name, value));
        }

        Some(parsed)
    }

    /// True when the flag `name` was given.
    pub(crate) fn flag(&self, name: &str) -> bool {
        self.flags.contains(&name)
    }

    /// The value of the option `name`, when given.
    pub(crate) fn value(&self, name: &str) -> Option<&'a str> {
        self.values
            .iter()
            .find(|(given, _)| *given == name)
            .map(|(_, value)| *value)
    }

    /// Every value of the option `name`, in the order given.
    pub(crate) fn values(&self, name: &str) -> Vec<&'a str> {
        self.values
            .iter()
            .filter(|(given, _)| *given == name)
            .map(|(_, value)| *value)
            .collect()
    }
}

/// Prints the usage of `commands`; exit 2.
pub fn usage(commands: &str) -> ExitCode {
    eprintln!("usage: harness-code {commands}");
    ExitCode::from(2)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn args<'a>(raw: &[&'a str]) -> Vec<Option<&'a str>> {
        raw.iter().map(|arg| Some(*arg)).collect()
    }

    #[test]
    fn options_take_values_either_way_and_never_twice() {
        let raw = args(&["list", "--json", "--project", "/w", "--query=a b", "7"]);
        let parsed = Args::parse(&raw, &["--json"], &["--project", "--query"]).expect("valid");

        assert_eq!(parsed.positional, ["list", "7"]);
        assert!(parsed.flag("--json"));
        assert_eq!(parsed.value("--project"), Some("/w"));
        assert_eq!(parsed.value("--query"), Some("a b"));

        for bad in [
            &["--json", "--json"][..],
            &["--project"],
            &["--project", "/a", "--project=/b"],
            &["--yaml"],
            &["--json=1"],
        ] {
            assert!(
                Args::parse(&args(bad), &["--json"], &["--project"]).is_none(),
                "{bad:?}"
            );
        }

        assert!(Args::parse(&[None], &[], &[]).is_none(), "not UTF-8");
    }

    #[test]
    fn repeated_options_are_collected_in_order() {
        let raw = args(&["--name", "a", "--path=b", "--name=c", "--json"]);
        let parsed = Args::parse_repeating(&raw, &["--json"], &[], &["--name", "--path"]).expect("valid");

        assert_eq!(parsed.values("--name"), ["a", "c"]);
        assert_eq!(parsed.values("--path"), ["b"]);
        assert!(parsed.values("--other").is_empty());
        assert!(Args::parse_repeating(&args(&["--other", "x"]), &[], &[], &["--name"]).is_none());
        assert!(Args::parse_repeating(&args(&["--name"]), &[], &[], &["--name"]).is_none());
    }
}
