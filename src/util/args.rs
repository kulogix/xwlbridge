//! A small X11-style argument parser, generated from a struct definition.
//!
//! X servers take single-dash long options (`-display`, `-nolisten`) and stop
//! at the first non-option argument. clap can't express that, so
//! `define_config!` generates a config struct, its parser, and `-help` text
//! (formatted like `Xwayland -help`) straight from the field definitions: the
//! option name is the field name with `_` replaced with `-`, matched
//! case-insensitively, and value/flag handling follows the field's kind.
//! Everything after the first non-option argument (or a `--`) is the command to
//! run. Values may be given as `-opt value` or `-opt=value`.
//!
//! An option that takes a value may be repeated: the same value again is
//! accepted with a warning (once per option), a different one is a usage error
//! naming the option and both values. Value types must be `PartialEq` for that
//! check (values compare as parsed, so `-opt X -opt x` is the same choice for a
//! case-insensitive `choice`, but not for a `value` such as a path).

/// A fixed-choice option value (an enum), for the `choice` field kind.
pub trait ArgEnum: Sized + Copy {
    /// The accepted value strings, in help-display order.
    const VARIANTS: &'static [&'static str];
    /// Parses a value string (case-insensitive); `None` if unrecognised.
    fn from_arg(s: &str) -> Option<Self>;
}

/// The long option name for a field: its name with underscores turned to dashes.
macro_rules! opt_name {
    ($field:ident) => {
        stringify!($field).replace('_', "-")
    };
}

/// The default value of a field, by kind.
macro_rules! arg_default {
    ($ty:ty, flag) => {
        false
    };
    ($ty:ty, value, $vn:literal) => {
        ::core::option::Option::None
    };
    ($ty:ty, choice, $def:literal) => {
        <$ty as $crate::util::ArgEnum>::from_arg($def).expect("invalid default")
    };
}

/// The `-option value` text shown in help, by kind.
macro_rules! arg_usage {
    ($ty:ty, flag, $field:ident) => {
        format!("-{}", opt_name!($field))
    };
    ($ty:ty, value, $field:ident, $vn:literal) => {
        format!("-{} {}", opt_name!($field), $vn)
    };
    ($ty:ty, choice, $field:ident, $def:literal) => {
        format!(
            "-{} [{}]",
            opt_name!($field),
            <$ty as $crate::util::ArgEnum>::VARIANTS.join("|")
        )
    };
}

/// Fetches an option's value: the inline `=value`, else the next argv token.
macro_rules! take_value {
    ($inline:ident, $argv:ident, $i:ident, $field:ident) => {
        match $inline.take() {
            ::core::option::Option::Some(v) => v,
            ::core::option::Option::None => {
                $i += 1;
                match $argv.get($i).and_then(|a| a.to_str()) {
                    ::core::option::Option::Some(v) => v.to_string(),
                    ::core::option::Option::None => {
                        return ::core::result::Result::Err(format!(
                            "option -{} requires a value",
                            opt_name!($field)
                        ));
                    }
                }
            }
        }
    };
}

/// The rule a repeated single-value option shares: `same` tells whether the
/// parsed values agree (evaluated only for a repeat). Disagreement is a usage
/// error naming the option and both values; agreement warns once per option
/// and keeps the first value.
macro_rules! arg_repeat {
    ($seen:ident, $warnings:ident, $name:expr, $raw:expr, $same:expr) => {{
        let name = $name;
        let raw = $raw;
        match $seen.entry(name.clone()) {
            ::std::collections::hash_map::Entry::Vacant(e) => {
                e.insert((raw.to_string(), false));
            }
            ::std::collections::hash_map::Entry::Occupied(mut e) => {
                let (prev, warned) = e.get_mut();
                if !$same {
                    return ::core::result::Result::Err(format!(
                        "option -{name} given twice with different values: {prev:?} and {raw:?}"
                    ));
                }
                if !*warned {
                    *warned = true;
                    $warnings.push(format!(
                        "option -{name} given twice with the same value {prev:?}"
                    ));
                }
            }
        }
    }};
}

/// Applies a parsed option to its field, by kind. `value` and `choice` options
/// go through `arg_repeat!`, the one rule for a repeated single-value option.
macro_rules! arg_apply {
    ($ty:ty, flag, $field:ident, $inline:ident, $argv:ident, $i:ident, $seen:ident, $warnings:ident) => {
        $field = true;
    };
    ($ty:ty, value, $field:ident, $inline:ident, $argv:ident, $i:ident, $seen:ident, $warnings:ident, $vn:literal) => {{
        let raw = take_value!($inline, $argv, $i, $field);
        match raw.parse() {
            ::core::result::Result::Ok(v) => {
                let same = $field.as_ref() == ::core::option::Option::Some(&v);
                arg_repeat!($seen, $warnings, opt_name!($field), &raw, same);
                $field = ::core::option::Option::Some(v);
            }
            ::core::result::Result::Err(_) => {
                return ::core::result::Result::Err(format!(
                    "invalid value {:?} for -{}",
                    raw,
                    opt_name!($field)
                ));
            }
        }
    }};
    ($ty:ty, choice, $field:ident, $inline:ident, $argv:ident, $i:ident, $seen:ident, $warnings:ident, $def:literal) => {{
        let raw = take_value!($inline, $argv, $i, $field);
        match <$ty as $crate::util::ArgEnum>::from_arg(&raw) {
            ::core::option::Option::Some(v) => {
                let same = $field == v;
                arg_repeat!($seen, $warnings, opt_name!($field), &raw, same);
                $field = v;
            }
            ::core::option::Option::None => {
                return ::core::result::Result::Err(format!(
                    "invalid value {:?} for -{} (expected one of: {})",
                    raw,
                    opt_name!($field),
                    <$ty as $crate::util::ArgEnum>::VARIANTS.join(", "),
                ));
            }
        }
    }};
}

/// Generates a configuration struct plus its X11-style parser and help text.
macro_rules! define_config {
    (
        $(#[doc = $sdoc:literal])*
        pub struct $name:ident {
            $(
                $(#[doc = $doc:literal])*
                $field:ident : $ty:ty = $kind:ident $(($karg:literal))?
            ),* $(,)?
        }
    ) => {
        #[derive(Debug)]
        pub struct $name {
            $( pub $field: $ty, )*
            /// The wrapped command and its arguments (everything after the
            /// options, or after a `--`).
            pub command: Vec<std::ffi::OsString>,
        }

        impl $name {
            /// Parses the process arguments X11-style, exiting on `-help`,
            /// `-version`, or a usage error. The warnings [`Self::parse_args`]
            /// hands back (a repeated option given the same value) are logged.
            pub fn parse() -> Self {
                let mut it = std::env::args_os();
                let prog0 = it.next().unwrap_or_default();
                let prog = std::path::Path::new(&prog0)
                    .file_name()
                    .map_or_else(|| env!("CARGO_PKG_NAME").to_string(), |s| s.to_string_lossy().into_owned());
                let argv: Vec<std::ffi::OsString> = it.collect();
                match Self::parse_args(&prog, &argv) {
                    Ok((config, warnings)) => {
                        for w in warnings {
                            $crate::warning!("{w}");
                        }
                        config
                    }
                    Err(msg) => Self::usage_error(&prog, &msg),
                }
            }

            /// The parser proper: `Err` is a usage error's message, `Ok` the
            /// configuration and the warnings to log. Exits on `-help` and
            /// `-version`.
            pub fn parse_args(
                prog: &str,
                argv: &[std::ffi::OsString],
            ) -> Result<(Self, Vec<String>), String> {
                $( let mut $field = arg_default!($ty, $kind $(, $karg)?); )*
                let mut command: Vec<std::ffi::OsString> = Vec::new();
                // The first raw value each single-value option was given, and
                // whether its repeat warning has fired (`arg_repeat!`).
                let mut seen: std::collections::HashMap<String, (String, bool)> =
                    std::collections::HashMap::new();
                let mut warnings: Vec<String> = Vec::new();

                let mut i = 0;
                'next: while i < argv.len() {
                    // Non-UTF-8 can't be one of our options; treat as command.
                    let Some(s) = argv[i].to_str() else { break };
                    if s == "--" {
                        i += 1;
                        break;
                    }
                    // First non-option argument: the command starts here.
                    let body = match s.strip_prefix('-') {
                        Some(b) if !b.is_empty() => b,
                        _ => break,
                    };
                    let (name, mut inline) = match body.split_once('=') {
                        Some((n, v)) => (n.to_ascii_lowercase(), Some(v.to_string())),
                        None => (body.to_ascii_lowercase(), None),
                    };
                    if name == "help" || name == "h" {
                        print!("{}", Self::help(prog));
                        std::process::exit(0);
                    }
                    if name == "version" || name == "v" {
                        println!("{} {}", env!("CARGO_PKG_NAME"), env!("CARGO_PKG_VERSION"));
                        println!("{}", env!("CARGO_PKG_REPOSITORY"));
                        std::process::exit(0);
                    }
                    $(
                        if name == opt_name!($field) {
                            arg_apply!($ty, $kind, $field, inline, argv, i, seen, warnings $(, $karg)?);
                            let _ = &mut inline;
                            i += 1;
                            continue 'next;
                        }
                    )*
                    return Err(format!("unknown option -{name}"));
                }
                command.extend_from_slice(&argv[i..]);
                Ok((Self { $( $field, )* command }, warnings))
            }

            fn usage_error(prog: &str, msg: &str) -> ! {
                eprintln!("{prog}: {msg}");
                eprintln!("Try '{prog} -help' for more information.");
                std::process::exit(2);
            }

            /// `Xwayland -help`-style usage text.
            fn help(prog: &str) -> String {
                let mut rows: Vec<(String, String)> = vec![
                    $((
                        arg_usage!($ty, $kind, $field $(, $karg)?),
                        [$($doc.trim()),*].join(" "),
                    ),)*
                ];
                rows.push(("-help".to_string(), "print this message".to_string()));
                rows.push(("-version".to_string(), "show the version and exit".to_string()));

                let mut out = format!("use: {prog} [options] command [args ...]\n");
                out.push_str(&format!("     {prog} run [-display N] [--] command [args ...]   (an X tool on a running bridge's display; {prog} run -help)\n"));
                for (opt, desc) in rows {
                    let pad = 45usize.saturating_sub(opt.len()).max(1);
                    out.push_str(&format!("{opt}{}{desc}\n", " ".repeat(pad)));
                }
                out
            }
        }
    };
}
